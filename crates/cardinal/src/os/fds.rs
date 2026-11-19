//! The descriptor scan (`docs/architecture.md` section 9.7): what each of the user's processes
//! uses through its working directory, root directory, executable, and descriptors on files
//! and directories, read with `proc_listallpids`, `proc_pidinfo` (`PROC_PIDVNODEPATHINFO`,
//! `PROC_PIDLISTFDS`), `proc_pidfdinfo` (`PROC_PIDFDVNODEPATHINFO`), and `proc_pidpath` on
//! macOS, and `/proc/<pid>/cwd`, `root`, `exe`, and `fd/*` on Linux.
//!
//! A scan decides only when it is complete (section 9.7, "Reads that fail"): a process table
//! that does not list this process, or a list that still fills its buffer after
//! [`TRIES`] reads into ever larger ones, fails the scan. `PROC_PIDLISTFDS` and
//! `proc_listallpids` return at most the buffer they are given without saying they cut the
//! list, so a list that fills its buffer is read again into one twice as large. One process
//! the scan cannot read is no such failure: it is listed in [`Scan::unscanned`], and other
//! users' processes, which cannot be read, are counted in [`Scan::others`]. Windows has no
//! scan: execution is not designed there (section 9.7).

use std::path::PathBuf;

/// How many buffers, each twice the size of the one before, a list is read into before a
/// list that fills them all fails the scan.
#[cfg(unix)]
pub const TRIES: u32 = 4;

/// How a process uses a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Way {
    WorkingDirectory,
    RootDirectory,
    Executable,
    Descriptor(i32),
}

/// A file a process uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Use {
    pub way: Way,
    /// The file's `dev` and `ino`, when they could be read.
    pub id: Option<(u64, u64)>,
    /// The path the OS names for it, when it names one.
    pub path: Option<PathBuf>,
}

/// What one process uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub uses: Vec<Use>,
}

/// A complete scan of the user's processes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    /// The user's processes, this one left out, each with what it uses.
    pub processes: Vec<Process>,
    /// The user's processes that could not be read, with why.
    pub unscanned: Vec<(u32, String)>,
    /// The processes of other users, which are not read.
    pub others: usize,
}

/// The first buffer of each list, in entries (the size the OS gives when `None`), and how
/// many buffers a list is read into.
#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "only macOS reads lists into buffers")
)]
struct Sizes {
    pids: Option<usize>,
    fds: Option<usize>,
    tries: u32,
}

#[cfg(unix)]
const SIZES: Sizes = Sizes {
    pids: None,
    fds: None,
    tries: TRIES,
};

/// Why one process's read ended before its end.
#[cfg(unix)]
enum Unread {
    /// It exited during the scan, or is a zombie: it uses nothing.
    Gone,
    /// It could not be read, which is listed.
    Failed(String),
    /// A list filled every buffer, which fails the scan.
    #[cfg_attr(
        not(target_os = "macos"),
        allow(dead_code, reason = "only macOS reads lists into buffers")
    )]
    Full(String),
}

/// What one read of a process's uses found: the uses read and the reads that failed, which
/// list the process as unscanned without ending its read.
#[cfg(unix)]
#[derive(Default)]
struct Read {
    uses: Vec<Use>,
    failed: Vec<String>,
}

/// Scans the user's processes, or says why the scan is not complete.
#[cfg(unix)]
pub fn scan() -> Result<Scan, String> {
    scan_with(SIZES)
}

#[cfg(unix)]
fn scan_with(sizes: Sizes) -> Result<Scan, String> {
    scan_pids(os::pids(sizes)?, sizes)
}

/// Scans the processes of `pids`, a table that must list this process.
#[cfg(unix)]
fn scan_pids(pids: Vec<u32>, sizes: Sizes) -> Result<Scan, String> {
    let own = std::process::id();
    if !pids.contains(&own) {
        return Err(format!(
            "the process table of {} processes does not list this one ({own})",
            pids.len()
        ));
    }
    let uid = rustix::process::getuid().as_raw();
    let mut scan = Scan::default();
    for pid in pids.into_iter().filter(|&pid| pid != own) {
        match os::user(pid) {
            Ok(Some(user)) if user == uid => {}
            Ok(Some(_)) => {
                scan.others += 1;
                continue;
            }
            Ok(None) => continue,
            Err(err) => {
                scan.unscanned.push((pid, err));
                continue;
            }
        }
        let mut read = Read::default();
        match os::uses(pid, sizes, &mut read) {
            Ok(()) => {}
            Err(Unread::Gone) => continue,
            Err(Unread::Failed(err)) => read.failed.push(err),
            Err(Unread::Full(err)) => return Err(format!("process {pid}: {err}")),
        }
        if let Some(why) = why_unscanned(&read.failed) {
            scan.unscanned.push((pid, why));
        }
        if !read.uses.is_empty() {
            scan.processes.push(Process {
                pid,
                uses: read.uses,
            });
        }
    }
    Ok(scan)
}

/// Why a process is unscanned: the first read that failed, with how many more did, so a
/// process whose every descriptor refuses its read gives one line.
#[cfg(unix)]
fn why_unscanned(failed: &[String]) -> Option<String> {
    let first = failed.first()?;
    Some(match failed.len() - 1 {
        0 => first.clone(),
        more => format!("{first} (and {more} more reads)"),
    })
}

/// Reads a list with `read`, which fills the buffer it is given and returns how many entries
/// it wrote, into a buffer of `first` entries and then into one twice as large, until the list
/// comes back shorter than its buffer; one that fills `tries` buffers is an error.
#[cfg(unix)]
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "only macOS reads lists into buffers")
)]
fn read_list<T: Copy, E: std::fmt::Display>(
    empty: T,
    first: usize,
    tries: u32,
    mut read: impl FnMut(&mut [T]) -> Result<usize, E>,
) -> Result<Vec<T>, Unread> {
    let mut len = first.max(1);
    for _ in 0..tries {
        let mut buf = vec![empty; len];
        let n = read(&mut buf).map_err(|err| Unread::Failed(err.to_string()))?;
        if n < len {
            buf.truncate(n);
            return Ok(buf);
        }
        len *= 2;
    }
    Err(Unread::Full(format!(
        "the list filled each of {tries} buffers, the last of {} entries",
        len / 2
    )))
}

#[cfg(target_os = "macos")]
mod os {
    use std::ffi::{CStr, OsStr};
    use std::io;
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::{Path, PathBuf};

    use libc::{c_int, c_void};

    use super::{Read, Sizes, Unread, Use, Way, read_list};

    /// `struct vnode_fdinfowithpath` of `<sys/proc_info.h>`, which `libc` lacks.
    #[repr(C)]
    struct VnodeFdInfoWithPath {
        /// `struct proc_fileinfo`, not read here: two `uint32_t`, an `off_t`, an `int32_t`,
        /// and a `uint32_t`, 24 bytes aligned to 8.
        pfi: [u64; 3],
        pvip: libc::vnode_info_path,
    }

    /// `PROC_PIDFDVNODEPATHINFO` of `<sys/proc_info.h>`, which `libc` lacks.
    const PROC_PIDFDVNODEPATHINFO: c_int = 2;

    /// `SZOMB` of `<sys/proc.h>`: a zombie.
    const SZOMB: u32 = 5;

    /// Every PID, read with `proc_listallpids`.
    pub(super) fn pids(sizes: Sizes) -> Result<Vec<u32>, String> {
        // SAFETY: without a buffer, the call returns how many PIDs the list needs room for.
        let needed = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        let needed = usize::try_from(needed)
            .map_err(|_| format!("proc_listallpids: {}", io::Error::last_os_error()))?;
        let list = read_list(
            0,
            sizes.pids.unwrap_or(needed),
            sizes.tries,
            |buf: &mut [c_int]| {
                let size = c_int::try_from(size_of_val(buf)).map_err(|err| err.to_string())?;
                // SAFETY: `buf` is valid for writes of `size` bytes, and the call writes at most
                // that many.
                let n = unsafe { libc::proc_listallpids(buf.as_mut_ptr().cast::<c_void>(), size) };
                usize::try_from(n).map_err(|_| io::Error::last_os_error().to_string())
            },
        );
        match list {
            Ok(pids) => Ok(pids
                .into_iter()
                .filter_map(|pid| u32::try_from(pid).ok())
                .collect()),
            Err(Unread::Full(err) | Unread::Failed(err)) => Err(format!("proc_listallpids: {err}")),
            Err(Unread::Gone) => Err("proc_listallpids: no list".to_owned()),
        }
    }

    /// One `proc_pidinfo` read of `flavor` into a `T`, which the call fills whole.
    fn pidinfo<T>(pid: c_int, flavor: c_int) -> io::Result<T> {
        let size = c_int::try_from(size_of::<T>()).map_err(io::Error::other)?;
        let mut info = std::mem::MaybeUninit::<T>::zeroed();
        // SAFETY: `info` is valid for writes of `size` bytes, and the call writes at most that
        // many.
        let n = unsafe { libc::proc_pidinfo(pid, flavor, 0, info.as_mut_ptr().cast(), size) };
        if n == size {
            // SAFETY: the call filled every byte of `info`, a struct of plain data.
            Ok(unsafe { info.assume_init() })
        } else if n <= 0 {
            Err(io::Error::last_os_error())
        } else {
            Err(io::Error::other(format!("{n} of {size} bytes")))
        }
    }

    fn unread(what: &str, err: &io::Error) -> Unread {
        if err.raw_os_error() == Some(libc::ESRCH) {
            Unread::Gone
        } else {
            Unread::Failed(format!("{what}: {err}"))
        }
    }

    /// The process's user, or `None` for a process that has exited.
    pub(super) fn user(pid: u32) -> Result<Option<u32>, String> {
        let pid = c_int::try_from(pid).map_err(|err| err.to_string())?;
        match pidinfo::<libc::proc_bsdshortinfo>(pid, libc::PROC_PIDT_SHORTBSDINFO) {
            Ok(info) if info.pbsi_status == SZOMB => Ok(None),
            Ok(info) => Ok(Some(info.pbsi_uid)),
            Err(err) if err.raw_os_error() == Some(libc::ESRCH) => Ok(None),
            Err(err) => Err(format!("PROC_PIDT_SHORTBSDINFO: {err}")),
        }
    }

    /// The `dev`, `ino`, and path of a vnode as `proc_info` gives them.
    fn vnode(info: &libc::vnode_info_path) -> (Option<(u64, u64)>, Option<PathBuf>) {
        let bytes: Vec<u8> = info
            .vip_path
            .iter()
            .flatten()
            .map(|&c| u8::from_ne_bytes(c.to_ne_bytes()))
            .take_while(|&b| b != 0)
            .collect();
        let stat = &info.vip_vi.vi_stat;
        // `dev_t` is signed: read as `lstat`'s `st_dev` is (`os::dir::stat`).
        let dev = u64::try_from(i32::from_ne_bytes(stat.vst_dev.to_ne_bytes())).unwrap_or_default();
        let path = (!bytes.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(&bytes)));
        ((stat.vst_ino != 0).then_some((dev, stat.vst_ino)), path)
    }

    /// Records `err` from reading `what` in `read`, or ends the read when the process is gone.
    fn failed(read: &mut Read, what: &str, err: &io::Error) -> Result<(), Unread> {
        match unread(what, err) {
            Unread::Failed(err) => {
                read.failed.push(err);
                Ok(())
            }
            other => Err(other),
        }
    }

    /// Reads what process `pid` uses into `read`.
    pub(super) fn uses(pid: u32, sizes: Sizes, read: &mut Read) -> Result<(), Unread> {
        let pid = c_int::try_from(pid).map_err(|err| Unread::Failed(err.to_string()))?;
        match pidinfo::<libc::proc_vnodepathinfo>(pid, libc::PROC_PIDVNODEPATHINFO) {
            Ok(dirs) => {
                for (way, info) in [
                    (Way::WorkingDirectory, &dirs.pvi_cdir),
                    (Way::RootDirectory, &dirs.pvi_rdir),
                ] {
                    // A process that never changed its root has none here.
                    let (id, path) = vnode(info);
                    if id.is_some() || path.is_some() {
                        read.uses.push(Use { way, id, path });
                    }
                }
            }
            Err(err) => failed(read, "PROC_PIDVNODEPATHINFO", &err)?,
        }
        let fds = match descriptors(pid, sizes) {
            Ok(fds) => fds,
            Err(Unread::Failed(err)) => {
                read.failed.push(err);
                Vec::new()
            }
            Err(err) => return Err(err),
        };
        for fd in fds {
            // SAFETY: the struct is plain data, for which all zeros is a valid value.
            let mut info: VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
            let size = c_int::try_from(size_of::<VnodeFdInfoWithPath>())
                .map_err(|err| Unread::Failed(err.to_string()))?;
            // SAFETY: the buffer is a `vnode_fdinfowithpath` of `size` bytes, which is what
            // the call may write for `PROC_PIDFDVNODEPATHINFO`.
            let n = unsafe {
                libc::proc_pidfdinfo(
                    pid,
                    fd,
                    PROC_PIDFDVNODEPATHINFO,
                    (&raw mut info).cast::<c_void>(),
                    size,
                )
            };
            if n == size {
                let (id, path) = vnode(&info.pvip);
                read.uses.push(Use {
                    way: Way::Descriptor(fd),
                    id,
                    path,
                });
            } else {
                let err = io::Error::last_os_error();
                // A descriptor closed since the list was read uses nothing.
                if err.raw_os_error() != Some(libc::EBADF) {
                    failed(read, &format!("PROC_PIDFDVNODEPATHINFO {fd}"), &err)?;
                }
            }
        }
        let mut buf = vec![0_u8; usize::try_from(libc::PROC_PIDPATHINFO_MAXSIZE).unwrap_or(4096)];
        let len = u32::try_from(buf.len()).map_err(|err| Unread::Failed(err.to_string()))?;
        // SAFETY: `buf` is valid for writes of `len` bytes, and the call writes at most that
        // many, a NUL-terminated path.
        let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), len) };
        if n <= 0 {
            return failed(read, "proc_pidpath", &io::Error::last_os_error());
        }
        let path = CStr::from_bytes_until_nul(&buf)
            .map(|c| PathBuf::from(OsStr::from_bytes(c.to_bytes())))
            .map_err(|err| Unread::Failed(format!("proc_pidpath: {err}")))?;
        read.uses.push(Use {
            way: Way::Executable,
            id: identity(&path),
            path: Some(path),
        });
        Ok(())
    }

    /// The `dev` and `ino` of the file at `path`, when it can be read.
    fn identity(path: &Path) -> Option<(u64, u64)> {
        let st = crate::os::dir::stat(&rustix::fs::stat(path).ok()?);
        Some((st.dev, st.ino))
    }

    /// The process's descriptors on files and directories (vnodes).
    fn descriptors(pid: c_int, sizes: Sizes) -> Result<Vec<c_int>, Unread> {
        let entry = size_of::<libc::proc_fdinfo>();
        // SAFETY: without a buffer, the call returns the bytes the list needs.
        let needed =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        let needed = usize::try_from(needed)
            .ok()
            .filter(|&n| n > 0)
            .ok_or_else(|| unread("PROC_PIDLISTFDS", &io::Error::last_os_error()))?;
        let empty = libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0,
        };
        let first = sizes.fds.unwrap_or(needed / entry);
        let list = read_list(
            empty,
            first,
            sizes.tries,
            |buf: &mut [libc::proc_fdinfo]| {
                let size = c_int::try_from(size_of_val(buf)).map_err(io::Error::other)?;
                // SAFETY: `buf` is valid for writes of `size` bytes, and the call writes at most
                // that many.
                let n = unsafe {
                    libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, buf.as_mut_ptr().cast(), size)
                };
                match usize::try_from(n) {
                    Ok(n) if n > 0 => Ok(n / entry),
                    _ => Err(io::Error::last_os_error()),
                }
            },
        )
        .map_err(|err| match err {
            Unread::Failed(err) => Unread::Failed(format!("PROC_PIDLISTFDS: {err}")),
            Unread::Full(err) => Unread::Full(format!("PROC_PIDLISTFDS: {err}")),
            Unread::Gone => Unread::Gone,
        })?;
        let vnode = u32::try_from(libc::PROX_FDTYPE_VNODE).unwrap_or(1);
        Ok(list
            .into_iter()
            .filter(|fd| fd.proc_fdtype == vnode)
            .map(|fd| fd.proc_fd)
            .collect())
    }
}

#[cfg(target_os = "linux")]
mod os {
    use std::io;
    use std::path::Path;

    use super::{Read, Sizes, Unread, Use, Way};

    /// Every PID `/proc` lists. Listing a directory has no buffer that can cut it short.
    pub(super) fn pids(_sizes: Sizes) -> Result<Vec<u32>, String> {
        let entries = std::fs::read_dir("/proc").map_err(|err| format!("/proc: {err}"))?;
        let mut pids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|err| format!("/proc: {err}"))?;
            if let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) {
                pids.push(pid);
            }
        }
        Ok(pids)
    }

    fn gone(err: &io::Error) -> bool {
        matches!(err.raw_os_error(), Some(libc::ENOENT | libc::ESRCH))
    }

    /// The process's real user, or `None` for a process that has exited. Its `/proc` entry
    /// belongs to root when it is not dumpable, so the user comes from `status`.
    pub(super) fn user(pid: u32) -> Result<Option<u32>, String> {
        let status = match std::fs::read_to_string(format!("/proc/{pid}/status")) {
            Ok(status) => status,
            Err(err) if gone(&err) => return Ok(None),
            Err(err) => return Err(format!("status: {err}")),
        };
        if status.lines().any(|l| l.starts_with("State:\tZ")) {
            return Ok(None);
        }
        status
            .lines()
            .find_map(|l| l.strip_prefix("Uid:"))
            .and_then(|uids| uids.split_whitespace().next()?.parse().ok())
            .map(Some)
            .ok_or_else(|| "status: no Uid line".to_owned())
    }

    /// Reads the use `link` (`cwd`, `root`, `exe`, or `fd/<n>`) names into `read`; one that
    /// names nothing (a kernel thread's `exe`, a descriptor closed meanwhile) is no use.
    fn read_use(pid: u32, link: &str, way: Way, read: &mut Read) {
        let path = format!("/proc/{pid}/{link}");
        let found = std::fs::read_link(&path).and_then(|target| {
            let st = rustix::fs::stat(Path::new(&path))?;
            Ok((target, crate::os::dir::stat(&st)))
        });
        match found {
            Ok((target, st)) => read.uses.push(Use {
                way,
                id: Some((st.dev, st.ino)),
                // A pipe, a socket, or an anonymous inode names no path.
                path: target.is_absolute().then_some(target),
            }),
            Err(err) if gone(&err) => {}
            Err(err) => read.failed.push(format!("{link}: {err}")),
        }
    }

    /// Reads what process `pid` uses into `read`.
    pub(super) fn uses(pid: u32, _sizes: Sizes, read: &mut Read) -> Result<(), Unread> {
        for (link, way) in [
            ("cwd", Way::WorkingDirectory),
            ("root", Way::RootDirectory),
            ("exe", Way::Executable),
        ] {
            read_use(pid, link, way, read);
        }
        let entries = match std::fs::read_dir(format!("/proc/{pid}/fd")) {
            Ok(entries) => entries,
            Err(err) if gone(&err) => return Err(Unread::Gone),
            Err(err) => return Err(Unread::Failed(format!("fd: {err}"))),
        };
        for entry in entries {
            let entry = entry.map_err(|err| Unread::Failed(format!("fd: {err}")))?;
            if let Some(fd) = entry.file_name().to_str().and_then(|n| n.parse().ok()) {
                read_use(pid, &format!("fd/{fd}"), Way::Descriptor(fd), read);
            }
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::Path;

    use super::{SIZES, Sizes, Unread, Way, read_list, scan, scan_pids, scan_with, why_unscanned};

    fn holder(scan: &super::Scan, pid: u32) -> &super::Process {
        scan.processes
            .iter()
            .find(|p| p.pid == pid)
            .unwrap_or_else(|| panic!("process {pid} not scanned: {scan:?}"))
    }

    fn holds(process: &super::Process, path: &Path) -> bool {
        let st = crate::os::dir::stat(&rustix::fs::stat(path).unwrap());
        let id = (st.dev, st.ino);
        process
            .uses
            .iter()
            .any(|u| matches!(u.way, Way::Descriptor(_)) && u.id == Some(id))
    }

    /// A helper whose standard error is open on `file`, its third descriptor, after its
    /// standard input and output.
    fn helper(scope: &cardinal_test_support::Scope, file: &Path) -> cardinal_test_support::Child {
        let mut command = scope.command("sleep", scope.root());
        let held = std::fs::OpenOptions::new().append(true).open(file).unwrap();
        command
            .arg("60")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(held);
        command.spawn().unwrap()
    }

    /// A scan names a helper's descriptor, working directory, and executable, and leaves this
    /// process out.
    #[test]
    fn a_process_and_what_it_uses_are_scanned() {
        let scope = cardinal_test_support::Scope::new();
        let file = scope.path("held");
        std::fs::write(&file, b"x").unwrap();
        let mut child = helper(&scope, &file);
        let scan = scan().unwrap();
        let process = holder(&scan, child.id());
        assert!(holds(process, &file), "{process:?}");
        let ways: Vec<Way> = process.uses.iter().map(|u| u.way).collect();
        assert!(ways.contains(&Way::WorkingDirectory), "{ways:?}");
        assert!(ways.contains(&Way::Executable), "{ways:?}");
        assert!(scan.processes.iter().all(|p| p.pid != std::process::id()));
        child.kill();
    }

    /// A helper's descriptor list that fills the scan's first buffer is read again into a
    /// larger one, which names the descriptor; a list that fills every buffer fails the scan.
    #[test]
    fn a_full_descriptor_list_is_read_again() {
        let scope = cardinal_test_support::Scope::new();
        let file = scope.path("held");
        std::fs::write(&file, b"x").unwrap();
        let mut child = helper(&scope, &file);
        // One entry, which lists the helper's standard input alone, doubled up to 32,768 for
        // the processes with many descriptors.
        let small = Sizes {
            fds: Some(1),
            tries: 16,
            ..SIZES
        };
        let scan = scan_with(small).unwrap();
        assert!(holds(holder(&scan, child.id()), &file));
        if cfg!(target_os = "macos") {
            let every = scan_with(Sizes { tries: 1, ..small });
            assert!(
                every.as_ref().is_err_and(|err| err.contains("filled")),
                "{every:?}"
            );
        }
        child.kill();
    }

    /// A process table without this process, or one that fills its buffer, decides nothing.
    #[test]
    fn an_incomplete_process_table_fails_the_scan() {
        let err = scan_pids(vec![1], SIZES).unwrap_err();
        assert!(err.contains("does not list this one"), "{err}");
        let err = scan_pids(Vec::new(), SIZES).unwrap_err();
        assert!(err.contains("does not list this one"), "{err}");
        if cfg!(target_os = "macos") {
            let full = scan_with(Sizes {
                pids: Some(1),
                tries: 2,
                ..SIZES
            });
            assert!(full.is_err_and(|err| err.contains("filled")));
        }
    }

    /// A list shorter than its buffer is complete; one that fills it is read into one twice as
    /// large; one that fills every buffer, or whose read fails, is no list.
    #[test]
    fn a_list_that_fills_its_buffer_is_read_again() {
        let list: Vec<u32> = (0..5).collect();
        let sizes = std::cell::RefCell::new(Vec::new());
        let mut read = |buf: &mut [u32]| -> Result<usize, String> {
            sizes.borrow_mut().push(buf.len());
            let n = list.len().min(buf.len());
            buf[..n].copy_from_slice(&list[..n]);
            Ok(n)
        };
        assert_eq!(read_list(0, 2, 3, &mut read).ok(), Some(list.clone()));
        assert_eq!(*sizes.borrow(), [2, 4, 8]);
        assert!(matches!(
            read_list(0, 5, 1, &mut read),
            Err(Unread::Full(_))
        ));
        assert!(matches!(
            read_list(0, 1, 3, &mut read),
            Err(Unread::Full(_))
        ));
        let failed = read_list(0, 8, 3, |_: &mut [u32]| Err::<usize, _>("EPERM"));
        assert!(matches!(failed, Err(Unread::Failed(err)) if err == "EPERM"));
    }

    /// An unscanned process names the first read that failed and counts the rest.
    #[test]
    fn an_unscanned_process_names_its_first_failed_read() {
        let failed =
            |n: usize| -> Vec<String> { (0..n).map(|fd| format!("fd {fd}: EPERM")).collect() };
        assert_eq!(why_unscanned(&failed(0)), None);
        assert_eq!(why_unscanned(&failed(1)).as_deref(), Some("fd 0: EPERM"));
        assert_eq!(
            why_unscanned(&failed(3)).as_deref(),
            Some("fd 0: EPERM (and 2 more reads)")
        );
    }
}
