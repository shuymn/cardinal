//! Process groups, children, and signals on Unix (`docs/architecture.md` sections 4.5 and
//! 5.5): the one implementation of these primitives in the product.
//!
//! - **Process groups:** [`lead_group`] and [`rejoin_group`] move this process; [`group_exists`]
//!   (`kill(-pgid, 0)`; only `ESRCH` means empty), [`group_emptied`] (the same, waiting out a
//!   group `kill` still reports once no live member is left), [`pgid_of`], [`group_members`],
//!   and [`other_group_members`] read; [`signal_group`] signals a whole group.
//! - **Children:** [`wait_unreaped`] reports a child's [`ChildState`] without reaping it, and
//!   never takes a stopped child for an ended one (macOS reports a stop even to a wait for
//!   exits alone); [`take_stop`] takes a stop report; [`reap`] reaps an ended child.
//! - **Signals:** [`signal_process`]; [`watch_signals`] and [`ignored`] for handlers;
//!   [`stop_self`] and [`die_by_signal`] for this process; [`reset_child_signal`] and
//!   [`restore_child_signal`] keep children unreaped while passing SIGCHLD on unchanged.
//!
//! **Signal an ID only while it is pinned.** A child's PID is not reused while the child is
//! unreaped, which [`wait_unreaped`] keeps it; a group's ID is not reused while the group has a
//! member, and only the process with that PID can create a group with that ID, so it is pinned
//! while its leader is this process or an unreaped child of it. Checking an ID and then
//! signalling it is no substitute: the process can end and its ID pass to another in between.
//! A child stays unreaped only while SIGCHLD is not ignored, which [`reset_child_signal`]
//! ensures.
//!
//! Signal handlers are installed through `signal-hook`; a handler is reset to the default
//! action by `exec`, so a child starts with default dispositions for the handled signals. A
//! signal this process ignores gets no handler and stays ignored for its children.

use std::io::{self, Read as _};
use std::os::unix::process::CommandExt as _;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, WaitOptions};

use super::{Detail, ProcessInfo, ProcessTable};

pub use signal_hook::consts::signal::{
    SIGCONT, SIGHUP, SIGINT, SIGKILL, SIGQUIT, SIGSTOP, SIGTERM,
};

/// The process group of this process before and after [`lead_group`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupSetup {
    /// The group this process was in before, which another process leads; the stage runs in
    /// it again on fail-open.
    pub original: u32,
    /// The group this process now leads: its own PID.
    pub pgid: u32,
}

fn pid(raw: u32) -> io::Result<Pid> {
    i32::try_from(raw)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("bad PID {raw}")))
}

fn raw(pid: Pid) -> u32 {
    pid.as_raw_nonzero().get().unsigned_abs()
}

fn signal(raw: i32) -> io::Result<Signal> {
    Signal::from_named_raw(raw)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("bad signal {raw}")))
}

/// Makes this process the leader of a new process group: `setpgid(0, 0)`. Fails when this
/// process already leads its group: the group it was given, for example as the first command
/// of a job-control shell's pipeline, may hold other processes, so it is not this process's
/// own.
pub fn lead_group() -> io::Result<GroupSetup> {
    let me = raw(rustix::process::getpid());
    let original = raw(rustix::process::getpgrp());
    if original == me {
        return Err(io::Error::other(format!(
            "already leads process group {me}, which others may share"
        )));
    }
    rustix::process::setpgid(None, None)?;
    let pgid = raw(rustix::process::getpgrp());
    if pgid != me {
        return Err(io::Error::other(format!(
            "process group is {pgid} after setpgid, not {me}"
        )));
    }
    Ok(GroupSetup { original, pgid })
}

/// Moves this process back into the group it was in before [`lead_group`]. Fails when the
/// original group no longer exists.
pub fn rejoin_group(setup: GroupSetup) -> io::Result<()> {
    rustix::process::setpgid(None, Some(pid(setup.original)?)).map_err(Into::into)
}

/// Whether process group `pgid` has a member: `kill(-pgid, 0)`. Only `ESRCH` means empty; any
/// other answer, `EPERM` included, means the group exists.
#[must_use]
pub fn group_exists(pgid: u32) -> bool {
    let Ok(pgid) = pid(pgid) else { return false };
    !matches!(
        rustix::process::test_kill_process_group(pgid),
        Err(rustix::io::Errno::SRCH)
    )
}

/// How long the runner lets [`group_emptied`] ask `kill(-pgid, 0)` again while the process
/// table lists no member of the group.
pub const EMPTYING: Duration = Duration::from_secs(1);

/// Whether group `pgid`, which this process has left, has no member left: [`group_exists`]
/// answers, and only `ESRCH` means empty. macOS can answer that a group exists for a moment
/// after its last member was reaped, while the process table already lists no member (up to
/// 16 ms seen on a loaded host); so while the table lists no live member, `kill` is asked again
/// until `until`, and once when `until` has passed. A group the table lists a live member of,
/// one whose table cannot be read, and one `kill` still reports at `until` are not empty.
#[must_use]
pub fn group_emptied(pgid: u32, until: Instant) -> bool {
    let mut delay = Duration::from_millis(1);
    loop {
        if !group_exists(pgid) {
            return true;
        }
        let listed = other_group_members(pgid).map(|members| members.is_empty());
        let now = Instant::now();
        if listed.ok() != Some(true) || now >= until {
            return false;
        }
        std::thread::sleep(delay.min(until - now));
        delay = (delay * 2).min(Duration::from_millis(50));
    }
}

/// The process group of `pid`, if the process exists.
#[must_use]
pub fn pgid_of(raw_pid: u32) -> Option<u32> {
    rustix::process::getpgid(Some(pid(raw_pid).ok()?))
        .ok()
        .map(raw)
}

/// Sends `sig` to every member of group `pgid`, this process included if it is a member. The
/// ID must be pinned (see the module documentation).
pub fn signal_group(pgid: u32, sig: i32) -> io::Result<()> {
    rustix::process::kill_process_group(pid(pgid)?, signal(sig)?).map_err(Into::into)
}

/// Sends `sig` to process `pid`. The PID must be pinned (see the module documentation).
pub fn signal_process(raw_pid: u32, sig: i32) -> io::Result<()> {
    rustix::process::kill_process(pid(raw_pid)?, signal(sig)?).map_err(Into::into)
}

/// This process's parent PID; `None` once the parent is gone and nothing adopted the process.
#[must_use]
pub fn parent_pid() -> Option<u32> {
    rustix::process::getppid().map(raw)
}

/// The live (not zombie) members of group `pgid` in `table`.
#[must_use]
pub fn group_members(table: &ProcessTable, pgid: u32) -> Vec<ProcessInfo> {
    table
        .iter()
        .filter(|p| !p.zombie && pgid_of(p.pid) == Some(pgid))
        .cloned()
        .collect()
}

/// The live members of group `pgid` other than this process, from a fresh process table.
pub fn other_group_members(pgid: u32) -> io::Result<Vec<ProcessInfo>> {
    let table = ProcessTable::snapshot(Detail::default()).map_err(io::Error::other)?;
    let me = super::current_pid();
    let mut members = group_members(&table, pgid);
    members.retain(|p| p.pid != me);
    Ok(members)
}

/// A child's state as [`wait_unreaped`] reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildState {
    /// Neither ended nor stopped with a report pending.
    Running,
    /// Stopped by this signal, and the report not taken yet ([`take_stop`]).
    Stopped(i32),
    /// Exited or killed by a signal; [`reap`] it.
    Ended,
}

/// How long [`wait_unreaped`] waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Until {
    /// Not at all.
    Now,
    /// Until the child has ended; stops do not count.
    Ended,
    /// Until the child has ended or a stop report is pending.
    EndedOrStopped,
}

/// The state of child `pid`, waiting as `until` says, without reaping the child or taking a
/// report, so the child's PID, and a group it leads, stay pinned. A stopped child is never
/// reported as ended. Fails when the child cannot be waited for, for example because this
/// process ignores SIGCHLD and the system reaped the child already.
pub fn wait_unreaped(raw_pid: u32, until: Until) -> io::Result<ChildState> {
    let id = pid(raw_pid)?;
    let options = WaitIdOptions::EXITED
        | WaitIdOptions::NOWAIT
        | match until {
            Until::Now => WaitIdOptions::STOPPED | WaitIdOptions::NOHANG,
            Until::Ended => WaitIdOptions::empty(),
            Until::EndedOrStopped => WaitIdOptions::STOPPED,
        };
    loop {
        let state = match rustix::process::waitid(WaitId::Pid(id), options) {
            Ok(Some(status)) if status.exited() || status.killed() || status.dumped() => {
                ChildState::Ended
            }
            Ok(Some(status)) => status
                .stopping_signal()
                .map_or(ChildState::Running, ChildState::Stopped),
            Ok(None) => ChildState::Running,
            Err(rustix::io::Errno::INTR) => continue,
            Err(err) => return Err(err.into()),
        };
        match (until, state) {
            (Until::Now, _)
            | (_, ChildState::Ended)
            | (Until::EndedOrStopped, ChildState::Stopped(_)) => return Ok(state),
            // A pending stop report stays until the child changes again; look again later
            // instead of spinning on it.
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// The signal that stopped child `pid`, if it stopped and that was not reported yet; takes
/// the report, so the stop is reported once. Never waits.
pub fn take_stop(raw_pid: u32) -> io::Result<Option<i32>> {
    let id = pid(raw_pid)?;
    loop {
        match rustix::process::waitid(
            WaitId::Pid(id),
            WaitIdOptions::STOPPED | WaitIdOptions::NOHANG,
        ) {
            Ok(status) => return Ok(status.and_then(|s| s.stopping_signal())),
            Err(rustix::io::Errno::INTR) => {}
            Err(err) => return Err(err.into()),
        }
    }
}

/// Stops this process with `sig`, as a terminal stops a job; returns once it is continued.
pub fn stop_self(sig: i32) {
    let _ = signal_hook::low_level::raise(sig);
}

/// The current disposition of `sig`, read without changing it.
fn disposition(sig: i32) -> io::Result<libc::sigaction> {
    let mut current = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
    // SAFETY: without a new action, `sigaction` only writes the current one into `current`,
    // which is valid for writes.
    if unsafe { libc::sigaction(sig, std::ptr::null(), current.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `current` is zero-initialized, a valid `sigaction`, and filled on success.
    Ok(unsafe { current.assume_init() })
}

/// Whether this process ignores `sig` (`SIG_IGN`, inherited across `exec` from the caller).
/// Reads the disposition without changing it.
#[must_use]
pub fn ignored(sig: i32) -> bool {
    disposition(sig).is_ok_and(|action| action.sa_sigaction == libc::SIG_IGN)
}

/// Whether SIGCHLD was ignored when [`reset_child_signal`] reset it.
static CHILD_SIGNAL_IGNORED: AtomicBool = AtomicBool::new(false);

/// Sets SIGCHLD to its default action, so that every child of this process stays unreaped
/// until this process reaps it. A caller that ignores SIGCHLD passes that on across `exec`, and
/// Linux then reaps the children itself: no wait could keep a child's PID from reuse, and the
/// child's status is lost. Call before the first child is started; [`restore_child_signal`]
/// gives a child the disposition this process started with.
pub fn reset_child_signal() {
    CHILD_SIGNAL_IGNORED.store(ignored(libc::SIGCHLD), Ordering::Relaxed);
    let mut action = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
    // SAFETY: `action` is a zeroed `sigaction` (`SIG_DFL`, no flags) whose mask `sigemptyset`
    // initializes; `sigaction` reads it and writes nothing back with a null old action.
    unsafe {
        libc::sigemptyset(&raw mut (*action.as_mut_ptr()).sa_mask);
        libc::sigaction(libc::SIGCHLD, action.as_ptr(), std::ptr::null_mut());
    }
}

/// Makes `command` start with SIGCHLD ignored when this process ignored it before
/// [`reset_child_signal`], as the child would have had it if started directly.
pub fn restore_child_signal(command: &mut Command) {
    if !CHILD_SIGNAL_IGNORED.load(Ordering::Relaxed) {
        return;
    }
    // SAFETY: the hook runs in the child between `fork` and `exec`, where only
    // async-signal-safe calls are allowed: it makes one `signal` call and touches no memory.
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGCHLD, libc::SIG_IGN);
            Ok(())
        });
    }
}

/// How a reaped child ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildEnd {
    Exited(i32),
    Signaled(i32),
}

/// Waits for child `pid` to end and reaps it; its PID is free for reuse afterwards.
pub fn reap(raw_pid: u32) -> io::Result<ChildEnd> {
    let id = pid(raw_pid)?;
    loop {
        match rustix::process::waitpid(Some(id), WaitOptions::empty()) {
            Ok(Some((_, status))) => {
                if let Some(code) = status.exit_status() {
                    return Ok(ChildEnd::Exited(code));
                }
                if let Some(sig) = status.terminating_signal() {
                    return Ok(ChildEnd::Signaled(sig));
                }
            }
            Ok(None) | Err(rustix::io::Errno::INTR) => {}
            Err(err) => return Err(err.into()),
        }
    }
}

/// Calls `on_signal` on a background thread for every delivery of one of `signals`. The
/// signals no longer take their default action in this process; `exec` restores it for
/// children. A signal this process ignores is skipped: it stays ignored here and in the
/// children, as it would for a stage started directly.
///
/// Each signal's handler writes one byte to an OS pipe of its own, read by a thread of its own
/// that calls `on_signal`; a pipe, unlike the socket pair `signal-hook`'s iterator uses, keeps
/// the runner free of sockets.
///
/// Every reader runs before the first handler is installed, so no handler writes to a pipe
/// nobody reads, which would swallow its signal. On a failure no handler stays installed: each
/// signal gets back the disposition it had, and the readers end.
///
/// The signals stay blocked in the calling thread until every handler is installed, or every
/// disposition put back: `signal-hook` installs a handler before it publishes the action that
/// handler runs, and drops a signal that arrives in between. A blocked signal stays pending and
/// is delivered once the watch is set up, or to the disposition put back on a failure. Call
/// this before this process starts any other thread: such a thread would not block the signals
/// and could take one in that window. The readers start with the signals blocked and keep them
/// blocked, so the handlers run on other threads.
pub fn watch_signals(
    signals: &[i32],
    on_signal: impl Fn(i32) + Send + Sync + 'static,
) -> io::Result<()> {
    watch_signals_with(
        signals,
        on_signal,
        |name, body| std::thread::Builder::new().name(name).spawn(body).map(drop),
        register,
    )
}

/// Installs the handler of `sig`, which writes one byte to `writer` on each delivery.
fn register(sig: i32, writer: io::PipeWriter) -> io::Result<signal_hook::SigId> {
    signal_hook::low_level::pipe::register(sig, writer)
}

/// A thread body for [`watch_signals_with`]'s `start`.
type Body = Box<dyn FnOnce() + Send + 'static>;

/// [`watch_signals`], with `start` starting each reader thread from a name and a body, and
/// `register` installing each handler as [`register`] does.
fn watch_signals_with(
    signals: &[i32],
    on_signal: impl Fn(i32) + Send + Sync + 'static,
    start: impl FnMut(String, Body) -> io::Result<()>,
    register: impl FnMut(i32, io::PipeWriter) -> io::Result<signal_hook::SigId>,
) -> io::Result<()> {
    let watched: Vec<i32> = signals
        .iter()
        .copied()
        .filter(|&sig| !ignored(sig))
        .collect();
    let previous = block(&watched)?;
    let result = install(&watched, on_signal, start, register);
    set_mask(&previous);
    result
}

/// Blocks `signals` in the calling thread; returns the mask the thread had.
fn block(signals: &[i32]) -> io::Result<libc::sigset_t> {
    let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    let mut previous = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: `sigemptyset` initializes `set`, which `sigaddset` and `pthread_sigmask` then
    // read; `pthread_sigmask` writes the current mask into `previous` on success.
    unsafe {
        libc::sigemptyset(set.as_mut_ptr());
        for &sig in signals {
            if libc::sigaddset(set.as_mut_ptr(), sig) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        match libc::pthread_sigmask(libc::SIG_BLOCK, set.as_ptr(), previous.as_mut_ptr()) {
            0 => Ok(previous.assume_init()),
            err => Err(io::Error::from_raw_os_error(err)),
        }
    }
}

/// Sets the calling thread's mask to `mask`, delivering the signals it unblocks that are
/// pending.
fn set_mask(mask: &libc::sigset_t) {
    // SAFETY: `mask` is an initialized signal set; a null old set makes `pthread_sigmask`
    // write nothing. It fails only for an invalid `how`.
    unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, mask, std::ptr::null_mut()) };
}

/// Starts the readers, then installs the handlers of `signals`, which are blocked.
fn install(
    signals: &[i32],
    on_signal: impl Fn(i32) + Send + Sync + 'static,
    mut start: impl FnMut(String, Body) -> io::Result<()>,
    mut register: impl FnMut(i32, io::PipeWriter) -> io::Result<signal_hook::SigId>,
) -> io::Result<()> {
    let on_signal = Arc::new(on_signal);
    // Dropping a writer before its handler holds it ends its reader at end of file.
    let mut writers = Vec::new();
    for &sig in signals {
        let (mut reader, writer) = io::pipe()?;
        let on_signal = Arc::clone(&on_signal);
        start(
            format!("cardinal-signal-{sig}"),
            Box::new(move || {
                let mut byte = [0u8; 1];
                loop {
                    match reader.read(&mut byte) {
                        Ok(1) => on_signal(sig),
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                        _ => return,
                    }
                }
            }),
        )?;
        writers.push((sig, writer));
    }
    let mut installed = Vec::new();
    for (sig, writer) in writers {
        let registered = disposition(sig).and_then(|previous| {
            let id = register(sig, writer)?;
            Ok((sig, id, previous))
        });
        match registered {
            Ok(entry) => installed.push(entry),
            Err(err) => {
                for (sig, id, previous) in installed.into_iter().rev() {
                    // signal-hook keeps its handler installed when its last action goes, so
                    // the previous disposition is put back first. Unregistering then closes
                    // the writer, which ends the reader.
                    // SAFETY: `previous` was read from `sigaction` for this signal; a null old
                    // action makes `sigaction` write nothing.
                    unsafe { libc::sigaction(sig, &raw const previous, std::ptr::null_mut()) };
                    signal_hook::low_level::unregister(id);
                }
                return Err(err);
            }
        }
    }
    Ok(())
}

/// Ends this process by `sig` with its default action: the handler is reset to the default,
/// the signal unblocked and raised. If the process survives (a signal whose default is to
/// ignore or stop), it exits with `128 + sig` as shells report a signal death.
pub fn die_by_signal(sig: i32) -> ! {
    let _ = signal_hook::low_level::emulate_default_handler(sig);
    // A stop signal returns here once continued; give the raise a moment to land otherwise.
    std::thread::sleep(Duration::from_millis(10));
    std::process::exit(128 + sig)
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use cardinal_test_support::process::deliverable;

    use super::{
        Body, SIGKILL, disposition, group_exists, ignored, parent_pid, pgid_of, register,
        watch_signals_with,
    };

    /// The handler of `sig` and the flags that change how it runs, which a failed watch must
    /// leave as they were. The raw flags can differ after a restore: glibc on `x86_64` adds
    /// `SA_RESTORER` to every action it installs.
    fn handler(sig: i32) -> (libc::sighandler_t, libc::c_int) {
        let behaviour = libc::SA_ONSTACK
            | libc::SA_RESTART
            | libc::SA_RESETHAND
            | libc::SA_NODEFER
            | libc::SA_SIGINFO
            | libc::SA_NOCLDSTOP
            | libc::SA_NOCLDWAIT;
        let action = disposition(sig).unwrap();
        (action.sa_sigaction, action.sa_flags & behaviour)
    }

    /// Whether `sig` is blocked on the calling thread.
    fn blocked(sig: i32) -> bool {
        let mut mask = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: with a null new mask, `pthread_sigmask` only writes the current one into
        // `mask`, which `sigismember` then reads.
        unsafe {
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), mask.as_mut_ptr()),
                0
            );
            libc::sigismember(mask.as_ptr(), sig) == 1
        }
    }

    /// Starts each reader on a real thread that reports on `ended` when it returns; the
    /// `fail_at`-th start (counting from 1) fails as thread creation does without resources.
    fn start_reporting(
        ended: &mpsc::Sender<()>,
        fail_at: Option<usize>,
    ) -> impl FnMut(String, Body) -> io::Result<()> {
        let ended = ended.clone();
        let mut calls = 0;
        move |name, body| {
            calls += 1;
            if Some(calls) == fail_at {
                return Err(io::Error::from_raw_os_error(libc::EAGAIN));
            }
            let ended = ended.clone();
            std::thread::Builder::new()
                .name(name)
                .spawn(move || {
                    body();
                    let _ = ended.send(());
                })
                .map(drop)
        }
    }

    /// A reader thread that cannot start fails the watch before any handler is installed: the
    /// signals keep their dispositions and stay unblocked, and the reader that did start ends.
    #[test]
    fn a_reader_that_cannot_start_leaves_no_handler() {
        let signals = [libc::SIGUSR1, libc::SIGWINCH];
        deliverable(&signals);
        let before = signals.map(handler);
        let (sender, ended) = mpsc::channel();
        let err = watch_signals_with(
            &signals,
            |_| {},
            start_reporting(&sender, Some(2)),
            register,
        )
        .unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EAGAIN), "{err}");
        assert_eq!(signals.map(handler), before);
        assert_eq!(signals.map(blocked), [false; 2]);
        ended
            .recv_timeout(Duration::from_secs(10))
            .expect("the started reader ended");
    }

    /// Whether [`record_previous`] ran.
    static PREVIOUS_RAN: AtomicBool = AtomicBool::new(false);

    extern "C" fn record_previous(_: libc::c_int) {
        PREVIOUS_RAN.store(true, Ordering::SeqCst);
    }

    /// Sets the disposition of `sig` to `handler` with no flags.
    fn set_handler(sig: i32, handler: libc::sighandler_t) {
        let mut action = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
        // SAFETY: `action` is a zeroed `sigaction` whose mask `sigemptyset` initializes; a null
        // old action makes `sigaction` write nothing.
        unsafe {
            (*action.as_mut_ptr()).sa_sigaction = handler;
            libc::sigemptyset(&raw mut (*action.as_mut_ptr()).sa_mask);
            assert_eq!(
                libc::sigaction(sig, action.as_ptr(), std::ptr::null_mut()),
                0
            );
        }
    }

    /// A handler that cannot be installed after another was takes that one back: its signal
    /// gets its disposition back, the signals stay unblocked, and both readers end. A signal
    /// that arrives meanwhile reaches the disposition put back, not the watch.
    #[test]
    fn a_handler_that_cannot_be_installed_leaves_no_handler() {
        let signals = [libc::SIGUSR2, libc::SIGURG];
        deliverable(&signals);
        set_handler(
            libc::SIGUSR2,
            record_previous as *const () as libc::sighandler_t,
        );
        let before = handler(libc::SIGUSR2);
        let (sender, ended) = mpsc::channel();
        let (seen, got) = mpsc::channel();
        let failing = |sig, writer| {
            if sig == libc::SIGURG {
                // SAFETY: `raise` sends the signal to this thread.
                unsafe { libc::raise(libc::SIGUSR2) };
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }
            register(sig, writer)
        };
        watch_signals_with(
            &signals,
            move |sig| {
                let _ = seen.send(sig);
            },
            start_reporting(&sender, None),
            failing,
        )
        .unwrap_err();
        assert_eq!(handler(libc::SIGUSR2), before);
        assert_eq!(signals.map(blocked), [false; 2]);
        for _ in signals {
            ended
                .recv_timeout(Duration::from_secs(10))
                .expect("every reader ended");
        }
        assert!(PREVIOUS_RAN.load(Ordering::SeqCst));
        assert_eq!(got.try_recv(), Err(mpsc::TryRecvError::Disconnected));
        set_handler(libc::SIGUSR2, libc::SIG_DFL);
    }

    /// A signal that arrives while its handler is being installed reaches the watch.
    /// `signal-hook` installs its handler before it publishes the action that handler runs, and
    /// drops a signal delivered in between. The test makes that window: before the real
    /// install, it gives the second signal `signal-hook`'s handler, copied from the first
    /// signal, raises the second signal, and puts its disposition back.
    #[test]
    fn a_signal_while_its_handler_is_installed_reaches_the_watch() {
        let signals = [libc::SIGVTALRM, libc::SIGALRM];
        deliverable(&signals);
        let (sender, ended) = mpsc::channel();
        drop(ended);
        let (seen, got) = mpsc::channel();
        let in_window = |sig, writer| {
            if sig == libc::SIGALRM {
                let installed = disposition(libc::SIGVTALRM)?;
                let previous = disposition(sig)?;
                // SAFETY: both actions were read from `sigaction`, and a null old action makes
                // `sigaction` write nothing; `raise` sends the signal to this thread.
                unsafe {
                    libc::sigaction(sig, &raw const installed, std::ptr::null_mut());
                    libc::raise(sig);
                    libc::sigaction(sig, &raw const previous, std::ptr::null_mut());
                }
            }
            register(sig, writer)
        };
        watch_signals_with(
            &signals,
            move |sig| {
                let _ = seen.send(sig);
            },
            start_reporting(&sender, None),
            in_window,
        )
        .unwrap();
        assert_eq!(got.recv_timeout(Duration::from_secs(10)), Ok(libc::SIGALRM));
    }

    #[test]
    fn own_group_exists_and_is_reported() {
        let me = std::process::id();
        let pgid = pgid_of(me).expect("own pgid");
        assert!(group_exists(pgid));
        assert!(parent_pid().is_some());
    }

    #[test]
    fn default_dispositions_are_not_ignored() {
        assert!(!ignored(SIGKILL));
    }

    #[test]
    fn missing_pids_have_no_group() {
        // PIDs are below 2^22 on Linux and 99999 on macOS.
        assert_eq!(pgid_of(i32::MAX.unsigned_abs()), None);
        assert!(!group_exists(i32::MAX.unsigned_abs()));
    }
}
