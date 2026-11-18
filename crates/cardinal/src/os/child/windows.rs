//! The Job Object each Windows child runs in.

use std::io;
use std::os::windows::io::AsRawHandle as _;
use std::os::windows::process::{ChildExt as _, CommandExt as _};
use std::process::{Child, Command};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{CREATE_SUSPENDED, ResumeThread};

/// A Job Object that kills its processes when its last handle closes. The handle is not
/// inheritable and never duplicated, so this process holds the only one: when this process
/// ends, by any means, the processes in the job end with it. Dropping it closes the handle.
pub(super) struct Job(HANDLE);

impl Job {
    fn new() -> io::Result<Self> {
        // SAFETY: null attributes and name create an unnamed job whose handle is not inherited.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Self(handle);
        // SAFETY: the structure is plain data, for which all zeros is valid.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = u32::try_from(std::mem::size_of_val(&info)).unwrap_or(u32::MAX);
        // SAFETY: `job.0` is an open job handle; `info` is valid for `size` bytes.
        let set = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                size,
            )
        };
        if set == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    /// Kills every process in the job.
    pub(super) fn terminate(&self) -> io::Result<()> {
        // SAFETY: `self.0` is an open job handle.
        if unsafe { TerminateJobObject(self.0, 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: `self.0` is open and closed exactly once, here.
        unsafe { CloseHandle(self.0) };
    }
}

/// Starts `command` suspended, assigns it to a new [`Job`], then resumes it, so it starts
/// nothing outside the job. This process ending between the start and the assignment leaves
/// the child suspended, before it ran anything; starting it inside the job
/// (`PROC_THREAD_ATTRIBUTE_JOB_LIST`) would need a hand-written `CreateProcessW`.
pub(super) fn spawn(command: Command) -> io::Result<(Child, Job)> {
    spawn_with(command, enter)
}

/// Assigns the suspended `child` to `job`, then resumes it.
fn enter(job: &Job, child: &Child) -> io::Result<()> {
    // SAFETY: both handles are open: `job` and `child` own them for the call.
    if unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `child` owns its main thread's handle, open for the call.
    if unsafe { ResumeThread(child.main_thread_handle().as_raw_handle()) } == u32::MAX {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// [`spawn`], with `enter` taking the suspended child into the job and resuming it. When
/// `enter` fails, the child, which has run nothing outside the job, is killed and reaped.
fn spawn_with(
    mut command: Command,
    enter: impl FnOnce(&Job, &Child) -> io::Result<()>,
) -> io::Result<(Child, Job)> {
    let job = Job::new()?;
    command.creation_flags(CREATE_SUSPENDED);
    #[expect(
        clippy::disallowed_methods,
        reason = "os::child's spawn site: a bounded child in a constructed environment"
    )]
    let mut child = command.spawn()?;
    if let Err(err) = enter(&job, &child) {
        let _ = job.terminate();
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }
    Ok((child, job))
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::Duration;

    use cardinal_test_support::Scope;

    use super::{enter, spawn_with};

    /// A child that cannot enter its job is killed and reaped: before its assignment it does
    /// not stay behind suspended, and after it does not run on. `ping` would run for 30 s.
    #[test]
    fn a_child_that_cannot_enter_its_job_is_killed() {
        let scope = Scope::new();
        let env = super::super::git_env(std::env::vars_os());
        let args: [std::ffi::OsString; 3] = ["-n", "31", "127.0.0.1"].map(Into::into);
        for assigned in [false, true] {
            let command = super::super::command("ping".as_ref(), None, &args, &env, scope.root());
            let mut started = None;
            let err = spawn_with(command, |job, child| {
                started = crate::os::start_time_s(child.id()).map(|s| (child.id(), s));
                if assigned {
                    // Assigns and resumes; the failure stands for one after both.
                    enter(job, child)?;
                }
                Err(io::Error::other("injected"))
            })
            .map(drop)
            .unwrap_err();
            assert_eq!(err.to_string(), "injected", "assigned: {assigned}");
            let (pid, start_s) = started.expect("the child was listed");
            let gone = (0..250).any(|_| {
                std::thread::sleep(Duration::from_millis(20));
                !crate::os::is_alive(pid, start_s)
            });
            assert!(gone, "assigned: {assigned}: child {pid} was killed");
        }
    }
}
