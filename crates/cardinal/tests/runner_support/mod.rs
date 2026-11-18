//! Helpers for the runner and admission tests: an environment with its own state, config,
//! test points, and a worktree; fake reservations that fill the budget; and the caller's
//! process group the runner starts in.
//!
//! Every child a test starts goes through the scope of [`Env`] (`docs/architecture.md` section
//! 13.3), so neither the runner nor a stage sees the caller's `HOME`, git configuration, or
//! `GIT_*` variables. Dropping an [`Env`] kills its caller group, then every process still
//! working inside its scope, and asserts that none is left.
//!
//! A test crate declares `mod support; mod runner_support;`.

#![allow(
    dead_code,
    reason = "each test crate uses a different subset of the helpers"
)]

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use crate::support::cardinal::Dirs;
use crate::support::testpoints::TestPoints;
use cardinal::contract::{
    AdmissionResult, GroupIdentity, Priority, ProcessIdentity, Record, RecordKind, Reservation,
    ReservationSchema, ReservationState, RunSpec, RunSpecSchema,
};
use cardinal::state::{self, FileLock, LockMode, StateDir};
#[cfg(unix)]
use cardinal_test_support::Child;
use cardinal_test_support::process::{LeftoverGuard, wait_for};
use cardinal_test_support::{Command, Scope};

pub const WAIT: Duration = Duration::from_secs(20);

/// A state dir, a config dir, a test-point dir, and a worktree (a directory with a `.git`
/// directory; no git runs), all in one scope.
pub struct Env {
    /// Stands in for the shell that runs the runner: an idle process leading the group every
    /// runner starts in, so the runner does not lead its group (section 5.5). Dropped first,
    /// with every fail-open or TTY stage left in its group.
    #[cfg(unix)]
    caller: Child,
    _leftovers: LeftoverGuard,
    pub dirs: Dirs,
    pub points: TestPoints,
}

impl Env {
    /// An environment whose `host.toml` is `host_toml`. Without a `[disk]` table of the
    /// test's own, the disk lines sit below any free space, so the job-end check never starts
    /// a `reconcile` pass (`docs/architecture.md` section 5.2).
    pub fn new(host_toml: &str) -> Self {
        let dirs = Dirs::new();
        let disk = if host_toml.contains("[disk]") {
            ""
        } else {
            "[disk]\nwarning_mib = 1\nrecovery_mib = 2\ncritical_mib = 0\n"
        };
        std::fs::write(
            dirs.config().join("host.toml"),
            format!("{disk}{host_toml}"),
        )
        .expect("write host.toml");
        let scope = dirs.scope();
        std::fs::create_dir_all(scope.path("wt/.git")).expect("create the worktree");
        std::fs::create_dir_all(scope.path("wt/sub")).expect("create a subdirectory");
        let points = TestPoints::new(scope);
        Self {
            #[cfg(unix)]
            caller: scope
                .command("cat", scope.root())
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .expect("start the caller group's leader"),
            _leftovers: LeftoverGuard::new(scope),
            dirs,
            points,
        }
    }

    /// A budget of `cpu` and every stage needing one CPU, with short lock deadlines.
    pub fn with_budget(cpu: u64) -> Self {
        Self::new(&format!(
            "[budget]\ncpu = {cpu}\n[admission]\ndefault_resources = {{ cpu = 1 }}\n\
             lock_timeout_ms = 2000\nmax_wait_ms = 20000\n"
        ))
    }

    pub fn scope(&self) -> &Scope {
        self.dirs.scope()
    }

    /// The canonical worktree root.
    pub fn worktree(&self) -> PathBuf {
        self.scope().path("wt")
    }

    /// The worktree's in-use key.
    pub fn worktree_key(&self) -> String {
        state::worktree_key(&self.worktree())
    }

    /// A path in the work dir, outside the worktree: counters, logs, flags.
    pub fn file(&self, name: &str) -> PathBuf {
        self.scope().path(name)
    }

    pub fn state(&self) -> StateDir {
        StateDir::open(self.dirs.state()).expect("open the state dir")
    }

    /// The process group every runner [`Env::command`] builds starts in.
    #[cfg(unix)]
    pub fn caller_group(&self) -> u32 {
        self.caller.id()
    }

    /// `program` in the worktree with stdin from /dev/null.
    pub fn in_worktree(&self, program: &str) -> Command {
        let mut command = self.scope().command(program, &self.worktree());
        command.stdin(Stdio::null());
        command
    }

    /// Starts `command` in the caller's group, for a program that becomes the runner by `exec`.
    #[cfg(unix)]
    pub fn in_caller_group<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        command.join_group(&self.caller)
    }

    /// `cardinal ARGS…` in `cwd` with its own state and config dirs, no test points, and
    /// stdin from /dev/null, leading its own process group.
    fn cardinal(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut command = self.scope().command(env!("CARGO_BIN_EXE_cardinal"), cwd);
        command
            .args(args)
            .stdin(Stdio::null())
            .env("CARDINAL_STATE_DIR", self.dirs.state())
            .env("CARDINAL_CONFIG_DIR", self.dirs.config());
        command
    }

    /// `cardinal ARGS…` in the worktree, as [`Env::cardinal`], in the caller's group.
    pub fn command(&self, args: &[&str]) -> Command {
        self.command_in(&self.worktree(), args)
    }

    /// [`Env::command`] in `cwd`.
    pub fn command_in(&self, cwd: &Path, args: &[&str]) -> Command {
        #[cfg_attr(windows, expect(unused_mut, reason = "no process groups on Windows"))]
        let mut command = self.cardinal(cwd, args);
        #[cfg(unix)]
        self.in_caller_group(&mut command);
        command
    }

    /// `cardinal run [--spec-b64 …] -- stage…`, as [`Env::command`].
    pub fn run(&self, spec: Option<&RunSpec>, stage: &[&str]) -> Command {
        self.command(&run_args(spec.map(RunSpec::encode_b64).as_deref(), stage))
    }

    /// [`Env::run`], but the runner leads a process group of its own, as the first command of
    /// a job-control shell's pipeline does.
    pub fn run_leading_its_group(&self, spec: Option<&RunSpec>, stage: &[&str]) -> Command {
        let encoded = spec.map(RunSpec::encode_b64);
        self.cardinal(&self.worktree(), &run_args(encoded.as_deref(), stage))
    }

    /// [`Env::run`] with the test points set.
    pub fn run_at(
        &self,
        spec: Option<&RunSpec>,
        stage: &[&str],
        pause: &[&str],
        fail: Option<&str>,
    ) -> Command {
        let mut command = self.run(spec, stage);
        self.points.apply(&mut command, pause, fail);
        command
    }

    /// `inner` run by `sh -c '"$0" "$@"<after>'`: the shell is the runner's parent, waits for
    /// it, can report its status, and leads the group `inner` runs in. The environment is
    /// `inner`'s.
    pub fn in_shell(&self, inner: &Command, after: &str) -> Command {
        let mut shell = self.in_worktree("sh");
        shell
            .arg("-c")
            .arg(format!("\"$0\" \"$@\"{after}"))
            .arg(inner.get_program())
            .args(inner.get_args())
            .copy_env(inner);
        shell
    }

    /// Every reservation file, parsed.
    pub fn reservations(&self) -> Vec<Reservation> {
        let mut found: Vec<Reservation> = std::fs::read_dir(self.state().reservations_dir())
            .expect("list reservations")
            .filter_map(|entry| std::fs::read(entry.ok()?.path()).ok())
            .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
            .collect();
        found.sort_by_key(|r| r.created_ms);
        found
    }

    pub fn reservation(&self, job: &str) -> Option<Reservation> {
        self.reservations().into_iter().find(|r| r.job_id == job)
    }

    /// Every record, oldest first.
    pub fn records(&self) -> Vec<Record> {
        cardinal::record::read_all(&self.state()).expect("read records")
    }

    pub fn records_of(&self, kind: RecordKind) -> Vec<Record> {
        self.records()
            .into_iter()
            .filter(|r| r.kind == kind)
            .collect()
    }

    /// The live lock files.
    pub fn live_locks(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.state().live_dir())
            .expect("list live locks")
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .collect();
        names.sort();
        names
    }

    /// Whether `act` could take the worktree's in-use lock now (exclusive `try_lock`).
    pub fn worktree_lock_free(&self) -> bool {
        state::try_lock(
            &self.state().worktree_lock(&self.worktree_key()),
            LockMode::Exclusive,
        )
        .expect("try the in-use lock")
        .is_some()
    }

    /// Holds the worktree's in-use lock exclusively, as `act` does.
    pub fn hold_worktree_exclusive(&self) -> FileLock {
        state::try_lock(
            &self.state().worktree_lock(&self.worktree_key()),
            LockMode::Exclusive,
        )
        .expect("take the in-use lock")
        .expect("the in-use lock is free")
    }

    /// A live reservation of `cpu` CPUs held by this test process: its live lock is held, so
    /// admission counts it until the returned guard is dropped. Its group does not exist, so
    /// once the lock is dropped the next pass releases it as stale.
    pub fn hold_budget(&self, cpu: u64) -> HeldReservation {
        let state = self.state();
        let job = state::new_id();
        let live = state::create_locked(&state.live_lock(&job)).expect("claim a live lock");
        let reservation = fake_reservation(&job, cpu, unused_pgid(), None);
        let bytes = serde_json::to_vec(&reservation).unwrap();
        std::fs::write(state.reservation(&job), bytes).unwrap();
        HeldReservation {
            job,
            live: Some(live),
        }
    }

    /// Waits until some process reached `point` and returns its PID.
    pub fn wait_any_reached(&self, point: &str) -> u32 {
        let prefix = format!("{point}.");
        wait_for(&format!("{point} reached"), WAIT, || {
            std::fs::read_dir(self.points.dir())
                .ok()?
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .find_map(|name| {
                    name.strip_prefix(&prefix)?
                        .strip_suffix(".reached")?
                        .parse()
                        .ok()
                })
        })
    }

    /// The lines of a file in the work dir; none if it does not exist.
    pub fn lines(&self, name: &str) -> Vec<String> {
        std::fs::read_to_string(self.file(name))
            .map_or_default(|text| text.lines().map(str::to_owned).collect())
    }
}

/// A fake reservation held live by the test.
pub struct HeldReservation {
    pub job: String,
    live: Option<FileLock>,
}

impl HeldReservation {
    /// Drops the live lock: the reservation becomes stale and the next pass releases it.
    pub fn release(&mut self) {
        self.live = None;
    }
}

/// A reservation for tests. Its group and child do not belong to this test unless given.
pub fn fake_reservation(
    job: &str,
    cpu: u64,
    pgid: u32,
    child: Option<ProcessIdentity>,
) -> Reservation {
    Reservation {
        schema: ReservationSchema,
        job_id: job.into(),
        state: ReservationState::Running,
        boot_s: cardinal::os::boot_time_s().expect("boot time"),
        created_ms: state::now_ms(),
        updated_ms: state::now_ms(),
        runner: ProcessIdentity {
            pid: pgid,
            start_s: 1,
        },
        group: GroupIdentity::Pgid {
            pgid,
            leader_start_s: 1,
        },
        child,
        stop_signal_ms: None,
        priority: Priority::Normal,
        resources: cardinal::admission::resources([("cpu", cpu)]),
        admission: AdmissionResult::WithinBudget,
        class: None,
        rule: None,
        worktree: None,
        session: None,
        cwd: "/".into(),
        argv: vec!["fake".into()],
    }
}

/// A PGID no process group has: the highest PID that is not in use.
pub fn unused_pgid() -> u32 {
    (1_000_000..4_000_000)
        .rev()
        .find(|&pid| cardinal::os::process(pid, cardinal::os::Detail::default()).is_none())
        .expect("a free PID")
}

/// A run spec for `cpu` CPUs and a Bash timeout of `timeout_ms`.
pub fn spec(cpu: u64, timeout_ms: u64) -> RunSpec {
    RunSpec {
        schema: RunSpecSchema,
        class: Some("test".into()),
        rule: Some("generic/test".into()),
        resources: cardinal::admission::resources([("cpu", cpu)]),
        timeout_ms,
        background: false,
        stage_index: 0,
        stage: "stage".into(),
        session_id: Some("session-1".into()),
        agent_id: None,
        tool_use_id: Some("toolu_1".into()),
    }
}

/// Runs `command` to completion with captured output, panicking when it has not exited and
/// closed its output within [`WAIT`] (it is killed then).
pub fn output(command: &mut Command) -> Output {
    command.output_within(WAIT).expect("run the command")
}

/// Waits until `pid` is no longer alive with `start_s`.
pub fn wait_gone(pid: u32, start_s: u64) {
    wait_for(&format!("process {pid} to end"), WAIT, || {
        (!cardinal::os::is_alive(pid, start_s)).then_some(())
    });
}

/// Elapsed time since `start`, for readable assertions.
pub fn since(start: Instant) -> Duration {
    start.elapsed()
}

/// `run [--spec-b64 ENCODED] -- stage…`.
fn run_args<'a>(encoded: Option<&'a str>, stage: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec!["run"];
    if let Some(encoded) = encoded {
        args.extend(["--spec-b64", encoded]);
    }
    args.push("--");
    args.extend(stage);
    args
}
