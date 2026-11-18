//! Windows process facts sysinfo does not report, and a file's link count, which std's
//! metadata does not expose on stable.

use std::io;
use std::path::Path;

use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    GetFileInformationByHandle,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

/// Whether `pid` names a process that has exited. Windows keeps an exited process in the
/// process table, with no threads, until the last handle to it is closed, and sysinfo lists
/// it as running. `false` when the process cannot be opened.
pub(super) fn has_exited(pid: u32) -> bool {
    // SAFETY: `OpenProcess` takes no pointers and returns a null handle on failure.
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return false;
    }
    // SAFETY: `handle` was opened above with `PROCESS_SYNCHRONIZE`; a zero timeout returns at
    // once.
    let signaled = unsafe { WaitForSingleObject(handle, 0) } == WAIT_OBJECT_0;
    // SAFETY: `handle` is open and is closed exactly once, here.
    unsafe { CloseHandle(handle) };
    signaled
}

/// The file index and the link count of the entry at `path`, not following a final reparse
/// point: `GetFileInformationByHandle` on a handle opened with no access rights, as
/// `std::fs::symlink_metadata` opens one.
pub(super) fn file_links(path: &Path) -> io::Result<(u64, u64)> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    let file = std::fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `file` stays open for the call, and `info` is a writable
    // `BY_HANDLE_FILE_INFORMATION`.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &raw mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    Ok((index, u64::from(info.nNumberOfLinks)))
}
