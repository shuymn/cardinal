//! Recovery of executions that died (A29, A37; `docs/architecture.md` sections 9.7 and 13.7):
//! the recovery table's decisions for absent, unreadable, and `committed` claims, run through
//! the library in a helper process on the states the crashes of section 13.7 leave, which
//! each test writes (`tests/execution_support`), and a second recovery kept out while one
//! holds the plan's `act.lock` at `recover.locked`; and, through `act` (unit X1c), SIGKILL at
//! every pause point of a live execution followed by `act` on the same plan, the `claimed`
//! rows, an unreadable claim followed by a new plan that executes, and `act` kept out at
//! `recover.locked`. X2 runs the crashes through a pass (section 14.2).
#![cfg(unix)]

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
