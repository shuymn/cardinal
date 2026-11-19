//! Executions on a full volume (C11; `docs/architecture.md` sections 9.7 and 13.7): a write
//! the deletion or its recovery needs that fails with `ENOSPC` or `EDQUOT` deletes the state
//! dir's reserve and is retried at once; without a reserve, a failed put-back or result leaves
//! the claim to a later recovery. Unit X1b injects the failures at a put-back and at the
//! result through the fake of section 13.7, in recovery of a `committed` execution
//! (`tests/execution_support`); X1c adds the execution's own writes, and X2 the pass and the
//! real volumes (section 14.2).
#![cfg(unix)]

mod execution_support;
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
