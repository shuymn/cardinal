//! Starting a cardinal process that outlives its starter (`docs/architecture.md` sections 5.2
//! and 10.5): the runner's job-end check starts `cardinal reconcile`, and a pass starts
//! `cardinal arbiter run`, and neither waits.
//!
//! The process gets null standard streams, so it holds none of its starter's output, and
//! starts a session of its own (Unix) or a process group without a console (Windows), so the
//! starter's terminal signals and job control do not reach it. It inherits the environment
//! and the working directory; every descriptor cardinal opens is close-on-exec. The starter
//! never waits for it: on Unix it stays a zombie until the starter exits, which happens right
//! after.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

use super::Unavailable;

/// Starts `program` with `args`; returns its PID once the spawn has returned. A spawn that
/// fails started nothing.
pub fn spawn(program: &Path, args: &[OsString]) -> Result<u32, Unavailable> {
    #[expect(
        clippy::disallowed_methods,
        reason = "os::spawn_detached's spawn site: a cardinal command that runs on its own"
    )]
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut command);
    #[expect(
        clippy::disallowed_methods,
        reason = "os::spawn_detached's spawn site: a cardinal command that runs on its own"
    )]
    let child = command
        .spawn()
        .map_err(|err| Unavailable(format!("{}: {err}", program.display())))?;
    // Dropping a `Child` neither waits for nor kills it.
    Ok(child.id())
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: `setsid` is async-signal-safe and touches no memory of the parent.
    unsafe {
        command.pre_exec(|| {
            rustix::process::setsid().map_err(std::io::Error::from)?;
            Ok(())
        });
    }
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn detach(_command: &mut Command) {}
