//! The Unix waits [`super`] needs.

use rustix::process::{Pid, WaitId, WaitIdOptions};

/// Whether child `pid` has ended (exited or killed), without reaping it or taking a report,
/// so its PID and the process group it leads stay pinned. A stop is not an end: macOS
/// reports a stop even to a wait for exits alone.
pub(super) fn ended(raw_pid: u32) -> std::io::Result<bool> {
    let id = i32::try_from(raw_pid)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| std::io::Error::other(format!("invalid PID {raw_pid}")))?;
    let options = WaitIdOptions::EXITED
        | WaitIdOptions::NOWAIT
        | WaitIdOptions::NOHANG
        | WaitIdOptions::STOPPED;
    loop {
        match rustix::process::waitid(WaitId::Pid(id), options) {
            Ok(Some(status)) => return Ok(status.exited() || status.killed() || status.dumped()),
            Ok(None) => return Ok(false),
            Err(rustix::io::Errno::INTR) => {}
            Err(err) => return Err(err.into()),
        }
    }
}
