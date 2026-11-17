//! Processes a test did not start itself, and polling.
//!
//! The guards own exactly the processes of their scope: on Unix those that hold the scope's
//! marker open (see `crate::marker`), checked when a [`PidGuard`] is made and again right
//! before each signal. A process that closed every descriptor is left to the end-of-run sweep
//! (`docs/architecture.md` section 13.4). On Windows, where the scope has no marker and a job
//! object holds every child, [`LeftoverGuard`] selects the processes working inside the root
//! and [`PidGuard`] goes by start time alone.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};

use crate::Scope;
#[cfg(unix)]
use crate::marker;

/// The processes of `scope`: on Unix those that hold its marker; on Windows any.
fn owned_by(scope: &Scope) -> impl Fn(u32) -> bool + use<> {
    #[cfg(unix)]
    {
        let marker = scope.marker();
        move |pid| marker::holds(pid, marker)
    }
    #[cfg(windows)]
    {
        let _ = scope;
        |_| true
    }
}

/// Kills a process that is not this process's child (a grandchild that reported its PID), by
/// PID and start time, when dropped. A process that does not hold the scope's marker when the
/// guard is made (a PID taken over by another process meanwhile) is never signalled, nor one
/// that no longer holds it, or whose PID names another process, by the time of the signal.
pub struct PidGuard {
    pid: u32,
    start: Option<StartTime>,
    owned: Box<dyn Fn(u32) -> bool>,
    /// On Linux, a pidfd opened when the guard was made, which carries the signal.
    #[cfg(target_os = "linux")]
    pidfd: Option<rustix::fd::OwnedFd>,
}

impl PidGuard {
    /// Records `pid` with its current start time if it is a process of `scope`, and on Linux
    /// holds it through a pidfd; anything else is never signalled.
    #[must_use]
    pub fn new(scope: &Scope, pid: u32) -> Self {
        let owned = owned_by(scope);
        #[cfg(target_os = "linux")]
        let (pidfd, gone) = match linux::hold(pid) {
            linux::Hold::Pidfd(pidfd) => (Some(pidfd), false),
            linux::Hold::Unsupported => (None, false),
            linux::Hold::Gone => (None, true),
        };
        #[cfg(not(target_os = "linux"))]
        let gone = false;
        let start = if gone { None } else { start_time(pid) };
        // The second read shows that the check read the process the first one did.
        let start = start.filter(|&start| owned(pid) && start_time(pid) == Some(start));
        Self {
            pid,
            start,
            owned: Box::new(owned),
            #[cfg(target_os = "linux")]
            pidfd,
        }
    }

    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Kills the process now (SIGKILL on Unix, `TerminateProcess` on Windows) if it is still
    /// the process recorded and still a process of the scope, as dropping the guard does.
    pub fn kill(&self) {
        if let Some(start) = self.start {
            let found = Listed {
                pid: self.pid,
                start,
            };
            kill_if_same(
                &found,
                &*self.owned,
                #[cfg(target_os = "linux")]
                self.pidfd.as_ref(),
            );
        }
    }
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Kills, when dropped, every live member of a process group that a process the test started
/// made for itself (`setpgid(0, 0)`), which no [`crate::Child`] cleans up; then, unless the
/// test is already panicking, asserts that none is left. Only processes of the scope count, so
/// a group ID that names another group by then (once the group emptied and a new process took
/// the ID) selects none of that group. Members are killed by PID until none is left, for at
/// most 2 s, as [`LeftoverGuard`] kills. This process's own group is never touched.
#[cfg(unix)]
pub struct GroupGuard {
    pgid: u32,
    marker: marker::FileId,
}

#[cfg(unix)]
impl GroupGuard {
    #[must_use]
    pub fn new(scope: &Scope, pgid: u32) -> Self {
        Self {
            pgid,
            marker: scope.marker(),
        }
    }
}

#[cfg(unix)]
impl Drop for GroupGuard {
    fn drop(&mut self) {
        let left = kill_group_members(self.pgid, self.marker);
        if !std::thread::panicking() {
            assert_eq!(
                left,
                Vec::<u32>::new(),
                "members of group {} are left",
                self.pgid
            );
        }
    }
}

/// Kills every live member of group `pgid` that holds `marker` by PID until none is left (at
/// most 2 s), and returns the ones still there.
#[cfg(unix)]
fn kill_group_members(pgid: u32, marker: marker::FileId) -> Vec<u32> {
    let pgid_of = |raw: u32| {
        let process = rustix::process::Pid::from_raw(i32::try_from(raw).ok()?)?;
        let group = rustix::process::getpgid(Some(process)).ok()?;
        u32::try_from(group.as_raw_nonzero().get()).ok()
    };
    if pgid_of(std::process::id()) == Some(pgid) {
        return Vec::new();
    }
    kill_until_gone(Duration::from_secs(2), &|pid| {
        pgid_of(pid) == Some(pgid) && marker::holds(pid, marker)
    })
}

/// Kills, when dropped, what a test left running: every process of the scope but this one, by
/// PID, until none is left or 5 s have passed; then, unless the test is already panicking,
/// asserts that none is left. For processes that leave every group a [`crate::Child`] or a
/// [`GroupGuard`] cleans up.
///
/// Each process is signalled only if its PID still names the process listed and it is still a
/// process of the scope, both read right before the signal: on Linux through a pidfd opened
/// before that read, which carries the signal; on macOS, and on Linux without pidfds, by PID
/// right after the read (`docs/architecture.md` section 13.4).
pub struct LeftoverGuard {
    root: PathBuf,
    #[cfg(unix)]
    marker: marker::FileId,
}

impl LeftoverGuard {
    #[must_use]
    pub fn new(scope: &Scope) -> Self {
        Self {
            root: scope.root().to_path_buf(),
            #[cfg(unix)]
            marker: scope.marker(),
        }
    }

    /// Whether `pid` is a process of the scope: it holds the marker (Unix), or works inside the
    /// root (Windows).
    fn owns(&self, pid: u32) -> bool {
        #[cfg(unix)]
        {
            marker::holds(pid, self.marker)
        }
        #[cfg(windows)]
        {
            cwd_of(pid).is_some_and(|cwd| cwd.starts_with(&self.root))
        }
    }
}

impl Drop for LeftoverGuard {
    fn drop(&mut self) {
        let left = kill_until_gone(Duration::from_secs(5), &|pid| self.owns(pid));
        if !std::thread::panicking() {
            assert_eq!(
                left,
                Vec::<u32>::new(),
                "processes of the scope at {} are left",
                self.root.display()
            );
        }
    }
}

/// The working directory of `pid`.
#[cfg(windows)]
fn cwd_of(pid: u32) -> Option<PathBuf> {
    let mut system = system()?;
    let pid = Pid::from_u32(pid);
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_cwd(sysinfo::UpdateKind::Always),
    );
    system.process(pid)?.cwd().map(std::path::Path::to_path_buf)
}

/// A live process other than this one, as listed.
struct Listed {
    pid: u32,
    start: StartTime,
}

/// Every live (not zombie) process other than this one; none if the table cannot be read.
fn live_processes() -> Vec<Listed> {
    let Some(mut system) = system() else {
        return Vec::new();
    };
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().without_tasks(),
    );
    let me = std::process::id();
    system
        .processes()
        .iter()
        .filter(|(pid, process)| {
            pid.as_u32() != me
                && process.thread_kind().is_none()
                && !matches!(
                    process.status(),
                    ProcessStatus::Zombie | ProcessStatus::Dead
                )
        })
        .filter_map(|(pid, _)| {
            Some(Listed {
                pid: pid.as_u32(),
                start: start_time(pid.as_u32())?,
            })
        })
        .collect()
}

/// Kills the live processes `owned` picks until none is left or `bound` has passed, and returns
/// the ones still there.
fn kill_until_gone(bound: Duration, owned: &dyn Fn(u32) -> bool) -> Vec<u32> {
    let deadline = Instant::now() + bound;
    loop {
        let found: Vec<Listed> = live_processes()
            .into_iter()
            .filter(|p| owned(p.pid))
            .collect();
        if found.is_empty() || Instant::now() >= deadline {
            return found.iter().map(|p| p.pid).collect();
        }
        for process in &found {
            kill_if_same(
                process,
                owned,
                #[cfg(target_os = "linux")]
                None,
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Kills `found` if its PID still names it (the same start time, read before and after) and
/// `owned` still picks it, all read right before the signal. On Linux a pidfd holds the process
/// and carries the signal, so the signal reaches the process checked or none: `held`, opened
/// when the process was registered, or one opened before those reads. Without one (macOS,
/// Linux before 5.3 or under an emulator that lacks pidfds) the signal goes by PID right after
/// the reads.
fn kill_if_same(
    found: &Listed,
    owned: &dyn Fn(u32) -> bool,
    #[cfg(target_os = "linux")] held: Option<&rustix::fd::OwnedFd>,
) {
    #[cfg(target_os = "linux")]
    let opened;
    #[cfg(target_os = "linux")]
    let pidfd = match held {
        Some(pidfd) => Some(pidfd),
        None => match linux::hold(found.pid) {
            linux::Hold::Gone => return,
            linux::Hold::Pidfd(pidfd) => {
                opened = pidfd;
                Some(&opened)
            }
            linux::Hold::Unsupported => None,
        },
    };
    let same = || start_time(found.pid) == Some(found.start);
    if !(same() && owned(found.pid) && same()) {
        return;
    }
    #[cfg(target_os = "linux")]
    if let Some(pidfd) = pidfd {
        let _ = rustix::process::pidfd_send_signal(pidfd, rustix::process::Signal::KILL);
        return;
    }
    signal_kill(found.pid);
}

/// SIGKILL to `pid` (Unix), `TerminateProcess` (Windows).
fn signal_kill(pid: u32) {
    #[cfg(unix)]
    if let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
    }
    #[cfg(windows)]
    {
        let Some(mut system) = system() else {
            return;
        };
        let pid = Pid::from_u32(pid);
        system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        if let Some(process) = system.process(pid) {
            process.kill();
        }
    }
}

/// Pidfds, which hold a process so that a signal sent through one reaches that process or none.
#[cfg(target_os = "linux")]
mod linux {
    use rustix::fd::OwnedFd;
    use rustix::io::Errno;
    use rustix::process::{Pid, PidfdFlags, pidfd_open};

    pub(super) enum Hold {
        Gone,
        Pidfd(OwnedFd),
        /// No pidfds here: before Linux 5.3, or under an emulator that lacks them.
        Unsupported,
    }

    /// A pidfd for `pid`; a process that cannot be held (gone, or any other failure) is never
    /// signalled.
    pub(super) fn hold(pid: u32) -> Hold {
        let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
            return Hold::Gone;
        };
        match pidfd_open(pid, PidfdFlags::empty()) {
            Ok(pidfd) => Hold::Pidfd(pidfd),
            Err(Errno::NOSYS) => Hold::Unsupported,
            Err(_) => Hold::Gone,
        }
    }
}

fn system() -> Option<System> {
    System::new().ok()
}

/// When a process started, at the finest resolution the OS reports: seconds and microseconds
/// on macOS, clock ticks since boot on Linux, seconds elsewhere. Two processes compare equal
/// only if they started within the same unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StartTime(u64, u64);

/// When `pid` started: `proc_pidinfo` on macOS, which `sysinfo` reads only to the second.
#[cfg(target_os = "macos")]
fn start_time(pid: u32) -> Option<StartTime> {
    let pid = libc::c_int::try_from(pid).ok()?;
    // SAFETY: proc_bsdinfo is plain data, for which all zeros is a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: the buffer is a proc_bsdinfo of `size` bytes, which is what the call may write
    // for PROC_PIDTBSDINFO.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast::<libc::c_void>(),
            size,
        )
    };
    (written == size).then_some(StartTime(info.pbi_start_tvsec, info.pbi_start_tvusec))
}

/// When `pid` started, in clock ticks since boot: field 22 of `/proc/<pid>/stat`, counted after
/// the command name, which is in parentheses and may hold spaces.
#[cfg(target_os = "linux")]
fn start_time(pid: u32) -> Option<StartTime> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let ticks = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()?;
    Some(StartTime(ticks, 0))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn start_time(pid: u32) -> Option<StartTime> {
    let mut system = system()?;
    let pid = Pid::from_u32(pid);
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system
        .process(pid)
        .map(|process| StartTime(process.start_time(), 0))
}

/// Whether `pid` is stopped by a signal (job control), as `ps` shows state `T`; false when it
/// does not exist. Reads the process table: a sandboxed test cannot run `ps` on macOS, where it
/// is setuid.
#[must_use]
pub fn is_stopped(pid: u32) -> bool {
    let Some(mut system) = system() else {
        return false;
    };
    let pid = Pid::from_u32(pid);
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system
        .process(pid)
        .is_some_and(|process| process.status() == ProcessStatus::Stop)
}

/// Waits until `check` returns `Some`, polling every 10 ms; panics after `timeout`.
pub fn wait_for<T>(what: &str, timeout: Duration, mut check: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits until `path` exists.
pub fn wait_for_file(path: &Path, timeout: Duration) {
    wait_for(&path.display().to_string(), timeout, || {
        path.exists().then_some(())
    });
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{GroupGuard, LeftoverGuard, Pid, PidGuard, ProcessesToUpdate};
    use super::{owned_by, start_time, system, wait_for};
    use crate::{Child, Scope};

    const WAIT: Duration = Duration::from_secs(5);

    fn sleeper(scope: &Scope) -> Child {
        let mut command = scope.command("sleep", scope.root());
        command.arg("30");
        command.spawn().expect("start a sleeper")
    }

    /// A process of `scope`, leading a group of its own, that changes to `dir` and sleeps.
    fn moved_to(scope: &Scope, dir: &Path) -> Child {
        let mut command = scope.shell("cd \"$0\" && exec sleep 30", scope.root());
        command.arg(dir);
        let child = command.spawn().expect("start the process");
        let pid = child.id();
        wait_for("the move", WAIT, || (cwd(pid)? == dir).then_some(()));
        child
    }

    fn cwd(pid: u32) -> Option<PathBuf> {
        let mut system = system()?;
        let id = Pid::from_u32(pid);
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[id]),
            true,
            sysinfo::ProcessRefreshKind::nothing().with_cwd(sysinfo::UpdateKind::Always),
        );
        system.process(id)?.cwd().map(Path::to_path_buf)
    }

    fn running(child: &mut Child) -> bool {
        child.try_wait().expect("look at the child").is_none()
    }

    /// Start times tell apart two processes that started within the same second, as a new
    /// process that took over a registered PID within a second would.
    #[test]
    fn start_times_tell_apart_processes_of_the_same_second() {
        let scope = Scope::new();
        let seconds = |pid: u32| {
            let mut system = system()?;
            let id = Pid::from_u32(pid);
            system.refresh_processes(ProcessesToUpdate::Some(&[id]), true);
            system.process(id).map(sysinfo::Process::start_time)
        };
        for _ in 0..5 {
            let first = sleeper(&scope);
            std::thread::sleep(Duration::from_millis(50));
            let second = sleeper(&scope);
            let (a, b) = (seconds(first.id()), seconds(second.id()));
            if a.is_none() || a != b {
                continue;
            }
            assert_ne!(start_time(first.id()), start_time(second.id()));
            return;
        }
        panic!("no two processes started within the same second");
    }

    /// A process that does not hold this scope's marker survives the scope's guards, though it
    /// works inside the scope's root and leads the group a guard is given.
    #[test]
    fn a_process_of_another_scope_in_the_root_survives_the_guards() {
        let scope = Scope::new();
        let other = Scope::new();
        let mut stranger = moved_to(&other, scope.root());
        drop(LeftoverGuard::new(&scope));
        assert!(running(&mut stranger), "LeftoverGuard killed it");
        drop(GroupGuard::new(&scope, stranger.id()));
        assert!(running(&mut stranger), "GroupGuard killed it");
    }

    /// A process that took over a registered PID survives the `PidGuard`: a guard made for its
    /// PID registers nothing, and one registered with its identity, as a guard made just before
    /// a takeover within the same start time would be (the seam), does not signal it.
    #[test]
    fn a_process_that_took_over_a_registered_pid_survives() {
        let scope = Scope::new();
        let other = Scope::new();
        let mut stranger = sleeper(&other);
        let pid = stranger.id();
        let made = PidGuard::new(&scope, pid);
        assert!(
            made.start.is_none(),
            "registered a process of another scope"
        );
        let registered = PidGuard {
            pid,
            start: start_time(pid),
            owned: Box::new(owned_by(&scope)),
            #[cfg(target_os = "linux")]
            pidfd: None,
        };
        registered.kill();
        std::thread::sleep(Duration::from_millis(200));
        assert!(running(&mut stranger), "the PidGuard killed it");
    }

    /// A process of the scope that left the root is still cleaned up by each guard.
    #[test]
    fn a_process_of_the_scope_outside_the_root_is_cleaned_up() {
        let scope = Scope::new();
        let root = Path::new("/");
        let mut left = moved_to(&scope, root);
        drop(LeftoverGuard::new(&scope));
        assert!(!running(&mut left), "LeftoverGuard left it running");
        let mut member = moved_to(&scope, root);
        drop(GroupGuard::new(&scope, member.id()));
        assert!(!running(&mut member), "GroupGuard left it running");
        let mut registered = moved_to(&scope, root);
        PidGuard::new(&scope, registered.id()).kill();
        wait_for("the PidGuard's kill", WAIT, || {
            (!running(&mut registered)).then_some(())
        });
    }
}
