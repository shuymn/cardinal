//! The runner: start boundary, fail-open, wait loop, child supervision, and release
//! (`docs/architecture.md` section 5).
//!
//! **The stage has started once the spawn has returned a child.** Before that, every failure
//! the runner can observe (signal handlers that cannot be set up, a configuration or state
//! error, a lock past its deadline, a failed spawn, a panic) ends in fail-open: the runner runs
//! the stage itself, once. After it, the runner never runs the stage again; an error ends in
//! waiting for the child and returning its status, or in exit 125 with one line on stderr when
//! the status is unknown.
//!
//! The runner adds no output to a command: waits are recorded, never printed.

use std::ffi::OsString;
use std::io::{IsTerminal as _, Write as _};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::admission::{AdmissionError, Admitter, Job, Pass};
use crate::config::{self, HostConfig};
use crate::contract::{
    AdmissionResult, GroupIdentity, Priority, ProcessIdentity, RecordKind, Reservation,
    ReservationSchema, ReservationSession, ReservationState, ReservationWorktree, Resources,
    RunSpec,
};
use crate::exec::{Event, ExecOutcome, Executor as _, Invocation, LocalExecutor, Running, Watch};
use crate::os;
use crate::record::Recorder;
use crate::state::{self, FileLock, LockMode, StateDir};
use crate::testpoint;

/// The exit status of an infrastructure failure after the stage started, as `env`, `nice`,
/// and `timeout` use it for "the wrapper itself failed".
pub const EXIT_INFRA_FAILURE: u8 = 125;

/// The exit status of a runner whose parent exited before the stage started: a cancelled call
/// nobody waits for (128 + SIGHUP).
const EXIT_CANCELLED: u8 = 129;

/// The budget wait: 250 ms doubling to 2 s between admission passes.
const BUDGET_BACKOFF_START: Duration = Duration::from_millis(250);
const BUDGET_BACKOFF_MAX: Duration = Duration::from_millis(2000);

/// The in-use lock wait: 5 ms doubling to 50 ms between attempts, as for every lock.
const INUSE_BACKOFF_START: Duration = Duration::from_millis(5);
const INUSE_BACKOFF_MAX: Duration = Duration::from_millis(50);

/// Reservations keep at most this many bytes of argv.
pub const ARGV_LIMIT: usize = 16 * 1024;

/// The wait cap of section 5.4: `max_wait_ms` for a background call or a run without a spec,
/// otherwise `min(max_wait_ms, timeout_ms × wait_fraction)`.
#[must_use]
pub fn wait_cap(spec: Option<&RunSpec>, host: &HostConfig) -> Duration {
    let max = host.admission.max_wait_ms;
    let ms = match spec {
        Some(spec) if !spec.background => {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss,
                reason = "a share of a millisecond timeout; the fraction is between 0 and 1"
            )]
            let share = (spec.timeout_ms as f64 * host.admission.wait_fraction) as u64;
            share.min(max)
        }
        _ => max,
    };
    Duration::from_millis(ms)
}

/// `argv` as strings, cut to [`ARGV_LIMIT`] bytes in total on a character boundary.
#[must_use]
pub fn truncate_argv(argv: &[OsString]) -> Vec<String> {
    let mut left = ARGV_LIMIT;
    let mut out = Vec::new();
    for arg in argv {
        if left == 0 {
            break;
        }
        let arg = arg.to_string_lossy();
        let kept = &arg[..arg.floor_char_boundary(left)];
        left -= kept.len();
        out.push(kept.to_owned());
    }
    out
}

/// Runs one stage: `cardinal run [--spec-b64 SPEC] -- COMMAND [ARG]...`. Returns the stage's
/// exit status, or ends this process by the signal that ended the stage (section 5.6).
pub fn run(spec_b64: Option<&str>, command: &[OsString]) -> ExitCode {
    let started = Instant::now();
    silence_panics();
    // Before the first child: the runner's children stay unreaped until it reaps them.
    #[cfg(unix)]
    os::unix::reset_child_signal();
    // Before any other thread: no thread may take a signal while its handler is set up.
    let (watch, signals) = Watch::start();
    let tty = std::io::stdin().is_terminal();
    let unwatched = signals.err().map(|err| FailOpen::new("signal_setup", &err));
    let mut runner = Runner::new(started, command, watch, unwatched);
    let mut result = catch_unwind(AssertUnwindSafe(|| runner.drive(spec_b64, tty)));
    // A panic in recovery re-enters it, and recovery goes on from what the runner holds: a
    // stage that has started is waited for, never run again. A runner that still cannot get
    // past its panics ends without unwinding, like one that crashed, and leaves its job to the
    // recovery pass.
    for _ in 0..RECOVERY_ROUNDS {
        if let Ok(code) = result {
            return code;
        }
        let message = panic_message();
        result = catch_unwind(AssertUnwindSafe(|| runner.after_panic(&message)));
    }
    result.unwrap_or_else(|_| std::process::abort())
}

/// How many times a panic in recovery re-enters recovery.
const RECOVERY_ROUNDS: usize = 3;

/// The last panic message; panics print nothing, because the runner's stderr is the stage's.
static PANIC: Mutex<Option<String>> = Mutex::new(None);

fn silence_panics() {
    std::panic::set_hook(Box::new(|info| {
        if let Ok(mut slot) = PANIC.lock() {
            *slot = Some(info.to_string());
        }
    }));
}

fn panic_message() -> String {
    PANIC
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
        .unwrap_or_else(|| "panic".into())
}

/// Why the runner fails open.
#[derive(Clone, Debug)]
struct FailOpen {
    reason: &'static str,
    detail: String,
}

impl FailOpen {
    fn new(reason: &'static str, detail: &dyn std::fmt::Display) -> Self {
        Self {
            reason,
            detail: detail.to_string(),
        }
    }
}

/// Why the runner stops before the spawn without running the stage.
#[derive(Clone, Copy, Debug)]
enum Cancel {
    Signal(i32),
    ParentExited,
}

fn cancel_of(event: Event) -> Option<Cancel> {
    match event {
        Event::Signal(sig) => Some(Cancel::Signal(sig)),
        Event::ParentExited => Some(Cancel::ParentExited),
        Event::ChildChanged => None,
    }
}

/// What the pre-start steps decided.
enum Next {
    /// The reservation is at `starting`; spawn now.
    Admitted,
    FailOpen(FailOpen),
    Passthrough,
    Cancelled(Cancel),
}

/// The stage once `spawn()` has returned a child: it is never run again (section 5.3).
struct Stage {
    path: &'static str,
    spawned: Instant,
    /// Until the wait takes it.
    running: Option<Box<dyn Running>>,
    /// Read right after the spawn.
    child: Option<ProcessIdentity>,
    /// How the stage ended, once known.
    outcome: Option<ExecOutcome>,
}

/// The state directory, `host.toml`, and the recorder.
struct Context {
    state: StateDir,
    host: HostConfig,
    recorder: Recorder,
}

struct Runner {
    started: Instant,
    inv: Invocation,
    watch: Watch,
    /// Why the signals are not watched (step 1), which makes the run fail open.
    unwatched: Option<FailOpen>,
    executor: LocalExecutor,
    job_id: String,
    spec: Option<RunSpec>,
    ctx: Option<Context>,
    /// The worktree in-use lock, held shared until the stage has exited.
    inuse: Option<FileLock>,
    worktree: Option<ReservationWorktree>,
    job: Option<Job>,
    waited: Option<Duration>,
    admission: Option<AdmissionResult>,
    stage: Option<Stage>,
    cancelled: Option<Cancel>,
}

impl Runner {
    fn new(
        started: Instant,
        command: &[OsString],
        watch: Watch,
        unwatched: Option<FailOpen>,
    ) -> Self {
        Self {
            started,
            inv: Invocation {
                argv: command.to_vec(),
            },
            executor: LocalExecutor::new(watch.clone()),
            watch,
            unwatched,
            job_id: state::new_id(),
            spec: None,
            ctx: None,
            inuse: None,
            worktree: None,
            job: None,
            waited: None,
            admission: None,
            stage: None,
            cancelled: None,
        }
    }

    /// Everything after the watch has started, inside the panic boundary of section 5.3.
    fn drive(&mut self, spec_b64: Option<&str>, tty: bool) -> ExitCode {
        match self.prepare(spec_b64, tty) {
            Next::Admitted => self.start_admitted(),
            Next::FailOpen(reason) => self.fail_open(&reason),
            Next::Passthrough => self.run_in_caller_group("tty"),
            Next::Cancelled(cancel) => self.cancel(cancel),
        }
    }

    /// After a panic anywhere in [`Runner::drive`] or in an earlier recovery: before the spawn
    /// the runner fails open and runs the stage once; after it, the runner waits for the child
    /// (again) and exits with its status, or with 125 when the status is unknown.
    fn after_panic(&mut self, message: &str) -> ExitCode {
        if let Some(cancel) = self.cancelled {
            return cancel_exit(cancel);
        }
        let Some(stage) = self.stage.as_mut() else {
            return self.fail_open(&FailOpen::new("panic", &message));
        };
        if let Some(outcome) = stage.outcome.take() {
            // The panic came while finishing: a reservation still there stays for recovery.
            if let Some(job) = self.job.take() {
                job.abandon();
            }
            drop(self.inuse.take());
            return exit_with(&outcome);
        }
        let running = stage.running.take();
        let child = stage.child;
        self.record(RecordKind::JobInfraFailure, json!({ "reason": message }));
        let outcome = catch_unwind(AssertUnwindSafe(|| match (running, child) {
            (Some(running), _) => running.wait(),
            (None, Some(child)) => match self.executor.adopt(child.pid) {
                Some(running) => Box::new(running).wait(),
                None => ExecOutcome::InfraFailure(message.to_owned()),
            },
            (None, None) => ExecOutcome::InfraFailure(message.to_owned()),
        }))
        .unwrap_or_else(|_| ExecOutcome::InfraFailure(panic_message()));
        match catch_unwind(AssertUnwindSafe(|| self.finish(&outcome))) {
            Ok(code) => code,
            Err(_) => exit_with(&outcome),
        }
    }

    /// Steps 3–7 of section 5.2 up to the spawn.
    fn prepare(&mut self, spec_b64: Option<&str>, tty: bool) -> Next {
        // A configuration or spec error still takes the in-use lock, with the defaults' wait
        // cap, before failing open; only an unusable state directory runs without it.
        let loaded = self.load(spec_b64);
        let deadline = self.started
            + self.ctx.as_ref().map_or(Duration::ZERO, |ctx| {
                wait_cap(self.spec.as_ref(), &ctx.host)
            });
        if let Some(cancel) = self.pending_cancel() {
            return Next::Cancelled(cancel);
        }
        if let Err(cancel) = self.take_inuse(deadline) {
            return Next::Cancelled(cancel);
        }
        if tty {
            self.record(RecordKind::TtyPassthrough, json!({ "cwd": cwd_string() }));
            return Next::Passthrough;
        }
        if let Some(fail) = self.unwatched.take() {
            return Next::FailOpen(fail);
        }
        if let Err(fail) = loaded {
            return Next::FailOpen(fail);
        }
        let group = match self.executor.prepare() {
            Ok(group) => group,
            Err(err) => return Next::FailOpen(FailOpen::new("group_setup", &err)),
        };
        let draft = match self.draft(group) {
            Ok(draft) => draft,
            Err(fail) => return Next::FailOpen(fail),
        };
        if let Err(next) = self.admit(&draft, deadline) {
            return next;
        }
        if let Some(cancel) = self.pending_cancel() {
            return Next::Cancelled(cancel);
        }
        if let Err(err) = testpoint::hit(testpoint::RUN_BEFORE_STARTING) {
            return Next::FailOpen(FailOpen::new("injected", &err));
        }
        if let Some(job) = &mut self.job
            && let Err(err) = job.starting()
        {
            return Next::FailOpen(FailOpen::new("state_error", &err));
        }
        Next::Admitted
    }

    /// Step 3: the state directory, `host.toml`, and the spec. The state directory comes first,
    /// so a configuration or spec error can still be recorded.
    fn load(&mut self, spec_b64: Option<&str>) -> Result<(), FailOpen> {
        let state = config::state_dir()
            .map_err(|err| err.to_string())
            .and_then(|dir| StateDir::open(&dir).map_err(|err| err.to_string()))
            .map_err(|err| FailOpen::new("state_unavailable", &err))?;
        let defaults = HostConfig::defaults();
        self.ctx = Some(Context {
            recorder: Recorder::new(&state, defaults.records.max_bytes, defaults.records.keep),
            state,
            host: defaults,
        });
        let host = config::config_dir()
            .and_then(|dir| HostConfig::load(&dir))
            .map_err(|err| FailOpen::new("config_invalid", &err))?;
        let ctx = self.ctx.as_mut().expect("set above");
        ctx.recorder = Recorder::new(&ctx.state, host.records.max_bytes, host.records.keep);
        ctx.host = host;
        self.spec = spec_b64
            .map(RunSpec::decode_b64)
            .transpose()
            .map_err(|err| FailOpen::new("spec_invalid", &err))?;
        Ok(())
    }

    /// A signal that arrived, or the parent's exit, before now.
    fn pending_cancel(&self) -> Option<Cancel> {
        while let Some(event) = self.watch.poll() {
            if let Some(cancel) = cancel_of(event) {
                return Some(cancel);
            }
        }
        self.watch.parent_exited().then_some(Cancel::ParentExited)
    }

    /// Step 4: the worktree in-use lock, shared, waiting until `deadline`. When it cannot be
    /// taken, the stage runs without it and `in_use_unprotected` is recorded.
    fn take_inuse(&mut self, deadline: Instant) -> Result<(), Cancel> {
        let Some(ctx) = &self.ctx else { return Ok(()) };
        let Some(root) = std::env::current_dir()
            .ok()
            .and_then(|cwd| state::find_worktree_root(&cwd))
        else {
            return Ok(());
        };
        let key = state::worktree_key(&root);
        let path = ctx.state.worktree_lock(&key);
        let mut delay = INUSE_BACKOFF_START;
        let mut waiting = false;
        let taken = loop {
            match state::try_lock(&path, LockMode::Shared) {
                Ok(Some(lock)) => break Ok(lock),
                Ok(None) => {}
                Err(err) => break Err(err.to_string()),
            }
            if !waiting {
                waiting = true;
                if let Err(err) = testpoint::hit(testpoint::INUSE_WAITING) {
                    break Err(err.to_string());
                }
            }
            let now = Instant::now();
            if now >= deadline {
                break Err("the in-use lock was not free within the wait cap".into());
            }
            if let Some(cancel) = self
                .watch
                .next(delay.min(deadline - now))
                .and_then(cancel_of)
            {
                return Err(cancel);
            }
            delay = (delay * 2).min(INUSE_BACKOFF_MAX);
        };
        let worktree = ReservationWorktree {
            key,
            path: root.to_string_lossy().into_owned(),
            protected: taken.is_ok(),
        };
        match taken {
            Ok(lock) => self.inuse = Some(lock),
            Err(reason) => self.record(
                RecordKind::InUseUnprotected,
                json!({ "worktree": worktree.path, "key": worktree.key, "reason": reason }),
            ),
        }
        self.worktree = Some(worktree);
        Ok(())
    }

    /// The reservation the admission passes write, before admission fills in its result.
    fn draft(&self, group: GroupIdentity) -> Result<Reservation, FailOpen> {
        let ctx = self.ctx.as_ref().expect("loaded");
        let boot_s = os::boot_time_s().map_err(|err| FailOpen::new("boot_time", &err))?;
        let GroupIdentity::Pgid { leader_start_s, .. } = group;
        let resources: Resources = self.spec.as_ref().map_or_else(
            || ctx.host.admission.default_resources.clone(),
            |spec| spec.resources.clone(),
        );
        let session = self.spec.as_ref().and_then(|spec| {
            Some(ReservationSession {
                id: spec.session_id.clone()?,
                agent_id: spec.agent_id.clone(),
                tool_use_id: spec.tool_use_id.clone(),
                stage_index: spec.stage_index,
            })
        });
        Ok(Reservation {
            schema: ReservationSchema,
            job_id: self.job_id.clone(),
            state: ReservationState::Reserved,
            boot_s,
            created_ms: 0,
            updated_ms: 0,
            runner: ProcessIdentity {
                pid: os::current_pid(),
                start_s: leader_start_s,
            },
            group,
            child: None,
            stop_signal_ms: None,
            priority: Priority::Normal,
            resources,
            admission: AdmissionResult::WithinBudget,
            class: self.spec.as_ref().and_then(|s| s.class.clone()),
            rule: self.spec.as_ref().and_then(|s| s.rule.clone()),
            worktree: self.worktree.clone(),
            session,
            cwd: cwd_string(),
            argv: truncate_argv(&self.inv.argv),
        })
    }

    /// Step 6: admission passes until the job is admitted, over budget once the wait cap is
    /// reached.
    fn admit(&mut self, draft: &Reservation, deadline: Instant) -> Result<(), Next> {
        let ctx = self.ctx.as_ref().expect("loaded");
        let mut admitter = Admitter::new(
            &ctx.state,
            &ctx.host,
            self.job_id.clone(),
            Some(ctx.recorder.clone()),
        );
        let mut delay = BUDGET_BACKOFF_START;
        loop {
            if let Some(cancel) = self.pending_cancel() {
                return Err(Next::Cancelled(cancel));
            }
            let overflow = Instant::now() >= deadline;
            let pass = admitter.pass(draft, overflow);
            admitter.job_id().clone_into(&mut self.job_id);
            match pass {
                Ok(Pass::Admitted(job)) => {
                    self.admission = Some(job.reservation().admission);
                    self.job = Some(*job);
                    break;
                }
                Ok(Pass::Full) => {}
                Err(AdmissionError::LockTimeout) => {
                    return Err(Next::FailOpen(FailOpen::new(
                        "admission_lock_timeout",
                        &AdmissionError::LockTimeout,
                    )));
                }
                Err(err) => return Err(Next::FailOpen(FailOpen::new("state_error", &err))),
            }
            if let Err(err) = testpoint::hit(testpoint::ADMISSION_WAITING) {
                return Err(Next::FailOpen(FailOpen::new("injected", &err)));
            }
            let sleep = delay.min(deadline.saturating_duration_since(Instant::now()));
            if let Some(cancel) = self.watch.next(sleep).and_then(cancel_of) {
                return Err(Next::Cancelled(cancel));
            }
            delay = (delay * 2).min(BUDGET_BACKOFF_MAX);
        }
        drop(admitter);
        self.waited = Some(self.started.elapsed());
        if let Err(err) = testpoint::hit(testpoint::ADMISSION_RESERVED) {
            return Err(Next::FailOpen(FailOpen::new("injected", &err)));
        }
        let reservation = self.job.as_ref().map(Job::reservation);
        let data = json!({
            "admission": self.admission,
            "waited_ms": self.waited_ms(),
            "resources": reservation.map(|r| &r.resources),
            "class": reservation.and_then(|r| r.class.as_deref()),
            "rule": reservation.and_then(|r| r.rule.as_deref()),
        });
        self.record(RecordKind::JobAdmitted, data);
        Ok(())
    }

    /// Step 7 onward on the admitted path.
    fn start_admitted(&mut self) -> ExitCode {
        // Every wait before the spawn may have outlasted the caller.
        if let Some(cancel) = self.pending_cancel() {
            return self.cancel(cancel);
        }
        if let Err(err) = testpoint::hit(testpoint::RUN_SPAWNING) {
            return self.fail_open(&FailOpen::new("injected", &err));
        }
        let running = match self.executor.start(&self.inv) {
            Ok(running) => running,
            // The OS guarantees that no child ran.
            Err(err) => return self.fail_open(&FailOpen::new("spawn_failed", &err)),
        };
        let child = self.begin(running, "admitted");
        self.after_spawn(child);
        self.wait_and_finish()
    }

    /// The spawn returned a child: the stage has started. This is marked before anything else
    /// happens, so that from here on a panic ends in waiting for the child, never in running
    /// the stage again.
    fn begin(&mut self, running: Box<dyn Running>, path: &'static str) -> ProcessIdentity {
        let stage = self.stage.insert(Stage {
            path,
            spawned: Instant::now(),
            running: Some(running),
            child: None,
            outcome: None,
        });
        let injected = testpoint::hit(testpoint::RUN_SPAWNED);
        let child = stage
            .running
            .as_ref()
            .map_or(ProcessIdentity { pid: 0, start_s: 0 }, |r| r.child());
        stage.child = Some(child);
        if let Err(err) = injected {
            self.record(
                RecordKind::JobInfraFailure,
                json!({ "reason": err.to_string() }),
            );
        }
        child
    }

    /// Step 8 and 9: waits for the child, then finishes.
    fn wait_and_finish(&mut self) -> ExitCode {
        let running = self.stage.as_mut().and_then(|stage| stage.running.take());
        let outcome = running.map_or_else(
            || ExecOutcome::InfraFailure("the command was already waited for".into()),
            Running::wait,
        );
        self.finish(&outcome)
    }

    fn after_spawn(&mut self, child: ProcessIdentity) {
        if let Some(job) = &mut self.job
            && let Err(err) = job.running(child)
        {
            self.record(
                RecordKind::JobInfraFailure,
                json!({ "reason": format!("cannot write `running`: {err}") }),
            );
        }
        let pgid = self.job.as_ref().map(|job| {
            let GroupIdentity::Pgid { pgid, .. } = job.reservation().group;
            pgid
        });
        self.record(
            RecordKind::JobStarted,
            json!({ "path": "admitted", "pid": child.pid, "start_s": child.start_s, "pgid": pgid }),
        );
        if let Err(err) = testpoint::hit(testpoint::RUN_AFTER_SPAWN) {
            self.record(
                RecordKind::JobInfraFailure,
                json!({ "reason": err.to_string() }),
            );
        }
    }

    /// Fail-open: drop the reservation and live lock if taken, restore the original process
    /// group, and run the stage in it, still holding the in-use lock until it exits.
    fn fail_open(&mut self, fail: &FailOpen) -> ExitCode {
        self.record(
            RecordKind::JobFailOpen,
            json!({ "reason": fail.reason, "detail": fail.detail }),
        );
        if let Some(job) = self.job.take() {
            let _ = job.release();
        }
        let _ = self.executor.restore_group();
        self.run_in_caller_group("fail_open")
    }

    /// Runs the stage in the caller's process group (fail-open and TTY passthrough). A stage
    /// that cannot be executed ends as a shell reports it: `<command>: <OS error>` and 127
    /// when it is not found, 126 otherwise.
    fn run_in_caller_group(&mut self, path: &'static str) -> ExitCode {
        if let Some(cancel) = self.pending_cancel() {
            return self.cancel(cancel);
        }
        match self.executor.spawn(&self.inv) {
            Ok(running) => {
                let child = self.begin(Box::new(running), path);
                self.record(
                    RecordKind::JobStarted,
                    json!({ "path": path, "pid": child.pid, "start_s": child.start_s }),
                );
                self.wait_and_finish()
            }
            Err(err) => {
                let code: u8 = if err.kind() == std::io::ErrorKind::NotFound {
                    127
                } else {
                    126
                };
                let command = self
                    .inv
                    .argv
                    .first()
                    .map_or_default(|c| c.to_string_lossy().into_owned());
                let _ = writeln!(std::io::stderr(), "{command}: {}", os_message(&err));
                self.record(
                    RecordKind::JobFinished,
                    json!({
                        "path": path, "exit": code, "signal": null, "spawn_error": err.to_string(),
                        "waited_ms": self.waited_ms(), "admission": self.admission,
                        "measurements": [],
                    }),
                );
                drop(self.inuse.take());
                ExitCode::from(code)
            }
        }
    }

    /// Step 9: release, record, and exit as the stage did. The reservation is released only
    /// when no member of the stage's group is left; otherwise it stays for the recovery pass,
    /// which releases it once the group is empty (section 4.5).
    fn finish(&mut self, outcome: &ExecOutcome) -> ExitCode {
        let Some(stage) = self.stage.as_mut() else {
            return exit_with(outcome);
        };
        stage.outcome = Some(outcome.clone());
        let path = stage.path;
        let wall_ms = u64::try_from(stage.spawned.elapsed().as_millis()).unwrap_or(u64::MAX);
        // The group that outlived the SIGTERM sent when the parent exited.
        let mut kill = None;
        if let Some(empty) = self.job.as_ref().map(|job| self.group_is_empty(job))
            && let Some(job) = self.job.take()
        {
            if empty {
                let _ = job.release();
            } else {
                let GroupIdentity::Pgid { pgid, .. } = job.reservation().group;
                kill = self.watch.group_kill_due().then_some(pgid);
                job.abandon();
            }
        }
        let (exit, signal) = match outcome {
            ExecOutcome::Exited(code) => (Some(*code), None),
            ExecOutcome::Signaled(sig) => (None, Some(*sig)),
            ExecOutcome::InfraFailure(_) => (None, None),
        };
        if let ExecOutcome::InfraFailure(reason) = outcome {
            self.record(RecordKind::JobInfraFailure, json!({ "reason": reason }));
        }
        self.record(
            RecordKind::JobFinished,
            json!({
                "path": path, "exit": exit, "signal": signal, "waited_ms": self.waited_ms(),
                "admission": self.admission,
                "measurements": [{ "kind": "wall_time", "ms": wall_ms }],
            }),
        );
        drop(self.inuse.take());
        if let Some(pgid) = kill {
            kill_group(pgid);
        }
        exit_with(outcome)
    }

    /// The parent exited or a signal arrived before the spawn: release everything and stop
    /// without starting the stage.
    fn cancel(&mut self, cancel: Cancel) -> ExitCode {
        self.cancelled = Some(cancel);
        let reason = match cancel {
            Cancel::Signal(sig) => format!("signal {sig}"),
            Cancel::ParentExited => "parent_exited".into(),
        };
        self.record(
            RecordKind::JobFinished,
            json!({
                "cancelled": reason, "exit": null, "signal": null,
                "waited_ms": self.waited_ms(), "admission": self.admission, "measurements": [],
            }),
        );
        if let Some(job) = self.job.take() {
            let _ = job.release();
        }
        drop(self.inuse.take());
        cancel_exit(cancel)
    }

    /// Whether the job's group has no member left besides the runner. The runner first moves
    /// back into the caller's group, so that `kill(-pgid, 0)` sees only the stage's processes;
    /// no other process can create a group with this ID while the runner is alive. When it
    /// cannot move, it reads the process table instead. A group that cannot be read counts as
    /// not empty.
    #[cfg(unix)]
    fn group_is_empty(&self, job: &Job) -> bool {
        let GroupIdentity::Pgid { pgid, .. } = job.reservation().group;
        if self.executor.restore_group() {
            !os::unix::group_exists(pgid)
        } else {
            os::unix::other_group_members(pgid).is_ok_and(|members| members.is_empty())
        }
    }

    #[cfg(not(unix))]
    #[allow(
        clippy::unused_self,
        reason = "the same signature as on Unix; no job is admitted on this platform"
    )]
    fn group_is_empty(&self, _job: &Job) -> bool {
        true
    }

    fn waited_ms(&self) -> Option<u64> {
        self.waited
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }

    /// Appends a record for this job. Records are best-effort (section 4.7): a failure, a
    /// panic included, loses the record and is never surfaced.
    fn record(&self, kind: RecordKind, data: Value) {
        let Some(ctx) = &self.ctx else { return };
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let mut record = ctx.recorder.record(kind);
            record.job_id = Some(self.job_id.clone());
            record.session_id = self.spec.as_ref().and_then(|s| s.session_id.clone());
            if let Value::Object(map) = data {
                record.data = map;
            }
            ctx.recorder.append(&record)
        }));
    }
}

/// Exits as the stage did (section 5.6).
fn exit_with(outcome: &ExecOutcome) -> ExitCode {
    match outcome {
        ExecOutcome::Exited(code) => exit_code(*code),
        ExecOutcome::Signaled(sig) => die_by_signal(*sig),
        ExecOutcome::InfraFailure(reason) => {
            let _ = writeln!(
                std::io::stderr(),
                "[cardinal] Infrastructure failure after the command started: {reason}. \
                 The command was not rerun."
            );
            ExitCode::from(EXIT_INFRA_FAILURE)
        }
    }
}

/// Exits as a cancelled run does: by the signal, or with 129 when the parent exited.
fn cancel_exit(cancel: Cancel) -> ExitCode {
    match cancel {
        Cancel::Signal(sig) => die_by_signal(sig),
        Cancel::ParentExited => ExitCode::from(EXIT_CANCELLED),
    }
}

/// Sends SIGKILL to the job's group once the job is recorded and its reservation left for
/// recovery. Nobody waits for the runner any more: when it is still a member, it ends here
/// too, and the reservation is released once the group is empty.
#[cfg(unix)]
fn kill_group(pgid: u32) {
    let _ = std::io::stderr().flush();
    let _ = os::unix::signal_group(pgid, os::unix::SIGKILL);
}

#[cfg(not(unix))]
fn kill_group(_pgid: u32) {}

fn cwd_string() -> String {
    std::env::current_dir().map_or_default(|cwd| cwd.to_string_lossy().into_owned())
}

/// The OS error text without Rust's ` (os error N)` suffix, as shells print it.
fn os_message(err: &std::io::Error) -> String {
    let text = err.to_string();
    match text.find(" (os error ") {
        Some(at) => text[..at].to_owned(),
        None => text,
    }
}

fn exit_code(code: i32) -> ExitCode {
    match u8::try_from(code) {
        Ok(code) => ExitCode::from(code),
        // Windows exit codes are 32 bits wide; everything is released by now.
        Err(_) => std::process::exit(code),
    }
}

#[cfg(unix)]
fn die_by_signal(sig: i32) -> ExitCode {
    let _ = std::io::stderr().flush();
    os::unix::die_by_signal(sig)
}

#[cfg(not(unix))]
fn die_by_signal(sig: i32) -> ExitCode {
    exit_code(128 + sig)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::time::Duration;

    use super::{ARGV_LIMIT, os_message, truncate_argv, wait_cap};
    use crate::config::HostConfig;
    use crate::contract::{RunSpec, RunSpecSchema};

    fn spec(timeout_ms: u64, background: bool) -> RunSpec {
        RunSpec {
            schema: RunSpecSchema,
            class: None,
            rule: None,
            resources: crate::admission::resources([("cpu", 1)]),
            timeout_ms,
            background,
            stage_index: 0,
            stage: "x".into(),
            session_id: None,
            agent_id: None,
            tool_use_id: None,
        }
    }

    #[test]
    fn wait_cap_is_a_share_of_the_bash_timeout_bounded_by_max_wait() {
        let mut host = HostConfig::defaults();
        host.admission.max_wait_ms = 30_000;
        host.admission.wait_fraction = 0.25;
        assert_eq!(
            wait_cap(Some(&spec(120_000, false)), &host),
            Duration::from_secs(30)
        );
        assert_eq!(
            wait_cap(Some(&spec(40_000, false)), &host),
            Duration::from_secs(10)
        );
        assert_eq!(
            wait_cap(Some(&spec(1_000, true)), &host),
            Duration::from_secs(30),
            "background calls wait up to max_wait_ms"
        );
        assert_eq!(wait_cap(None, &host), Duration::from_secs(30));
    }

    #[test]
    fn argv_is_cut_to_the_limit_on_a_char_boundary() {
        let long = "é".repeat(ARGV_LIMIT);
        let argv = [
            OsString::from("make"),
            OsString::from(long),
            OsString::from("x"),
        ];
        let cut = truncate_argv(&argv);
        assert_eq!(cut[0], "make");
        assert!(cut.iter().map(String::len).sum::<usize>() <= ARGV_LIMIT);
        assert!(cut[1].chars().all(|c| c == 'é'));
        assert_eq!(cut.len(), 2);
    }

    #[test]
    fn os_messages_drop_the_error_number() {
        let err = std::io::Error::from_raw_os_error(2);
        assert!(
            !os_message(&err).contains("os error"),
            "{}",
            os_message(&err)
        );
    }
}
