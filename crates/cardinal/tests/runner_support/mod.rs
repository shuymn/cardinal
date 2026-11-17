//! Helpers for the runner and admission tests: an environment with its own state, config,
//! test points, and a worktree; fake reservations that fill the budget; and guards that kill
//! what a test started, by PID.
//!
//! A test crate declares `mod support; mod runner_support;`.

#![allow(
    dead_code,
    reason = "each test crate uses a different subset of the helpers"
)]

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use cardinal::contract::{
    AdmissionResult, GroupIdentity, Priority, ProcessIdentity, Record, RecordKind, Reservation,
    ReservationSchema, ReservationState, RunSpec, RunSpecSchema,
};
use cardinal::state::{self, FileLock, LockMode, StateDir};
use tempfile::TempDir;

use crate::support::cardinal::Dirs;
use crate::support::helper::wait_for;
use crate::support::testpoints::TestPoints;

pub const WAIT: Duration = Duration::from_secs(20);

/// A state dir, a config dir, a test-point dir, and a worktree (a directory with a `.git`
/// directory, outside any repository; no git runs).
pub struct Env {
    pub dirs: Dirs,
    pub points: TestPoints,
    _work: TempDir,
    work: PathBuf,
}

impl Env {
    /// An environment whose `host.toml` is `host_toml`.
    pub fn new(host_toml: &str) -> Self {
        let dirs = Dirs::new();
        std::fs::write(dirs.config().join("host.toml"), host_toml).expect("write host.toml");
        let work_dir = tempfile::Builder::new()
            .prefix("cardinal-work-")
            .tempdir()
            .expect("create the work dir");
        let work = dunce::canonicalize(work_dir.path()).expect("canonical work dir");
        std::fs::create_dir_all(work.join("wt/.git")).expect("create the worktree");
        std::fs::create_dir_all(work.join("wt/sub")).expect("create a subdirectory");
        Self {
            dirs,
            points: TestPoints::new(),
            _work: work_dir,
            work,
        }
    }

    /// A budget of `cpu` and every stage needing one CPU, with short lock deadlines.
    pub fn with_budget(cpu: u64) -> Self {
        Self::new(&format!(
            "[budget]\ncpu = {cpu}\n[admission]\ndefault_resources = {{ cpu = 1 }}\n\
             lock_timeout_ms = 2000\nmax_wait_ms = 20000\n"
        ))
    }

    /// The canonical worktree root.
    pub fn worktree(&self) -> PathBuf {
        self.work.join("wt")
    }

    /// The worktree's in-use key.
    pub fn worktree_key(&self) -> String {
        state::worktree_key(&self.worktree())
    }

    /// A path in the work dir, outside the worktree: counters, logs, flags.
    pub fn file(&self, name: &str) -> PathBuf {
        self.work.join(name)
    }

    pub fn state(&self) -> StateDir {
        StateDir::open(self.dirs.state()).expect("open the state dir")
    }

    /// `cardinal ARGS…` in the worktree, with its own state and config dirs, stdin from
    /// /dev/null, and no test points.
    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cardinal"));
        command
            .args(args)
            .current_dir(self.worktree())
            .env("CARDINAL_STATE_DIR", self.dirs.state())
            .env("CARDINAL_CONFIG_DIR", self.dirs.config())
            .env_remove("CARDINAL_TEST_DIR")
            .env_remove("CARDINAL_TEST_PAUSE")
            .env_remove("CARDINAL_TEST_FAIL")
            .stdin(Stdio::null());
        command
    }

    /// `cardinal run [--spec-b64 …] -- stage…`, as [`Env::command`].
    pub fn run(&self, spec: Option<&RunSpec>, stage: &[&str]) -> Command {
        let encoded = spec.map(RunSpec::encode_b64);
        let mut args = vec!["run"];
        if let Some(encoded) = &encoded {
            args.extend(["--spec-b64", encoded]);
        }
        args.push("--");
        args.extend(stage);
        self.command(&args)
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
    /// it, and can report its status. The environment and cwd are `inner`'s.
    pub fn in_shell(&self, inner: &Command, after: &str) -> Command {
        let mut shell = Command::new("sh");
        shell
            .arg("-c")
            .arg(format!("\"$0\" \"$@\"{after}"))
            .arg(inner.get_program())
            .args(inner.get_args())
            .current_dir(self.worktree())
            .stdin(Stdio::null());
        for (key, value) in inner.get_envs() {
            match value {
                Some(value) => shell.env(key, value),
                None => shell.env_remove(key),
            };
        }
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

/// Runs `command` to completion with captured output, panicking after [`WAIT`].
pub fn output(command: &mut Command) -> Output {
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start");
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    if let Ok(output) = rx.recv_timeout(WAIT) {
        return output.expect("wait");
    }
    crate::support::helper::kill_pid(pid);
    panic!("the command did not finish within {WAIT:?}");
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

/// Kills, by PID, every process of a process group the test caused to exist, when dropped.
/// Never touches this test's own group.
#[cfg(unix)]
pub struct GroupGuard(pub u32);

#[cfg(unix)]
impl Drop for GroupGuard {
    fn drop(&mut self) {
        kill_group(self.0);
    }
}

/// Kills every live member of group `pgid` by PID until none is left (at most 2 s).
#[cfg(unix)]
pub fn kill_group(pgid: u32) {
    let me = std::process::id();
    if cardinal::os::unix::pgid_of(me) == Some(pgid) {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let Ok(table) = cardinal::os::ProcessTable::snapshot(cardinal::os::Detail::default())
        else {
            return;
        };
        let members = cardinal::os::unix::group_members(&table, pgid);
        if members.is_empty() {
            return;
        }
        for member in members {
            crate::support::helper::kill_pid(member.pid);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A pseudo-terminal whose slave end can be a child's stdin. Keep it until the child exits.
#[cfg(unix)]
pub struct Pty {
    _master: std::os::fd::OwnedFd,
    slave: std::fs::File,
}

#[cfg(unix)]
impl Pty {
    pub fn open() -> Self {
        use rustix::fs::{Mode, OFlags};
        use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).expect("open a pty");
        grantpt(&master).expect("grantpt");
        unlockpt(&master).expect("unlockpt");
        let name = ptsname(&master, Vec::new()).expect("ptsname");
        let slave = rustix::fs::open(
            std::path::Path::new(name.to_str().expect("UTF-8 pty name")),
            OFlags::RDWR | OFlags::NOCTTY,
            Mode::empty(),
        )
        .expect("open the pty slave");
        Self {
            _master: master,
            slave: std::fs::File::from(slave),
        }
    }

    /// A stdin for a child.
    pub fn stdin(&self) -> Stdio {
        Stdio::from(self.slave.try_clone().expect("clone the pty slave"))
    }
}
