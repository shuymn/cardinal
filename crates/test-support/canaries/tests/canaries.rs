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

/// Leaves a child running in a session of its own.
#[cfg(unix)]
#[test]
fn leaves_setsid_process() {
    use std::os::unix::process::CommandExt;
    unsafe extern "C" {
        fn setsid() -> i32;
    }
    let mut command = sleeper();
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
