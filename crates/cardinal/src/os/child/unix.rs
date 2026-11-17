//! The Unix waits and signal handling [`super`] needs. `os::unix` (the runner work unit)
//! carries the same `ignored`, `reset_child_signal`, `watch_signals`, and `die_by_signal`,
//! and `wait_unreaped` covers `ended`; these fold into it once both are in one tree.

use std::io::{self, Read as _};
use std::sync::Arc;

use rustix::process::{Pid, WaitId, WaitIdOptions};
use signal_hook::SigId;

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
///
/// The signals are blocked on the calling thread, and so on every reader it starts, until
/// each handler is in place or every one is rolled back: signal-hook installs its OS handler
/// before it publishes the action that handler runs, and a signal in between would reach a
/// handler that does nothing with it. Blocked, it stays pending until the calling thread's
/// previous mask is back, and then reaches the finished handler (or the previous
/// disposition). A signal sent to the process meanwhile can still go to another thread that
/// does not block it, so call this before the process starts other threads.
pub(super) fn watch_signals(
    signals: &[i32],
    on_signal: impl Fn(i32) + Send + Sync + 'static,
) -> io::Result<()> {
    watch_signals_with(
        signals,
        on_signal,
        start_thread,
        signal_hook::low_level::pipe::register,
    )
}

/// A thread body for [`watch_signals_with`]'s `start`.
pub(super) type Body = Box<dyn FnOnce() + Send + 'static>;

/// Starts `body` on a new thread named `name`.
pub(super) fn start_thread(name: String, body: Body) -> io::Result<()> {
    std::thread::Builder::new().name(name).spawn(body).map(drop)
}

/// [`watch_signals`], with `start` starting each reader thread (a name and a body) and
/// `register` installing the handler that writes to a pipe.
pub(super) fn watch_signals_with(
    signals: &[i32],
    on_signal: impl Fn(i32) + Send + Sync + 'static,
    mut start: impl FnMut(String, Body) -> io::Result<()>,
    mut register: impl FnMut(i32, io::PipeWriter) -> io::Result<SigId>,
) -> io::Result<()> {
    let watched: Vec<i32> = signals
        .iter()
        .copied()
        .filter(|&sig| !ignored(sig))
        .collect();
    // Dropped last, after the handlers are in place or rolled back.
    let _blocked = Blocked::new(&watched)?;
    let on_signal = Arc::new(on_signal);
    // Dropping a writer before its handler holds it ends its reader at end of file.
    let mut writers = Vec::new();
    for sig in watched {
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

/// Signals blocked on the calling thread until this is dropped, which puts back the mask the
/// thread had. A thread started meanwhile starts with them blocked.
struct Blocked(libc::sigset_t);

impl Blocked {
    fn new(signals: &[i32]) -> io::Result<Self> {
        let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        let mut previous = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: `sigemptyset` initializes `set` before `sigaddset` and `pthread_sigmask`
        // read it; `pthread_sigmask` writes the previous mask into `previous` on success. A
        // number `sigaddset` refuses is not a signal and has no handler to wait for.
        unsafe {
            libc::sigemptyset(set.as_mut_ptr());
            for &sig in signals {
                libc::sigaddset(set.as_mut_ptr(), sig);
            }
            let err = libc::pthread_sigmask(libc::SIG_BLOCK, set.as_ptr(), previous.as_mut_ptr());
            if err != 0 {
                return Err(io::Error::from_raw_os_error(err));
            }
            Ok(Self(previous.assume_init()))
        }
    }
}

impl Drop for Blocked {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the mask `pthread_sigmask` returned; a null old mask makes it
        // write nothing. A signal that became pending meanwhile is delivered now.
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &raw const self.0, std::ptr::null_mut())
        };
    }
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

    /// [`watch_signals_with`]'s registration with signal-hook.
    const REGISTER: fn(i32, io::PipeWriter) -> io::Result<signal_hook::SigId> =
        signal_hook::low_level::pipe::register;

    /// The flags that change how a signal is handled. glibc on `x86_64` also sets
    /// `SA_RESTORER` on every action it installs, a restore included: it names the
    /// trampoline a handler returns through and changes nothing about the disposition.
    const BEHAVIOUR: i32 = libc::SA_NOCLDSTOP
        | libc::SA_NOCLDWAIT
        | libc::SA_SIGINFO
        | libc::SA_ONSTACK
        | libc::SA_RESTART
        | libc::SA_NODEFER
        | libc::SA_RESETHAND;

    /// The handler of `sig` and the flags that change its handling, which a failed watch
    /// must leave as they were.
    fn handler(sig: i32) -> (libc::sighandler_t, i32) {
        let action = disposition(sig).unwrap();
        (action.sa_sigaction, action.sa_flags & BEHAVIOUR)
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

    /// A reader thread that cannot start fails the watch before any handler is installed:
    /// the signals keep their dispositions and stay unblocked, and the reader that did start
    /// ends.
    #[test]
    fn a_reader_that_cannot_start_leaves_no_handler() {
        let signals = [libc::SIGUSR1, libc::SIGWINCH];
        let before = signals.map(handler);
        let (sender, ended) = mpsc::channel();
        let err = watch_signals_with(
            &signals,
            |_| {},
            start_reporting(&sender, Some(2)),
            REGISTER,
        )
        .unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EAGAIN), "{err}");
        assert_eq!(signals.map(handler), before);
        assert_eq!(signals.map(blocked), [false; 2]);
        ended
            .recv_timeout(Duration::from_secs(10))
            .expect("the started reader ended");
    }

    /// A handler that cannot be installed after another was takes that one back: its signal
    /// gets its disposition back and stays unblocked, and both readers end.
    #[test]
    fn a_handler_that_cannot_be_installed_leaves_no_handler() {
        // Signal 0 starts a reader, but has no disposition to read or replace.
        let signals = [libc::SIGUSR2, 0];
        let before = handler(libc::SIGUSR2);
        let (sender, ended) = mpsc::channel();
        watch_signals_with(&signals, |_| {}, start_reporting(&sender, None), REGISTER).unwrap_err();
        assert_eq!(handler(libc::SIGUSR2), before);
        assert!(!blocked(libc::SIGUSR2));
        for _ in signals {
            ended
                .recv_timeout(Duration::from_secs(10))
                .expect("every reader ended");
        }
    }
}
