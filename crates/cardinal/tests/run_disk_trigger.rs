//! The runner's job-end disk check (A18, C18, `docs/architecture.md` sections 5.2 and 13.6):
//! below the warning line, and with no pass in the last 60 s, the runner starts `cardinal
//! reconcile --trigger job-end` detached, only once the stage's status is read and recorded
//! and its locks are dropped; a failure of the check leaves the stage's output and status as
//! they are without it. The check reads the real volume (`statfs`), so the lines decide:
//! [`LOW`] puts the warning line above any free space, and the runner environment's default
//! puts it below.

#![cfg(unix)]

mod runner_support;
mod support;

use cardinal::contract::{ReconcileState, RecordKind};
use cardinal::testpoint::{RUN_FINISHED, RUN_RECONCILE_SPAWNING};
use cardinal_test_support::process::wait_for_file;
use runner_support::{Env, WAIT, output};

#[test]
fn helper() {
    support::helper::run_if_requested();
}

/// A warning line above any free space.
const LOW: &str = "[disk]\ncritical_mib = 1\nwarning_mib = 1099511627776\n\
                   recovery_mib = 1099511627777\n";

const BUDGET: &str = "[budget]\ncpu = 4\n[admission]\ndefault_resources = { cpu = 1 }\n";

const STAGE: &[&str] = &["sh", "-c", "echo out; echo err >&2; exit 3"];

/// Paused at `run.finished`, the runner has recorded `job_finished`, dropped its locks, and
/// started nothing; released, it reaches `run.reconcile_spawning` and a pass runs. A job that
/// ends within 60 s of that pass starts none.
#[test]
fn starts_after_the_status_is_recorded() {
    let env = Env::new(&format!("{BUDGET}{LOW}"));
    let state = env.state();
    let mut runner = env
        .run_at(None, STAGE, &[RUN_FINISHED, RUN_RECONCILE_SPAWNING], None)
        .spawn()
        .unwrap();
    let pid = runner.id();
    env.points.wait_reached(RUN_FINISHED, pid, WAIT);
    let finished = env.records_of(RecordKind::JobFinished);
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].data["exit"], 3);
    assert!(env.worktree_lock_free());
    assert_eq!(env.reservations().len(), 0, "the reservation is released");
    assert!(!env.points.reached(RUN_RECONCILE_SPAWNING, pid));
    assert!(!state.reconcile_state().exists());

    env.points.release(RUN_FINISHED);
    env.points.wait_reached(RUN_RECONCILE_SPAWNING, pid, WAIT);
    assert!(
        !state.reconcile_state().exists(),
        "no pass before the spawn"
    );
    env.points.release(RUN_RECONCILE_SPAWNING);
    assert_eq!(runner.wait().unwrap().code(), Some(3));
    wait_for_file(&state.reconcile_state(), WAIT);
    let pass: ReconcileState =
        serde_json::from_slice(&std::fs::read(state.reconcile_state()).unwrap()).unwrap();
    assert!(pass.last_pass_ms >= finished[0].ts_ms, "{pass:?}");

    // Within 60 s of that pass: no other.
    let mut again = env
        .run_at(None, STAGE, &[RUN_RECONCILE_SPAWNING], None)
        .spawn()
        .unwrap();
    assert_eq!(again.wait().unwrap().code(), Some(3));
    assert!(!env.points.reached(RUN_RECONCILE_SPAWNING, again.id()));
}

/// An injected failure or panic at either point of the check changes neither the stage's
/// output nor its status, and starts no pass; the control has the default lines.
#[test]
fn failure_changes_nothing() {
    let control = Env::new(BUDGET);
    let expected = output(&mut control.run(None, STAGE));
    assert_eq!(expected.status.code(), Some(3));
    assert_eq!(expected.stdout, b"out\n");
    assert_eq!(expected.stderr, b"err\n");
    assert!(!control.state().reconcile_state().exists());

    for point in [RUN_FINISHED, RUN_RECONCILE_SPAWNING] {
        for panic in [false, true] {
            let env = Env::new(&format!("{BUDGET}{LOW}"));
            let mut command = if panic {
                let mut command = env.run_at(None, STAGE, &[], None);
                command.env("CARDINAL_TEST_PANIC", point);
                command
            } else {
                env.run_at(None, STAGE, &[], Some(point))
            };
            let out = output(&mut command);
            let what = format!("{point}, panic {panic}");
            assert_eq!(out.status.code(), expected.status.code(), "{what}");
            assert_eq!(out.stdout, expected.stdout, "{what}");
            assert_eq!(out.stderr, expected.stderr, "{what}");
            assert_eq!(env.records_of(RecordKind::JobFinished).len(), 1, "{what}");
            assert!(!env.state().reconcile_state().exists(), "{what}");
        }
    }
}
