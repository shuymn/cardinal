//! Execution: the local invocation, placement, the executor contract, and child supervision
//! (`docs/architecture.md` sections 5.5 and 5.7).
//!
//! The local executor runs the stage's argv directly, resolved through `PATH` as `env` does,
//! with inherited stdio, cwd, and environment; it never proxies output. A [`Watch`] started
//! when the runner starts reports signals and the exit of the runner's parent, so the runner
//! can cancel before the spawn and supervise the child after it.

use std::ffi::OsString;
use std::process::{Child, Command};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use crate::contract::{GroupIdentity, ProcessIdentity};
use crate::os;

/// A local invocation: the stage's argv as the calling shell expanded it. The cwd and the
/// environment are inherited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub argv: Vec<OsString>,
}

/// Where an invocation runs. Phase 0 always runs locally.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Placement {
    #[default]
    Local,
}

/// The executor failed before anything started; the caller may run the stage itself.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not started: {0}")]
pub struct NotStarted(pub String);

/// How a started invocation ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecOutcome {
    Exited(i32),
    Signaled(i32),
    InfraFailure(String),
}

/// Starts invocations. The start boundary is the same for every executor: `starting` is
/// written before [`Executor::start`], and an `Ok` from it means the stage may have run.
pub trait Executor {
    /// Sets up the process group; runs before the reservation is written.
    fn prepare(&self) -> Result<GroupIdentity, NotStarted>;
    /// Starts the invocation. `Err` guarantees that nothing started.
    fn start(&self, inv: &Invocation) -> Result<Box<dyn Running>, NotStarted>;
}

/// A started invocation.
pub trait Running {
    /// The child, written into the reservation as `running`.
    fn child(&self) -> ProcessIdentity;
    /// Blocks until the child exits; forwards cancellation.
    fn wait(self: Box<Self>) -> ExecOutcome;
}

/// How long the runner lets a group stop after SIGTERM before it sends SIGKILL, once its
/// parent has exited.
pub const PARENT_EXIT_KILL_AFTER: Duration = Duration::from_secs(5);

/// How long the runner keeps sending SIGKILL to a group that does not empty (a member in
/// uninterruptible sleep) before it stops waiting. The runner then exits without releasing
/// the reservation, which the recovery pass handles as for a runner that died.
#[cfg(unix)]
const PARENT_EXIT_GIVE_UP: Duration = Duration::from_secs(30);

/// How often the parent watcher looks at the parent PID.
#[cfg(unix)]
const PARENT_POLL: Duration = Duration::from_millis(50);

/// How often a supervisor that waits for a group to empty looks at the group.
#[cfg(unix)]
const GROUP_POLL: Duration = Duration::from_millis(100);

/// Something the runner must react to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A signal the runner handles: SIGINT, SIGTERM, SIGHUP, or SIGQUIT. Copies of a signal
    /// the runner forwarded to its own group are not reported.
    Signal(i32),
    /// The runner's parent exited (the Bash call was cancelled).
    ParentExited,
    /// The child exited; it is not reaped yet.
    ChildExited,
}

/// The signals the runner handles (Unix).
#[cfg(unix)]
const HANDLED: [i32; 4] = [
    os::unix::SIGINT,
    os::unix::SIGTERM,
    os::unix::SIGHUP,
    os::unix::SIGQUIT,
];

struct WatchInner {
    /// Kept on every platform: an open channel makes waits time out instead of returning at
    /// once.
    #[cfg_attr(
        not(unix),
        allow(dead_code, reason = "nothing reports events on this platform")
    )]
    tx: Sender<Event>,
    rx: Mutex<Receiver<Event>>,
    parent_exited: std::sync::atomic::AtomicBool,
    /// Per handled signal: copies the runner sent to its own group and has not seen yet.
    #[cfg(unix)]
    own: [std::sync::atomic::AtomicU32; 4],
}

/// Signals and parent exit, reported as [`Event`]s from the runner's start. Watching is best
/// effort: when a handler cannot be installed, the signal keeps its default action.
#[derive(Clone)]
pub struct Watch {
    inner: Arc<WatchInner>,
}

impl Watch {
    /// Starts watching signals and the parent process.
    #[must_use]
    pub fn start() -> Self {
        let (tx, rx) = mpsc::channel();
        let watch = Self {
            inner: Arc::new(WatchInner {
                tx,
                rx: Mutex::new(rx),
                parent_exited: std::sync::atomic::AtomicBool::new(false),
                #[cfg(unix)]
                own: Default::default(),
            }),
        };
        #[cfg(unix)]
        {
            let inner = Arc::clone(&watch.inner);
            let _ = os::unix::watch_signals(&HANDLED, move |sig| {
                if let Some(slot) = HANDLED.iter().position(|&s| s == sig)
                    && take_one(&inner.own[slot])
                {
                    return;
                }
                let _ = inner.tx.send(Event::Signal(sig));
            });
            if let Some(parent) = os::unix::parent_pid() {
                let inner = Arc::clone(&watch.inner);
                let _ = std::thread::Builder::new()
                    .name("cardinal-parent".into())
                    .spawn(move || {
                        while os::unix::parent_pid() == Some(parent) {
                            std::thread::sleep(PARENT_POLL);
                        }
                        inner
                            .parent_exited
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                        let _ = inner.tx.send(Event::ParentExited);
                    });
            }
        }
        watch
    }

    /// The next event, waiting at most `timeout`.
    #[must_use]
    pub fn next(&self, timeout: Duration) -> Option<Event> {
        let rx = self
            .inner
            .rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        rx.recv_timeout(timeout).ok()
    }

    /// Whether the runner's parent has exited.
    #[must_use]
    pub fn parent_exited(&self) -> bool {
        self.inner
            .parent_exited
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The next event if one is pending.
    #[must_use]
    pub fn poll(&self) -> Option<Event> {
        self.next(Duration::ZERO)
    }

    #[cfg(unix)]
    fn sender(&self) -> Sender<Event> {
        self.inner.tx.clone()
    }

    /// Sends `sig` to group `pgid`, which this process leads. The copy this process receives
    /// is not reported as an event.
    #[cfg(unix)]
    fn signal_own_group(&self, pgid: u32, sig: i32) {
        let slot = HANDLED.iter().position(|&s| s == sig);
        if let Some(slot) = slot {
            self.inner.own[slot].fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        if os::unix::signal_group(pgid, sig).is_err()
            && let Some(slot) = slot
        {
            take_one(&self.inner.own[slot]);
        }
    }
}

/// Decrements `counter` unless it is zero; whether it did.
#[cfg(unix)]
fn take_one(counter: &std::sync::atomic::AtomicU32) -> bool {
    counter
        .try_update(
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
            |n| n.checked_sub(1),
        )
        .is_ok()
}

/// The local executor. After [`Executor::prepare`] the runner leads a new process group and
/// the stage starts in it; otherwise (fail-open, TTY passthrough) the stage starts in the
/// caller's group.
pub struct LocalExecutor {
    watch: Watch,
    #[cfg(unix)]
    group: std::cell::Cell<Option<os::unix::GroupSetup>>,
}

impl LocalExecutor {
    #[must_use]
    pub fn new(watch: Watch) -> Self {
        Self {
            watch,
            #[cfg(unix)]
            group: std::cell::Cell::new(None),
        }
    }

    /// Moves the runner back into the group it was in before [`Executor::prepare`], so a stage
    /// started afterwards shares the caller's group. When that group no longer exists, the
    /// runner stays in its own group and the stage starts there.
    pub fn restore_group(&self) {
        #[cfg(unix)]
        if let Some(setup) = self.group.take()
            && os::unix::rejoin_group(setup).is_err()
        {
            self.group.set(Some(setup));
        }
    }

    /// Starts `inv`. The error is the spawn's own: the OS guarantees that no child ran.
    pub fn spawn(&self, inv: &Invocation) -> std::io::Result<LocalRunning> {
        let (program, args) = inv
            .argv
            .split_first()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "empty command"))?;
        let child = Command::new(program).args(args).spawn()?;
        let pid = child.id();
        // A zombie keeps its start time on Linux; macOS drops an exited child from the table,
        // and 0 then never matches a live process, so such a child is never verified.
        let start_s = os::process(pid, os::Detail::default()).map_or(0, |p| p.start_s);
        #[cfg(unix)]
        {
            let tx = self.watch.sender();
            // Without the waiter thread the supervisor still polls the child.
            let _ = std::thread::Builder::new()
                .name("cardinal-child".into())
                .spawn(move || {
                    let _ = os::unix::wait_exited_unreaped(pid);
                    let _ = tx.send(Event::ChildExited);
                });
        }
        Ok(LocalRunning {
            child,
            identity: ProcessIdentity { pid, start_s },
            watch: self.watch.clone(),
            #[cfg(unix)]
            group: self.group.get().map(|g| g.pgid),
        })
    }
}

impl Executor for LocalExecutor {
    #[cfg(unix)]
    fn prepare(&self) -> Result<GroupIdentity, NotStarted> {
        let setup = os::unix::lead_group()
            .map_err(|err| NotStarted(format!("cannot lead a process group: {err}")))?;
        self.group.set(Some(setup));
        let leader_start_s = os::start_time_s(os::current_pid())
            .ok_or_else(|| NotStarted("cannot read the runner's start time".into()))?;
        Ok(GroupIdentity::Pgid {
            pgid: setup.pgid,
            leader_start_s,
        })
    }

    #[cfg(not(unix))]
    fn prepare(&self) -> Result<GroupIdentity, NotStarted> {
        Err(NotStarted(
            "process groups are not supported on this platform".into(),
        ))
    }

    fn start(&self, inv: &Invocation) -> Result<Box<dyn Running>, NotStarted> {
        self.spawn(inv)
            .map(|running| Box::new(running) as Box<dyn Running>)
            .map_err(|err| NotStarted(err.to_string()))
    }
}

/// A child started by the local executor. The supervisor that waits for it is the only code
/// that reaps it, so its PID cannot be reused while the supervisor signals it.
pub struct LocalRunning {
    child: Child,
    identity: ProcessIdentity,
    watch: Watch,
    /// The group the runner leads and the child runs in; `None` when the child shares the
    /// caller's group.
    #[cfg(unix)]
    group: Option<u32>,
}

impl Running for LocalRunning {
    fn child(&self) -> ProcessIdentity {
        self.identity
    }

    fn wait(self: Box<Self>) -> ExecOutcome {
        self.supervise()
    }
}

impl LocalRunning {
    /// Waits for the child, forwarding signals:
    ///
    /// - in its own group: SIGINT, SIGTERM, SIGHUP, and SIGQUIT go to the whole group;
    /// - in the caller's group: SIGINT and SIGQUIT are ignored, as `system()` does, because the
    ///   child receives terminal signals itself; SIGTERM and SIGHUP go to the child.
    ///
    /// When the runner's parent exits, the group (or the child) gets SIGTERM and, 5 s later,
    /// SIGKILL; the wait then ends only once no other member of the group is left, or after
    /// [`PARENT_EXIT_GIVE_UP`].
    #[cfg(unix)]
    fn supervise(mut self) -> ExecOutcome {
        use std::os::unix::process::ExitStatusExt as _;

        let mut status = None;
        let mut parent_exited: Option<Instant> = None;
        loop {
            if status.is_none() {
                match self.child.try_wait() {
                    Ok(found) => status = found,
                    Err(err) => {
                        return ExecOutcome::InfraFailure(format!(
                            "cannot wait for the command: {err}"
                        ));
                    }
                }
            }
            let gave_up = parent_exited.is_some_and(|since| since.elapsed() >= PARENT_EXIT_GIVE_UP);
            if let Some(exit) = status {
                let members_left = match (parent_exited, self.group) {
                    (Some(_), Some(pgid)) if !gave_up => {
                        os::unix::other_group_members(pgid).map_or(true, |m| !m.is_empty())
                    }
                    _ => false,
                };
                if !members_left {
                    return match (exit.code(), exit.signal()) {
                        (Some(code), _) => ExecOutcome::Exited(code),
                        (None, Some(sig)) => ExecOutcome::Signaled(sig),
                        (None, None) => ExecOutcome::InfraFailure(format!(
                            "the command ended without an exit status: {exit}"
                        )),
                    };
                }
            } else if gave_up {
                return ExecOutcome::InfraFailure("the command did not stop after SIGKILL".into());
            }
            if parent_exited.is_some_and(|since| since.elapsed() >= PARENT_EXIT_KILL_AFTER) {
                self.kill_all(status.is_none());
            }
            let timeout = if parent_exited.is_some() || status.is_some() {
                GROUP_POLL
            } else {
                // The waiter thread wakes the loop when the child exits; the timeout only
                // covers a waiter that could not be started.
                Duration::from_secs(1)
            };
            match self.watch.next(timeout) {
                Some(Event::Signal(sig)) => self.forward(sig, status.is_none()),
                Some(Event::ParentExited) if parent_exited.is_none() => {
                    parent_exited = Some(Instant::now());
                    self.forward_stop(os::unix::SIGTERM, status.is_none());
                }
                Some(_) | None => {}
            }
        }
    }

    #[cfg(unix)]
    fn forward(&self, sig: i32, child_unreaped: bool) {
        match self.group {
            Some(pgid) => self.watch.signal_own_group(pgid, sig),
            None if child_unreaped && (sig == os::unix::SIGTERM || sig == os::unix::SIGHUP) => {
                let _ = os::unix::signal_process(self.identity.pid, sig);
            }
            None => {}
        }
    }

    #[cfg(unix)]
    fn forward_stop(&self, sig: i32, child_unreaped: bool) {
        match self.group {
            Some(pgid) => self.watch.signal_own_group(pgid, sig),
            None if child_unreaped => {
                let _ = os::unix::signal_process(self.identity.pid, sig);
            }
            None => {}
        }
    }

    #[cfg(unix)]
    fn kill_all(&self, child_unreaped: bool) {
        match self.group {
            Some(pgid) => {
                let _ = os::unix::kill_other_group_members(pgid);
            }
            None if child_unreaped => {
                let _ = os::unix::signal_process(self.identity.pid, os::unix::SIGKILL);
            }
            None => {}
        }
    }

    #[cfg(not(unix))]
    fn supervise(mut self) -> ExecOutcome {
        let _ = &self.watch;
        match self.child.wait() {
            Ok(exit) => match exit.code() {
                Some(code) => ExecOutcome::Exited(code),
                None => ExecOutcome::InfraFailure(format!(
                    "the command ended without an exit status: {exit}"
                )),
            },
            Err(err) => ExecOutcome::InfraFailure(format!("cannot wait for the command: {err}")),
        }
    }
}
