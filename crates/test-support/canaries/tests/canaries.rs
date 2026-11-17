//! Careless tests. Each one does what the harness must stop or catch; `scripts/test
//! --self-check` runs them one at a time and checks the outcome.

use std::process::{Command, Stdio};

/// Outside the per-run root: the checkout's `target/`.
const ESCAPE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../target/canary-escape");

/// Writes outside the per-run root. Confined, the write fails and so does the test.
#[test]
fn writes_outside_root() {
    std::fs::write(ESCAPE, b"escaped\n").expect("write outside the per-run root");
}

/// Sees none of the variables the launcher must drop.
#[test]
fn environment_is_clean() {
    for name in [
        "BASH_ENV",
        "ENV",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
    ] {
        assert_eq!(std::env::var_os(name), None, "{name} reached a test");
    }
    let home = std::env::var_os("HOME").expect("HOME is set");
    let tmp = std::env::temp_dir();
    assert!(
        std::path::Path::new(&home).starts_with(tmp.parent().expect("temp dir has a parent")),
        "HOME {} is outside the per-run root",
        std::path::Path::new(&home).display()
    );
}

fn sleeper() -> Command {
    let mut command = if cfg!(windows) {
        let mut command = Command::new("ping");
        command.args(["-n", "120", "127.0.0.1"]);
        command
    } else {
        let mut command = Command::new("sleep");
        command.arg("120");
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Leaves a child running with its output closed, which nextest's leak check cannot see.
#[test]
fn leaves_process() {
    let child = sleeper().spawn().expect("start a child");
    std::mem::forget(child);
}

#[cfg(unix)]
mod unix {
    use std::os::unix::process::CommandExt;

    unsafe extern "C" {
        fn setsid() -> i32;
        fn fork() -> i32;
        fn close(fd: i32) -> i32;
        fn _exit(status: i32) -> !;
    }

    /// Closes every descriptor a child could have inherited, stdio included, as a daemon
    /// does (and, from 3 up, `subprocess.Popen` by default).
    fn close_all() {
        for fd in 0..1024 {
            // SAFETY: closing a descriptor touches no memory; one that is not open fails.
            unsafe { close(fd) };
        }
    }

    /// Leaves a child running in a session of its own.
    #[test]
    fn leaves_setsid_process() {
        let mut command = super::sleeper();
        // SAFETY: setsid is async-signal-safe and touches no memory of this process.
        unsafe {
            command.pre_exec(|| {
                setsid();
                Ok(())
            });
        }
        let child = command.spawn().expect("start a child");
        std::mem::forget(child);
    }

    /// Leaves a child running that closed every descriptor it inherited.
    #[test]
    fn leaves_process_without_descriptors() {
        let mut command = super::sleeper();
        // SAFETY: close is async-signal-safe and touches no memory of this process.
        unsafe {
            command.pre_exec(|| {
                close_all();
                Ok(())
            });
        }
        let child = command.spawn().expect("start a child");
        std::mem::forget(child);
    }

    /// Leaves a daemon: forked twice, in a session of its own, every descriptor closed, its
    /// parent gone.
    #[test]
    fn leaves_daemon() {
        let mut command = super::sleeper();
        // SAFETY: fork, setsid, close, and _exit are async-signal-safe; the forked children
        // touch no memory shared with this process and leave only through exec or _exit.
        unsafe {
            command.pre_exec(|| {
                if fork() != 0 {
                    _exit(0);
                }
                setsid();
                if fork() != 0 {
                    _exit(0);
                }
                close_all();
                Ok(())
            });
        }
        let mut child = command.spawn().expect("start the daemon");
        child.wait().expect("the first fork exits");
    }
}
