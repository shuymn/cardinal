//! The durable order of an execution (A30, A37; `docs/architecture.md` sections 9.7 and
//! 13.7), for the deletion, the cleanup, and recovery, which unit X1b brings. Killing a
//! process cannot drop a write the page cache holds, so two kinds of test stand in for a
//! power loss: the write log that `state` and `act` keep with `CARDINAL_TEST_DIR` set is
//! checked against each row of the durable order (every change after a sync that succeeded
//! for what that row makes durable first), and the steps a power loss could undo (a put-back,
//! the result) are undone by hand before recovery runs again, which must handle the state as
//! its table says. X1c adds the move, the claim's stages, and the restore (section 14.2).
#![cfg(unix)]

mod execution_support;
mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use execution_support::Execution;
use serde_json::Value;

#[test]
fn helper() {
    execution_support::run_if_requested();
    support::helper::run_if_requested();
}

/// A `committed` execution with an entry in the hold (`a/f2`, which recovery puts back and
/// deletes) and one whose mtime changed (`b/h`, which the deletion puts back and keeps).
fn with_put_backs() -> Execution {
    let mut x = Execution::new("node_modules", &["a/f1", "a/f2", "b/h", "c"]);
    x.commit();
    x.take("a/f2");
    touch(&x.t().join("b/h"));
    x
}

/// Moves the mtime of `path` an hour back.
fn touch(path: &Path) {
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
}

fn path(op: &Value, key: &str) -> PathBuf {
    PathBuf::from(op[key].as_str().unwrap_or_default())
}

/// The position of the first operation that `matches`, which must exist.
fn first(log: &[Value], what: &str, matches: impl Fn(&Value) -> bool) -> usize {
    log.iter()
        .position(matches)
        .unwrap_or_else(|| panic!("no {what} in the write log: {log:#?}"))
}

/// The position of the last operation that `matches`, which must exist.
fn last(log: &[Value], what: &str, matches: impl Fn(&Value) -> bool) -> usize {
    log.iter()
        .rposition(matches)
        .unwrap_or_else(|| panic!("no {what} in the write log: {log:#?}"))
}

/// Whether `op` is a sync of `dir` that succeeded.
fn synced(op: &Value, dir: &Path) -> bool {
    op["op"] == "sync" && path(op, "path") == dir && op["error"].is_null()
}

/// Whether `op` changes something: anything but a sync.
fn changes(op: &Value) -> bool {
    op["op"] != "sync"
}

/// The rename that publishes the result.
fn result_written(x: &Execution) -> impl Fn(&Value) -> bool {
    let result = x.state.execution_result(&x.plan);
    move |op: &Value| op["op"] == "rename" && path(op, "to") == result && op["error"].is_null()
}

/// Recovery's first change, the first take, and the result each follow a sync that succeeded
/// of what their row of the durable order makes durable first: what recovery found (the
/// claim, its directory, `<state>/trash/`, the trash entry, `t`, and `h`); `committed`; and
/// every directory a put-back changed, and the trash entry's cleanup.
#[test]
fn recovery_and_its_deletion_follow_the_durable_order() {
    let x = with_put_backs();
    let got = x.recover(true);
    assert_eq!(got["result"]["status"], "partial", "{got}");
    let log = x.write_log();
    let first_change = first(&log, "a change", changes);
    let found = [
        x.state.execution_claim(&x.plan),
        x.state.execution_dir(&x.plan),
        x.state.trash_dir(),
        x.entry(),
        x.t(),
        x.h(),
    ];
    for dir in &found {
        let at = first(&log, &format!("a sync of {}", dir.display()), |op| {
            synced(op, dir)
        });
        assert!(at < first_change, "{} synced after a change", dir.display());
    }
    // The first take: an entry renamed from `t` into the hold, after `committed` is durable.
    let first_take = first(&log, "a take", |op| {
        op["op"] == "rename"
            && path(op, "path").starts_with(x.t())
            && path(op, "to").starts_with(x.h())
    });
    for dir in &found[..2] {
        assert!(log[..first_take].iter().any(|op| synced(op, dir)));
    }
    // Every put-back is followed by syncs of the directory it went into and of the hold,
    // both before the result.
    let result = first(&log, "the result", result_written(&x));
    let put_backs: Vec<usize> = log
        .iter()
        .enumerate()
        .filter(|(_, op)| {
            op["op"] == "rename"
                && op["error"].is_null()
                && path(op, "path").starts_with(x.h())
                && path(op, "to").starts_with(x.t())
        })
        .map(|(at, _)| at)
        .collect();
    assert!(
        put_backs.len() >= 2,
        "a held entry and a changed one: {log:#?}"
    );
    for at in put_backs {
        let into = path(&log[at], "to").parent().unwrap().to_path_buf();
        for dir in [into, x.h()] {
            assert!(
                log[at..result].iter().any(|op| synced(op, &dir)),
                "{} not synced between the put-back at {at} and the result",
                dir.display()
            );
        }
    }
    // `t` stays with what was kept, and so does the hold, whose last change is synced before
    // the result.
    let in_hold = last(&log, "a change in the hold", |op| {
        changes(op) && (path(op, "path").starts_with(x.h()) || path(op, "to").starts_with(x.h()))
    });
    assert!(log[in_hold..result].iter().any(|op| synced(op, &x.h())));
}

/// A directory that stays after an entry was taken from it is synced before the result,
/// though nothing was put back into it: `t/a`, kept for the repository made in `a/nested`,
/// after `a/f` was deleted.
#[test]
fn a_directory_left_changed_is_synced_before_the_result() {
    let mut x = Execution::new("node_modules", &["a/f", "a/nested/x"]);
    x.commit();
    std::fs::create_dir(x.t().join("a/nested/.git")).unwrap();
    let got = x.recover(true);
    assert_eq!(got["result"]["status"], "partial", "{got}");
    let log = x.write_log();
    let dir = x.t().join("a");
    let take = first(&log, "the take of a/f", |op| {
        op["op"] == "rename" && path(op, "path") == dir.join("f") && op["error"].is_null()
    });
    let result = first(&log, "the result", result_written(&x));
    assert!(
        log[take..result].iter().any(|op| synced(op, &dir)),
        "{log:#?}"
    );
}

/// A result is read whole, and its directory synced, before recovery counts the execution
/// finished: a second recovery syncs the execution directory, and a result that does not
/// parse fails recovery instead of finishing it.
#[test]
fn a_result_counts_once_read_whole_and_durable() {
    let mut x = Execution::new("node_modules", &["a/f"]);
    x.commit();
    assert_eq!(x.recover(true)["result"]["status"], "executed");
    let before = x.write_log().len();
    assert_eq!(x.recover(true)["outcome"], "finished");
    let dir = x.state.execution_dir(&x.plan);
    assert!(
        x.write_log()[before..].iter().any(|op| synced(op, &dir)),
        "the execution directory is synced"
    );
    let result = x.state.execution_result(&x.plan);
    let whole = std::fs::read(&result).unwrap();
    std::fs::write(&result, &whole[..whole.len() / 2]).unwrap();
    let got = x.recover(true);
    assert_eq!(got["outcome"], "error", "{got}");
}

/// A claim whose rename succeeded counts as published when its directory's sync fails: the
/// claim is in place, and the failure is told apart from a rename that failed (section 9.7,
/// durable order).
#[test]
fn a_claim_renamed_counts_as_published_when_its_sync_fails() {
    let mut x = Execution::new("node_modules", &["a/f"]);
    x.exclude();
    x.manifest = x.walk_manifest();
    x.fail_write("claim_sync", "ENOSPC", "always");
    let claim = x.claim(cardinal::contract::ClaimStage::Committed);
    let got = x.publish(&claim);
    assert_eq!(got["outcome"], "not_synced", "{got}");
    let written: cardinal::contract::ActClaim =
        serde_json::from_slice(&std::fs::read(x.state.execution_claim(&x.plan)).unwrap()).unwrap();
    assert_eq!(written, claim);
}

/// The cleanup of a deletion that kept nothing removes `t`, `h`, and the trash entry and
/// syncs `<state>/trash/` before the result.
#[test]
fn the_cleanup_is_durable_before_the_result() {
    let mut x = Execution::new("node_modules", &["a/f", "b"]);
    x.commit();
    let got = x.recover(true);
    assert_eq!(got["result"]["status"], "executed", "{got}");
    let log = x.write_log();
    let result = first(&log, "the result", result_written(&x));
    let entry_removed = first(&log, "the trash entry's removal", |op| {
        op["op"] == "rmdir" && path(op, "path") == x.entry() && op["error"].is_null()
    });
    assert!(entry_removed < result);
    assert!(
        log[entry_removed..result]
            .iter()
            .any(|op| synced(op, &x.state.trash_dir()))
    );
    // The result's own file is synced before its rename, and its directory after it.
    let tmp = path(&log[result], "path");
    assert!(log[..result].iter().any(|op| synced(op, &tmp)));
    assert!(
        log[result..]
            .iter()
            .any(|op| synced(op, &x.state.execution_dir(&x.plan)))
    );
}

/// A result a power loss undid: recovery finds `committed` and no trash entry, and writes the
/// result again.
#[test]
fn an_undone_result_is_written_again() {
    let mut x = Execution::new("node_modules", &["a/f"]);
    x.commit();
    assert_eq!(x.recover(true)["result"]["status"], "executed");
    std::fs::remove_file(x.state.execution_result(&x.plan)).unwrap();
    let again = x.recover(true);
    assert_eq!(again["outcome"], "done", "{again}");
    assert_eq!(again["result"]["status"], "executed");
}

/// A put-back a power loss undid, the entry back in the hold: recovery puts it back again by
/// its index and keeps it, and its bytes survive.
#[test]
fn an_undone_put_back_is_put_back_again() {
    let x = with_put_backs();
    // The result's write fails with no reserve, which leaves the claim to recovery after the
    // put-back of `b/h` was made durable.
    x.fail_write("result", "ENOSPC", "always");
    let stopped = x.recover(true);
    assert_eq!(stopped["outcome"], "stranded", "{stopped}");
    assert_eq!(std::fs::read(x.t().join("b/h")).unwrap(), b"b/h");
    std::fs::remove_file(x.points.dir().join("write-failures")).unwrap();
    // The put-back undone, and with it the removal of the hold it left empty.
    std::fs::create_dir_all(x.h()).unwrap();
    std::fs::rename(x.t().join("b/h"), x.held("b/h")).unwrap();
    let got = x.recover(true);
    assert_eq!(got["result"]["status"], "partial", "{got}");
    assert_eq!(std::fs::read(x.t().join("b/h")).unwrap(), b"b/h");
    assert!(!x.held("b/h").exists());
}
