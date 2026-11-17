//! Processes a test did not start itself, and polling.

use std::path::Path;
use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessesToUpdate, System};

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
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        if self.start.is_some() && start_time(self.pid) == self.start {
            kill(self.pid);
        }
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
