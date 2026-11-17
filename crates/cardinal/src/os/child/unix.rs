//! The Unix waits and signal handling [`super`] needs. `os::unix` (the runner work unit)
//! carries the same `ignored`, `reset_child_signal`, `watch_signals`, and `die_by_signal`,
//! and `wait_unreaped` covers `ended`; these fold into it once both are in one tree.

use std::io::{self, Read as _};
use std::sync::Arc;

use rustix::process::{Pid, WaitId, WaitIdOptions};

/// Whether child `pid` has ended (exited or killed), without reaping it or taking a report,
/// so its PID and the process group it leads stay pinned. A stop is not an end: macOS
/// reports a stop even to a wait for exits alone.
pub(super) fn ended(raw_pid: u32) -> io::Result<bool> {
    let id = i32::try_from(raw_pid)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| io::Error::other(format!("invalid PID {raw_pid}")))?;
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
fn ignored(sig: i32) -> bool {
    disposition(sig).is_ok_and(|action| action.sa_sigaction == libc::SIG_IGN)
}

/// Sets SIGCHLD to its default action, so that every child of this process stays unreaped
/// until this process reaps it. A caller that ignores SIGCHLD passes that on across `exec`,
/// and Linux then reaps the children itself (macOS does when this process sets it): no wait
/// could keep a child's PID, and the process group it leads, from reuse. Call before the
/// first child is started; children then start with the default too.
pub(super) fn reset_child_signal() {
    let mut action = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
    // SAFETY: `action` is a zeroed `sigaction` (`SIG_DFL`, no flags) whose mask `sigemptyset`
    // initializes; `sigaction` reads it and writes nothing back with a null old action.
    unsafe {
        libc::sigemptyset(&raw mut (*action.as_mut_ptr()).sa_mask);
        libc::sigaction(libc::SIGCHLD, action.as_ptr(), std::ptr::null_mut());
    }
}

/// Calls `on_signal` on a background thread for every delivery of one of `signals`, which
/// no longer take their default action in this process. A signal this process ignores is
/// skipped and stays ignored. Each signal's handler writes one byte to an OS pipe of its
/// own, read by a thread of its own.
///
/// Every reader runs before the first handler is installed, so no handler writes to a pipe
/// nobody reads. On a failure no handler stays installed: each signal gets back the
/// disposition it had, and the readers end.
pub(super) fn watch_signals(
    signals: &[i32],
    on_signal: impl Fn(i32) + Send + Sync + 'static,
) -> io::Result<()> {
    watch_signals_with(signals, on_signal, |name, body| {
        std::thread::Builder::new().name(name).spawn(body).map(drop)
    })
}

/// A thread body for [`watch_signals_with`]'s `start`.
type Body = Box<dyn FnOnce() + Send + 'static>;

/// [`watch_signals`], with `start` starting each reader thread: a name and a body.
fn watch_signals_with(
    signals: &[i32],
    on_signal: impl Fn(i32) + Send + Sync + 'static,
    mut start: impl FnMut(String, Body) -> io::Result<()>,
) -> io::Result<()> {
    let on_signal = Arc::new(on_signal);
    // Dropping a writer before its handler holds it ends its reader at end of file.
    let mut writers = Vec::new();
    for &sig in signals.iter().filter(|&&sig| !ignored(sig)) {
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
            let id = signal_hook::low_level::pipe::register(sig, writer)?;
            Ok((sig, id, previous))
        });
        match registered {
            Ok(entry) => installed.push(entry),
            Err(err) => {
                for (sig, id, previous) in installed.into_iter().rev() {
                    // signal-hook keeps its handler installed when its last action goes, so
                    // the previous disposition is put back first. Unregistering then closes
                    // the writer, which ends the reader.
                    // SAFETY: `previous` was read from `sigaction` for this signal; a null
                    // old action makes `sigaction` write nothing.
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
pub(super) fn die_by_signal(sig: i32) -> ! {
    let _ = signal_hook::low_level::emulate_default_handler(sig);
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::process::exit(128 + sig)
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{Body, disposition, watch_signals_with};

    /// The handler and flags of `sig`, which a failed watch must leave as they were.
    fn handler(sig: i32) -> (libc::sighandler_t, i32) {
        let action = disposition(sig).unwrap();
        (action.sa_sigaction, action.sa_flags)
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

    /// A reader thread that cannot start fails the watch before any handler is installed:
    /// the signals keep their dispositions, and the reader that did start ends.
    #[test]
    fn a_reader_that_cannot_start_leaves_no_handler() {
        let signals = [libc::SIGUSR1, libc::SIGWINCH];
        let before = signals.map(handler);
        let (sender, ended) = mpsc::channel();
        let err =
            watch_signals_with(&signals, |_| {}, start_reporting(&sender, Some(2))).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EAGAIN), "{err}");
        assert_eq!(signals.map(handler), before);
        ended
            .recv_timeout(Duration::from_secs(10))
            .expect("the started reader ended");
    }

    /// A handler that cannot be installed after another was takes that one back: its signal
    /// gets its disposition back, and both readers end.
    #[test]
    fn a_handler_that_cannot_be_installed_leaves_no_handler() {
        // Signal 0 starts a reader, but has no disposition to read or replace.
        let signals = [libc::SIGUSR2, 0];
        let before = handler(libc::SIGUSR2);
        let (sender, ended) = mpsc::channel();
        watch_signals_with(&signals, |_| {}, start_reporting(&sender, None)).unwrap_err();
        assert_eq!(handler(libc::SIGUSR2), before);
        for _ in signals {
            ended
                .recv_timeout(Duration::from_secs(10))
                .expect("every reader ended");
        }
    }
}
