//! Platform process facts behind one API: the process table, start times, parent PIDs,
//! working directories, and boot time (`docs/architecture.md` section 2.2).
//!
//! Process-group setup and signals (Unix) live in `os/unix.rs`, owned by the runner work unit.
//! Short-lived children whose output the caller reads run through [`child`].
//! Mount identity (Unix) lives in `os/mount.rs`, shared by `plan` and `act`, and listing a
//! directory through the descriptor that identifies it (Unix, `plan`'s walk) in `os/dir.rs`.
//! The free space and the volume of a path ([`disk`] and [`volume`], section 10.4) live in
//! `os/disk.rs`, and starting a detached cardinal process ([`spawn_detached`], sections 5.2
//! and 10.5) in `os/detached.rs`.
//!
//! A process is identified by its PID and start time (`start_s`, Unix seconds as the OS
//! reports it), so a reused PID never passes for the original process. A process that has
//! exited is not alive even while the OS still lists it: a zombie on Unix, a process kept by
//! an open handle on Windows.

pub mod child;
mod detached;
#[cfg(unix)]
pub mod dir;
mod disk;
#[cfg(unix)]
pub mod mount;
#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
mod windows;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind};

pub use disk::{Volume, entry_volume, volume};

/// One process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub parent: Option<u32>,
    pub start_s: u64,
    pub name: String,
    /// The command line, if requested and readable.
    pub cmd: Vec<String>,
    /// The working directory, if requested and readable.
    pub cwd: Option<PathBuf>,
    /// Exited but still listed: not yet reaped (Unix), or kept by an open handle (Windows).
    /// A zombie is not alive.
    pub zombie: bool,
}

impl ProcessInfo {
    /// The command line joined with spaces and cut to at most `max_bytes` bytes on a
    /// character boundary.
    #[must_use]
    pub fn command_line(&self, max_bytes: usize) -> String {
        let mut line = self.cmd.join(" ");
        line.truncate(line.floor_char_boundary(max_bytes));
        line
    }
}

/// What to read beyond PID, parent, start time, and name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Detail {
    pub cmd: bool,
    pub cwd: bool,
}

impl Detail {
    #[must_use]
    pub fn all() -> Self {
        Self {
            cmd: true,
            cwd: true,
        }
    }
}

/// Why the process table could not be read.
#[derive(Debug, thiserror::Error)]
#[error("cannot read the process table: {0}")]
pub struct OsError(String);

/// A snapshot of every process the OS lets this user see.
#[derive(Clone, Debug, Default)]
pub struct ProcessTable {
    processes: BTreeMap<u32, ProcessInfo>,
}

fn refresh_kind(detail: Detail) -> ProcessRefreshKind {
    let mut kind = ProcessRefreshKind::nothing().without_tasks();
    if detail.cmd {
        kind = kind.with_cmd(UpdateKind::Always);
    }
    if detail.cwd {
        kind = kind.with_cwd(UpdateKind::Always);
    }
    kind
}

fn convert(pid: Pid, process: &sysinfo::Process) -> Option<ProcessInfo> {
    // Linux lists threads as tasks; only whole processes belong in the table.
    if process.thread_kind().is_some() {
        return None;
    }
    Some(ProcessInfo {
        pid: pid.as_u32(),
        parent: process.parent().map(Pid::as_u32),
        start_s: process.start_time(),
        name: process.name().to_string_lossy().into_owned(),
        cmd: process
            .cmd()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect(),
        cwd: process.cwd().map(std::path::Path::to_path_buf),
        zombie: matches!(
            process.status(),
            ProcessStatus::Zombie | ProcessStatus::Dead
        ) || exited(pid.as_u32()),
    })
}

fn system() -> Result<System, OsError> {
    System::new().map_err(|err| OsError(err.to_string()))
}

impl ProcessTable {
    /// Reads every process.
    pub fn snapshot(detail: Detail) -> Result<Self, OsError> {
        let mut system = system()?;
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh_kind(detail));
        let processes = system
            .processes()
            .iter()
            .filter_map(|(pid, process)| convert(*pid, process))
            .map(|info| (info.pid, info))
            .collect();
        Ok(Self { processes })
    }

    /// A table built from given processes, for tests of code that reads the table.
    #[must_use]
    pub fn from_processes(processes: impl IntoIterator<Item = ProcessInfo>) -> Self {
        Self {
            processes: processes.into_iter().map(|p| (p.pid, p)).collect(),
        }
    }

    #[must_use]
    pub fn get(&self, pid: u32) -> Option<&ProcessInfo> {
        self.processes.get(&pid)
    }

    /// Every process, by PID.
    pub fn iter(&self) -> impl Iterator<Item = &ProcessInfo> {
        self.processes.values()
    }

    /// Whether `pid` is alive (not a zombie) with start time `start_s`.
    #[must_use]
    pub fn is_alive(&self, pid: u32, start_s: u64) -> bool {
        self.get(pid)
            .is_some_and(|p| p.start_s == start_s && !p.zombie)
    }

    /// The direct children of `pid`.
    #[must_use]
    pub fn children(&self, pid: u32) -> Vec<u32> {
        self.iter()
            .filter(|p| p.parent == Some(pid) && p.pid != pid)
            .map(|p| p.pid)
            .collect()
    }

    /// Every descendant of `pid`, breadth first, without `pid` itself.
    #[must_use]
    pub fn descendants(&self, pid: u32) -> Vec<u32> {
        let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for process in self.iter() {
            if let Some(parent) = process.parent
                && parent != process.pid
            {
                children.entry(parent).or_default().push(process.pid);
            }
        }
        let mut found = Vec::new();
        let mut queue = std::collections::VecDeque::from([pid]);
        while let Some(next) = queue.pop_front() {
            for &child in children.get(&next).into_iter().flatten() {
                if child != pid && !found.contains(&child) {
                    found.push(child);
                    queue.push_back(child);
                }
            }
        }
        found
    }

    /// The ancestors of `pid`, nearest first, stopping at a cycle or a missing parent.
    #[must_use]
    pub fn ancestors(&self, pid: u32) -> Vec<u32> {
        let mut found = Vec::new();
        let mut next = self.get(pid).and_then(|p| p.parent);
        while let Some(parent) = next {
            if parent == pid || found.contains(&parent) {
                break;
            }
            found.push(parent);
            next = self.get(parent).and_then(|p| p.parent);
        }
        found
    }
}

/// Whether a listed process has exited although its status says otherwise: sysinfo reports
/// every Windows process as running.
#[cfg(windows)]
fn exited(pid: u32) -> bool {
    windows::has_exited(pid)
}

#[cfg(not(windows))]
fn exited(_pid: u32) -> bool {
    false
}

/// The file index and the link count of the entry at `path` (Windows), not following a final
/// reparse point.
#[cfg(windows)]
pub fn file_links(path: &Path) -> std::io::Result<(u64, u64)> {
    windows::file_links(path)
}

/// Reads one process. `None` if it does not exist or cannot be read.
#[must_use]
pub fn process(pid: u32, detail: Detail) -> Option<ProcessInfo> {
    let mut system = system().ok()?;
    let target = Pid::from_u32(pid);
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[target]),
        true,
        refresh_kind(detail),
    );
    system
        .process(target)
        .and_then(|process| convert(target, process))
}

/// The start time of `pid`, if it exists and is not a zombie.
#[must_use]
pub fn start_time_s(pid: u32) -> Option<u64> {
    process(pid, Detail::default())
        .filter(|p| !p.zombie)
        .map(|p| p.start_s)
}

/// Whether `pid` is alive (not a zombie) with start time `start_s`.
#[must_use]
pub fn is_alive(pid: u32, start_s: u64) -> bool {
    start_time_s(pid) == Some(start_s)
}

/// The parent PID of `pid`.
#[must_use]
pub fn parent_pid(pid: u32) -> Option<u32> {
    process(pid, Detail::default()).and_then(|p| p.parent)
}

/// The working directory of `pid`, if the OS lets this user read it.
#[must_use]
pub fn cwd(pid: u32) -> Option<PathBuf> {
    process(
        pid,
        Detail {
            cmd: false,
            cwd: true,
        },
    )
    .and_then(|p| p.cwd)
}

/// The boot time in Unix seconds. On Windows it is derived from the uptime and can move by a
/// second between reads; reservations compare it with a 2 s tolerance.
pub fn boot_time_s() -> Result<u64, OsError> {
    System::boot_time().map_err(|err| OsError(err.to_string()))
}

/// This process's PID.
#[must_use]
pub fn current_pid() -> u32 {
    std::process::id()
}

/// Which measure of free space [`disk`] reads (`docs/architecture.md` section 10.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Measure {
    /// What new data can use: important-usage capacity through `CFURL` on macOS (purgeable
    /// space included), `statvfs` on Linux. `reconcile` reads this.
    Usable,
    /// `statfs`/`statvfs` alone, which takes microseconds: the runner's job-end check.
    Statfs,
}

/// A volume's free and total bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Space {
    pub free_bytes: u64,
    pub total_bytes: u64,
}

/// Why free space could not be read or a detached process could not start.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Unavailable(String);

/// The free space of the volume holding `path`. A read that fails is unknown and changes no
/// disk state (section 10.4).
pub fn disk(path: &Path, measure: Measure) -> Result<Space, Unavailable> {
    disk::space(path, measure)
}

/// Starts `program` with `args` as a cardinal process in a new session with null standard
/// streams and does not wait for it; returns its PID (section 10.5).
pub fn spawn_detached(program: &Path, args: &[OsString]) -> Result<u32, Unavailable> {
    detached::spawn(program, args)
}

/// This host's name, if the OS reports one.
#[must_use]
pub fn hostname() -> Option<String> {
    System::host_name().ok()
}

#[cfg(test)]
mod tests {
    use super::{ProcessInfo, ProcessTable};

    fn proc(pid: u32, parent: Option<u32>) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent,
            start_s: 100 + u64::from(pid),
            name: format!("p{pid}"),
            cmd: vec![],
            cwd: None,
            zombie: false,
        }
    }

    #[test]
    fn tree_queries() {
        let table = ProcessTable::from_processes([
            proc(1, None),
            proc(10, Some(1)),
            proc(11, Some(10)),
            proc(12, Some(11)),
            proc(20, Some(1)),
            ProcessInfo {
                zombie: true,
                ..proc(13, Some(10))
            },
        ]);
        assert_eq!(table.children(10), [11, 13]);
        assert_eq!(table.descendants(10), [11, 13, 12]);
        assert_eq!(table.ancestors(12), [11, 10, 1]);
        assert!(table.is_alive(11, 111));
        assert!(
            !table.is_alive(11, 112),
            "another start time is another process"
        );
        assert!(!table.is_alive(13, 113), "zombies are not alive");
        assert!(!table.is_alive(99, 199));
    }

    #[test]
    fn cycles_terminate() {
        let table = ProcessTable::from_processes([proc(1, Some(2)), proc(2, Some(1))]);
        assert_eq!(table.ancestors(1), [2]);
        assert_eq!(table.descendants(1), [2]);
    }

    #[test]
    fn command_line_is_cut_on_a_char_boundary() {
        let process = ProcessInfo {
            cmd: vec!["é".repeat(200)],
            ..proc(1, None)
        };
        let line = process.command_line(255);
        assert!(line.len() <= 255);
        assert!(line.chars().all(|c| c == 'é'));
    }
}
