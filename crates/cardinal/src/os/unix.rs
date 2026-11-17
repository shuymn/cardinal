//! Process groups, children, and signals on Unix (`docs/architecture.md` sections 4.5 and
//! 5.5): the one implementation of these primitives in the product.
//!
//! - **Process groups:** [`lead_group`] and [`rejoin_group`] move this process; [`group_exists`]
//!   (`kill(-pgid, 0)`; only `ESRCH` means empty), [`pgid_of`], [`group_members`], and
//!   [`other_group_members`] read; [`signal_group`] signals a whole group.
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
use std::time::Duration;

use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, WaitOptions};

use super::{Detail, ProcessInfo, ProcessTable};

pub use signal_hook::consts::signal::{
    SIGCONT, SIGHUP, SIGINT, SIGKILL, SIGQUIT, SIGSTOP, SIGTERM,
};

/// The process group of this process before and after [`lead_group`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupSetup {
    /// The group this process was in before; the stage runs in it again on fail-open.
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

/// Makes this process the leader of a new process group: `setpgid(0, 0)`. `EPERM` is accepted
/// when this process already leads its group (for example as a session leader); any other
/// failure is returned.
pub fn lead_group() -> io::Result<GroupSetup> {
    let me = raw(rustix::process::getpid());
    let original = raw(rustix::process::getpgrp());
    match rustix::process::setpgid(None, None) {
        Ok(()) => {}
        Err(rustix::io::Errno::PERM) if original == me => {}
        Err(err) => return Err(err.into()),
    }
    let pgid = raw(rustix::process::getpgrp());
    if pgid != me {
        return Err(io::Error::other(format!(
            "process group is {pgid} after setpgid, not {me}"
        )));
    }
    Ok(GroupSetup { original, pgid })
}

/// Moves this process back into the group it was in before [`lead_group`]. A no-op when the
/// group did not change. Fails when the original group no longer exists.
pub fn rejoin_group(setup: GroupSetup) -> io::Result<()> {
    if setup.original == setup.pgid {
        return Ok(());
    }
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

/// Whether this process ignores `sig` (`SIG_IGN`, inherited across `exec` from the caller).
/// Reads the disposition without changing it.
#[must_use]
pub fn ignored(sig: i32) -> bool {
    let mut current = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
    // SAFETY: without a new action, `sigaction` only writes the current one into `current`,
    // which is valid for writes.
    let read = unsafe { libc::sigaction(sig, std::ptr::null(), current.as_mut_ptr()) };
    // SAFETY: `current` is zero-initialized, a valid `sigaction`, and filled on success.
    read == 0 && unsafe { current.assume_init() }.sa_sigaction == libc::SIG_IGN
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
/// Each signal's handler writes one byte to an OS pipe of its own, read by a thread that calls
/// `on_signal`; a pipe, unlike the socket pair `signal-hook`'s iterator uses, keeps the runner
/// free of sockets. Watching is best effort: signals registered before a failure stay watched.
pub fn watch_signals(
    signals: &[i32],
    on_signal: impl Fn(i32) + Send + Sync + 'static,
) -> io::Result<()> {
    let on_signal = Arc::new(on_signal);
    for &sig in signals.iter().filter(|&&sig| !ignored(sig)) {
        let (mut reader, writer) = io::pipe()?;
        signal_hook::low_level::pipe::register(sig, writer)?;
        let on_signal = Arc::clone(&on_signal);
        std::thread::Builder::new()
            .name(format!("cardinal-signal-{sig}"))
            .spawn(move || {
                let mut byte = [0u8; 1];
                loop {
                    match reader.read(&mut byte) {
                        Ok(1) => on_signal(sig),
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                        _ => return,
                    }
                }
            })?;
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
    use super::{SIGKILL, group_exists, ignored, parent_pid, pgid_of};

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
