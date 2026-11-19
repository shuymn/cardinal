//! Executions on a full volume (C11; `docs/architecture.md` sections 9.7 and 13.7): a write
//! the deletion or its recovery needs that fails with `ENOSPC` or `EDQUOT` deletes the state
//! dir's reserve and is retried at once; without a reserve, a failed put-back or result leaves
//! the claim to a later recovery. Unit X1b injects the failures at a put-back and at the
//! result through the fake of section 13.7, in recovery of a `committed` execution
//! (`tests/execution_support`); X1c adds the execution's own writes through a granted
//! owner's `act` (`tests/live_support`), and X2 the pass and the real volumes (section 14.2).
#![cfg(unix)]

mod execution_support;
mod live_support;
mod observe_common;
mod plan_support;
mod support;

use std::time::{Duration, SystemTime};

use execution_support::Execution;

#[test]
fn helper() {
    execution_support::run_if_requested();
    support::helper::run_if_requested();
}

const ERRORS: [&str; 2] = ["ENOSPC", "EDQUOT"];

/// A `committed` execution whose `b/h` changed its mtime, so the deletion puts it back.
fn with_a_put_back() -> Execution {
    let mut x = Execution::new("node_modules", &["a/f", "b/h"]);
    x.commit();
    let file = std::fs::File::options()
        .write(true)
        .open(x.t().join("b/h"))
        .unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
    x
}

/// With the reserve present, a put-back that fails for lack of space deletes it and is
/// retried at once: the entry goes back, and the execution finishes.
#[test]
fn a_put_back_spends_the_reserve() {
    for error in ERRORS {
        let x = with_a_put_back();
        x.write_reserve();
        x.fail_write("put_back", error, "once");
        let got = x.recover(true);
        assert_eq!(got["outcome"], "done", "{error}: {got}");
        assert_eq!(got["result"]["status"], "partial");
        assert_eq!(std::fs::read(x.t().join("b/h")).unwrap(), b"b/h");
        assert!(!x.state.reserve_file().exists(), "{error}");
    }
}

/// Without a reserve, a put-back that fails for lack of space stops the deletion and leaves
/// the claim to recovery, the entry in the hold; a later recovery puts it back and finishes.
#[test]
fn a_put_back_without_a_reserve_leaves_the_claim() {
    for error in ERRORS {
        let x = with_a_put_back();
        x.fail_write("put_back", error, "always");
        let got = x.recover(true);
        assert_eq!(got["outcome"], "stranded", "{error}: {got}");
        assert!(got["why"].as_str().unwrap().contains(error), "{got}");
        assert!(!x.state.execution_result(&x.plan).exists());
        assert_eq!(std::fs::read(x.held("b/h")).unwrap(), b"b/h");
        std::fs::remove_file(x.points.dir().join("write-failures")).unwrap();
        let again = x.recover(true);
        assert_eq!(again["result"]["status"], "partial", "{error}: {again}");
        assert_eq!(std::fs::read(x.t().join("b/h")).unwrap(), b"b/h");
    }
}

/// With the reserve present, a claim's rename that fails for lack of space deletes it and is
/// tried again at once, which publishes the claim; without a reserve, the claim is not
/// published, and its temporary file is gone.
#[test]
fn a_claims_rename_spends_the_reserve() {
    for error in ERRORS {
        for reserve in [true, false] {
            let mut x = Execution::new("node_modules", &["a/f"]);
            x.exclude();
            x.manifest = x.walk_manifest();
            if reserve {
                x.write_reserve();
            }
            x.fail_write("claim_rename", error, "once");
            let got = x.publish(&x.claim(cardinal::contract::ClaimStage::Committed));
            let claim = x.state.execution_claim(&x.plan);
            if reserve {
                assert_eq!(got["outcome"], "durable", "{error}: {got}");
                assert!(claim.exists());
                assert!(!x.state.reserve_file().exists(), "{error}");
            } else {
                assert_eq!(got["outcome"], "error", "{error}: {got}");
                assert!(got["why"].as_str().unwrap().contains(error), "{got}");
                assert!(!claim.exists());
                let tmp = std::fs::read_dir(x.state.tmp_dir()).map_or(0, Iterator::count);
                assert_eq!(tmp, 0, "{error}: a temporary file is left");
            }
        }
    }
}

/// With the reserve present, a result that fails for lack of space deletes it and is written
/// at once.
#[test]
fn the_result_spends_the_reserve() {
    for error in ERRORS {
        let mut x = Execution::new("node_modules", &["a/f"]);
        x.commit();
        x.write_reserve();
        x.fail_write("result", error, "once");
        let got = x.recover(true);
        assert_eq!(got["outcome"], "done", "{error}: {got}");
        assert!(x.state.execution_result(&x.plan).exists());
        assert!(!x.state.reserve_file().exists());
    }
}

/// Without a reserve, a result that cannot be written leaves the claim to recovery, which
/// writes it once the volume has room.
#[test]
fn the_result_without_a_reserve_leaves_the_claim() {
    for error in ERRORS {
        let mut x = Execution::new("node_modules", &["a/f"]);
        x.commit();
        x.fail_write("result", error, "always");
        let got = x.recover(true);
        assert_eq!(got["outcome"], "stranded", "{error}: {got}");
        assert!(!x.state.execution_result(&x.plan).exists());
        assert!(x.state.execution_claim(&x.plan).exists());
        std::fs::remove_file(x.points.dir().join("write-failures")).unwrap();
        let again = x.recover(true);
        assert_eq!(again["outcome"], "done", "{error}: {again}");
        assert_eq!(again["result"]["status"], "executed");
    }
}

/// The execution's own writes (section 13.7, "Full volumes"): the execution directory's
/// creation, the manifest, the claim, the move, the commit, and a restore.
mod live {
    use cardinal::contract::ActStatus;
    use cardinal::testpoint;
    use live_support::{Live, assert_restored};

    use super::ERRORS;
    use crate::live_support;

    /// The writes before the commit, each of which stops an execution without a reserve
    /// before anything stays moved.
    const BEFORE_THE_COMMIT: [&str; 5] = ["execution_dir", "manifest", "claim", "move", "commit"];

    /// With the reserve present, each write that fails once for lack of space deletes it and
    /// is retried at once, and the execution completes.
    #[test]
    fn each_write_spends_the_reserve() {
        for kind in BEFORE_THE_COMMIT {
            for error in ERRORS {
                let x = Live::new("reserve", "target", &["debug/app"]);
                x.write_reserve();
                let result = x.spawn_failing_write(&[], kind, error, "once").finish(&[]);
                assert_eq!(
                    result.status,
                    ActStatus::Executed,
                    "{kind} {error}: {result:#?}"
                );
                assert!(!x.target().exists(), "{kind} {error}");
                assert!(!x.s.state.reserve_file().exists(), "{kind} {error}");
            }
        }
    }

    /// Without a reserve, a write before the commit that fails for lack of space gives
    /// `rejected` (`no_space`) with the target in place: before the move nothing moved, and
    /// at the commit the target is restored.
    #[test]
    fn without_a_reserve_nothing_stays_moved() {
        for kind in BEFORE_THE_COMMIT {
            for error in ERRORS {
                let x = Live::new("no-reserve", "target", &["debug/app"]);
                let before = x.files();
                let result = x
                    .spawn_failing_write(&[], kind, error, "always")
                    .finish(&[]);
                assert_restored(&x, &result, ActStatus::Rejected, &["no_space"], &before);
            }
        }
    }

    /// A restore that fails for lack of space with the reserve present spends it and goes
    /// back; without one, `t` stays under its `claimed` claim with `reclaim_stranded`, `act`
    /// reports `rejected` (`no_space`) naming the trash entry, and a later `act` restores it.
    #[test]
    fn a_restore_without_space() {
        for (reserve, times) in [(true, "once"), (false, "always")] {
            let x = Live::new("restore-space", "target", &["debug/app", "debug/log"]);
            if reserve {
                x.write_reserve();
            }
            let paused = x.spawn_failing_write(&[testpoint::ACT_MOVED], "restore", "ENOSPC", times);
            paused.wait(testpoint::ACT_MOVED);
            std::fs::write(x.t().join("debug/log"), "changed").unwrap();
            let result = paused.finish(&[testpoint::ACT_MOVED]);
            assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
            if reserve {
                assert_eq!(result.reasons, ["changed_during_act"], "{result:#?}");
                assert!(x.target().join("debug/app").exists());
                assert!(!x.s.state.reserve_file().exists());
                continue;
            }
            assert_eq!(result.reasons, ["no_space"], "{result:#?}");
            assert_eq!(result.trash, Some(x.entry().display().to_string()));
            assert!(x.t().join("debug/app").exists());
            assert!(!x.target().exists());
            let again = x.act();
            assert_eq!(again.status, ActStatus::Rejected, "{again:#?}");
            assert_eq!(again.reasons, ["interrupted"], "{again:#?}");
            assert_eq!(
                std::fs::read_to_string(x.target().join("debug/log")).unwrap(),
                "changed"
            );
            assert!(!x.entry().exists());
        }
    }
}
