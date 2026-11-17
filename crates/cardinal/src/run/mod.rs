//! The runner: start boundary, fail-open, wait loop, child supervision, and release
//! (`docs/architecture.md` section 5).
//!
//! **The stage has started once the spawn has returned a child.** Before that, every failure
//! the runner can observe (a configuration or state error, a lock past its deadline, a failed
//! spawn, a panic) ends in fail-open: the runner runs the stage itself, once. After it, the
//! runner never runs the stage again; an error ends in waiting for the child and returning its
//! status, or in exit 125 with one line on stderr when the status is unknown.
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
use crate::config::{self, Config, HostConfig};
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
        let mut arg = arg.to_string_lossy().into_owned();
        arg.truncate(arg.floor_char_boundary(left));
        left -= arg.len();
        out.push(arg);
    }
    out
}

/// Runs one stage: `cardinal run [--spec-b64 SPEC] -- COMMAND [ARG]...`. Returns the stage's
/// exit status, or ends this process by the signal that ended the stage (section 5.6).
pub fn run(spec_b64: Option<&str>, command: &[OsString]) -> ExitCode {
    let started = Instant::now();
    silence_panics();
    let watch = Watch::start();
    let tty = std::io::stdin().is_terminal();
    let mut runner = Runner::new(started, command, watch);
    let next = catch_unwind(AssertUnwindSafe(|| runner.prepare(spec_b64, tty)))
        .unwrap_or_else(|_| Next::FailOpen(FailOpen::new("panic", &panic_message())));
    match next {
        Next::Admitted => runner.start_admitted(),
        Next::FailOpen(reason) => runner.fail_open(&reason),
        Next::Passthrough => runner.run_in_caller_group("tty"),
        Next::Cancelled(cancel) => runner.cancel(cancel),
    }
}

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
        Event::ChildExited => None,
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

/// The state directory, `host.toml`, and the recorder.
struct Context {
    state: StateDir,
    host: HostConfig,
    recorder: Recorder,
}

struct Runner {
    started: Instant,
    argv: Vec<OsString>,
    watch: Watch,
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
}

impl Runner {
    fn new(started: Instant, command: &[OsString], watch: Watch) -> Self {
        Self {
            started,
            argv: command.to_vec(),
            executor: LocalExecutor::new(watch.clone()),
            watch,
            job_id: state::new_id(),
            spec: None,
            ctx: None,
            inuse: None,
            worktree: None,
            job: None,
            waited: None,
            admission: None,
        }
    }

    /// Steps 3–7 of section 5.2 up to the spawn.
    fn prepare(&mut self, spec_b64: Option<&str>, tty: bool) -> Next {
        if let Err(fail) = self.load(spec_b64) {
            return if tty {
                Next::Passthrough
            } else {
                Next::FailOpen(fail)
            };
        }
        let host = &self.ctx.as_ref().expect("loaded").host;
        let deadline = self.started + wait_cap(self.spec.as_ref(), host);
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
            .and_then(|dir| Config::load(&dir))
            .map(|config| config.host)
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

    fn pending_cancel(&self) -> Option<Cancel> {
        while let Some(event) = self.watch.poll() {
            if let Some(cancel) = cancel_of(event) {
                return Some(cancel);
            }
        }
        None
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
            argv: truncate_argv(&self.argv),
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
    fn start_admitted(mut self) -> ExitCode {
        let inv = Invocation {
            argv: self.argv.clone(),
        };
        let running = match self.executor.start(&inv) {
            Ok(running) => running,
            // The OS guarantees that no child ran.
            Err(err) => return self.fail_open(&FailOpen::new("spawn_failed", &err)),
        };
        // The stage has started: from here on it is never run again.
        let spawned = Instant::now();
        let child = running.child();
        let after = catch_unwind(AssertUnwindSafe(|| self.after_spawn(child)));
        if after.is_err() {
            self.record(
                RecordKind::JobInfraFailure,
                json!({ "reason": panic_message() }),
            );
        }
        let outcome = catch_unwind(AssertUnwindSafe(move || running.wait()))
            .unwrap_or_else(|_| ExecOutcome::InfraFailure(panic_message()));
        self.finish(&outcome, spawned, "admitted")
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
    fn fail_open(mut self, fail: &FailOpen) -> ExitCode {
        self.record(
            RecordKind::JobFailOpen,
            json!({ "reason": fail.reason, "detail": fail.detail }),
        );
        if let Some(job) = self.job.take() {
            let _ = job.release();
        }
        self.executor.restore_group();
        self.run_in_caller_group("fail_open")
    }

    /// Runs the stage in the caller's process group (fail-open and TTY passthrough). A stage
    /// that cannot be executed ends as a shell reports it: `<command>: <OS error>` and 127
    /// when it is not found, 126 otherwise.
    fn run_in_caller_group(mut self, path: &'static str) -> ExitCode {
        let inv = Invocation {
            argv: self.argv.clone(),
        };
        match self.executor.spawn(&inv) {
            Ok(running) => {
                let spawned = Instant::now();
                let child = running.child();
                self.record(
                    RecordKind::JobStarted,
                    json!({ "path": path, "pid": child.pid, "start_s": child.start_s }),
                );
                let outcome = catch_unwind(AssertUnwindSafe(move || Box::new(running).wait()))
                    .unwrap_or_else(|_| ExecOutcome::InfraFailure(panic_message()));
                self.finish(&outcome, spawned, path)
            }
            Err(err) => {
                let code: u8 = if err.kind() == std::io::ErrorKind::NotFound {
                    127
                } else {
                    126
                };
                let command = self
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

    /// Step 9: release, record, and exit as the stage did.
    fn finish(mut self, outcome: &ExecOutcome, spawned: Instant, path: &'static str) -> ExitCode {
        let wall_ms = u64::try_from(spawned.elapsed().as_millis()).unwrap_or(u64::MAX);
        if let Some(job) = self.job.take() {
            if self.group_outlived_parent(&job) {
                job.abandon();
            } else {
                let _ = job.release();
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

    /// Whether the runner's parent exited and the job's group still has members besides the
    /// runner: the supervisor gave up waiting, and the reservation stays for recovery.
    #[cfg(unix)]
    fn group_outlived_parent(&self, job: &Job) -> bool {
        let GroupIdentity::Pgid { pgid, .. } = job.reservation().group;
        self.watch.parent_exited()
            && os::unix::other_group_members(pgid).map_or(true, |m| !m.is_empty())
    }

    #[cfg(not(unix))]
    fn group_outlived_parent(&self, _job: &Job) -> bool {
        false
    }

    /// The parent exited or a signal arrived before the spawn: release everything and stop
    /// without starting the stage.
    fn cancel(mut self, cancel: Cancel) -> ExitCode {
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
        match cancel {
            Cancel::Signal(sig) => die_by_signal(sig),
            Cancel::ParentExited => ExitCode::from(EXIT_CANCELLED),
        }
    }

    fn waited_ms(&self) -> Option<u64> {
        self.waited
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }

    /// Appends a record for this job; a record failure is never surfaced.
    fn record(&self, kind: RecordKind, data: Value) {
        let Some(ctx) = &self.ctx else { return };
        let mut record = ctx.recorder.record(kind);
        record.job_id = Some(self.job_id.clone());
        record.session_id = self.spec.as_ref().and_then(|s| s.session_id.clone());
        if let Value::Object(map) = data {
            record.data = map;
        }
        let _ = ctx.recorder.append(&record);
    }
}

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
            OsString::from("cmd"),
            OsString::from(long),
            OsString::from("x"),
        ];
        let cut = truncate_argv(&argv);
        assert_eq!(cut[0], "cmd");
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
