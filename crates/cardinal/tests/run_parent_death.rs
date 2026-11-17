//! Parent death (A12, `docs/architecture.md` section 5.5): when the runner's parent exits (the
//! Bash call was cancelled), the runner does not start the stage, or, after the spawn, stops
//! the group, and its reservation stays until the group is empty.

#![cfg(unix)]

mod runner_support;
mod support;

#[test]
fn helper() {
    support::helper::run_if_requested();
}

use std::process::Stdio;
use std::time::{Duration, Instant};

use cardinal::admission::{list_reservations, recovery_pass};
use cardinal::config::HostConfig;
use cardinal::contract::{DerivedState, RecordKind};
use cardinal::os::unix::{SIGCONT, SIGSTOP, signal_process};
use cardinal::testpoint::{ADMISSION_WAITING, RUN_AFTER_SPAWN, RUN_BEFORE_STARTING};
use cardinal_test_support::Child;
use cardinal_test_support::process::{GroupGuard, PidGuard, is_stopped, wait_for, wait_for_file};
use cardinal_test_support::pty::Pty;
use runner_support::{Env, WAIT, wait_gone};
use support::helper::{self, Mode};

#[test]
fn parent_exit_before_the_spawn_leaves_nothing() {
    let env = Env::with_budget(1);
    let _full = env.hold_budget(1);
    let counter = env.file("counter");
    let script = format!("echo run >> '{}'", counter.display());
    let runner = env.run_at(None, &["sh", "-c", &script], &[ADMISSION_WAITING], None);
    let mut parent = env.in_shell(&runner, "; exit $?").spawn().unwrap();
    let runner_pid = env.wait_any_reached(ADMISSION_WAITING);
    let runner_start = cardinal::os::start_time_s(runner_pid).unwrap();
    let live_before = env.live_locks().len();
    assert_eq!(live_before, 2, "the held job and the waiting runner");
    assert!(!env.worktree_lock_free());

    parent.kill_leader();
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
        let mut parent = env.in_shell(&runner, "; exit $?").spawn().unwrap();
        let runner_pid = env.wait_any_reached(RUN_BEFORE_STARTING);
        let runner_start = cardinal::os::start_time_s(runner_pid).unwrap();

        let parent_pid = parent.id();
        parent.kill_leader();
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
    let mut parent = env.in_shell(&runner, "; exit $?").spawn().unwrap();
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

    parent.kill_leader();
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
    let mut parent = env.in_shell(&runner, "; exit $?").spawn().unwrap();
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
    parent.kill_leader();
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
        let job = StoppedJob::start(&env, path, Caller::Anchored, "");
        let sibling = job.member.expect("an anchored group has a stopped member");

        PidGuard::new(job.parent).kill();
        wait_orphaned(job.runner, job.parent);
        // Long enough for anything that watches the parent to act.
        std::thread::sleep(Duration::from_secs(1));
        for (what, pid) in [
            ("sibling", sibling),
            ("runner", job.runner),
            ("stage", job.stage),
        ] {
            assert!(
                is_stopped(pid),
                "{path}: the {what} stays stopped; {}",
                job.layout()
            );
        }
        // The runner only. It stays stopped until this signal, so the PID is still its own.
        signal_process(job.runner, SIGCONT).unwrap();
        job.wait_cancelled(&env, path);
        assert!(
            is_stopped(sibling),
            "{path}: the sibling stays stopped; {}",
            job.layout()
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
        PidGuard::new(job.parent).kill();
        job.wait_cancelled(&env, path);
    }
}

/// The session the runner's parent shell runs in, always a session of its own: a job-control
/// shell leads the session and gives the parent a group of its own, as it gives a job.
#[derive(Clone, Copy)]
enum Caller {
    /// The job is a pipeline whose second command, a helper this test stops, stays in the
    /// parent's group. That member's parent, the session leader, is in another group of the
    /// session, so the group does not become orphaned when the parent exits. A member whose
    /// parent is this test would not do on every host: when this test runs in launchd's
    /// session, macOS orphans the group once the runner, adopted by launchd, exits.
    Anchored,
    /// Once the parent exits, no member of the group has a parent in another group of the
    /// session, so the group becomes orphaned wherever the runner is adopted (a container's
    /// init may share this test's session).
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
    /// The session leader.
    leader: u32,
    /// The stopped member of the caller's group on [`Caller::Anchored`].
    member: Option<u32>,
    _group: GroupGuard,
    _caller: Child,
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
        let parent = env.in_shell(&runner, "; exit $?");
        let member_file = env.file("member");
        // bash without startup files leads the session; `set -m` gives the job, started in the
        // background so the shell never hands it a terminal, a group of its own. The parent's
        // stdout is /dev/null, as is the leader's.
        let job = match caller {
            Caller::Anchored => {
                let out = member_file.clone();
                let member = helper::command(env.scope(), &Mode::Pid { out }, &env.worktree());
                let words: Vec<String> = std::iter::once(member.get_program())
                    .chain(member.get_args())
                    .map(quoted)
                    .collect();
                format!(
                    "set -m; \"$0\" \"$@\" >/dev/null | {} & exec sleep 60",
                    words.join(" ")
                )
            }
            Caller::Session => "set -m; \"$0\" \"$@\" >/dev/null & wait $!".to_owned(),
        };
        let mut leader = env.in_worktree("bash");
        leader
            .args(["--norc", "--noprofile", "-c", &job])
            .arg(parent.get_program())
            .args(parent.get_args())
            .copy_env(&parent)
            .new_session()
            .stdout(Stdio::null());
        let pty = Pty::open();
        if path == "tty" {
            leader.stdin(pty.stdin());
        }
        let leader = leader.spawn().unwrap();
        let stage: u32 = wait_for("the stage PID", WAIT, || {
            std::fs::read_to_string(&pid_file).ok()?.trim().parse().ok()
        });
        let runner = cardinal::os::parent_pid(stage).unwrap();
        let parent = cardinal::os::parent_pid(runner).unwrap();
        let group = GroupGuard(parent);
        assert_eq!(cardinal::os::unix::pgid_of(runner), Some(parent), "{path}");
        assert_eq!(
            cardinal::os::parent_pid(parent),
            Some(leader.id()),
            "{path}"
        );
        // The member writes its PID once it runs the helper. Stopping it sooner could miss:
        // macOS drops a stop signal that arrives while the process is in `execve`.
        let member = matches!(caller, Caller::Anchored).then(|| {
            let member = wait_for("the member PID", WAIT, || {
                std::fs::read_to_string(&member_file)
                    .ok()?
                    .trim()
                    .parse()
                    .ok()
            });
            // The leader, `sleep` by now, never reaps it, so the PID is still the member's.
            signal_process(member, SIGSTOP).unwrap();
            wait_for("the member to stop", WAIT, || {
                is_stopped(member).then_some(())
            });
            member
        });
        wait_for("the runner to stop", WAIT, || {
            is_stopped(runner).then_some(())
        });
        Self {
            stage,
            stage_start: cardinal::os::start_time_s(stage).unwrap(),
            runner,
            runner_start: cardinal::os::start_time_s(runner).unwrap(),
            parent,
            leader: leader.id(),
            member,
            _group: group,
            _caller: leader,
            _pty: pty,
        }
    }

    /// Each process's parent, group, session, and state, for a failure message.
    fn layout(&self) -> String {
        let mut processes = vec![
            ("test", std::process::id()),
            ("leader", self.leader),
            ("parent", self.parent),
            ("runner", self.runner),
            ("stage", self.stage),
        ];
        processes.extend(self.member.map(|member| ("member", member)));
        processes
            .into_iter()
            .map(|(what, pid)| {
                let session = rustix::process::Pid::from_raw(pid.cast_signed())
                    .and_then(|pid| rustix::process::getsid(Some(pid)).ok())
                    .map(rustix::process::Pid::as_raw_nonzero);
                format!(
                    "{what} {pid}: parent {:?}, group {:?}, session {session:?}, state {:?}",
                    cardinal::os::parent_pid(pid),
                    cardinal::os::unix::pgid_of(pid),
                    state(pid),
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
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

/// `word` quoted for a POSIX shell.
fn quoted(word: &std::ffi::OsStr) -> String {
    let word = word.to_str().expect("a UTF-8 word");
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// `pid`'s state, for a failure message.
fn state(pid: u32) -> &'static str {
    if is_stopped(pid) {
        "stopped"
    } else if cardinal::os::start_time_s(pid).is_some() {
        "running"
    } else {
        "gone"
    }
}
