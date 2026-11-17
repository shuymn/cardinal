//! Windows process facts sysinfo does not report.

use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
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
