//! Parent death (A12, `docs/architecture.md` section 5.5): when the runner's parent exits (the
//! Bash call was cancelled), the runner does not start the stage, or, after the spawn, stops
//! the group, and its reservation stays until the group is empty.

#![cfg(unix)]

mod runner_support;
mod support;

use std::os::unix::process::CommandExt as _;
use std::process::Stdio;
use std::time::{Duration, Instant};

use cardinal::admission::{list_reservations, recovery_pass};
use cardinal::config::HostConfig;
use cardinal::contract::{DerivedState, RecordKind};
use cardinal::os::unix::{SIGCONT, SIGSTOP, signal_process};
use cardinal::testpoint::{ADMISSION_WAITING, RUN_AFTER_SPAWN, RUN_BEFORE_STARTING};
use runner_support::{Env, GroupGuard, Pty, WAIT, output, wait_gone};
use support::helper::{ChildGuard, Mode, kill_pid, wait_for, wait_for_file};

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
/// the parent exits while both are stopped in a group that another member keeps from becoming
/// orphaned, nothing continues them, and cardinal never signals the caller's group: an
/// unrelated stopped member stays stopped. Once the runner alone is continued, it sees that its
/// parent exited and ends the stage by SIGTERM within the bound, which frees the in-use lock.
#[test]
fn a_stopped_runner_cancels_once_continued_and_leaves_its_group_alone() {
    for path in ["fail_open", "tty"] {
        let env = Env::with_budget(1);
        let job = StoppedJob::start(&env, path, Caller::Group, "");
        // A member whose parent, this test, is outside the group keeps the group from
        // becoming orphaned when the runner's parent exits, so the kernel does not continue it.
        let mut sibling = env.isolated("sleep");
        sibling.arg("30").process_group(job.parent.cast_signed());
        let sibling = ChildGuard::new(sibling.spawn().unwrap());
        // This test's unreaped child: its PID is pinned.
        signal_process(sibling.id(), SIGSTOP).unwrap();
        wait_for("the sibling to stop", WAIT, || {
            stopped(&env, sibling.id()).then_some(())
        });

        kill_pid(job.parent);
        wait_orphaned(job.runner, job.parent);
        // Long enough for anything that watches the parent to act.
        std::thread::sleep(Duration::from_secs(1));
        for (what, pid) in [
            ("sibling", sibling.id()),
            ("runner", job.runner),
            ("stage", job.stage),
        ] {
            assert!(stopped(&env, pid), "{path}: the {what} stays stopped");
        }
        // The runner only. It stays stopped until this signal, so the PID is still its own.
        signal_process(job.runner, SIGCONT).unwrap();
        job.wait_cancelled(&env, path);
        assert!(
            stopped(&env, sibling.id()),
            "{path}: the sibling stays stopped"
        );
    }
}

/// When the parent exits while the runner is stopped with its stage and the group becomes
/// orphaned, the kernel sends SIGHUP and SIGCONT to the group; the runner, continued, sees that
/// its parent exited and ends the stage by SIGTERM within the bound. The stage ignores SIGHUP,
/// so only the runner's cancellation can end it.
#[test]
fn orphaning_a_stopped_runner_cancels_its_stage() {
    for path in ["fail_open", "tty"] {
        let env = Env::with_budget(1);
        let job = StoppedJob::start(&env, path, Caller::Session, "trap '' HUP; ");
        kill_pid(job.parent);
        job.wait_cancelled(&env, path);
    }
}

/// Where the runner's parent shell runs. It leads a group of its own either way, as a
/// job-control shell gives a job.
#[derive(Clone, Copy)]
enum Caller {
    /// In this test's session; this test is the parent's parent.
    Group,
    /// In a session of its own, whose leader is the parent's parent: once the parent exits, no
    /// member of the group has a parent in another group of the session, so the group becomes
    /// orphaned wherever the runner is adopted (a container's init may share this test's
    /// session).
    Session,
}

/// A stage on `path` (`fail_open` or `tty`) that runs `trap`, then stops itself, with the
/// runner stopped as well.
struct StoppedJob {
    stage: u32,
    stage_start: u64,
    runner: u32,
    runner_start: u64,
    /// The runner's parent shell, which leads the caller's group.
    parent: u32,
    _group: GroupGuard,
    _caller: ChildGuard,
    _pty: Pty,
}

impl StoppedJob {
    fn start(env: &Env, path: &str, caller: Caller, trap: &str) -> Self {
        let pid_file = env.file("pid");
        let script = format!(
            "{trap}echo $$ > '{}'; kill -STOP $$; exec sleep 30",
            pid_file.display()
        );
        let fail = (path == "fail_open").then_some(RUN_BEFORE_STARTING);
        let runner = env.run_at(None, &["sh", "-c", &script], &[], fail);
        let mut parent = env.in_shell(&runner, "; exit $?");
        let mut caller = match caller {
            Caller::Group => {
                parent.process_group(0);
                parent
            }
            Caller::Session => {
                let argv = std::iter::once(parent.get_program())
                    .chain(parent.get_args())
                    .map(|arg| arg.to_str().unwrap().to_owned())
                    .collect();
                let mut lead = support::helper::command(&Mode::LeadGroup { argv });
                lead.envs(
                    parent
                        .get_envs()
                        .filter_map(|(key, value)| Some((key, value?))),
                );
                let mut leader = env.in_shell(&lead, "; exit $?");
                // SAFETY: `setsid` is async-signal-safe.
                unsafe {
                    leader.pre_exec(|| {
                        rustix::process::setsid()?;
                        Ok(())
                    });
                }
                leader
            }
        };
        let pty = Pty::open();
        if path == "tty" {
            caller.stdin(pty.stdin());
        }
        // The helper's test harness prints its banner.
        caller.stdout(Stdio::null());
        let caller = ChildGuard::new(caller.spawn().unwrap());
        let stage: u32 = wait_for("the stage PID", WAIT, || {
            std::fs::read_to_string(&pid_file).ok()?.trim().parse().ok()
        });
        let runner = cardinal::os::parent_pid(stage).unwrap();
        let parent = cardinal::os::parent_pid(runner).unwrap();
        let group = GroupGuard(parent);
        assert_eq!(cardinal::os::unix::pgid_of(runner), Some(parent), "{path}");
        wait_for("the runner to stop", WAIT, || {
            stopped(env, runner).then_some(())
        });
        Self {
            stage,
            stage_start: cardinal::os::start_time_s(stage).unwrap(),
            runner,
            runner_start: cardinal::os::start_time_s(runner).unwrap(),
            parent,
            _group: group,
            _caller: caller,
            _pty: pty,
        }
    }

    /// Waits for the runner to end the stage by SIGTERM within the bound and to exit, and
    /// checks that the in-use lock is free.
    fn wait_cancelled(&self, env: &Env, path: &str) {
        wait_for(
            &format!("{path}: the stopped stage to end"),
            Duration::from_secs(6),
            || (!cardinal::os::is_alive(self.stage, self.stage_start)).then_some(()),
        );
        wait_gone(self.runner, self.runner_start);
        assert!(env.worktree_lock_free(), "{path}");
        let finished = env.records_of(RecordKind::JobFinished);
        assert_eq!(finished[0].data["signal"], 15, "{path}: SIGTERM");
    }
}

/// Whether `pid` is stopped, from `ps`.
fn stopped(env: &Env, pid: u32) -> bool {
    let mut ps = env.isolated("ps");
    ps.args(["-o", "stat=", "-p", &pid.to_string()]);
    output(&mut ps).stdout.trim_ascii_start().starts_with(b"T")
}
