//! `run-processes list|kill <sandbox> <marker> [--empty-table]`: finds the processes a run of
//! `scripts/test` started, and with `kill` kills them (`docs/architecture.md` section 13.4).
//!
//! A run's processes are, on macOS, those whose sandbox denies looking up the Mach service
//! `<sandbox>.denied` and allows `<sandbox>.allowed`: the profile of the run denies the first
//! name, which is unique to the run, and only that profile does both. The sandbox is inherited
//! by every descendant and cannot be left, so a process that closed its descriptors, started a
//! new session, or runs a platform binary (whose environment macOS hides) is still found. On
//! Linux they are this user's processes whose environment holds `<marker>`.
//!
//! `list` prints the PID of each, one per line, and `?<pid>` for each process whose membership
//! cannot be read. `kill` sends SIGKILL to each and repeats until none is left, printing each
//! PID it signalled. A PID is signalled only while it names the process that was found: on
//! Linux through a pidfd opened before the membership check, with the check repeated right
//! before the signal; on macOS, and on Linux without pidfds (before 5.3, or under an emulator
//! that lacks them), after checking right before the signal that the process still belongs to
//! the run and has the start time it had when it was found. Without a pidfd, a process that
//! exits between that check and the signal leaves its PID free, and a new process given that
//! PID in the meantime gets the signal.
//!
//! The process table must list this process: one that does not (empty, or cut short) fails
//! instead of reading as "nothing left". `--empty-table` makes the table read as empty, as
//! `proc_listallpids` does when it fails, so `scripts/test --self-check` can show that.
//!
//! Exit status: 0 when the list is complete or nothing of the run is left, 1 when the process
//! table cannot be read, 2 on bad usage or on an OS without this check, 3 when processes of the
//! run survived `kill`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (empty_table, args) = match args.split_last() {
        Some((last, rest)) if last == "--empty-table" => (true, rest),
        _ => (false, args.as_slice()),
    };
    let [mode, sandbox, marker] = args else {
        return usage();
    };
    match mode.as_str() {
        "list" | "kill" => run(mode == "kill", sandbox, marker, empty_table),
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: run-processes list|kill <sandbox-name> <marker> [--empty-table]");
    ExitCode::from(2)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn run(_kill: bool, _sandbox: &str, _marker: &str, _empty_table: bool) -> ExitCode {
    eprintln!("run-processes: only macOS and Linux processes can be found");
    ExitCode::from(2)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn run(kill: bool, sandbox: &str, marker: &str, empty_table: bool) -> ExitCode {
    let membership = os::Membership::new(sandbox, marker);
    let member = |pid: i32| membership.check(pid);
    if kill {
        return sweep::kill_all(&member, empty_table);
    }
    match sweep::find(&member, empty_table) {
        Ok((found, unknown)) => {
            for process in found {
                println!("{}", process.pid);
            }
            for pid in unknown {
                println!("?{pid}");
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("run-processes: cannot list processes: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod sweep {
    use std::io;
    use std::process::ExitCode;
    use std::time::Duration;

    use super::os;

    /// Whether a process belongs to the run, or `None` if that cannot be read.
    pub(super) type Member<'a> = dyn Fn(i32) -> Option<bool> + 'a;

    /// What a PID of the table turned out to be.
    pub(super) enum Hold {
        Member(os::Found),
        Other,
        Unknown,
        Gone,
    }

    /// Kill passes before giving up on processes that keep appearing.
    const PASSES: u32 = 40;

    /// The run's processes, and the PIDs whose membership cannot be read.
    pub(super) fn find(
        member: &Member<'_>,
        empty_table: bool,
    ) -> io::Result<(Vec<os::Found>, Vec<i32>)> {
        let mut pids = os::all_pids()?;
        if empty_table {
            pids.clear();
        }
        let me = i32::try_from(std::process::id()).map_err(io::Error::other)?;
        if !pids.contains(&me) {
            return Err(io::Error::other(
                "the process table does not list this process",
            ));
        }
        let (mut found, mut unknown) = (Vec::new(), Vec::new());
        for pid in pids.into_iter().filter(|&pid| pid != me) {
            match os::hold(pid, member)? {
                Hold::Member(process) => found.push(process),
                Hold::Unknown => unknown.push(pid),
                Hold::Other | Hold::Gone => {}
            }
        }
        Ok((found, unknown))
    }

    /// Kills the run's processes until none is left.
    pub(super) fn kill_all(member: &Member<'_>, empty_table: bool) -> ExitCode {
        let mut left = Vec::new();
        for _ in 0..PASSES {
            let found = match find(member, empty_table) {
                Ok((found, _)) => found,
                Err(err) => {
                    eprintln!("run-processes: cannot list processes: {err}");
                    return ExitCode::FAILURE;
                }
            };
            if found.is_empty() {
                return ExitCode::SUCCESS;
            }
            left.clear();
            for process in &found {
                match os::kill(process, member) {
                    Ok(true) => println!("{}", process.pid),
                    Ok(false) => {}
                    Err(err) => eprintln!("run-processes: cannot kill {}: {err}", process.pid),
                }
                left.push(process.pid);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        eprintln!("run-processes: still running after SIGKILL: {left:?}");
        ExitCode::from(3)
    }
}

#[cfg(target_os = "macos")]
mod os {
    use std::ffi::{CStr, CString, c_char, c_int, c_void};
    use std::io;

    use super::sweep::{Hold, Member};

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

    /// The run's sandbox, by the name it denies and the name it allows.
    pub(super) struct Membership {
        denied: CString,
        allowed: CString,
    }

    impl Membership {
        /// `sandbox` comes from the command line, which cannot hold NUL.
        pub(super) fn new(sandbox: &str, _marker: &str) -> Self {
            let name = |suffix| {
                CString::new(format!("{sandbox}.{suffix}"))
                    .expect("a command-line argument has no NUL")
            };
            Self {
                denied: name("denied"),
                allowed: name("allowed"),
            }
        }

        /// Whether `pid` is in the run's sandbox. A process that is gone reads as denying
        /// every name, and so does a sandbox that denies by default; only the run's sandbox
        /// allows the second name. An unsandboxed process allows everything.
        pub(super) fn check(&self, pid: i32) -> Option<bool> {
            match (denies(pid, &self.denied)?, denies(pid, &self.allowed)?) {
                (true, false) => Some(true),
                _ => Some(false),
            }
        }
    }

    /// A process of the run: its PID and the start time it had when it was found.
    pub(super) struct Found {
        pub(super) pid: i32,
        start: (u64, u64),
    }

    impl Found {
        /// Whether a pidfd holds the process: never on macOS.
        #[cfg(test)]
        #[expect(
            clippy::unused_self,
            reason = "the signature of the Linux version, which asks the record"
        )]
        pub(super) fn has_pidfd(&self) -> bool {
            false
        }
    }

    /// Every PID in the system. A full buffer may have cut the list short, so the buffer
    /// grows until the list fits with room to spare.
    pub(super) fn all_pids() -> io::Result<Vec<i32>> {
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

    #[expect(
        clippy::unnecessary_wraps,
        reason = "the signature of the Linux version, which fails when no pidfd can be opened"
    )]
    pub(super) fn hold(pid: i32, member: &Member<'_>) -> io::Result<Hold> {
        Ok(match member(pid) {
            None => Hold::Unknown,
            Some(false) => Hold::Other,
            Some(true) => match start_time(pid) {
                Ok(start) => Hold::Member(Found { pid, start }),
                Err(err) if err.raw_os_error() == Some(libc::ESRCH) => Hold::Gone,
                Err(_) => Hold::Unknown,
            },
        })
    }

    /// Sends SIGKILL to `process` if its PID still names a process of the run with the start
    /// time it was found with; whether it did.
    pub(super) fn kill(process: &Found, member: &Member<'_>) -> io::Result<bool> {
        if member(process.pid) != Some(true) {
            return Ok(false);
        }
        if start_time(process.pid).ok() != Some(process.start) {
            return Ok(false);
        }
        // SAFETY: kill takes plain integers and touches no memory.
        if unsafe { libc::kill(process.pid, libc::SIGKILL) } == 0 {
            return Ok(true);
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            Ok(false)
        } else {
            Err(err)
        }
    }

    /// When `pid` started, to the microsecond.
    fn start_time(pid: i32) -> io::Result<(u64, u64)> {
        // SAFETY: proc_bsdinfo is plain data, for which all zeros is a valid value.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = c_int::try_from(size_of::<libc::proc_bsdinfo>()).map_err(io::Error::other)?;
        // SAFETY: the buffer is a proc_bsdinfo of `size` bytes, which is what the call may
        // write for PROC_PIDTBSDINFO.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&raw mut info).cast::<c_void>(),
                size,
            )
        };
        if written != size {
            let err = io::Error::last_os_error();
            return Err(if err.raw_os_error() == Some(0) {
                io::Error::from_raw_os_error(libc::ESRCH)
            } else {
                err
            });
        }
        Ok((info.pbi_start_tvsec, info.pbi_start_tvusec))
    }

    /// Whether `pid`'s sandbox denies looking up the Mach service `name`, or `None` if the
    /// check fails.
    fn denies(pid: libc::pid_t, name: &CStr) -> Option<bool> {
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

#[cfg(target_os = "linux")]
mod os {
    use std::io;
    use std::os::fd::OwnedFd;
    use std::os::unix::fs::MetadataExt;

    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    use rustix::io::Errno;
    use rustix::process::{
        Pid, PidfdFlags, Signal, getuid, kill_process, pidfd_open, pidfd_send_signal,
    };

    use super::sweep::{Hold, Member};

    /// The run's marker, `NAME=value` as it appears in a process's environment.
    pub(super) struct Membership {
        marker: Vec<u8>,
    }

    impl Membership {
        pub(super) fn new(_sandbox: &str, marker: &str) -> Self {
            Self {
                marker: marker.as_bytes().to_vec(),
            }
        }

        /// Whether `pid`'s environment holds the marker, or `None` if it cannot be read (a
        /// process that is not dumpable hides it even from its owner). It is read by PID, so
        /// a caller holding a pidfd checks afterwards that the process is still there.
        pub(super) fn check(&self, pid: i32) -> Option<bool> {
            let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
            Some(
                environ
                    .split(|&byte| byte == 0)
                    .any(|var| var == self.marker),
            )
        }
    }

    /// A process of the run: its PID, the start time it had when it was found, and a pidfd
    /// where the kernel has them. A signal sent through the pidfd reaches that process or
    /// nothing, whoever has its PID by then.
    pub(super) struct Found {
        pub(super) pid: i32,
        start: u64,
        pidfd: Option<OwnedFd>,
    }

    impl Found {
        /// Whether a pidfd holds the process.
        #[cfg(test)]
        pub(super) fn has_pidfd(&self) -> bool {
            self.pidfd.is_some()
        }
    }

    /// Every PID in `/proc`.
    pub(super) fn all_pids() -> io::Result<Vec<i32>> {
        let mut pids = Vec::new();
        for entry in std::fs::read_dir("/proc")? {
            if let Some(pid) = entry?
                .file_name()
                .to_str()
                .and_then(|name| name.parse().ok())
            {
                pids.push(pid);
            }
        }
        Ok(pids)
    }

    /// Opens a pidfd for `pid` first, then checks membership, then that the process is still
    /// there: the check read the environment of the process the pidfd holds.
    pub(super) fn hold(pid: i32, member: &Member<'_>) -> io::Result<Hold> {
        match std::fs::metadata(format!("/proc/{pid}")) {
            Ok(meta) if meta.uid() == getuid().as_raw() => {}
            Ok(_) => return Ok(Hold::Other),
            Err(_) => return Ok(Hold::Gone),
        }
        let Some(raw) = Pid::from_raw(pid) else {
            return Ok(Hold::Gone);
        };
        let pidfd = match pidfd_open(raw, PidfdFlags::empty()) {
            Ok(pidfd) => Some(pidfd),
            Err(Errno::SRCH) => return Ok(Hold::Gone),
            // No pidfds: the start time stands in, as on macOS.
            Err(Errno::NOSYS) => None,
            Err(err) => return Err(err.into()),
        };
        let Some(start) = start_time(pid) else {
            return Ok(Hold::Gone);
        };
        let membership = member(pid);
        if pidfd.as_ref().is_some_and(exited) {
            return Ok(Hold::Gone);
        }
        Ok(match membership {
            Some(true) => Hold::Member(Found { pid, start, pidfd }),
            Some(false) => Hold::Other,
            None => Hold::Unknown,
        })
    }

    /// Sends SIGKILL to `process` if it is still running and still a process of the run:
    /// through its pidfd, or without one, if its PID still has the start time it was found
    /// with. Whether it did.
    pub(super) fn kill(process: &Found, member: &Member<'_>) -> io::Result<bool> {
        if member(process.pid) != Some(true) {
            return Ok(false);
        }
        let sent = if let Some(pidfd) = &process.pidfd {
            if exited(pidfd) {
                return Ok(false);
            }
            pidfd_send_signal(pidfd, Signal::KILL)
        } else {
            let Some(raw) = Pid::from_raw(process.pid) else {
                return Ok(false);
            };
            if start_time(process.pid) != Some(process.start) {
                return Ok(false);
            }
            kill_process(raw, Signal::KILL)
        };
        match sent {
            Ok(()) => Ok(true),
            Err(Errno::SRCH) => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    /// When `pid` started, in clock ticks since boot: field 22 of `/proc/<pid>/stat`, counted
    /// after the command name, which is in parentheses and may hold spaces.
    fn start_time(pid: i32) -> Option<u64> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(19)?
            .parse()
            .ok()
    }

    /// Whether the process a pidfd holds has exited: the pidfd turns readable then. A pidfd
    /// that cannot be polled counts as exited, so nothing is signalled through it.
    fn exited(pidfd: &OwnedFd) -> bool {
        let mut fds = [PollFd::new(pidfd, PollFlags::IN)];
        let now = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        !matches!(poll(&mut fds, Some(&now)), Ok(0))
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::time::Duration;

    use cardinal_test_support::Scope;

    use super::os;
    use super::sweep::Hold;

    fn sleeper(scope: &Scope) -> cardinal_test_support::Child {
        scope
            .command("sleep", scope.root())
            .arg("30")
            .spawn()
            .expect("start sleep")
    }

    /// SIGKILL, the same number on Linux and macOS.
    const SIGKILL: i32 = 9;

    /// A PID found as a process of the run that names another process by the time of the
    /// kill leaves that other process alone, even if it belongs to the run too: the seam
    /// below gives the found process's record the PID of a second process, as PID reuse
    /// would. Through a pidfd the signal reaches the found process; without one the start
    /// time has changed and nothing is signalled, and the unchanged record then kills the
    /// found process.
    #[test]
    fn a_pid_that_names_another_process_by_the_kill_is_not_signalled() {
        let scope = Scope::new();
        let mut found = sleeper(&scope);
        // Start times differ by more than their resolution.
        std::thread::sleep(Duration::from_millis(20));
        let mut other = sleeper(&scope);
        let pids = [found.id(), other.id()].map(|pid| i32::try_from(pid).unwrap());
        // Both count as the run's, so only the identity the sweep holds tells them apart.
        let member = move |pid: i32| Some(pids.contains(&pid));

        let Hold::Member(mut process) = os::hold(pids[0], &member).unwrap() else {
            panic!("the first process is not found");
        };
        let by_pidfd = process.has_pidfd();
        process.pid = pids[1];
        let signalled = os::kill(&process, &member).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            other.try_wait().unwrap().is_none(),
            "the process that took over the found PID was killed"
        );

        if by_pidfd {
            assert!(signalled, "the pidfd did not reach the found process");
        } else {
            assert!(!signalled, "a changed start time was signalled");
            let Hold::Member(process) = os::hold(pids[0], &member).unwrap() else {
                panic!("the first process is not found");
            };
            assert!(os::kill(&process, &member).unwrap());
        }
        assert_eq!(found.wait().unwrap().signal(), Some(SIGKILL));
    }
}
