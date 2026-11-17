//! Processes a test did not start itself, and polling.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind};

use crate::Scope;

/// Kills a process that is not this process's child (a grandchild that reported its PID), by
/// PID and start time, when dropped. A PID that names another process by then is left alone.
pub struct PidGuard {
    pid: u32,
    start: Option<StartTime>,
    /// On Linux, a pidfd opened when the guard was made, which carries the signal.
    #[cfg(target_os = "linux")]
    pidfd: Option<rustix::fd::OwnedFd>,
}

impl PidGuard {
    /// Records `pid` with its current start time, and on Linux holds it through a pidfd; a
    /// process that is already gone is never signalled.
    #[must_use]
    pub fn new(pid: u32) -> Self {
        #[cfg(target_os = "linux")]
        let (pidfd, gone) = match linux::hold(pid) {
            linux::Hold::Pidfd(pidfd) => (Some(pidfd), false),
            linux::Hold::Unsupported => (None, false),
            linux::Hold::Gone => (None, true),
        };
        #[cfg(not(target_os = "linux"))]
        let gone = false;
        Self {
            pid,
            start: if gone { None } else { start_time(pid) },
            #[cfg(target_os = "linux")]
            pidfd,
        }
    }

    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Kills the process now (SIGKILL on Unix, `TerminateProcess` on Windows) if its PID still
    /// names the process recorded, as dropping the guard does.
    pub fn kill(&self) {
        if let Some(start) = self.start {
            let found = Listed {
                pid: self.pid,
                start,
                cwd: None,
            };
            kill_if_same(
                &found,
                false,
                &|_| true,
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
/// test is already panicking, asserts that none is left. Only members working inside the
/// scope's root count, so a group ID that names another group by then (once the group emptied
/// and a new process took the ID) selects none of that group. Members are killed by PID until
/// none is left, for at most 2 s, as [`LeftoverGuard`] kills. This process's own group is never
/// touched.
#[cfg(unix)]
pub struct GroupGuard {
    pgid: u32,
    root: PathBuf,
}

#[cfg(unix)]
impl GroupGuard {
    #[must_use]
    pub fn new(scope: &Scope, pgid: u32) -> Self {
        Self {
            pgid,
            root: scope.root().to_path_buf(),
        }
    }
}

#[cfg(unix)]
impl Drop for GroupGuard {
    fn drop(&mut self) {
        let left = kill_group_members(self.pgid, &self.root);
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

/// Kills every live member of group `pgid` working inside `root` by PID until none is left (at
/// most 2 s), and returns the ones still there.
#[cfg(unix)]
fn kill_group_members(pgid: u32, root: &Path) -> Vec<u32> {
    let pgid_of = |raw: u32| {
        let process = rustix::process::Pid::from_raw(i32::try_from(raw).ok()?)?;
        let group = rustix::process::getpgid(Some(process)).ok()?;
        u32::try_from(group.as_raw_nonzero().get()).ok()
    };
    if pgid_of(std::process::id()) == Some(pgid) {
        return Vec::new();
    }
    kill_until_gone(Duration::from_secs(2), true, &|p| {
        p.inside(root) && pgid_of(p.pid) == Some(pgid)
    })
}

/// Kills, when dropped, what a test left working inside its scope: every process but this one
/// whose working directory is inside the scope's root, by PID, until none is left or 5 s have
/// passed; then, unless the test is already panicking, asserts that none is left. For processes
/// that leave every group a [`crate::Child`] or a [`GroupGuard`] cleans up.
///
/// Each process is signalled only if its PID still names the process listed and it still works
/// inside the root, both read right before the signal: on Linux through a pidfd opened before
/// that read, which carries the signal; on macOS, and on Linux without pidfds, by PID right
/// after the read (`docs/architecture.md` section 13.4).
pub struct LeftoverGuard {
    root: PathBuf,
}

impl LeftoverGuard {
    #[must_use]
    pub fn new(scope: &Scope) -> Self {
        Self {
            root: scope.root().to_path_buf(),
        }
    }
}

impl Drop for LeftoverGuard {
    fn drop(&mut self) {
        let left = kill_until_gone(Duration::from_secs(5), true, &|p| p.inside(&self.root));
        if !std::thread::panicking() {
            assert_eq!(
                left,
                Vec::<u32>::new(),
                "processes left in {}",
                self.root.display()
            );
        }
    }
}

/// A live process other than this one, as listed.
struct Listed {
    pid: u32,
    start: StartTime,
    cwd: Option<PathBuf>,
}

impl Listed {
    fn inside(&self, root: &Path) -> bool {
        self.cwd.as_deref().is_some_and(|cwd| cwd.starts_with(root))
    }
}

fn refresh_kind(cwd: bool) -> ProcessRefreshKind {
    let kind = ProcessRefreshKind::nothing().without_tasks();
    if cwd {
        kind.with_cwd(UpdateKind::Always)
    } else {
        kind
    }
}

/// `process` unless it is this process, a thread, or a zombie.
fn listed(pid: Pid, process: &sysinfo::Process) -> Option<Listed> {
    let live = pid.as_u32() != std::process::id()
        && process.thread_kind().is_none()
        && !matches!(
            process.status(),
            ProcessStatus::Zombie | ProcessStatus::Dead
        );
    if !live {
        return None;
    }
    Some(Listed {
        pid: pid.as_u32(),
        start: start_time(pid.as_u32())?,
        cwd: process.cwd().map(Path::to_path_buf),
    })
}

/// Every live (not zombie) process other than this one; none if the table cannot be read.
fn live_processes(cwd: bool) -> Vec<Listed> {
    let Some(mut system) = system() else {
        return Vec::new();
    };
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh_kind(cwd));
    system
        .processes()
        .iter()
        .filter_map(|(&pid, process)| listed(pid, process))
        .collect()
}

/// Kills the live processes `select` picks until none is left or `bound` has passed, and
/// returns the ones still there.
fn kill_until_gone(bound: Duration, cwd: bool, select: &dyn Fn(&Listed) -> bool) -> Vec<u32> {
    let deadline = Instant::now() + bound;
    loop {
        let found: Vec<Listed> = live_processes(cwd)
            .into_iter()
            .filter(|p| select(p))
            .collect();
        if found.is_empty() || Instant::now() >= deadline {
            return found.iter().map(|p| p.pid).collect();
        }
        for process in &found {
            kill_if_same(
                process,
                cwd,
                select,
                #[cfg(target_os = "linux")]
                None,
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Kills `found` if its PID still names it (the same start time) and `select` still picks it,
/// both read in the one refresh that leads to the signal. On Linux a pidfd holds the process
/// and carries the signal, so the signal reaches the process checked or none: `held`, opened
/// when the process was registered, or one opened before that refresh. Without one (macOS,
/// Linux before 5.3 or under an emulator that lacks pidfds) the signal goes by PID right after
/// the refresh.
fn kill_if_same(
    found: &Listed,
    cwd: bool,
    select: &dyn Fn(&Listed) -> bool,
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
    let Some(mut system) = system() else {
        return;
    };
    let pid = Pid::from_u32(found.pid);
    system.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, refresh_kind(cwd));
    let Some(process) = system.process(pid) else {
        return;
    };
    let Some(now) = listed(pid, process) else {
        return;
    };
    if now.start != found.start || !select(&now) {
        return;
    }
    #[cfg(target_os = "linux")]
    if let Some(pidfd) = pidfd {
        let _ = rustix::process::pidfd_send_signal(pidfd, rustix::process::Signal::KILL);
        return;
    }
    process.kill();
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
    use std::time::Duration;

    use super::{GroupGuard, Listed, Pid, ProcessesToUpdate, kill_if_same, listed, refresh_kind};
    use super::{start_time, system, wait_for};
    use crate::Scope;

    const WAIT: Duration = Duration::from_secs(5);

    fn sleeper(scope: &Scope) -> crate::Child {
        let mut command = scope.command("sleep", scope.root());
        command.arg("30");
        command.spawn().expect("start a sleeper")
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

    /// A group guard selects no member outside its scope's root, as when the group's ID names
    /// another group by the time the guard is dropped.
    #[test]
    fn a_group_guard_kills_only_members_inside_its_scope() {
        let theirs = Scope::new();
        let mut child = sleeper(&theirs);
        drop(GroupGuard::new(&Scope::new(), child.id()));
        assert!(
            child.try_wait().unwrap().is_none(),
            "a process of another scope survives"
        );
    }

    /// A process listed inside the root that has left it by the kill is not signalled: the
    /// selection is read again in the refresh that leads to the signal.
    #[test]
    fn a_process_no_longer_selected_by_the_kill_is_not_signalled() {
        let scope = Scope::new();
        let elsewhere = Scope::new();
        let mut command = scope.shell("cd \"$0\" && exec sleep 30", scope.root());
        command.arg(elsewhere.root());
        let mut child = command.spawn().expect("start the process");
        let pid = child.id();
        let start = wait_for("the start time", WAIT, || start_time(pid));
        wait_for("the move out of the root", WAIT, || {
            let mut system = system()?;
            let id = Pid::from_u32(pid);
            system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[id]),
                true,
                refresh_kind(true),
            );
            listed(id, system.process(id)?)?
                .inside(elsewhere.root())
                .then_some(())
        });
        let stale = Listed {
            pid,
            start,
            cwd: Some(scope.root().to_path_buf()),
        };
        kill_if_same(
            &stale,
            true,
            &|p| p.inside(scope.root()),
            #[cfg(target_os = "linux")]
            None,
        );
        std::thread::sleep(Duration::from_millis(200));
        assert!(child.try_wait().unwrap().is_none(), "not signalled");
    }
}
