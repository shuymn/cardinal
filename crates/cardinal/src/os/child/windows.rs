//! The Windows Job Object that ends this process's children with it.

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Puts this process in a new job that kills its members when its last handle closes, and
/// keeps that handle open, unshared, for the life of the process. When the process ends, by
/// any means, the handle closes and every process started after this call goes too; each
/// child's own job is nested in this one. Returns whether the job is in place.
pub(super) fn contain_self() -> bool {
    // SAFETY: null attributes and name create an unnamed job whose handle is not inherited.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return false;
    }
    // SAFETY: the structure is plain data, for which all zeros is valid.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let size = u32::try_from(std::mem::size_of_val(&info)).unwrap_or(u32::MAX);
    // SAFETY: `job` is a job handle this function owns; `info` is valid for `size` bytes.
    let set = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            size,
        )
    };
    // SAFETY: `job` is valid, and the current-process pseudo handle needs no closing.
    if set == 0 || unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) } == 0 {
        // SAFETY: `job` is owned here and not used afterwards.
        unsafe { CloseHandle(job) };
        return false;
    }
    // The handle stays open on purpose: closing it would kill this process.
    true
}
