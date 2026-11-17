//! Parent death (A12, `docs/architecture.md` section 5.5): when the runner's parent exits (the
//! Bash call was cancelled), the runner does not start the stage, or, after the spawn, stops
//! the group, and its reservation stays until the group is empty.

#![cfg(unix)]

mod runner_support;
mod support;

use std::os::unix::process::CommandExt as _;
use std::time::{Duration, Instant};

use cardinal::admission::{list_reservations, recovery_pass};
use cardinal::config::HostConfig;
use cardinal::contract::{DerivedState, RecordKind};
use cardinal::testpoint::{ADMISSION_WAITING, RUN_AFTER_SPAWN, RUN_BEFORE_STARTING, RUN_STOPPING};
use runner_support::{Env, GroupGuard, Pty, WAIT, output, wait_gone};
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

/// The parent exits while the runner is paused right before the spawn, on the admitted path
/// and on the fail-open path: the runner looks again at the start boundary and does not start
/// the stage.
#[test]
fn parent_exit_at_the_start_boundary_cancels() {
    for fail in [None, Some(RUN_BEFORE_STARTING)] {
        let env = Env::with_budget(1);
        let script = format!("echo run >> '{}'", env.file("counter").display());
        let runner = env.run_at(None, &["sh", "-c", &script], &[RUN_BEFORE_STARTING], fail);
        let mut parent = ChildGuard::new(env.in_shell(&runner, "; exit $?").spawn().unwrap());
        let runner_pid = env.wait_any_reached(RUN_BEFORE_STARTING);
        let runner_start = cardinal::os::start_time_s(runner_pid).unwrap();

        let parent_pid = parent.id();
        parent.kill();
        wait_orphaned(runner_pid, parent_pid);
        env.points.release(RUN_BEFORE_STARTING);
        wait_gone(runner_pid, runner_start);

        assert_eq!(env.records_of(RecordKind::JobStarted), [], "{fail:?}");
        assert_eq!(env.lines("counter"), Vec::<String>::new(), "{fail:?}");
        assert_eq!(env.reservations(), [], "{fail:?}");
        assert_eq!(env.live_locks(), Vec::<String>::new(), "{fail:?}");
        assert!(env.worktree_lock_free());
        let finished = env.records_of(RecordKind::JobFinished);
        assert_eq!(finished.len(), 1, "{fail:?}");
        assert_eq!(finished[0].data["cancelled"], "parent_exited", "{fail:?}");
    }
}

/// Waits until the runner is no longer `parent`'s child.
fn wait_orphaned(runner: u32, parent: u32) {
    wait_for("the runner to lose its parent", WAIT, || {
        let now = cardinal::os::process(runner, cardinal::os::Detail::default())?.parent;
        (now != Some(parent)).then_some(())
    });
}

/// After the spawn: SIGTERM to the group; a grandchild that ignores it keeps the group, and
/// the reservation, alive until the runner sends SIGKILL to the group 5 s later. The
/// reservation is left for recovery, which releases it now that the group is empty.
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
    assert!(env.worktree_lock_free());
    let finished = env.records_of(RecordKind::JobFinished);
    assert_eq!(finished.len(), 1, "recorded before SIGKILL");
    let views = list_reservations(&env.state()).unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].derived, DerivedState::Stale);
    let report = recovery_pass(&env.state(), &HostConfig::defaults()).unwrap();
    assert_eq!(report.released, [finished[0].job_id.clone().unwrap()]);
    assert_eq!(env.reservations(), []);
}

/// The direct child exited and left a descendant in the group when the parent exits: the
/// runner takes in the parent's exit before it returns, stops the group with SIGTERM, and
/// releases the reservation once the group is empty.
#[test]
fn parent_exit_after_the_child_exited_stops_its_descendants() {
    let env = Env::with_budget(1);
    let pids = env.file("pids");
    let script = format!(
        "sleep 30 </dev/null >/dev/null 2>&1 & echo $! > '{}'",
        pids.display()
    );
    let runner = env.run_at(None, &["sh", "-c", &script], &[RUN_AFTER_SPAWN], None);
    let mut parent = ChildGuard::new(env.in_shell(&runner, "; exit $?").spawn().unwrap());
    let runner_pid = env.wait_any_reached(RUN_AFTER_SPAWN);
    let _group = GroupGuard(runner_pid);
    let runner_start = cardinal::os::start_time_s(runner_pid).unwrap();
    let descendant: u32 = wait_for("the descendant PID", WAIT, || {
        std::fs::read_to_string(&pids).ok()?.trim().parse().ok()
    });
    let descendant_start = cardinal::os::start_time_s(descendant).unwrap();
    let child = env.reservations()[0].child.unwrap();
    wait_gone(child.pid, child.start_s);

    let parent_pid = parent.id();
    parent.kill();
    wait_orphaned(runner_pid, parent_pid);
    env.points.release(RUN_AFTER_SPAWN);

    wait_for(
        "the descendant to stop on SIGTERM",
        Duration::from_secs(4),
        || (!cardinal::os::is_alive(descendant, descendant_start)).then_some(()),
    );
    wait_gone(runner_pid, runner_start);
    assert_eq!(env.reservations(), [], "released: the group is empty");
    assert!(env.worktree_lock_free());
    let finished = env.records_of(RecordKind::JobFinished);
    assert_eq!(finished[0].data["exit"], 0);
}

/// On the fail-open and TTY paths a stage that stops stops the runner too (job control). When
/// the parent exits meanwhile, the stopped runner cannot notice; its watchdog, outside the
/// stopped process, continues the group, and the runner stops the stage as for any parent
/// exit: the stage, the runner, and the watchdog are gone within the 5 s bound, and the in-use
/// lock is free.
#[test]
fn parent_exit_while_the_stage_is_stopped_stops_it() {
    for path in ["fail_open", "tty"] {
        parent_exit_with_a_stopped_stage(path, false);
    }
}

/// The same when the parent exits after the runner found it alive and before the runner
/// stopped itself.
#[test]
fn parent_exit_as_the_runner_stops_stops_the_stage() {
    for path in ["fail_open", "tty"] {
        parent_exit_with_a_stopped_stage(path, true);
    }
}

/// Runs a stage that stops itself on `path` under a parent shell that leads a group of its
/// own, kills the parent once the runner has stopped (or, with `at_stop`, while the runner is
/// paused at `run.stopping`), and checks that the stage is stopped within the bound.
fn parent_exit_with_a_stopped_stage(path: &str, at_stop: bool) {
    let env = Env::with_budget(1);
    let pid_file = env.file("pid");
    let script = format!(
        "echo $$ > '{}'; kill -STOP $$; exec sleep 30",
        pid_file.display()
    );
    let fail = (path == "fail_open").then_some(RUN_BEFORE_STARTING);
    let pause: &[&str] = if at_stop { &[RUN_STOPPING] } else { &[] };
    let runner = env.run_at(None, &["sh", "-c", &script], pause, fail);
    let mut parent = env.in_shell(&runner, "; exit $?");
    // The caller's group is the parent's own, as a job-control shell gives a job.
    parent.process_group(0);
    let pty = Pty::open();
    if path == "tty" {
        parent.stdin(pty.stdin());
    }
    let mut parent = ChildGuard::new(parent.spawn().unwrap());
    let parent_pid = parent.id();
    let _group = GroupGuard(parent_pid);
    // A member whose parent is outside the group keeps the group from becoming orphaned when
    // the parent exits, so the kernel does not continue it (SIGHUP and SIGCONT).
    let mut anchor = env.isolated("sleep");
    anchor.arg("30").process_group(parent_pid.cast_signed());
    let _anchor = ChildGuard::new(anchor.spawn().unwrap());
    let stage: u32 = wait_for("the stage PID", WAIT, || {
        std::fs::read_to_string(&pid_file).ok()?.trim().parse().ok()
    });
    let stage_start = cardinal::os::start_time_s(stage).unwrap();
    let runner_pid = cardinal::os::parent_pid(stage).unwrap();
    let runner_start = cardinal::os::start_time_s(runner_pid).unwrap();
    let stopped = |pid: u32| {
        let mut ps = env.isolated("ps");
        ps.args(["-o", "stat=", "-p", &pid.to_string()]);
        output(&mut ps).stdout.starts_with(b"T")
    };
    if at_stop {
        env.wait_any_reached(RUN_STOPPING);
    } else {
        wait_for("the runner to stop", WAIT, || {
            stopped(runner_pid).then_some(())
        });
    }
    let others: Vec<(u32, u64)> =
        cardinal::os::ProcessTable::snapshot(cardinal::os::Detail::default())
            .unwrap()
            .iter()
            .filter(|p| p.parent == Some(runner_pid) && p.pid != stage)
            .map(|p| (p.pid, p.start_s))
            .collect();

    parent.kill();
    if at_stop {
        wait_orphaned(runner_pid, parent_pid);
        env.points.release(RUN_STOPPING);
    }
    wait_for(
        &format!("{path}: the stopped stage to end"),
        Duration::from_secs(6),
        || (!cardinal::os::is_alive(stage, stage_start)).then_some(()),
    );
    wait_gone(runner_pid, runner_start);
    assert_eq!(others.len(), 1, "{path}: the watchdog");
    for (pid, start_s) in others {
        wait_gone(pid, start_s);
    }
    assert!(env.worktree_lock_free(), "{path}");
    let finished = env.records_of(RecordKind::JobFinished);
    assert_eq!(finished[0].data["signal"], 15, "{path}: SIGTERM");
}
