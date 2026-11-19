//! Every reservation write and deletion of a runner takes the admission lock (C2,
//! `docs/architecture.md` sections 4.3 and 4.5): a directory listing that runs while another
//! process replaces or deletes an entry may omit entries, so a pass that listed
//! `reservations/` meanwhile would count too few and admit over budget. Each test holds the
//! lock while the runner reaches one write with `lock_timeout_ms = 0`, and shows the file
//! unchanged and the outcome the lock timeout has there.

#![cfg(unix)]

mod runner_support;
mod support;

use cardinal::admission::{self, AdmissionLock, in_use, list_reservations, recovery_pass};
use cardinal::config::HostConfig;
use cardinal::contract::{DerivedState, Inventory, RecordKind, ReservationState};
use cardinal::testpoint::{RUN_AFTER_SPAWN, RUN_BEFORE_STARTING, RUN_REAPED, RUN_SPAWNED};
use cardinal_test_support::process::GroupGuard;
use runner_support::{Env, WAIT, output};

#[test]
fn helper() {
    support::helper::run_if_requested();
}

/// A budget of four one-CPU jobs; every wait for the admission lock gives up at once.
fn env() -> Env {
    Env::new(
        "[budget]\ncpu = 4\n[admission]\ndefault_resources = { cpu = 1 }\nlock_timeout_ms = 0\n",
    )
}

/// A stage that appends one line to `counter` and exits 7.
fn counting_stage(env: &Env) -> String {
    format!("echo run >> '{}'; exit 7", env.file("counter").display())
}

/// The admission lock, held by this test as another process's pass would hold it.
fn hold(env: &Env) -> AdmissionLock {
    admission::lock(&env.state(), WAIT).expect("take the admission lock")
}

/// The only reservation's job ID and the text of its file.
fn only_reservation(env: &Env) -> (String, String) {
    let reservations = env.reservations();
    assert_eq!(reservations.len(), 1, "{reservations:?}");
    let job = reservations[0].job_id.clone();
    let text = file(env, &job).expect("read the reservation");
    (job, text)
}

/// The text of `job`'s reservation file; none once it is deleted.
fn file(env: &Env, job: &str) -> Option<String> {
    std::fs::read_to_string(env.state().reservation(job)).ok()
}

fn reasons(env: &Env, kind: RecordKind) -> Vec<String> {
    env.records_of(kind)
        .iter()
        .map(|r| r.data["reason"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// `starting` that cannot take the lock fails open: the reservation stays `reserved`, the
/// release that fail-open does cannot take the lock either and keeps it for the recovery pass,
/// and the stage runs once, in the caller's group. The pass releases the reservation once the
/// runner has left its group.
#[test]
fn starting_without_the_lock_fails_open_and_writes_nothing() {
    let env = env();
    let stage = counting_stage(&env);
    let mut runner = env
        .run_at(
            None,
            &["sh", "-c", &stage],
            &[RUN_BEFORE_STARTING, RUN_SPAWNED],
            None,
        )
        .spawn()
        .unwrap();
    env.points
        .wait_reached(RUN_BEFORE_STARTING, runner.id(), WAIT);
    let (job, before) = only_reservation(&env);
    let held = hold(&env);
    env.points.release(RUN_BEFORE_STARTING);
    env.points.wait_reached(RUN_SPAWNED, runner.id(), WAIT);
    assert_eq!(
        file(&env, &job).as_deref(),
        Some(before.as_str()),
        "nothing wrote the reservation while the lock was held"
    );
    assert_eq!(
        reasons(&env, RecordKind::JobFailOpen),
        ["admission_lock_timeout"]
    );
    drop(held);
    env.points.release(RUN_SPAWNED);
    assert_eq!(runner.wait().unwrap().code(), Some(7));
    assert_eq!(env.lines("counter"), ["run"], "the stage ran once");
    let report = recovery_pass(&env.state(), &HostConfig::defaults()).unwrap();
    assert_eq!(report.released, [job]);
    assert_eq!(env.live_locks(), Vec::<String>::new());
}

/// `running` that cannot take the lock keeps the complete `starting` reservation, records the
/// failure, and leaves the stage running; the release at the end takes the lock again.
#[test]
fn running_without_the_lock_keeps_starting() {
    let env = env();
    let stage = counting_stage(&env);
    let mut runner = env
        .run_at(
            None,
            &["sh", "-c", &stage],
            &[RUN_SPAWNED, RUN_AFTER_SPAWN],
            None,
        )
        .spawn()
        .unwrap();
    env.points.wait_reached(RUN_SPAWNED, runner.id(), WAIT);
    let (job, before) = only_reservation(&env);
    let held = hold(&env);
    env.points.release(RUN_SPAWNED);
    env.points.wait_reached(RUN_AFTER_SPAWN, runner.id(), WAIT);
    assert_eq!(
        file(&env, &job).as_deref(),
        Some(before.as_str()),
        "nothing wrote the reservation while the lock was held"
    );
    assert_eq!(
        env.reservation(&job).map(|r| r.state),
        Some(ReservationState::Starting)
    );
    assert_eq!(
        reasons(&env, RecordKind::JobInfraFailure),
        ["cannot write `running`: timed out waiting for the admission lock"]
    );
    drop(held);
    env.points.release(RUN_AFTER_SPAWN);
    assert_eq!(runner.wait().unwrap().code(), Some(7));
    assert_eq!(env.lines("counter"), ["run"]);
    assert_eq!(env.reservations(), []);
    assert_eq!(env.live_locks(), Vec::<String>::new());
}

/// A release that cannot take the lock deletes nothing: the reservation and the live lock
/// file stay, the live lock is let go, and the recovery pass releases them once the group is
/// empty.
#[test]
fn release_without_the_lock_keeps_the_reservation() {
    let env = env();
    let stage = counting_stage(&env);
    let mut runner = env
        .run_at(None, &["sh", "-c", &stage], &[RUN_REAPED], None)
        .spawn()
        .unwrap();
    env.points.wait_reached(RUN_REAPED, runner.id(), WAIT);
    let (job, before) = only_reservation(&env);
    let held = hold(&env);
    env.points.release(RUN_REAPED);
    assert_eq!(runner.wait().unwrap().code(), Some(7));
    assert_eq!(
        file(&env, &job).as_deref(),
        Some(before.as_str()),
        "nothing deleted the reservation while the lock was held"
    );
    assert_eq!(env.live_locks(), [format!("{job}.lock")]);
    assert!(
        cardinal::state::try_lock(
            &env.state().live_lock(&job),
            cardinal::state::LockMode::Exclusive
        )
        .unwrap()
        .is_some(),
        "the live lock is let go"
    );
    assert_eq!(env.records_of(RecordKind::JobFinished).len(), 1);
    drop(held);
    let report = recovery_pass(&env.state(), &HostConfig::defaults()).unwrap();
    assert_eq!(report.released, [job]);
    assert_eq!(env.reservations(), []);
    assert_eq!(env.live_locks(), Vec::<String>::new());
}

/// A runner killed after its `running` write timed out leaves the `starting` reservation,
/// which counts as `recovery_needed` and keeps its worktree in use while the stage's group
/// lives.
#[test]
fn a_starting_reservation_left_by_a_killed_runner_still_counts() {
    let env = env();
    let mut runner = env
        .run_at(
            None,
            &["sh", "-c", "sleep 30"],
            &[RUN_SPAWNED, RUN_AFTER_SPAWN],
            None,
        )
        .spawn()
        .unwrap();
    env.points.wait_reached(RUN_SPAWNED, runner.id(), WAIT);
    let group = GroupGuard::new(env.scope(), runner.id());
    let (job, before) = only_reservation(&env);
    let held = hold(&env);
    env.points.release(RUN_SPAWNED);
    env.points.wait_reached(RUN_AFTER_SPAWN, runner.id(), WAIT);
    runner.kill_leader();
    assert!(
        env.live_lock_freed(&job),
        "the killed runner's live lock is let go"
    );
    drop(held);
    assert_eq!(file(&env, &job).as_deref(), Some(before.as_str()));

    let host = HostConfig::defaults();
    let views = list_reservations(&env.state(), &host).unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].reservation.state, ReservationState::Starting);
    assert_eq!(views[0].reservation.child, None);
    assert_eq!(views[0].derived, DerivedState::RecoveryNeeded);
    let busy = in_use(&env.state(), &host, &env.worktree_key()).unwrap();
    assert_eq!(busy.jobs, std::slice::from_ref(&job));
    let report = recovery_pass(&env.state(), &host).unwrap();
    assert!(report.released.is_empty(), "{report:?}");
    assert_eq!(report.reservations.len(), 1);
    drop(group);
}

/// `inventory` reads the reservations under the admission lock: while another process holds
/// it past `lock_timeout_ms`, they are reported unreadable rather than as an empty list, and
/// the library readers fail the same way.
#[test]
fn readers_report_reservations_behind_a_held_lock_as_unreadable() {
    let env = env();
    let mut reservation = env.hold_budget(1);
    let inventory = || -> Inventory {
        let out = output(&mut env.command(&["inventory"]));
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        serde_json::from_slice(&out.stdout).expect("an inventory document")
    };
    let unreadable = |inv: &Inventory| {
        inv.errors.iter().any(|e| {
            e.scope == "reservations" && e.message == "timed out waiting for the admission lock"
        })
    };
    let mut host = HostConfig::defaults();
    host.admission.lock_timeout_ms = 0;

    let held = hold(&env);
    let inv = inventory();
    assert!(unreadable(&inv), "{:#?}", inv.errors);
    assert!(list_reservations(&env.state(), &host).is_err());
    assert!(in_use(&env.state(), &host, &env.worktree_key()).is_err());
    drop(held);

    let inv = inventory();
    assert!(!unreadable(&inv), "{:#?}", inv.errors);
    let jobs: Vec<&str> = inv
        .reservations
        .iter()
        .map(|v| v.reservation.job_id.as_str())
        .collect();
    assert_eq!(jobs, [reservation.job.as_str()]);
    reservation.release();
}
