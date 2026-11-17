//! A scope's marker (Unix): a file in the scope's root that every child the scope starts holds
//! open, and its descendants with it unless they close it. The guards of [`crate::process`]
//! own exactly the processes that hold it, identified by device and inode
//! (`docs/architecture.md` section 13.3). The environment would not do: macOS hides it for
//! platform binaries such as `/bin/sh` and `/bin/sleep`, even from their owner.

use std::os::fd::{AsRawFd as _, OwnedFd};
use std::path::Path;

/// The marker's file name in a scope's root.
const NAME: &str = ".cardinal-scope";

/// The lowest descriptor a marker takes in the test process, and so in every child: above the
/// small numbers scripts redirect (`exec 3>file`) and shells keep for themselves (10 and up),
/// so that neither replaces it.
const LOWEST: i32 = 200;

/// A file by device and inode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileId {
    dev: u64,
    ino: u64,
}

/// A scope's marker, open close-on-exec in the test process.
pub(crate) struct Marker {
    fd: OwnedFd,
    id: FileId,
    /// The descriptors every child keeps, sorted: this marker and the markers this process
    /// inherited, so a helper's children carry the scope that started the helper too.
    keep: Vec<i32>,
}

impl Marker {
    /// Creates the marker file in `root` and opens it at the first free descriptor from
    /// [`LOWEST`] (any free one if the descriptor limit is lower).
    pub(crate) fn create(root: &Path) -> Self {
        let file = std::fs::File::create(root.join(NAME)).expect("create the scope marker");
        let fd = rustix::io::fcntl_dupfd_cloexec(&file, LOWEST).unwrap_or_else(|_| file.into());
        let raw = fd.as_raw_fd();
        let id = id_of(std::process::id(), raw).expect("read the scope marker's identity");
        let mut keep = inherited();
        keep.push(raw);
        keep.sort_unstable();
        Self { fd, id, keep }
    }

    pub(crate) fn id(&self) -> FileId {
        self.id
    }

    /// The marker's descriptor in this process and in every child.
    pub(crate) fn raw(&self) -> i32 {
        self.fd.as_raw_fd()
    }

    /// The descriptors a child keeps, sorted.
    pub(crate) fn keep(&self) -> &[i32] {
        &self.keep
    }
}

/// The markers this process inherited from the scope that started it: descriptors that are
/// open across exec and name a file called [`NAME`].
fn inherited() -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir("/dev/fd") else {
        return Vec::new();
    };
    let fds: Vec<i32> = entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .filter(|&fd| fd > 2)
        .collect();
    fds.into_iter()
        .filter(|&raw| {
            // SAFETY: only the flags and the path of `raw` are read; a number closed since the
            // listing (such as the listing's own) fails.
            let fd = unsafe { rustix::fd::BorrowedFd::borrow_raw(raw) };
            rustix::io::fcntl_getfd(fd)
                .is_ok_and(|flags| !flags.contains(rustix::io::FdFlags::CLOEXEC))
                && path_of(raw).is_some_and(|path| path.file_name() == Some(NAME.as_ref()))
        })
        .collect()
}

/// Whether process `pid` holds `marker` open; false when its descriptors cannot be read.
pub(crate) fn holds(pid: u32, marker: FileId) -> bool {
    descriptors(pid)
        .iter()
        .any(|&fd| id_of(pid, fd) == Some(marker))
}

/// The path of this process's descriptor `raw`.
#[cfg(target_os = "macos")]
fn path_of(raw: i32) -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;
    let mut buf = [0_u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes, a NUL-terminated path, into `buf`.
    let got = unsafe { libc::fcntl(raw, libc::F_GETPATH, buf.as_mut_ptr()) };
    if got == -1 {
        return None;
    }
    let len = buf.iter().position(|&b| b == 0)?;
    Some(std::ffi::OsStr::from_bytes(&buf[..len]).into())
}

#[cfg(target_os = "linux")]
fn path_of(raw: i32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{raw}")).ok()
}

/// `pid`'s open descriptors that refer to files (macOS: vnodes).
#[cfg(target_os = "macos")]
fn descriptors(pid: u32) -> Vec<i32> {
    let Ok(pid) = libc::c_int::try_from(pid) else {
        return Vec::new();
    };
    let entry = size_of::<libc::proc_fdinfo>();
    // SAFETY: without a buffer, `proc_pidinfo` only returns the size the list needs.
    let needed =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    let Ok(needed) = usize::try_from(needed) else {
        return Vec::new();
    };
    let empty = libc::proc_fdinfo {
        proc_fd: 0,
        proc_fdtype: 0,
    };
    // Room for descriptors opened since.
    let mut fds = vec![empty; needed / entry + 16];
    let Ok(size) = libc::c_int::try_from(fds.len() * entry) else {
        return Vec::new();
    };
    // SAFETY: `fds` is valid for writes of `size` bytes.
    let filled =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), size) };
    let Ok(filled) = usize::try_from(filled) else {
        return Vec::new();
    };
    fds.truncate(filled / entry);
    let vnode = u32::try_from(libc::PROX_FDTYPE_VNODE).expect("a small constant");
    fds.iter()
        .filter(|fd| fd.proc_fdtype == vnode)
        .map(|fd| fd.proc_fd)
        .collect()
}

#[cfg(target_os = "linux")]
fn descriptors(pid: u32) -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect()
}

/// `struct vnode_fdinfowithpath` of `<sys/proc_info.h>`, which `libc` lacks.
#[cfg(target_os = "macos")]
#[repr(C)]
struct VnodeFdInfoWithPath {
    /// `struct proc_fileinfo`, not read here: two `uint32_t`, an `off_t`, an `int32_t`, and a
    /// `uint32_t`, 24 bytes aligned to 8.
    pfi: [u64; 3],
    pvip: libc::vnode_info_path,
}

/// `PROC_PIDFDVNODEPATHINFO` of `<sys/proc_info.h>`, which `libc` lacks.
#[cfg(target_os = "macos")]
const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;

/// The file that descriptor `fd` of process `pid` refers to.
#[cfg(target_os = "macos")]
fn id_of(pid: u32, fd: i32) -> Option<FileId> {
    let pid = libc::c_int::try_from(pid).ok()?;
    // SAFETY: the struct is plain data, for which all zeros is a valid value.
    let mut info: VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(size_of::<VnodeFdInfoWithPath>()).ok()?;
    // SAFETY: the buffer is a vnode_fdinfowithpath of `size` bytes, which is what the call may
    // write for PROC_PIDFDVNODEPATHINFO.
    let written = unsafe {
        libc::proc_pidfdinfo(
            pid,
            fd,
            PROC_PIDFDVNODEPATHINFO,
            (&raw mut info).cast::<libc::c_void>(),
            size,
        )
    };
    let stat = &info.pvip.vip_vi.vi_stat;
    (written == size).then(|| FileId {
        dev: u64::from(stat.vst_dev),
        ino: stat.vst_ino,
    })
}

#[cfg(target_os = "linux")]
fn id_of(pid: u32, fd: i32) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::metadata(format!("/proc/{pid}/fd/{fd}")).ok()?;
    Some(FileId {
        dev: meta.dev(),
        ino: meta.ino(),
    })
}
