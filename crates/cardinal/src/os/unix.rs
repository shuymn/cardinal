//! Process groups and signals on Unix (`docs/architecture.md` sections 4.5 and 5.5).
//!
//! The runner leads its job's process group, so the group ID is the runner's PID and is known
//! before any reservation names it. A group is empty when `kill(-pgid, 0)` fails with `ESRCH`.
//! Signal handlers are installed through `signal-hook`; a handler is reset to the default
//! action by `exec`, so the stage starts with default dispositions for the handled signals.

use std::io::{self, Read as _};
use std::sync::Arc;
use std::time::Duration;

use rustix::process::{Pid, Signal, WaitId, WaitIdOptions};

use super::{Detail, ProcessInfo, ProcessTable};

pub use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGKILL, SIGQUIT, SIGTERM};

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

/// Sends `sig` to every member of group `pgid`, this process included if it is a member.
pub fn signal_group(pgid: u32, sig: i32) -> io::Result<()> {
    rustix::process::kill_process_group(pid(pgid)?, signal(sig)?).map_err(Into::into)
}

/// Sends `sig` to process `pid`.
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

/// Whether child `pid` has exited, without waiting and without reaping it, so its PID stays
/// reserved while the caller may still signal it. Fails when the child cannot be waited for,
/// for example because the parent ignores SIGCHLD and the system reaped the child already.
pub fn child_exited(raw_pid: u32) -> io::Result<bool> {
    let id = pid(raw_pid)?;
    loop {
        match rustix::process::waitid(
            WaitId::Pid(id),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
        ) {
            Ok(status) => return Ok(status.is_some()),
            Err(rustix::io::Errno::INTR) => {}
            Err(err) => return Err(err.into()),
        }
    }
}

/// Blocks until child `pid` has exited without reaping it, so the caller can still reap it
/// with `Child::try_wait` and no PID is reused while the caller may signal it. Returns an
/// error when the child cannot be waited for, for example because the parent ignores
/// SIGCHLD and the system reaped the child already.
pub fn wait_exited_unreaped(raw_pid: u32) -> io::Result<()> {
    let id = pid(raw_pid)?;
    loop {
        match rustix::process::waitid(
            WaitId::Pid(id),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
        ) {
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => {}
            Err(err) => return Err(err.into()),
        }
    }
}

/// Calls `on_signal` on a background thread for every delivery of one of `signals`. The
/// signals no longer take their default action in this process; `exec` restores it for
/// children.
///
/// Each signal's handler writes one byte to an OS pipe of its own, read by a thread that calls
/// `on_signal`; a pipe, unlike the socket pair `signal-hook`'s iterator uses, keeps the runner
/// free of sockets. Watching is best effort: signals registered before a failure stay watched.
pub fn watch_signals(
    signals: &[i32],
    on_signal: impl Fn(i32) + Send + Sync + 'static,
) -> io::Result<()> {
    let on_signal = Arc::new(on_signal);
    for &sig in signals {
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
    use super::{group_exists, parent_pid, pgid_of};

    #[test]
    fn own_group_exists_and_is_reported() {
        let me = std::process::id();
        let pgid = pgid_of(me).expect("own pgid");
        assert!(group_exists(pgid));
        assert!(parent_pid().is_some());
    }

    #[test]
    fn missing_pids_have_no_group() {
        // PIDs are below 2^22 on Linux and 99999 on macOS.
        assert_eq!(pgid_of(i32::MAX.unsigned_abs()), None);
        assert!(!group_exists(i32::MAX.unsigned_abs()));
    }
}
