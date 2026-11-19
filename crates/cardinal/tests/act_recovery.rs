//! Recovery of executions that died (A29, A37; `docs/architecture.md` sections 9.7 and 13.7):
//! the recovery table's decisions for absent, unreadable, and `committed` claims, run through
//! the library in a helper process on the states the crashes of section 13.7 leave, which
//! each test writes (`tests/execution_support`), and a second recovery kept out while one
//! holds the plan's `act.lock` at `recover.locked`; and, through `act` (unit X1c), SIGKILL at
//! every pause point of a live execution followed by `act` on the same plan, the `claimed`
//! rows, an unreadable claim followed by a new plan that executes, and `act` kept out at
//! `recover.locked`; and, through a `reconcile` pass (unit X2), the same crashes, a trash entry
//! that no execution names, reported and kept, and the pass's reclaim budget.
#![cfg(unix)]

mod disk_support;
mod execution_support;
mod live_support;
mod observe_common;
mod plan_support;
mod support;

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, SystemTime};

use cardinal::testpoint;
use execution_support::{Execution, outcome};

#[test]
fn helper() {
    execution_support::run_if_requested();
    support::helper::run_if_requested();
}

const WAIT: Duration = Duration::from_secs(30);

/// A `node_modules` with nested directories, a link, and files at several depths.
fn committed() -> Execution {
    let mut x = Execution::new(
        "node_modules",
        &["a/f1", "a/f2", "a/deep/g", "b/h", "c", "empty/"],
    );
    std::os::unix::fs::symlink("a/f1", x.target().join("link")).unwrap();
    x.commit();
    x
}

/// The result recovery wrote, checked against the outcome it reported.
fn result(x: &Execution, outcome: &serde_json::Value) -> serde_json::Value {
    assert_eq!(outcome["outcome"], "done", "{outcome}");
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(x.state.execution_result(&x.plan)).unwrap()).unwrap();
    assert_eq!(written, outcome["result"]);
    assert_eq!(written["recovered"], true);
    assert_eq!(written["plan_id"], x.plan.as_str());
    written
}

/// Absent claim: nothing was changed, since the claim precedes every change, so recovery
/// changes nothing either, a manifest written before the claim included.
#[test]
fn an_absent_claim_changes_nothing() {
    let mut x = Execution::new("node_modules", &["a/f"]);
    x.exclude();
    x.manifest = x.walk_manifest();
    cardinal::act::recover::write_manifest(&x.state, &x.plan, &x.manifest).unwrap();
    let before = x.listing();
    assert_eq!(x.recover(true)["outcome"], "absent");
    assert_eq!(x.listing(), before);
}

/// Unreadable claim: nothing is touched, and the outcome names the plan's trash entry, for
/// an empty claim, a torn one, and one of another plan.
#[test]
fn an_unreadable_claim_touches_nothing() {
    let mut x = committed();
    let claim = x.state.execution_claim(&x.plan);
    let whole = std::fs::read(&claim).unwrap();
    let mut other = x.claim(cardinal::contract::ClaimStage::Committed);
    other.plan_id = "0000000000000000".into();
    for bytes in [
        Vec::new(),
        whole[..whole.len() / 2].to_vec(),
        serde_json::to_vec(&other).unwrap(),
    ] {
        std::fs::write(&claim, &bytes).unwrap();
        let before = x.listing();
        let got = x.recover(true);
        assert_eq!(got["outcome"], "unreadable", "{got}");
        assert_eq!(got["trash"], x.entry().display().to_string());
        assert_eq!(x.listing(), before);
    }
    // Without a trash entry, none is named.
    std::fs::rename(x.t(), x.target()).unwrap();
    std::fs::remove_dir(x.h()).unwrap();
    std::fs::remove_dir(x.entry()).unwrap();
    let got = x.recover(true);
    assert_eq!(got["outcome"], "unreadable");
    assert!(got["trash"].is_null(), "{got}");
    x.manifest.clear();
}

/// `committed` at `act.committed`, nothing taken: recovery deletes every entry, removes `t`,
/// `h`, and the trash entry, and writes `executed`, leaving the worktree without its target.
/// The result lists the processes its scans could not read in `not_in_use`.
#[test]
fn committed_finishes_the_deletion() {
    let x = committed();
    let got = x.recover(true);
    let result = result(&x, &got);
    assert_eq!(result["status"], "executed", "{result}");
    assert_eq!(result["skipped_changed"], serde_json::json!([]));
    assert!(result["trash"].is_null());
    assert_eq!(result["checks"][0]["name"], "not_in_use", "{result}");
    assert_eq!(
        result["checks"][0]["facts"][0]["name"],
        "in_use.descriptors.unscanned"
    );
    assert!(!x.entry().exists());
    assert!(!x.target().exists());
    assert!(x.worktree.is_dir());
    // A second recovery finds the result and touches nothing.
    let before = x.listing();
    assert_eq!(x.recover(true)["outcome"], "finished");
    assert_eq!(x.listing(), before);
}

/// `committed` at `act.taken` and `act.held`: the entry left in the hold goes back by its
/// index and is deleted as live deletion deletes it, a directory's entries included.
#[test]
fn a_held_entry_is_put_back_and_deleted() {
    for held in ["a/f2", "a/deep", "c", "link"] {
        let x = committed();
        x.take(held);
        let result = result(&x, &x.recover(true));
        assert_eq!(result["status"], "executed", "{held}: {result}");
        assert!(!x.entry().exists(), "{held}");
    }
}

/// `committed` at `act.finishing`: `t` is gone and `h` and the trash entry are left; recovery
/// removes them and writes the result.
#[test]
fn an_interrupted_cleanup_is_finished() {
    let x = committed();
    let deleted = x.recover(true);
    assert_eq!(deleted["outcome"], "done");
    // The state a crash at `act.finishing` leaves.
    std::fs::remove_file(x.state.execution_result(&x.plan)).unwrap();
    std::fs::create_dir_all(x.h()).unwrap();
    let result = result(&x, &x.recover(true));
    assert_eq!(result["status"], "executed");
    assert!(!x.entry().exists());
}

/// `committed` without a trash entry: the deletion and its cleanup were done, and recovery
/// writes the result, counting no bytes of its own.
#[test]
fn committed_without_a_trash_entry_writes_the_result() {
    let x = committed();
    std::fs::remove_dir_all(x.t()).unwrap();
    std::fs::remove_dir(x.h()).unwrap();
    std::fs::remove_dir(x.entry()).unwrap();
    let result = result(&x, &x.recover(true));
    assert_eq!(result["status"], "executed");
    assert!(
        result["freed_bytes"] == 0 || result["freed_bytes"].is_null(),
        "{result}"
    );
}

/// An entry of the hold whose path inside `t` now runs through a replaced directory stays in
/// the hold, and so does a name of the hold that is no manifest index; the rest is deleted,
/// and the result is `partial` with what stayed and the trash entry.
#[test]
fn what_cannot_go_back_stays_in_the_hold() {
    let x = committed();
    x.take("a/deep/g");
    // `a` replaced: moved away and made again at its name, with the held entry's directory.
    std::fs::create_dir_all(x.scope.path("outside")).unwrap();
    std::fs::rename(x.t().join("a"), x.scope.path("outside/a")).unwrap();
    std::fs::create_dir_all(x.t().join("a/deep")).unwrap();
    std::fs::write(x.h().join("stray"), b"not an index").unwrap();
    let result = result(&x, &x.recover(true));
    assert_eq!(result["status"], "partial", "{result}");
    assert_eq!(result["trash"], x.entry().display().to_string());
    let kept: Vec<&str> = result["skipped_changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert!(kept.contains(&"a/deep/g"), "{kept:?}");
    assert!(kept.contains(&"a"), "{kept:?}");
    assert_eq!(std::fs::read(x.held("a/deep/g")).unwrap(), b"a/deep/g");
    assert_eq!(std::fs::read(x.h().join("stray")).unwrap(), b"not an index");
    // The replaced directory's original is untouched where it went.
    assert_eq!(
        std::fs::read(x.scope.path("outside/a/f1")).unwrap(),
        b"a/f1"
    );
    assert!(!x.t().join("b").exists());
    assert!(
        !x.t().join("a/deep/g").exists(),
        "nothing goes into the replacement"
    );
}

/// A repository made in a directory of the moved tree after the commit (`git init` from a
/// shell inside it) keeps that directory whole, and so do `HEAD`, `objects`, and `refs`
/// together; the rest is deleted, and the result is `partial`.
#[test]
fn a_repository_made_inside_keeps_its_directory() {
    let x = committed();
    std::fs::create_dir(x.t().join("a/.git")).unwrap();
    for name in ["HEAD", "objects/", "refs/"] {
        let path = x.t().join("b").join(name.trim_end_matches('/'));
        if name.ends_with('/') {
            std::fs::create_dir(&path).unwrap();
        } else {
            std::fs::write(&path, b"ref: refs/heads/main\n").unwrap();
        }
    }
    let result = result(&x, &x.recover(true));
    assert_eq!(result["status"], "partial", "{result}");
    for kept in ["a/f1", "a/f2", "a/deep/g", "b/h"] {
        assert_eq!(std::fs::read(x.t().join(kept)).unwrap(), kept.as_bytes());
    }
    assert!(!x.t().join("c").exists());
    assert!(!x.t().join("empty").exists());
}

/// An empty target made protected after the commit is taken, fails its check, and goes back:
/// `t` passes the checks its entries pass before it is removed, so it stays with its trash
/// entry, and the result names the target kept.
#[test]
fn a_target_made_protected_is_kept() {
    let mut x = Execution::new("node_modules", &[]);
    x.commit();
    std::fs::write(
        x.config.join("repos/app.toml"),
        "[data]\nregenerable = [\"node_modules\"]\nprotected = [\"node_modules\"]\n",
    )
    .unwrap();
    let result = result(&x, &x.recover(true));
    assert_eq!(result["status"], "partial", "{result}");
    assert_eq!(result["skipped_changed"], serde_json::json!([""]));
    assert_eq!(result["trash"], x.entry().display().to_string());
    assert!(x.t().is_dir());
}

/// `t` replaced after the commit by an empty directory of another inode is not entered, not
/// taken, and not removed; the original, moved away, keeps everything.
#[test]
fn a_replaced_t_is_not_removed() {
    let x = committed();
    std::fs::create_dir_all(x.scope.path("outside")).unwrap();
    std::fs::rename(x.t(), x.scope.path("outside/t")).unwrap();
    std::fs::create_dir(x.t()).unwrap();
    let result = result(&x, &x.recover(true));
    assert_eq!(result["status"], "partial", "{result}");
    assert!(x.t().is_dir(), "the replacement stays");
    assert_eq!(
        std::fs::read(x.scope.path("outside/t/a/f1")).unwrap(),
        b"a/f1"
    );
}

/// A directory of `t` that cannot be opened (mode 0300: writable and searchable, not
/// readable) is not entered and not taken, though it is empty: it stays, and the rest is
/// deleted.
#[test]
fn a_directory_that_cannot_be_opened_stays() {
    let mut x = Execution::new("node_modules", &["locked/", "b/g"]);
    x.commit();
    let locked = x.t().join("locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o300)).unwrap();
    if std::fs::read_dir(&locked).is_ok() {
        // Permissions do not bind this user (root): nothing to show.
        return;
    }
    let got = x.recover(true);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    let result = result(&x, &got);
    assert_eq!(result["status"], "partial", "{result}");
    assert!(locked.is_dir());
    assert!(
        result["skipped_changed"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("locked")),
        "{result}"
    );
    assert!(!x.t().join("b").exists());
}

/// A hold that a cleanup removed does not stop a later recovery: after a result that failed
/// with what the deletion kept in `t`, with `t` holding a kept entry and no hold, and with
/// only the empty trash entry of an interrupted cleanup, recovery makes the hold again,
/// deletes as live deletion does, and writes the result, `partial` while data remains.
#[test]
fn a_removed_hold_is_made_again() {
    let changed = || {
        let mut x = Execution::new("node_modules", &["a/f", "b/h"]);
        x.commit();
        let file = std::fs::File::options()
            .write(true)
            .open(x.t().join("b/h"))
            .unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
        x
    };
    // The result fails once with no reserve, after the deletion kept `b/h`.
    let x = changed();
    x.fail_write("result", "ENOSPC", "always");
    assert_eq!(x.recover(true)["outcome"], "stranded");
    std::fs::remove_file(x.points.dir().join("write-failures")).unwrap();
    let written = result(&x, &x.recover(true));
    assert_eq!(written["status"], "partial", "{written}");
    assert_eq!(std::fs::read(x.t().join("b/h")).unwrap(), b"b/h");

    // `t` with the kept entry, and no hold.
    let x = changed();
    std::fs::remove_dir(x.h()).unwrap();
    let written = result(&x, &x.recover(true));
    assert_eq!(written["status"], "partial", "{written}");
    assert_eq!(std::fs::read(x.t().join("b/h")).unwrap(), b"b/h");
    assert!(!x.t().join("a").exists());

    // Only the empty trash entry.
    let x = committed();
    std::fs::remove_dir_all(x.t()).unwrap();
    std::fs::remove_dir(x.h()).unwrap();
    let written = result(&x, &x.recover(true));
    assert_eq!(written["status"], "executed", "{written}");
    assert!(!x.entry().exists());
}

/// A directory of the chain above the target that cannot be read (mode 0) fails recovery's
/// read of the target's parent, which is no unresolved chain: recovery touches nothing and
/// waits, as for any read that fails.
#[test]
fn an_unreadable_chain_strands_recovery() {
    let mut x = Execution::new("sub/node_modules", &["a/f"]);
    x.commit();
    let before = x.listing();
    let sub = x.worktree.join("sub");
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read_dir(&sub).is_ok() {
        // Permissions do not bind this user (root): nothing to show.
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    let got = x.recover(true);
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(got["outcome"], "stranded", "{got}");
    assert!(got["why"].as_str().unwrap().contains("durable"), "{got}");
    assert_eq!(x.listing(), before);
}

/// With the caller's grant withdrawn, the unit's `act` set to false, or the target's
/// regenerable pattern dropped, recovery touches nothing and says why.
#[test]
fn without_the_grant_or_the_gate_nothing_is_touched() {
    let x = committed();
    x.take("b/h");
    let before = x.listing();
    let got = x.recover(false);
    assert_eq!(got["outcome"], "stranded", "{got}");
    assert!(got["why"].as_str().unwrap().contains("grant"), "{got}");
    assert_eq!(x.listing(), before);

    let mut x = Execution::new("node_modules", &["a/f"]);
    x.caller = "unit:disk-reclaim".into();
    std::fs::write(
        x.config.join("host.toml"),
        "[unit.disk-reclaim]\nact = false\n",
    )
    .unwrap();
    x.commit();
    let before = x.listing();
    let got = x.recover(true);
    assert_eq!(got["outcome"], "stranded", "{got}");
    assert!(
        got["why"]
            .as_str()
            .unwrap()
            .contains("unit.disk-reclaim.act"),
        "{got}"
    );
    assert_eq!(x.listing(), before);
    // The control: the gate open.
    std::fs::write(
        x.config.join("host.toml"),
        "[unit.disk-reclaim]\nact = true\n",
    )
    .unwrap();
    assert_eq!(result(&x, &x.recover(true))["status"], "executed");

    let x = committed();
    std::fs::write(
        x.config.join("repos/app.toml"),
        "[data]\nregenerable = [\"target\"]\n",
    )
    .unwrap();
    let before = x.listing();
    let got = x.recover(true);
    assert_eq!(got["outcome"], "stranded", "{got}");
    assert!(
        got["why"].as_str().unwrap().contains("regenerable"),
        "{got}"
    );
    assert_eq!(x.listing(), before);
}

/// With a process holding a descriptor on the trash entry, recovery waits for the next pass
/// and touches nothing; once it is gone, recovery finishes.
#[test]
fn a_process_using_the_trash_entry_makes_recovery_wait() {
    let x = committed();
    x.take("a/f1");
    let ready = x.scope.path("holder.ready");
    let script = format!(
        "exec 3< '{}'; : > '{}'; exec sleep 60",
        x.t().join("b/h").display(),
        ready.display()
    );
    let mut holder = x.scope.shell(&script, x.scope.root()).spawn().unwrap();
    cardinal_test_support::process::wait_for_file(&ready, WAIT);
    let before = x.listing();
    let got = x.recover(true);
    assert_eq!(got["outcome"], "stranded", "{got}");
    assert!(got["why"].as_str().unwrap().contains("in use"), "{got}");
    assert_eq!(x.listing(), before);
    holder.kill();
    assert_eq!(result(&x, &x.recover(true))["status"], "executed");
}

/// A recovery paused at `recover.locked` holds the plan's `act.lock`: a second recovery is
/// kept out (`in_progress`) and changes nothing, and the first finishes once released.
#[test]
fn a_second_recovery_is_kept_out_at_recover_locked() {
    let x = committed();
    let (mut first, first_out) = x.recover_command(true);
    first.env("CARDINAL_TEST_PAUSE", testpoint::RECOVER_LOCKED);
    let mut first = first.spawn().unwrap();
    x.points
        .wait_reached(testpoint::RECOVER_LOCKED, first.id(), WAIT);
    let before = x.listing();
    assert_eq!(x.recover(true)["outcome"], "in_progress");
    assert_eq!(x.listing(), before);
    x.points.release(testpoint::RECOVER_LOCKED);
    assert!(first.wait().unwrap().success());
    assert_eq!(result(&x, &outcome(&first_out))["status"], "executed");
}

/// A plan with no execution directory has nothing to recover.
#[test]
fn no_execution_no_recovery() {
    let x = Execution::new("node_modules", &["a/f"]);
    let before = x.listing();
    assert_eq!(x.recover(true)["outcome"], "not_found");
    assert_eq!(x.listing(), before);
}

/// Crashes of a live execution (section 13.7, "Crashes"): SIGKILL at each pause point, then
/// the owner's `act` on the same plan, which recovers the claim the dead execution left.
mod through_act {
    use cardinal::contract::ActStatus;
    use live_support::{Live, assert_restored};

    use super::*;

    /// At `act.excluded` nothing changed, and the next `act` executes.
    #[test]
    fn killed_while_excluded() {
        let x = Live::new("excluded", "target", &["debug/app"]);
        x.kill_at(testpoint::ACT_EXCLUDED);
        assert!(x.target().exists());
        assert!(!x.s.state.execution_claim(&x.plan.id).exists());
        assert_eq!(x.act().status, ActStatus::Executed);
    }

    /// At `act.publishing` no claim exists, the temporary file is left to the cleanup of
    /// temp files, and the next `act` executes (A30).
    #[test]
    fn killed_while_publishing_the_claim() {
        let x = Live::new("publishing", "target", &["debug/app"]);
        x.kill_at(testpoint::ACT_PUBLISHING);
        assert!(!x.s.state.execution_claim(&x.plan.id).exists());
        assert!(x.target().exists());
        assert_eq!(x.act().status, ActStatus::Executed);
    }

    /// At `act.claimed` the next `act` reports `rejected` (`interrupted`) with the target in
    /// place and the empty trash entry removed.
    #[test]
    fn killed_once_claimed() {
        let x = Live::new("claimed", "target", &["debug/app"]);
        let before = x.files();
        x.kill_at(testpoint::ACT_CLAIMED);
        assert!(x.entry().join("h").is_dir());
        let result = x.act();
        assert_restored(&x, &result, ActStatus::Rejected, &["interrupted"], &before);
        assert!(result.recovered, "{result:#?}");
        assert_eq!(x.act().status, ActStatus::AlreadyExecuted);
    }

    /// At `act.moved` and `act.checked` the next `act` restores the target.
    #[test]
    fn killed_once_moved_or_checked() {
        for point in [testpoint::ACT_MOVED, testpoint::ACT_CHECKED] {
            let x = Live::new("moved", "target", &["debug/app", "debug/deps/lib"]);
            let before = x.files();
            x.kill_at(point);
            assert!(!x.target().exists(), "{point}");
            let result = x.act();
            assert_restored(&x, &result, ActStatus::Rejected, &["interrupted"], &before);
        }
    }

    /// At the publication of `committed` (its `act.publishing`, the second the execution
    /// reaches), the next `act` restores the target: the claim is still `claimed` (A37).
    #[test]
    fn killed_publishing_committed() {
        let x = Live::new("publishing-committed", "target", &["debug/app"]);
        let before = x.files();
        let mut paused = x.spawn(&[testpoint::ACT_PUBLISHING, testpoint::ACT_CHECKED], None);
        paused.wait(testpoint::ACT_PUBLISHING);
        paused.release(testpoint::ACT_PUBLISHING);
        paused.wait(testpoint::ACT_CHECKED);
        paused.rearm(testpoint::ACT_PUBLISHING);
        paused.release(testpoint::ACT_CHECKED);
        paused.wait(testpoint::ACT_PUBLISHING);
        paused.child.kill();
        let claim = std::fs::read_to_string(x.s.state.execution_claim(&x.plan.id)).unwrap();
        assert!(claim.contains("\"claimed\""), "{claim}");
        let result = x.act();
        assert_restored(&x, &result, ActStatus::Rejected, &["interrupted"], &before);
    }

    /// At `act.committed`, `committed` renamed into place and not synced, the next `act`
    /// finishes the execution: nothing is restored (A37).
    #[test]
    fn killed_before_committed_is_synced() {
        let x = Live::new("committed", "target", &["debug/app", "debug/log"]);
        x.kill_at(testpoint::ACT_COMMITTED);
        let claim = std::fs::read_to_string(x.s.state.execution_claim(&x.plan.id)).unwrap();
        assert!(claim.contains("\"committed\""), "{claim}");
        let result = x.act();
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
        assert!(result.recovered, "{result:#?}");
        assert!(!x.target().exists());
        assert!(!x.entry().exists());
    }

    /// A failure injected at `act.committed`, standing for a failed sync of `committed`,
    /// leaves the claim to recovery with nothing restored or taken: `partial` with the trash
    /// entry, and the next `act` finishes it (A37).
    #[test]
    fn committed_sync_failure_left_to_recovery() {
        let x = Live::new("sync-failed", "target", &["debug/app", "debug/log"]);
        let paused = x.spawn(&[], Some(testpoint::ACT_COMMITTED));
        let result = paused.finish(&[]);
        assert_eq!(result.status, ActStatus::Partial, "{result:#?}");
        assert_eq!(result.trash, Some(x.entry().display().to_string()));
        assert_eq!(read(&x.t().join("debug/app")), "debug/app");
        assert_eq!(read(&x.t().join("debug/log")), "debug/log");
        assert!(!x.s.state.execution_result(&x.plan.id).exists());
        let result = x.act();
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
        assert!(!x.entry().exists());
    }

    /// At `act.taken` and `act.held` the next `act` finishes the execution, the entry left in
    /// the hold deleted.
    #[test]
    fn killed_while_deleting() {
        for point in [testpoint::ACT_TAKEN, testpoint::ACT_HELD] {
            let x = Live::new("deleting", "target", &["debug/app", "debug/log"]);
            x.kill_at(point);
            let result = x.act();
            assert_eq!(result.status, ActStatus::Executed, "{point}: {result:#?}");
            assert!(!x.entry().exists(), "{point}");
        }
    }

    /// At `act.finishing` the next `act` removes `h` and the trash entry and writes the
    /// result.
    #[test]
    fn killed_while_finishing() {
        let x = Live::new("finishing", "target", &["debug/app"]);
        x.kill_at(testpoint::ACT_FINISHING);
        assert!(x.entry().join("h").exists());
        let result = x.act();
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
        assert!(!x.entry().exists());
        assert!(x.s.state.execution_result(&x.plan.id).exists());
    }

    /// At `act.restored`, a restore renamed back and not synced, the next `act` syncs it and
    /// writes the result.
    #[test]
    fn killed_while_restoring() {
        let x = Live::new("restoring", "target", &["debug/app", "debug/log"]);
        let mut paused = x.spawn(&[testpoint::ACT_MOVED, testpoint::ACT_RESTORED], None);
        paused.wait(testpoint::ACT_MOVED);
        std::fs::write(x.t().join("debug/log"), "changed").unwrap();
        paused.release(testpoint::ACT_MOVED);
        paused.wait(testpoint::ACT_RESTORED);
        paused.child.kill();
        assert_eq!(read(&x.target().join("debug/log")), "changed");
        let result = x.act();
        assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
        assert_eq!(result.reasons, ["interrupted"], "{result:#?}");
        assert!(!x.entry().exists());
        assert_eq!(read(&x.target().join("debug/app")), "debug/app");
    }

    /// Under a `claimed` claim with the parent renamed, recovery through `act` touches
    /// nothing and raises `reclaim_stranded`; `act` reports `rejected` (`interrupted`) with
    /// the trash entry.
    #[test]
    fn a_renamed_parent_strands_the_target() {
        let x = Live::new("stranded", "pkg/node_modules", &["a/index.js"]);
        x.kill_at(testpoint::ACT_MOVED);
        std::fs::rename(x.wt.join("pkg"), x.wt.join("pkg2")).unwrap();
        let result = x.act();
        assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
        assert_eq!(result.reasons, ["interrupted"], "{result:#?}");
        assert_eq!(result.trash, Some(x.entry().display().to_string()));
        assert_eq!(read(&x.t().join("a/index.js")), "a/index.js");
        assert!(x.stranded_items() >= 1);
    }

    /// Under a `claimed` claim with the owner's grant withdrawn, the owner's next `act`
    /// restores the target, the safe direction, and reports `rejected` (`interrupted`); with
    /// `--dry-run` first it only reports, recovering nothing.
    #[test]
    fn a_withdrawn_grant_still_restores() {
        let mut x = Live::new("ungranted", "target", &["debug/app"]);
        let before = x.files();
        x.kill_at(testpoint::ACT_MOVED);
        x.s.scene.clear_observe_config();
        let dry = cardinal::act::ActOptions {
            dry_run: true,
            ..live_support::owner()
        };
        let reported = x.s.act(&x.plan.id, &dry).unwrap();
        assert!(!reported.recovered, "{reported:#?}");
        assert!(x.t().is_dir() && !x.target().exists(), "{reported:#?}");
        let result = x.act();
        assert_restored(&x, &result, ActStatus::Rejected, &["interrupted"], &before);
        assert!(result.recovered, "{result:#?}");
    }

    /// Under a `claimed` claim with no `t`, recovery reports the execution `interrupted` only
    /// when the target's name holds the claimed inode. With `t` moved out of the trash entry,
    /// and the name empty or holding a new directory, the claimed target is nowhere recovery
    /// can vouch for: `act` reports `rejected` (`interrupted`) with the trash entry, which
    /// stays, and `reclaim_stranded`.
    #[test]
    fn a_claimed_target_found_nowhere_strands() {
        for remade in [false, true] {
            let x = Live::new("nowhere", "target", &["debug/app"]);
            x.kill_at(testpoint::ACT_MOVED);
            let away = x.wt.join("away");
            std::fs::rename(x.t(), &away).unwrap();
            if remade {
                std::fs::create_dir(x.target()).unwrap();
            }
            let result = x.act();
            assert_eq!(result.status, ActStatus::Rejected, "{remade}: {result:#?}");
            assert_eq!(result.reasons, ["interrupted"], "{remade}: {result:#?}");
            assert_eq!(
                result.trash,
                Some(x.entry().display().to_string()),
                "{remade}"
            );
            assert!(x.entry().is_dir(), "{remade}");
            assert!(x.stranded_items() >= 1, "{remade}");
            assert_eq!(read(&away.join("debug/app")), "debug/app");
        }
    }

    /// An unreadable claim, written by the test, leaves everything untouched with
    /// `reclaim_stranded` (`already_executed`, `claim_unreadable`), and a new plan for the
    /// same target executes.
    #[test]
    fn an_unreadable_claim_then_a_new_plan() {
        let mut x = Live::new("unreadable", "target", &["debug/app"]);
        x.kill_at(testpoint::ACT_CLAIMED);
        std::fs::write(x.s.state.execution_claim(&x.plan.id), b"{ not json").unwrap();
        let result = x.act();
        assert_eq!(result.status, ActStatus::AlreadyExecuted, "{result:#?}");
        assert_eq!(result.reasons, ["claim_unreadable"], "{result:#?}");
        assert!(x.target().exists());
        assert!(x.stranded_items() >= 1);
        x.replan();
        assert_eq!(x.act().status, ActStatus::Executed);
    }

    /// A recovery paused at `recover.locked` keeps an `act` on the plan out (`in_progress`).
    #[test]
    fn act_is_kept_out_at_recover_locked() {
        let x = Live::new("locked", "target", &["debug/app"]);
        x.kill_at(testpoint::ACT_MOVED);
        let paused = x.spawn(&[testpoint::RECOVER_LOCKED], None);
        paused.wait(testpoint::RECOVER_LOCKED);
        let second = x.act();
        assert_eq!(second.status, ActStatus::Rejected, "{second:#?}");
        assert_eq!(second.reasons, ["in_progress"], "{second:#?}");
        let first = paused.finish(&[testpoint::RECOVER_LOCKED]);
        assert_eq!(first.reasons, ["interrupted"], "{first:#?}");
        assert!(x.target().exists());
    }

    fn read(path: &std::path::Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }
}

/// The crashes of section 13.7 followed by a `reconcile` pass (unit X2): step 2 recovers each
/// execution that died as `act` on its plan would, reports it in `recovered`, and leaves what
/// it cannot account for with `reclaim_stranded`; a trash entry that no execution names is
/// reported and kept; and a pass that has spent `reclaim_budget_s` starts no more recoveries.
mod through_a_pass {
    use cardinal::contract::{ActStatus, InboxKind, Reconcile, RecoveredExecution};
    use live_support::{GRANT, Live};

    use super::*;
    use crate::disk_support;
    use crate::support::testpoints::TestPoints;

    /// A live execution as [`Live::new`] makes one, with the disk tests' configuration: the
    /// volumes are never under pressure, and the arbiter's command does nothing.
    fn live(name: &str, files: &[&str]) -> Live {
        live_with(name, files, "")
    }

    fn live_with(name: &str, files: &[&str], extra: &str) -> Live {
        let host = disk_support::host((0, 1, 2), &format!("{extra}{GRANT}"));
        Live::with_host(&host, name, "target", files, |_, _| {})
    }

    /// One pass, with no error but those of `allowed` scopes.
    fn pass(x: &Live) -> Reconcile {
        let points = TestPoints::new(x.s.fx.scope());
        let pass = disk_support::reconcile(&x.s, &points);
        let errors: Vec<_> = pass
            .errors
            .iter()
            .filter(|e| e.scope == "recover" || e.scope == "trash")
            .collect();
        assert!(errors.is_empty(), "{pass:#?}");
        pass
    }

    fn recovered(x: &Live, status: ActStatus) -> Vec<RecoveredExecution> {
        vec![RecoveredExecution {
            plan_id: x.plan.id.clone(),
            status,
        }]
    }

    /// The result recovery wrote.
    fn result(x: &Live) -> cardinal::contract::ActResult {
        serde_json::from_slice(&std::fs::read(x.s.state.execution_result(&x.plan.id)).unwrap())
            .unwrap()
    }

    /// At `act.excluded` and `act.publishing` no claim exists: the pass recovers nothing, and
    /// the next `act` executes.
    #[test]
    fn nothing_claimed_nothing_recovered() {
        for point in [testpoint::ACT_EXCLUDED, testpoint::ACT_PUBLISHING] {
            let x = live("unclaimed", &["debug/app"]);
            x.kill_at(point);
            let pass = pass(&x);
            assert_eq!(pass.recovered, [], "{point}");
            assert!(x.target().exists(), "{point}");
            assert_eq!(x.act().status, ActStatus::Executed, "{point}");
        }
    }

    /// At `act.claimed`, `act.moved`, `act.checked`, and the publication of `committed`, the
    /// pass restores the target whole and writes `rejected` (`interrupted`) with no trash
    /// entry; the next `act` on the plan reports `already_executed`.
    #[test]
    fn claimed_executions_are_restored() {
        for point in [
            testpoint::ACT_CLAIMED,
            testpoint::ACT_MOVED,
            testpoint::ACT_CHECKED,
        ] {
            let x = live("claimed", &["debug/app", "debug/deps/lib"]);
            let before = x.files();
            x.kill_at(point);
            let pass = pass(&x);
            assert_eq!(
                pass.recovered,
                recovered(&x, ActStatus::Rejected),
                "{point}"
            );
            assert_eq!(result(&x).reasons, ["interrupted"], "{point}");
            assert_eq!(x.files(), before, "{point}");
            assert!(!x.entry().exists(), "{point}");
            assert_eq!(x.act().status, ActStatus::AlreadyExecuted, "{point}");
        }

        let x = live("publishing-committed", &["debug/app"]);
        let before = x.files();
        let mut paused = x.spawn(&[testpoint::ACT_PUBLISHING, testpoint::ACT_CHECKED], None);
        paused.wait(testpoint::ACT_PUBLISHING);
        paused.release(testpoint::ACT_PUBLISHING);
        paused.wait(testpoint::ACT_CHECKED);
        paused.rearm(testpoint::ACT_PUBLISHING);
        paused.release(testpoint::ACT_CHECKED);
        paused.wait(testpoint::ACT_PUBLISHING);
        paused.child.kill();
        let pass = pass(&x);
        assert_eq!(pass.recovered, recovered(&x, ActStatus::Rejected));
        assert_eq!(x.files(), before);
    }

    /// At `act.committed`, `act.taken`, `act.held`, and `act.finishing`, and after a failure
    /// injected at `act.committed`, the pass finishes the execution: `executed`, the target
    /// and its trash entry gone.
    #[test]
    fn committed_executions_are_finished() {
        for point in [
            testpoint::ACT_COMMITTED,
            testpoint::ACT_TAKEN,
            testpoint::ACT_HELD,
            testpoint::ACT_FINISHING,
        ] {
            let x = live("committed", &["debug/app", "debug/log"]);
            x.kill_at(point);
            let pass = pass(&x);
            assert_eq!(
                pass.recovered,
                recovered(&x, ActStatus::Executed),
                "{point}"
            );
            let result = result(&x);
            assert!(result.recovered, "{point}: {result:#?}");
            assert!(!x.target().exists(), "{point}");
            assert!(!x.entry().exists(), "{point}");
        }

        let x = live("sync-failed", &["debug/app", "debug/log"]);
        let left = x.spawn(&[], Some(testpoint::ACT_COMMITTED)).finish(&[]);
        assert_eq!(left.status, ActStatus::Partial, "{left:#?}");
        let pass = pass(&x);
        assert_eq!(pass.recovered, recovered(&x, ActStatus::Executed));
        assert!(!x.entry().exists());
    }

    /// At `act.restored`, a restore renamed back and not synced, the pass syncs it and writes
    /// `rejected` (`interrupted`), the target as the restore left it.
    #[test]
    fn a_restore_is_finished() {
        let x = live("restoring", &["debug/app", "debug/log"]);
        let mut paused = x.spawn(&[testpoint::ACT_MOVED, testpoint::ACT_RESTORED], None);
        paused.wait(testpoint::ACT_MOVED);
        std::fs::write(x.t().join("debug/log"), "changed").unwrap();
        paused.release(testpoint::ACT_MOVED);
        paused.wait(testpoint::ACT_RESTORED);
        paused.child.kill();
        let pass = pass(&x);
        assert_eq!(pass.recovered, recovered(&x, ActStatus::Rejected));
        assert_eq!(
            std::fs::read_to_string(x.target().join("debug/log")).unwrap(),
            "changed"
        );
        assert!(!x.entry().exists());
    }

    /// What recovery cannot account for stays, with `reclaim_stranded`: a `claimed` target
    /// whose parent was renamed, an unreadable claim, and a committed execution whose
    /// caller's grant was withdrawn. The pass reports none of them as recovered.
    #[test]
    fn what_cannot_be_accounted_for_stays() {
        let x = Live::with_host(
            &disk_support::host((0, 1, 2), GRANT),
            "renamed",
            "pkg/node_modules",
            &["a/index.js"],
            |_, _| {},
        );
        x.kill_at(testpoint::ACT_MOVED);
        std::fs::rename(x.wt.join("pkg"), x.wt.join("pkg2")).unwrap();
        let first = pass(&x);
        assert_eq!(first.recovered, []);
        assert!(x.t().join("a/index.js").exists());
        assert!(x.stranded_items() >= 1);

        let x = live("unreadable", &["debug/app"]);
        x.kill_at(testpoint::ACT_CLAIMED);
        std::fs::write(x.s.state.execution_claim(&x.plan.id), b"{ not json").unwrap();
        let before = x.files();
        assert_eq!(pass(&x).recovered, []);
        assert_eq!(x.files(), before);
        assert!(x.entry().is_dir());
        assert!(x.stranded_items() >= 1);

        let mut x = live("ungranted", &["debug/app", "debug/log"]);
        x.kill_at(testpoint::ACT_COMMITTED);
        x.s.scene.clear_observe_config();
        x.s.scene.observe_config(&disk_support::host((0, 1, 2), ""));
        assert_eq!(pass(&x).recovered, []);
        assert!(x.t().join("debug/app").exists());
        assert!(x.stranded_items() >= 1);
    }

    /// A trash entry that no execution directory names is reported with `reclaim_stranded`,
    /// naming the entry, and kept, whatever it holds, pass after pass.
    #[test]
    fn a_trash_entry_no_execution_names_is_kept() {
        let x = live("control", &["debug/app"]);
        let entry = x.s.state.trash_dir().join("0123456789abcdef");
        std::fs::create_dir_all(entry.join("t/sub")).unwrap();
        std::fs::write(entry.join("t/sub/file"), "data").unwrap();
        let before = plan_support::listing(&entry);
        for _ in 0..2 {
            let pass = pass(&x);
            assert_eq!(pass.recovered, []);
            assert_eq!(plan_support::listing(&entry), before);
        }
        let items = cardinal::inbox::list(
            &x.s.config(),
            &x.s.state,
            Some(cardinal::inbox::StatusFilter::All),
            false,
        )
        .unwrap()
        .items;
        let stranded: Vec<_> = items
            .iter()
            .filter(|i| i.kind == InboxKind::ReclaimStranded)
            .collect();
        assert_eq!(stranded.len(), 1, "{stranded:#?}");
        assert!(
            stranded[0]
                .key
                .starts_with("reclaim_stranded:0123456789abcdef@")
        );
        assert_eq!(
            stranded[0].evidence["trash"],
            entry.display().to_string(),
            "{stranded:#?}"
        );
        assert_eq!(stranded[0].evidence["entries_left"], 3);
        // The control: a live execution beside it still executes.
        assert_eq!(x.act().status, ActStatus::Executed);
        assert!(entry.join("t/sub/file").exists());
    }

    /// The `reclaim_stranded` items of the state's inbox, ended ones included.
    fn stranded(x: &Live) -> Vec<cardinal::contract::InboxItem> {
        cardinal::inbox::list(
            &x.s.config(),
            &x.s.state,
            Some(cardinal::inbox::StatusFilter::All),
            false,
        )
        .unwrap()
        .items
        .into_iter()
        .filter(|i| i.kind == InboxKind::ReclaimStranded)
        .collect()
    }

    /// A trash entry whose execution directory cannot be read is not taken for one no
    /// execution names: the pass reports the error and raises nothing, and once the directory
    /// reads as missing, the entry is reported.
    #[test]
    fn an_entry_whose_naming_cannot_be_read_raises_nothing() {
        use std::os::unix::fs::PermissionsExt as _;
        let x = live("unnamed", &["debug/app"]);
        let entry = x.s.state.trash_dir().join("0123456789abcdef");
        std::fs::create_dir_all(entry.join("t")).unwrap();
        let executions = x.s.state.executions_dir();
        std::fs::create_dir_all(&executions).unwrap();
        let mode = |m| std::fs::set_permissions(&executions, std::fs::Permissions::from_mode(m));
        mode(0o000).unwrap();
        let points = TestPoints::new(x.s.fx.scope());
        let unread = disk_support::reconcile(&x.s, &points);
        mode(0o755).unwrap();
        assert!(
            unread.errors.iter().any(|e| e.scope == "trash"),
            "{unread:#?}"
        );
        assert_eq!(stranded(&x).len(), 0, "{unread:#?}");
        pass(&x);
        assert_eq!(stranded(&x).len(), 1);
    }

    /// The `reclaim_stranded` a pass's recovery raises for an unreadable claim stays open while
    /// the claim cannot be read, with no trash entry left to list.
    #[test]
    fn an_unreadable_claim_keeps_its_item_open() {
        let x = live("unreadable", &["debug/app"]);
        x.kill_at(testpoint::ACT_CLAIMED);
        std::fs::write(x.s.state.execution_claim(&x.plan.id), b"{ not json").unwrap();
        std::fs::remove_dir_all(x.entry()).unwrap();
        for _ in 0..2 {
            let pass = pass(&x);
            let items = stranded(&x);
            assert_eq!(items.len(), 1, "{items:#?}");
            assert_eq!(items[0].ended_ms, None, "{pass:#?}\n{items:#?}");
        }
    }

    /// A committed execution whose caller lost its grant is stranded on the grant before its
    /// manifest is read, and a pass that strands it again for the same reason leaves its item
    /// as it is.
    #[test]
    fn a_stranded_execution_costs_a_pass_its_grant_check() {
        let mut x = live("ungranted", &["debug/app", "debug/log"]);
        x.kill_at(testpoint::ACT_COMMITTED);
        x.s.scene.clear_observe_config();
        x.s.scene.observe_config(&disk_support::host((0, 1, 2), ""));
        std::fs::write(x.s.state.execution_manifest(&x.plan.id), b"{ cut").unwrap();
        pass(&x);
        let first = stranded(&x);
        assert_eq!(first.len(), 1, "{first:#?}");
        let reason = first[0].evidence["reason"].as_str().unwrap_or_default();
        assert!(reason.contains("grant"), "{reason}");
        pass(&x);
        assert_eq!(stranded(&x), first);
    }

    /// A trash entry no execution names, part of which cannot be read, is reported with its
    /// entries and bytes unknown.
    #[test]
    fn an_unreadable_trash_entry_is_sized_unknown() {
        use std::os::unix::fs::PermissionsExt as _;
        let x = live("control", &["debug/app"]);
        let entry = x.s.state.trash_dir().join("0123456789abcdef");
        let locked = entry.join("t/locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("file"), "data").unwrap();
        let mode = |m| std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(m));
        mode(0o000).unwrap();
        let pass = pass(&x);
        mode(0o755).unwrap();
        let items = stranded(&x);
        assert_eq!(items.len(), 1, "{pass:#?}");
        assert_eq!(items[0].evidence["entries_left"], serde_json::Value::Null);
        assert_eq!(items[0].evidence["bytes_left"], serde_json::Value::Null);
    }

    /// An execution with a result is recovered too: a result that does not read whole is
    /// reported, where the presence of the file would have passed it over.
    #[test]
    fn a_result_is_read_whole() {
        let x = live("finished", &["debug/app"]);
        assert_eq!(x.act().status, ActStatus::Executed);
        pass(&x);
        std::fs::write(x.s.state.execution_result(&x.plan.id), b"{ \"cut\": ").unwrap();
        let points = TestPoints::new(x.s.fx.scope());
        let cut = disk_support::reconcile(&x.s, &points);
        assert!(
            cut.errors
                .iter()
                .any(|e| e.scope == "recover" && e.message.contains(&x.plan.id)),
            "{cut:#?}"
        );
    }

    /// A finished execution, its result read whole and synced, costs a pass none of its
    /// `reclaim_budget_s`: one that sorts first and whose recovery is paused past the budget at
    /// `recover.locked` still leaves the pass the recovery of an execution that died.
    #[test]
    fn finished_executions_cost_no_budget() {
        let budget = "[reconcile]\nreclaim_budget_s = 1\n";
        let x = live_with("first", &["debug/app"], budget);
        let wt = x.s.worktree("second");
        plan_support::write_synced(&wt, "target/debug/app", b"debug/app");
        let other =
            x.s.plan(cardinal::contract::PlanKind::ReclaimDir, &wt.join("target"));
        let (done, died) = if x.plan.id < other.id {
            (x.plan.id.clone(), other.id.clone())
        } else {
            (other.id.clone(), x.plan.id.clone())
        };
        let owner = live_support::owner();
        assert_eq!(x.s.act(&done, &owner).unwrap().status, ActStatus::Executed);
        let scope = cardinal_test_support::Scope::new();
        let points = TestPoints::new(&scope);
        let mut command = x.s.act_command(&died, &owner);
        points.apply(&mut command, &[testpoint::ACT_MOVED], None);
        let mut child = command
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        points.wait_reached(testpoint::ACT_MOVED, child.id(), WAIT);
        child.kill();

        let pass_points = TestPoints::new(x.s.fx.scope());
        let mut command = disk_support::reconcile_command(&x.s, &pass_points);
        pass_points.apply(&mut command, &[testpoint::RECOVER_LOCKED], None);
        let running = command
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        pass_points.wait_reached(testpoint::RECOVER_LOCKED, running.id(), WAIT);
        std::thread::sleep(Duration::from_millis(1200));
        pass_points.release(testpoint::RECOVER_LOCKED);
        let pass = disk_support::pass(&running.wait_with_output().unwrap());
        assert_eq!(
            pass.recovered,
            [RecoveredExecution {
                plan_id: died,
                status: ActStatus::Rejected,
            }],
            "{pass:#?}"
        );
    }

    /// Once a pass has spent `reclaim_budget_s`, it starts no more recoveries: the one under
    /// way (paused past the budget at `recover.locked`) finishes, the next waits for a later
    /// pass, which recovers it.
    #[test]
    fn the_budget_stops_further_recoveries() {
        let budget = "[reconcile]\nreclaim_budget_s = 1\n";
        let first = live_with("first", &["debug/app"], budget);
        first.kill_at(testpoint::ACT_MOVED);
        let wt = first.s.worktree("second");
        plan_support::write_synced(&wt, "target/debug/app", b"debug/app");
        let plan = first
            .s
            .plan(cardinal::contract::PlanKind::ReclaimDir, &wt.join("target"));
        let scope = cardinal_test_support::Scope::new();
        let points = TestPoints::new(&scope);
        let mut command = first.s.act_command(&plan.id, &live_support::owner());
        points.apply(&mut command, &[testpoint::ACT_MOVED], None);
        let mut child = command
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        points.wait_reached(testpoint::ACT_MOVED, child.id(), WAIT);
        child.kill();

        let pass_points = TestPoints::new(first.s.fx.scope());
        let mut command = disk_support::reconcile_command(&first.s, &pass_points);
        pass_points.apply(&mut command, &[testpoint::RECOVER_LOCKED], None);
        let running = command
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        pass_points.wait_reached(testpoint::RECOVER_LOCKED, running.id(), WAIT);
        std::thread::sleep(Duration::from_millis(1200));
        pass_points.release(testpoint::RECOVER_LOCKED);
        let output = running.wait_with_output().unwrap();
        let spent = disk_support::pass(&output);
        assert_eq!(spent.recovered.len(), 1, "{spent:#?}");
        assert!(
            spent
                .errors
                .iter()
                .any(|e| e.scope == "recover" && e.message.contains("reclaim_budget_s")),
            "{spent:#?}"
        );
        let next = pass(&first);
        assert_eq!(next.recovered.len(), 1, "{next:#?}");
        assert!(first.target().exists());
        assert!(wt.join("target/debug/app").exists());
    }
}
