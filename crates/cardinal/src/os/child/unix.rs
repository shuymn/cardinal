//! The Unix waits and signal handling [`super`] needs. `os::unix` (the runner work unit)
//! carries the same `ignored`, `watch_signals`, and `die_by_signal`, and `wait_unreaped`
//! covers `ended`; these fold into it once both are in one tree.

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

/// Whether this process ignores `sig` (`SIG_IGN`, inherited across `exec` from the caller).
/// Reads the disposition without changing it.
fn ignored(sig: i32) -> bool {
    let mut current = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
    // SAFETY: without a new action, `sigaction` only writes the current one into `current`,
    // which is valid for writes.
    let read = unsafe { libc::sigaction(sig, std::ptr::null(), current.as_mut_ptr()) };
    // SAFETY: `current` is zero-initialized, a valid `sigaction`, and filled on success.
    read == 0 && unsafe { current.assume_init() }.sa_sigaction == libc::SIG_IGN
}

/// Calls `on_signal` on a background thread for every delivery of one of `signals`, which
/// no longer take their default action in this process. A signal this process ignores is
/// skipped and stays ignored. Each signal's handler writes one byte to an OS pipe of its
/// own, read by a thread that calls `on_signal`. Best effort: signals registered before a
/// failure stay watched.
pub(super) fn watch_signals(
    signals: &[i32],
    on_signal: impl Fn(i32) + Send + Sync + 'static,
) -> std::io::Result<()> {
    use std::io::Read;

    let on_signal = std::sync::Arc::new(on_signal);
    for &sig in signals.iter().filter(|&&sig| !ignored(sig)) {
        let (mut reader, writer) = std::io::pipe()?;
        signal_hook::low_level::pipe::register(sig, writer)?;
        let on_signal = std::sync::Arc::clone(&on_signal);
        std::thread::Builder::new()
            .name(format!("cardinal-signal-{sig}"))
            .spawn(move || {
                let mut byte = [0u8; 1];
                loop {
                    match reader.read(&mut byte) {
                        Ok(1) => on_signal(sig),
                        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
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
pub(super) fn die_by_signal(sig: i32) -> ! {
    let _ = signal_hook::low_level::emulate_default_handler(sig);
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::process::exit(128 + sig)
}
