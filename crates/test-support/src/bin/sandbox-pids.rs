//! `sandbox-pids <denied> <allowed>`: prints the PID of every process whose sandbox denies
//! the Mach service name `<denied>` and allows `<allowed>`, one per line. `scripts/test` gives
//! its sandbox profile a rule that denies a name unique to the run, so this lists the run's
//! processes on macOS (`docs/architecture.md` section 13.4). The sandbox is inherited by every
//! descendant and cannot be left, so a process that closed its descriptors, started a new
//! session, or runs a platform binary (whose environment macOS hides) is still found.
//!
//! A process whose sandbox cannot be checked is printed as `?<pid>`. Exit status: 0 when the
//! list is complete (it may be empty), 1 when the process table cannot be read, 2 on bad
//! usage or on an OS without this check.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [denied, allowed] = args.as_slice() else {
        eprintln!("usage: sandbox-pids <denied-name> <allowed-name>");
        return ExitCode::from(2);
    };
    run(denied, allowed)
}

#[cfg(target_os = "macos")]
fn run(denied: &str, allowed: &str) -> ExitCode {
    use std::ffi::CString;

    let (Ok(denied), Ok(allowed)) = (CString::new(denied), CString::new(allowed)) else {
        eprintln!("sandbox-pids: a name contains NUL");
        return ExitCode::from(2);
    };
    let pids = match macos::all_pids() {
        Ok(pids) => pids,
        Err(err) => {
            eprintln!("sandbox-pids: cannot list processes: {err}");
            return ExitCode::FAILURE;
        }
    };
    for pid in pids {
        // A process that is gone reads as denying every name, and so does a sandbox that
        // denies by default; only the run's sandbox allows the second name.
        match (macos::denies(pid, &denied), macos::denies(pid, &allowed)) {
            (Some(true), Some(false)) => println!("{pid}"),
            (Some(_), Some(_)) => {}
            _ => println!("?{pid}"),
        }
    }
    ExitCode::SUCCESS
}

#[cfg(not(target_os = "macos"))]
fn run(_denied: &str, _allowed: &str) -> ExitCode {
    eprintln!("sandbox-pids: only macOS sandboxes can be checked");
    ExitCode::from(2)
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{CStr, c_char, c_int, c_void};
    use std::io;

    /// `SANDBOX_FILTER_GLOBAL_NAME` of the sandbox's `enum sandbox_filter_type`: the argument
    /// is a Mach service name.
    const FILTER_GLOBAL_NAME: c_int = 2;

    unsafe extern "C" {
        /// `libsystem_sandbox`: whether process `pid`'s sandbox denies `operation` with the
        /// filter argument that follows (1), allows it (0), or fails (-1).
        fn sandbox_check(pid: libc::pid_t, operation: *const c_char, filter: c_int, ...) -> c_int;
        /// The flag that keeps a check from being logged as a violation.
        static SANDBOX_CHECK_NO_REPORT: c_int;
    }

    /// Every PID in the system. A full buffer may have cut the list short, so the buffer
    /// grows until the list fits with room to spare.
    pub(super) fn all_pids() -> io::Result<Vec<libc::pid_t>> {
        // SAFETY: a null buffer of size 0 asks only for the number of processes.
        let estimate = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        let mut capacity = usize::try_from(estimate).map_err(|_| io::Error::last_os_error())? + 64;
        loop {
            let mut pids: Vec<libc::pid_t> = vec![0; capacity];
            let bytes = c_int::try_from(capacity * size_of::<libc::pid_t>())
                .map_err(|_| io::Error::other("too many processes"))?;
            // SAFETY: the buffer holds `bytes` bytes of `pid_t`s, which is what the call may
            // write.
            let count =
                unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast::<c_void>(), bytes) };
            let count = usize::try_from(count).map_err(|_| io::Error::last_os_error())?;
            if count < capacity {
                pids.truncate(count);
                pids.retain(|&pid| pid > 0);
                return Ok(pids);
            }
            capacity *= 2;
        }
    }

    /// Whether `pid`'s sandbox denies looking up the Mach service `name`, or `None` if the
    /// check fails. An unsandboxed process allows everything.
    pub(super) fn denies(pid: libc::pid_t, name: &CStr) -> Option<bool> {
        // SAFETY: the operation and the name are NUL-terminated strings that outlive the
        // call, which reads them only; the global-name filter takes one string argument.
        let result = unsafe {
            sandbox_check(
                pid,
                c"mach-lookup".as_ptr(),
                FILTER_GLOBAL_NAME | SANDBOX_CHECK_NO_REPORT,
                name.as_ptr(),
            )
        };
        match result {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
}
