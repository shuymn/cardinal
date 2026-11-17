//! Processes a test did not start itself, and polling.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind};

/// Kills a process that is not this process's child (a grandchild that reported its PID), by
/// PID and start time, when dropped. A PID that names another process by then is left alone.
pub struct PidGuard {
    pid: u32,
    start: Option<u64>,
}

impl PidGuard {
    /// Records `pid` with its current start time; a process that is already gone is never
    /// signalled.
    #[must_use]
    pub fn new(pid: u32) -> Self {
        Self {
            pid,
            start: start_time(pid),
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
            kill_if_same(self.pid, start);
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
/// test is already panicking, asserts that none is left. Members are killed by PID until
/// none is left, for at most 2 s, each only while its PID still names the process listed.
/// This process's own group is never touched.
#[cfg(unix)]
pub struct GroupGuard(pub u32);

#[cfg(unix)]
impl Drop for GroupGuard {
    fn drop(&mut self) {
        let left = kill_group_members(self.0);
        if !std::thread::panicking() {
            assert_eq!(
                left,
                Vec::<u32>::new(),
                "members of group {} are left",
                self.0
            );
        }
    }
}

/// Kills every live member of group `pgid` by PID until none is left (at most 2 s), and
/// returns the ones still there.
#[cfg(unix)]
fn kill_group_members(pgid: u32) -> Vec<u32> {
    let pgid_of = |raw: u32| {
        let process = rustix::process::Pid::from_raw(i32::try_from(raw).ok()?)?;
        let group = rustix::process::getpgid(Some(process)).ok()?;
        u32::try_from(group.as_raw_nonzero().get()).ok()
    };
    if pgid_of(std::process::id()) == Some(pgid) {
        return Vec::new();
    }
    kill_until_gone(Duration::from_secs(2), false, |p| {
        pgid_of(p.pid) == Some(pgid)
    })
}

/// Kills, when dropped, what a test left working inside a directory: every process but this
/// one whose working directory is inside `root`, by PID, until none is left or 5 s have
/// passed, each only while its PID still names the process listed; then, unless the test is
/// already panicking, asserts that none is left. For processes that leave every group a
/// [`crate::Child`] or a [`GroupGuard`] cleans up.
pub struct LeftoverGuard {
    root: PathBuf,
}

impl LeftoverGuard {
    #[must_use]
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }
}

impl Drop for LeftoverGuard {
    fn drop(&mut self) {
        let left = kill_until_gone(Duration::from_secs(5), true, |p| {
            p.cwd
                .as_deref()
                .is_some_and(|cwd| cwd.starts_with(&self.root))
        });
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
    start: u64,
    cwd: Option<PathBuf>,
}

/// Every live (not zombie) process other than this one; none if the table cannot be read.
fn live_processes(cwd: bool) -> Vec<Listed> {
    let Some(mut system) = system() else {
        return Vec::new();
    };
    let mut kind = ProcessRefreshKind::nothing().without_tasks();
    if cwd {
        kind = kind.with_cwd(UpdateKind::Always);
    }
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
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
        .map(|(pid, process)| Listed {
            pid: pid.as_u32(),
            start: process.start_time(),
            cwd: process.cwd().map(Path::to_path_buf),
        })
        .collect()
}

/// Kills the live processes `select` picks until none is left or `bound` has passed, and
/// returns the ones still there.
fn kill_until_gone(bound: Duration, cwd: bool, select: impl Fn(&Listed) -> bool) -> Vec<u32> {
    let deadline = Instant::now() + bound;
    loop {
        let found: Vec<Listed> = live_processes(cwd).into_iter().filter(&select).collect();
        if found.is_empty() || Instant::now() >= deadline {
            return found.iter().map(|p| p.pid).collect();
        }
        for process in &found {
            kill_if_same(process.pid, process.start);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Kills `pid` if it still has start time `start`.
fn kill_if_same(pid: u32, start: u64) {
    if start_time(pid) == Some(start) {
        kill(pid);
    }
}

fn system() -> Option<System> {
    System::new().ok()
}

fn start_time(pid: u32) -> Option<u64> {
    let mut system = system()?;
    let pid = Pid::from_u32(pid);
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).map(sysinfo::Process::start_time)
}

fn kill(pid: u32) {
    let Some(mut system) = system() else {
        return;
    };
    let pid = Pid::from_u32(pid);
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    if let Some(process) = system.process(pid) {
        process.kill();
    }
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
