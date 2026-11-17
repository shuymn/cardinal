//! Jobs of the real runner (`cardinal run`) for tests of what reads reservations: inventory,
//! the inbox, `plan`, and `act` (`docs/architecture.md` sections 4.5 and 13.2). A job runs a
//! stage that leaves its directory for `/` and starts two background `cat`s; the test pauses
//! its runner after the spawn until the stage is in its group, then lets it wait for the
//! stage (a live reservation that holds the worktree's in-use lock), SIGKILLs it (a verified `recovery_needed` reservation that keeps counting while its
//! group lives), or SIGKILLs it before the start boundary (a `stale` reservation). Once the
//! runner is gone, no process of the job works inside the worktree, so only the reservation
//! marks it in use.
//!
//! The stage's processes read the runner's stdin, a pipe the test holds: they live until the
//! test closes it or ends the group, however long the test takes. The config dir must hold a
//! `host.toml` whose budget admits every job of the test at once; a runner is still given the
//! configured admission wait, and more, to reach its pause point.
//!
//! Every runner starts in a process group that an idle process of the test leads, as under a
//! shell without job control, so it makes a group of its own for the stage (section 5.5).
//! The group a runner made is ended by a `GroupGuard` when its job is dropped.

#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use cardinal::config::Config;
use cardinal::contract::{DerivedState, RunSpec, RunSpecSchema};
use cardinal::state::StateDir;
use cardinal::testpoint::{RUN_AFTER_SPAWN, RUN_BEFORE_STARTING};
use cardinal_test_support::process::{GroupGuard, PidGuard, wait_for};
use cardinal_test_support::{Child, Command, Scope};

use crate::support::testpoints::TestPoints;

/// How long a step of a job may take once the runner is admitted.
const WAIT: Duration = Duration::from_secs(20);

/// The stage every job runs: a shell that moves to `/`, starts two `cat`s that read the
/// runner's stdin, prints its own PID and theirs on one line, and waits for them.
const STAGE: [&str; 3] = [
    "sh",
    "-c",
    "exec 3<&0; cd / || exit; cat <&3 >/dev/null & a=$!; cat <&3 >/dev/null & echo $$ $a $!; wait",
];

/// The processes of a job's stage, as its shell reported them.
#[derive(Clone, Copy, Debug)]
pub struct Stage {
    /// The shell: the runner's child, which the reservation records.
    pub shell: u32,
    /// The two `cat`s the shell started in the background.
    pub cats: [u32; 2],
}

/// Starts runners on one state and config dir.
pub struct Jobs<'a> {
    scope: &'a Scope,
    state: PathBuf,
    config: PathBuf,
    /// Leads the group every runner starts in.
    caller: Child,
    points: TestPoints,
}

/// A job whose runner, once its stage is in its group, waits for the stage: its reservation
/// is `live`, and it holds the in-use lock of the worktree it runs in.
pub struct Running {
    pub job: String,
    pub pgid: u32,
    pub stage: Stage,
    runner: Child,
    /// The write end of the pipe the stage's processes read.
    pipe: ChildStdin,
    group: GroupGuard,
}

impl Running {
    /// Kills the runner by SIGKILL, leaving its group running: the reservation becomes
    /// `recovery_needed`.
    pub fn crash(mut self) -> Crashed {
        self.runner.kill();
        Crashed {
            job: self.job,
            pgid: self.pgid,
            stage: self.stage,
            pipe: Some(self.pipe),
            group: Some(self.group),
        }
    }
}

/// A job whose runner was killed after the spawn, with its group still running.
pub struct Crashed {
    pub job: String,
    pub pgid: u32,
    pub stage: Stage,
    pipe: Option<ChildStdin>,
    group: Option<GroupGuard>,
}

impl Crashed {
    /// Kills the stage's shell, the child the reservation records, and leaves both `cat`s in
    /// the group: the reservation stays `recovery_needed`, no longer verified.
    pub fn end_shell(&self, scope: &Scope) {
        PidGuard::new(scope, self.stage.shell).kill();
        wait_for(
            &format!("the end of the shell {}", self.stage.shell),
            WAIT,
            || {
                cardinal::os::start_time_s(self.stage.shell)
                    .is_none()
                    .then_some(())
            },
        );
    }

    /// Ends every member of the job's group, as the test's guard does, and waits until the
    /// group is empty.
    pub fn end_group(&mut self) {
        drop(self.pipe.take());
        drop(self.group.take());
        let pgid = self.pgid;
        wait_for(&format!("group {pgid} to be empty"), WAIT, || {
            (!cardinal::os::unix::group_exists(pgid)).then_some(())
        });
    }
}

impl<'a> Jobs<'a> {
    /// Runners in `scope` with the state dir `state` and the config dir `config`.
    pub fn new(scope: &'a Scope, state: &Path, config: &Path) -> Self {
        let caller = scope
            .command("cat", scope.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("start the caller group's leader");
        Self {
            scope,
            state: state.to_path_buf(),
            config: config.to_path_buf(),
            caller,
            points: TestPoints::new(scope),
        }
    }

    fn state(&self) -> StateDir {
        StateDir::open(&self.state).expect("open the state dir")
    }

    /// `cardinal run --spec-b64 … -- STAGE` in `cwd` for `cpu` CPUs, paused at `point`, and
    /// how long it may take to reach that point: the wait cap of section 5.4 under the
    /// configured admission, and [`WAIT`] more.
    fn runner(&self, cwd: &Path, cpu: u64, point: &str) -> (Command, Duration) {
        let spec = RunSpec {
            schema: RunSpecSchema,
            class: Some("test".into()),
            rule: Some("generic/test".into()),
            resources: cardinal::admission::resources([("cpu", cpu)]),
            timeout_ms: 120_000,
            background: false,
            stage_index: 0,
            stage: STAGE.join(" "),
            session_id: Some("session-1".into()),
            agent_id: None,
            tool_use_id: Some("toolu_1".into()),
        };
        let config = Config::load(&self.config).expect("load the jobs' configuration");
        let reach = cardinal::run::wait_cap(Some(&spec), &config.host) + WAIT;
        let mut command = self.scope.command(env!("CARGO_BIN_EXE_cardinal"), cwd);
        command
            .args(["run", "--spec-b64", &spec.encode_b64(), "--"])
            .args(STAGE)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .env("CARDINAL_STATE_DIR", &self.state)
            .env("CARDINAL_CONFIG_DIR", &self.config)
            .join_group(&self.caller);
        self.points.apply(&mut command, &[point], None);
        (command, reach)
    }

    /// The job of the reservation the runner `pid` wrote.
    fn job_of(&self, pid: u32) -> String {
        let views = cardinal::admission::list_reservations(&self.state()).expect("reservations");
        views
            .into_iter()
            .find(|v| v.reservation.runner.pid == pid)
            .expect("the runner's reservation")
            .reservation
            .job_id
    }

    /// A job for `cpu` CPUs whose runner, started in `cwd`, is paused after the spawn once
    /// both `cat`s are in its group.
    pub fn running(&self, cwd: &Path, cpu: u64) -> Running {
        let (mut command, reach) = self.runner(cwd, cpu, RUN_AFTER_SPAWN);
        let mut runner = command.spawn().expect("start the runner");
        let pipe = runner.stdin.take().expect("the runner's stdin");
        let stdout = runner.stdout.take().expect("the runner's stdout");
        self.points
            .wait_reached(RUN_AFTER_SPAWN, runner.id(), reach);
        let pgid = runner.id();
        let group = GroupGuard::new(self.scope, pgid);
        let job = self.job_of(pgid);
        let stage = stage_line(stdout);
        wait_for("the shell and both cats in the job's group", WAIT, || {
            let views = cardinal::admission::list_reservations(&self.state()).ok()?;
            let view = views.into_iter().find(|v| v.reservation.job_id == job)?;
            (view.members.len() >= 4).then_some(())
        });
        // The pause has done its work: the stage is in its group. A runner left paused would
        // abort at the pause limit (section 13.1) and turn the reservation `recovery_needed`
        // under a slow test; released, it waits for its stage, which lives on the pipe.
        self.points.release(RUN_AFTER_SPAWN);
        Running {
            job,
            pgid,
            stage,
            runner,
            pipe,
            group,
        }
    }

    /// A job whose runner, started in `cwd`, was killed after the spawn: a verified
    /// `recovery_needed` reservation.
    pub fn crashed(&self, cwd: &Path, cpu: u64) -> Crashed {
        let crashed = self.running(cwd, cpu).crash();
        let views = cardinal::admission::list_reservations(&self.state()).expect("reservations");
        let view = views
            .iter()
            .find(|v| v.reservation.job_id == crashed.job)
            .expect("the crashed job's reservation");
        assert_eq!(view.derived, DerivedState::RecoveryNeeded);
        assert!(view.verified, "{view:#?}");
        crashed
    }

    /// A job whose runner, started in `cwd`, was killed before the start boundary: its group
    /// held the runner alone, so the reservation is `stale` until a recovery pass deletes it.
    pub fn stale(&self, cwd: &Path, cpu: u64) -> String {
        let (mut command, reach) = self.runner(cwd, cpu, RUN_BEFORE_STARTING);
        let mut runner = command.spawn().expect("start the runner");
        self.points
            .wait_reached(RUN_BEFORE_STARTING, runner.id(), reach);
        let job = self.job_of(runner.id());
        runner.kill();
        job
    }
}

/// The PIDs the stage's shell printed on the runner's stdout, read within [`WAIT`].
fn stage_line(stdout: impl std::io::Read + Send + 'static) -> Stage {
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = send.send(line);
    });
    let line = receive
        .recv_timeout(WAIT)
        .expect("the stage's PIDs on the runner's stdout");
    let pids: Vec<u32> = line
        .split_whitespace()
        .map(|pid| pid.parse().expect("a PID"))
        .collect();
    match pids[..] {
        [shell, a, b] => Stage {
            shell,
            cats: [a, b],
        },
        _ => panic!("the stage printed {line:?}"),
    }
}
