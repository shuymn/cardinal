//! Parent death (A12, `docs/architecture.md` section 5.5): when the runner's parent exits (the
//! Bash call was cancelled), the runner does not start the stage, or, after the spawn, stops
//! the group and holds its reservation until the group is empty.

#![cfg(unix)]

mod runner_support;
mod support;

use std::time::{Duration, Instant};

use cardinal::contract::RecordKind;
use cardinal::testpoint::{ADMISSION_WAITING, RUN_AFTER_SPAWN};
use runner_support::{Env, GroupGuard, WAIT, wait_gone};
use support::helper::{ChildGuard, wait_for, wait_for_file};

#[test]
fn helper() {
    support::helper::run_if_requested();
}

#[test]
fn parent_exit_before_the_spawn_leaves_nothing() {
    let env = Env::with_budget(1);
    let _full = env.hold_budget(1);
    let counter = env.file("counter");
    let script = format!("echo run >> '{}'", counter.display());
    let runner = env.run_at(None, &["sh", "-c", &script], &[ADMISSION_WAITING], None);
    let mut parent = ChildGuard::new(env.in_shell(&runner, "; exit $?").spawn().unwrap());
    let runner_pid = env.wait_any_reached(ADMISSION_WAITING);
    let runner_start = cardinal::os::start_time_s(runner_pid).unwrap();
    let live_before = env.live_locks().len();
    assert_eq!(live_before, 2, "the held job and the waiting runner");
    assert!(!env.worktree_lock_free());

    parent.kill();
    env.points.release(ADMISSION_WAITING);
    wait_gone(runner_pid, runner_start);

    assert_eq!(env.lines("counter"), Vec::<String>::new(), "never started");
    assert_eq!(env.reservations().len(), 1, "only the held reservation");
    assert_eq!(env.live_locks().len(), 1, "the runner's live lock is gone");
    assert!(env.worktree_lock_free());
    let finished = env.records_of(RecordKind::JobFinished);
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].data["cancelled"], "parent_exited");
}

/// After the spawn: SIGTERM to the group; a grandchild that ignores it keeps the group, and
/// the reservation, alive until SIGKILL 5 s later.
#[test]
fn parent_exit_after_the_spawn_stops_the_group() {
    let env = Env::with_budget(1);
    let grandchild_file = env.file("grandchild");
    let script = format!(
        "(trap '' TERM; exec sleep 30) & echo $! > '{}'; wait",
        grandchild_file.display()
    );
    let runner = env.run_at(None, &["sh", "-c", &script], &[RUN_AFTER_SPAWN], None);
    let mut parent = ChildGuard::new(env.in_shell(&runner, "; exit $?").spawn().unwrap());
    let runner_pid = env.wait_any_reached(RUN_AFTER_SPAWN);
    let _group = GroupGuard(runner_pid);
    let runner_start = cardinal::os::start_time_s(runner_pid).unwrap();
    wait_for_file(&grandchild_file, WAIT);
    let grandchild: u32 = wait_for("the grandchild PID", WAIT, || {
        std::fs::read_to_string(&grandchild_file)
            .ok()?
            .trim()
            .parse()
            .ok()
    });
    let grandchild_start = cardinal::os::start_time_s(grandchild).unwrap();
    let child = env.reservations()[0].child.unwrap();

    parent.kill();
    let killed_at = Instant::now();
    env.points.release(RUN_AFTER_SPAWN);

    wait_gone(child.pid, child.start_s);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        cardinal::os::is_alive(grandchild, grandchild_start),
        "the grandchild ignores SIGTERM"
    );
    assert_eq!(
        env.reservations().len(),
        1,
        "held while the group is not empty"
    );
    assert!(!env.worktree_lock_free());

    wait_gone(grandchild, grandchild_start);
    let stopped_after = killed_at.elapsed();
    assert!(
        stopped_after >= Duration::from_secs(4),
        "SIGKILL comes 5 s after SIGTERM: {stopped_after:?}"
    );
    wait_gone(runner_pid, runner_start);
    assert!(!cardinal::os::unix::group_exists(runner_pid));
    assert_eq!(env.reservations(), []);
    assert!(env.worktree_lock_free());
}
