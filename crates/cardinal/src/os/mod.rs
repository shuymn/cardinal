//! Platform process facts behind one API: the process table, start times, parent PIDs,
//! working directories, boot time, and the boot ID (`docs/architecture.md` section 2.2).
//!
//! Process-group setup and signals (Unix) live in `os/unix.rs`, owned by the runner work unit.
//! Short-lived children whose output the caller reads run through [`child`].
//! Mount identity (Unix) lives in `os/mount.rs`, shared by `plan` and `act`, listing a
//! directory through the descriptor that identifies it (Unix, `plan`'s walk) in `os/dir.rs`,
//! and the descriptor scan of `not_in_use` (Unix, section 9.7) in `os/fds.rs`.
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
pub mod fds;
#[cfg(unix)]
pub mod mount;
#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
mod windows;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Instant;

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
    /// Reads every process. A table that does not list this process is an error: the
    /// enumeration failed (`proc_listallpids` returns 0 when it fails) or was cut short, and
    /// read as complete it would answer "no such process" for live ones.
    pub fn snapshot(detail: Detail) -> Result<Self, OsError> {
        let mut system = system()?;
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh_kind(detail));
        Self::listing_this_process(
            system
                .processes()
                .iter()
                .filter_map(|(pid, process)| convert(*pid, process)),
        )
    }

    /// The table of `processes`, or an error when it does not list this process.
    fn listing_this_process(
        processes: impl IntoIterator<Item = ProcessInfo>,
    ) -> Result<Self, OsError> {
        let table = Self::from_processes(processes);
        if table.get(current_pid()).is_none() {
            return Err(OsError(format!(
                "the table of {} processes does not list this process",
                table.processes.len()
            )));
        }
        Ok(table)
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
        self.descendants_until(pid, None).unwrap_or_default()
    }

    /// [`Self::descendants`], or `None` once `deadline` has passed: the walk checks it as it
    /// goes, for a hook within its budget (section 9.8).
    #[must_use]
    pub fn descendants_until(&self, pid: u32, deadline: Option<Instant>) -> Option<Vec<u32>> {
        let passed = || deadline.is_some_and(|deadline| Instant::now() >= deadline);
        let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for process in self.iter() {
            if let Some(parent) = process.parent
                && parent != process.pid
            {
                children.entry(parent).or_default().push(process.pid);
            }
        }
        let mut found = Vec::new();
        let mut seen = BTreeSet::from([pid]);
        let mut queue = std::collections::VecDeque::from([pid]);
        while let Some(next) = queue.pop_front() {
            if passed() {
                return None;
            }
            for &child in children.get(&next).into_iter().flatten() {
                if seen.insert(child) {
                    found.push(child);
                    queue.push_back(child);
                }
            }
        }
        Some(found)
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

/// Opens `path` with `options` as a regular file only: the open neither follows a final
/// symbolic link (Unix `O_NOFOLLOW`, Windows `FILE_FLAG_OPEN_REPARSE_POINT`) nor blocks
/// (`O_NONBLOCK`, which a FIFO would otherwise wait in), and anything but a regular file is
/// an error.
pub fn open_regular(
    options: &mut std::fs::OpenOptions,
    path: &Path,
) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(options, libc::O_NONBLOCK | libc::O_NOFOLLOW);
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::custom_flags(
        options,
        windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
    );
    let file = options.open(path)?;
    if file.metadata()?.is_file() {
        Ok(file)
    } else {
        Err(std::io::Error::other(format!(
            "{}: not a regular file",
            path.display()
        )))
    }
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

/// This boot's ID (section 9.8): `kern.bootsessionuuid` on macOS,
/// `/proc/sys/kernel/random/boot_id` on Linux; `None` elsewhere and when it cannot be read. A
/// reboot is told by it, never by a time.
#[must_use]
pub fn boot_id() -> Option<String> {
    let id = read_boot_id()?;
    let id = id.trim();
    is_uuid(id).then(|| id.to_owned())
}

/// Whether `id` is a whole UUID string: a partial read is no boot ID, which would pass for
/// another boot.
fn is_uuid(id: &str) -> bool {
    id.len() == 36
        && id.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// What one fresh read of a PID establishes (section 9.8): only an absent or exited process,
/// or one with a readable start time, is decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// No process has the PID (`kill(pid, 0)` fails with `ESRCH`), or it has exited (a zombie).
    Gone,
    /// A running process with this start time (Unix seconds, never 0) and its start time at
    /// the OS's finest resolution ([`start_us`]) when that could be read.
    Running { start_s: u64, start_us: Option<u64> },
    /// It could not be read whole: listed without a start time, not listed while it exists, or
    /// a platform without the check.
    Unknown,
}

/// Reads `pid` once, now: never from an earlier table.
#[cfg(unix)]
#[must_use]
pub fn probe(pid: u32) -> Probe {
    // PID 0 and PIDs past `i32::MAX` name no process, and `kill` would take them for groups.
    let Ok(raw) = libc::pid_t::try_from(pid) else {
        return Probe::Gone;
    };
    if raw <= 0 {
        return Probe::Gone;
    }
    let absent = || {
        // SAFETY: signal 0 sends nothing; it only checks that `raw`, a positive PID, exists.
        let status = unsafe { libc::kill(raw, 0) };
        status != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    };
    if absent() {
        return Probe::Gone;
    }
    match process(pid, Detail::default()) {
        Some(p) if p.zombie => Probe::Gone,
        Some(p) if p.start_s == 0 => Probe::Unknown,
        Some(p) => Probe::Running {
            start_s: p.start_s,
            start_us: start_us(pid),
        },
        // Listed by `kill` but not read: gone only if it is gone now.
        None if absent() => Probe::Gone,
        None => Probe::Unknown,
    }
}

#[cfg(not(unix))]
#[must_use]
pub fn probe(_pid: u32) -> Probe {
    Probe::Unknown
}

/// The start time of `pid` in microseconds at the OS's finest resolution, comparable only
/// within one boot: since the epoch on macOS (`proc_pidinfo` `PROC_PIDTBSDINFO`, 1 µs), since
/// boot on Linux (`/proc/<pid>/stat` field 22, in clock ticks); `None` when it cannot be read
/// and elsewhere.
#[cfg(target_os = "macos")]
#[must_use]
pub fn start_us(pid: u32) -> Option<u64> {
    let raw = libc::c_int::try_from(pid).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: `info` is writable for `size` bytes, the size of the struct the flavor fills.
    let written = unsafe {
        libc::proc_pidinfo(
            raw,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: the call filled the whole struct.
    let info = unsafe { info.assume_init() };
    let us = info
        .pbi_start_tvsec
        .checked_mul(1_000_000)?
        .checked_add(info.pbi_start_tvusec)?;
    (us > 0).then_some(us)
}

#[cfg(target_os = "linux")]
#[must_use]
pub fn start_us(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name, in parentheses, can hold spaces and parentheses: fields after it are
    // counted from its last `)`, field 3 first.
    let rest = &stat[stat.rfind(')')? + 1..];
    let ticks: u64 = rest.split_whitespace().nth(22 - 3)?.parse().ok()?;
    // SAFETY: `sysconf` reads a constant.
    let hz = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok()?;
    (hz > 0).then(|| ticks.saturating_mul(1_000_000) / hz)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[must_use]
pub fn start_us(_pid: u32) -> Option<u64> {
    None
}

/// The file name of the executable `pid` runs, from `/proc/<pid>/exe` on Linux, which a
/// process cannot rename as it can its `comm`; `None` when it cannot be read and elsewhere.
#[cfg(target_os = "linux")]
#[must_use]
pub fn exe_name(pid: u32) -> Option<String> {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    exe.file_name()?.to_str().map(str::to_owned)
}

#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn exe_name(_pid: u32) -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
fn read_boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()
}

#[cfg(target_os = "macos")]
fn read_boot_id() -> Option<String> {
    const NAME: &std::ffi::CStr = c"kern.bootsessionuuid";
    // A UUID string with its NUL is 37 bytes.
    let mut buffer = [0_u8; 64];
    let mut len = buffer.len();
    // SAFETY: `NAME` is NUL-terminated, `buffer` is writable for `len` bytes and `len` is a
    // valid `size_t` the call updates; no new value is passed.
    let status = unsafe {
        libc::sysctlbyname(
            NAME.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return None;
    }
    let bytes = buffer.get(..len)?;
    let bytes = bytes.split(|b| *b == 0).next()?;
    String::from_utf8(bytes.to_vec()).ok()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_boot_id() -> Option<String> {
    None
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

/// Starts `program` with `args` as a cardinal process in a new session (on Windows, outside
/// the starter's job objects, refused where a job does not allow that) with null standard
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
        // A walk past its deadline stops, where without one it finishes (control).
        let passed = std::time::Instant::now();
        assert_eq!(table.descendants_until(10, Some(passed)), None);
        assert_eq!(table.descendants_until(10, None), Some(vec![11, 13, 12]));
        assert_eq!(table.ancestors(12), [11, 10, 1]);
        assert!(table.is_alive(11, 111));
        assert!(
            !table.is_alive(11, 112),
            "another start time is another process"
        );
        assert!(!table.is_alive(13, 113), "zombies are not alive");
        assert!(!table.is_alive(99, 199));
    }

    /// An enumeration that failed or was cut short is an error, never a table without those
    /// processes.
    #[test]
    fn a_table_without_this_process_is_an_error() {
        let me = super::current_pid();
        let empty = ProcessTable::listing_this_process([]);
        assert!(empty.is_err(), "{empty:?}");
        let without_me = ProcessTable::listing_this_process([proc(me + 1, None)]);
        assert!(without_me.is_err(), "{without_me:?}");
        let with_me = ProcessTable::listing_this_process([proc(me, None)]).unwrap();
        assert!(with_me.get(me).is_some());
        let snapshot = ProcessTable::snapshot(super::Detail::default()).unwrap();
        assert!(snapshot.get(me).is_some());
    }

    /// A boot ID is a whole UUID: a partial or padded read is none.
    #[test]
    fn boot_id_is_a_whole_uuid() {
        assert!(super::is_uuid("0f3b2a8e-1c4d-4e5f-8a9b-0c1d2e3f4a5b"));
        for partial in [
            "",
            "0f3b2a8e-1c4d-4e5f",
            "0f3b2a8e-1c4d-4e5f-8a9b-0c1d2e3f4a5",
            "0f3b2a8e-1c4d-4e5f-8a9b-0c1d2e3f4a5bb",
            "0f3b2a8e_1c4d-4e5f-8a9b-0c1d2e3f4a5b",
            "0f3b2a8e-1c4d-4e5f-8a9b-0c1d2e3f4a5g",
        ] {
            assert!(!super::is_uuid(partial), "{partial}");
        }
    }

    /// A probe reads the process now: this process runs with the start time the table lists,
    /// PID 0 and an unused PID are gone, and the start time at the finest resolution is within
    /// the second the table lists (macOS) or readable (Linux).
    #[cfg(unix)]
    #[test]
    fn probe_reads_now() {
        let me = super::current_pid();
        assert_eq!(
            super::probe(me),
            super::Probe::Running {
                start_s: super::start_time_s(me).unwrap(),
                start_us: super::start_us(me),
            }
        );
        assert_eq!(super::probe(0), super::Probe::Gone);
        assert_eq!(super::probe(u32::MAX), super::Probe::Gone);
        let fine = super::start_us(me).expect("a start time");
        if cfg!(target_os = "macos") {
            assert_eq!(fine / 1_000_000, super::start_time_s(me).unwrap());
        }
        assert!(fine > 0);
    }

    /// The boot ID is read where the OS has one, the same on every read.
    #[test]
    fn boot_id_is_stable() {
        let id = super::boot_id();
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            let id = id.expect("a boot ID");
            assert_eq!(id.len(), 36, "{id}");
            assert_eq!(super::boot_id().as_deref(), Some(id.as_str()));
        } else {
            assert_eq!(id, None);
        }
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
