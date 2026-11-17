//! Execution: the local invocation, placement, the executor contract, and child supervision
//! (`docs/architecture.md` sections 5.5 and 5.7).
//!
//! The local executor runs the stage's argv directly, resolved through `PATH` as `env` does,
//! with inherited stdio, cwd, and environment; it never proxies output. A [`Watch`] started
//! when the runner starts reports signals and the exit of the runner's parent, so the runner
//! can cancel before the spawn and supervise the child after it.

use std::ffi::OsString;
#[cfg(not(unix))]
use std::process::Child;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use crate::contract::{GroupIdentity, ProcessIdentity};
use crate::os;
use crate::testpoint;

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

/// How long after its parent exited the runner waits for a child that shares the caller's
/// group and outlived SIGKILL (a process in uninterruptible sleep) before it stops waiting.
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
    /// The child exited or stopped; it is not reaped yet.
    ChildChanged,
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
    /// The parent when the watch started.
    #[cfg(unix)]
    parent: Option<u32>,
    /// Set by the supervisor: the group outlived the SIGTERM sent after the parent exited.
    group_kill_due: std::sync::atomic::AtomicBool,
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
                parent: os::unix::parent_pid(),
                group_kill_due: std::sync::atomic::AtomicBool::new(false),
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
            if watch.inner.parent.is_some() {
                let poller = watch.clone();
                let _ = std::thread::Builder::new()
                    .name("cardinal-parent".into())
                    .spawn(move || {
                        while !poller.parent_exited() {
                            std::thread::sleep(PARENT_POLL);
                        }
                        let _ = poller.inner.tx.send(Event::ParentExited);
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

    /// Whether the runner's parent has exited, from the parent PID now: the watcher thread's
    /// event can lag by its polling interval.
    #[must_use]
    pub fn parent_exited(&self) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        #[cfg(unix)]
        if self.inner.parent.is_some() && os::unix::parent_pid() != self.inner.parent {
            self.inner.parent_exited.store(true, SeqCst);
        }
        self.inner.parent_exited.load(SeqCst)
    }

    /// Whether the supervisor found the group still alive [`PARENT_EXIT_KILL_AFTER`] after the
    /// SIGTERM it sent when the parent exited. The runner then records the job and sends
    /// SIGKILL to the group, itself included if it is still a member.
    #[must_use]
    pub fn group_kill_due(&self) -> bool {
        self.inner
            .group_kill_due
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
    /// runner stays in its own group and the stage starts there. Returns whether the runner
    /// left a group it led: false when it led none, already led its group before `prepare`,
    /// or could not move.
    #[must_use]
    pub fn restore_group(&self) -> bool {
        #[cfg(unix)]
        if let Some(setup) = self.group.take() {
            if setup.original != setup.pgid && os::unix::rejoin_group(setup).is_ok() {
                return true;
            }
            self.group.set(Some(setup));
        }
        false
    }

    /// Starts `inv`. The error is the spawn's own: the OS guarantees that no child ran. Returns
    /// as soon as the spawn did; nothing about the child is read yet.
    pub fn spawn(&self, inv: &Invocation) -> std::io::Result<LocalRunning> {
        let (program, args) = inv
            .argv
            .split_first()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "empty command"))?;
        let mut command = Command::new(program);
        command.args(args);
        #[cfg(unix)]
        os::unix::restore_child_signal(&mut command);
        let child = command.spawn()?;
        let pid = child.id();
        #[cfg(unix)]
        {
            // The supervisor waits for the child by its PID; dropping `Child` neither waits
            // nor kills.
            drop(child);
            let tx = self.watch.sender();
            // A child in the caller's group is under its job control: its stops are reported.
            let stops = self.group.get().is_none();
            // Without the waiter thread the supervisor still polls the child.
            let _ = std::thread::Builder::new()
                .name("cardinal-child".into())
                .spawn(move || {
                    loop {
                        let until = if stops {
                            os::unix::Until::EndedOrStopped
                        } else {
                            os::unix::Until::Ended
                        };
                        let exited = os::unix::wait_unreaped(pid, until)
                            .map_or(true, |state| state == os::unix::ChildState::Ended);
                        let _ = tx.send(Event::ChildChanged);
                        if exited {
                            return;
                        }
                        // The supervisor takes the stop report; until then it is seen again.
                        std::thread::sleep(GROUP_POLL);
                    }
                });
        }
        Ok(LocalRunning {
            #[cfg(not(unix))]
            child,
            pid,
            watch: self.watch.clone(),
            #[cfg(unix)]
            group: self.group.get().map(|g| g.pgid),
        })
    }

    /// Supervises child `pid` again after the wait for it panicked. A child that the failed
    /// wait already reaped ends in an infrastructure failure. `None` where a child can only be
    /// waited for through the handle the spawn returned.
    #[must_use]
    pub fn adopt(&self, pid: u32) -> Option<LocalRunning> {
        #[cfg(unix)]
        return Some(LocalRunning {
            pid,
            watch: self.watch.clone(),
            group: self.group.get().map(|g| g.pgid),
        });
        #[cfg(not(unix))]
        {
            let _ = pid;
            None
        }
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
    #[cfg(not(unix))]
    child: Child,
    pid: u32,
    watch: Watch,
    /// The group the runner leads and the child runs in; `None` when the child shares the
    /// caller's group.
    #[cfg(unix)]
    group: Option<u32>,
}

impl Running for LocalRunning {
    fn child(&self) -> ProcessIdentity {
        // A zombie keeps its start time on Linux; macOS drops an exited child from the table,
        // and 0 then never matches a live process, so such a child is never verified.
        let start_s = os::process(self.pid, os::Detail::default()).map_or(0, |p| p.start_s);
        ProcessIdentity {
            pid: self.pid,
            start_s,
        }
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
    ///   child receives terminal signals itself; SIGTERM and SIGHUP go to the child. When the
    ///   child stops, the runner stops with the same signal, so the caller's job control sees
    ///   the stop, and continues the child once it is continued itself.
    ///
    /// The child is reaped only when the wait ends, so its PID stays reserved while the runner
    /// may signal it; the group's ID is the runner's own PID and stays reserved while the
    /// runner lives.
    ///
    /// Once the runner's parent has exited, found before the wait ends even when the child
    /// exited first, the group (or the child) gets SIGTERM, and the wait ends when the child
    /// has exited and no other member of the group is left. When the group is still alive
    /// [`PARENT_EXIT_KILL_AFTER`] later, the wait ends with [`Watch::group_kill_due`] set; a
    /// child in the caller's group gets SIGKILL instead and is waited for up to
    /// [`PARENT_EXIT_GIVE_UP`].
    #[cfg(unix)]
    fn supervise(self) -> ExecOutcome {
        let pid = self.pid;
        let mut exited = false;
        // When the runner's parent exited: the time SIGTERM went out.
        let mut stopping: Option<Instant> = None;
        let mut killed = false;
        loop {
            if !exited {
                match os::unix::wait_unreaped(pid, os::unix::Until::Now) {
                    Ok(state) => exited = state == os::unix::ChildState::Ended,
                    Err(err) => {
                        return ExecOutcome::InfraFailure(format!(
                            "cannot wait for the command: {err}"
                        ));
                    }
                }
            }
            if !exited
                && self.group.is_none()
                && let Ok(Some(sig)) = os::unix::take_stop(pid)
            {
                // Once the parent is gone nobody would continue the runner; the child only
                // goes on.
                if stopping.is_none() && !self.watch.parent_exited() {
                    os::unix::stop_self(sig);
                }
                let _ = os::unix::signal_process(pid, os::unix::SIGCONT);
                continue;
            }
            if stopping.is_none() && self.watch.parent_exited() {
                stopping = Some(Instant::now());
                self.forward_stop(os::unix::SIGTERM, !exited);
            }
            let Some(since) = stopping else {
                if exited {
                    return self.reap();
                }
                // The waiter thread wakes the loop when the child exits; the timeout only
                // covers a waiter that could not be started.
                if let Some(Event::Signal(sig)) = self.watch.next(Duration::from_secs(1)) {
                    self.forward(sig, true);
                }
                continue;
            };
            if exited && self.group_stopped() {
                return self.reap();
            }
            if since.elapsed() >= PARENT_EXIT_KILL_AFTER {
                if self.group.is_some() {
                    self.watch
                        .inner
                        .group_kill_due
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    return if exited {
                        self.reap()
                    } else {
                        ExecOutcome::InfraFailure(
                            "the command did not stop within 5 s of SIGTERM".into(),
                        )
                    };
                }
                if !exited && !killed {
                    killed = true;
                    let _ = os::unix::signal_process(pid, os::unix::SIGKILL);
                }
                if since.elapsed() >= PARENT_EXIT_GIVE_UP {
                    return ExecOutcome::InfraFailure(
                        "the command did not stop after SIGKILL".into(),
                    );
                }
            }
            if let Some(Event::Signal(sig)) = self.watch.next(GROUP_POLL) {
                self.forward(sig, !exited);
            }
        }
    }

    /// Whether nothing of the stage is left once its child has exited: no other member of the
    /// runner's group, or, in the caller's group, the child alone.
    #[cfg(unix)]
    fn group_stopped(&self) -> bool {
        self.group.is_none_or(|pgid| {
            os::unix::other_group_members(pgid).is_ok_and(|members| members.is_empty())
        })
    }

    /// Reaps the exited child.
    #[cfg(unix)]
    fn reap(&self) -> ExecOutcome {
        let end = os::unix::reap(self.pid);
        if let Err(err) = testpoint::hit(testpoint::RUN_REAPED) {
            return ExecOutcome::InfraFailure(format!("cannot wait for the command: {err}"));
        }
        match end {
            Ok(os::unix::ChildEnd::Exited(code)) => ExecOutcome::Exited(code),
            Ok(os::unix::ChildEnd::Signaled(sig)) => ExecOutcome::Signaled(sig),
            Err(err) => ExecOutcome::InfraFailure(format!("cannot wait for the command: {err}")),
        }
    }

    #[cfg(unix)]
    fn forward(&self, sig: i32, child_unreaped: bool) {
        match self.group {
            Some(pgid) => self.watch.signal_own_group(pgid, sig),
            None if child_unreaped && (sig == os::unix::SIGTERM || sig == os::unix::SIGHUP) => {
                let _ = os::unix::signal_process(self.pid, sig);
            }
            None => {}
        }
    }

    #[cfg(unix)]
    fn forward_stop(&self, sig: i32, child_unreaped: bool) {
        match self.group {
            Some(pgid) => self.watch.signal_own_group(pgid, sig),
            None if child_unreaped => {
                let _ = os::unix::signal_process(self.pid, sig);
            }
            None => {}
        }
    }

    #[cfg(not(unix))]
    fn supervise(mut self) -> ExecOutcome {
        let _ = &self.watch;
        let end = self.child.wait();
        if let Err(err) = testpoint::hit(testpoint::RUN_REAPED) {
            return ExecOutcome::InfraFailure(format!("cannot wait for the command: {err}"));
        }
        match end {
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
