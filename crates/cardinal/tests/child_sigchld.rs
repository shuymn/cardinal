//! `os::child` in a process that ignores SIGCHLD, which makes the kernel reap children
//! itself: Linux when SIGCHLD was ignored across `exec`, macOS when the process sets it.
//! `os::child` puts SIGCHLD back to its default before its first child, so the child stays
//! unreaped, and its process group pinned, until its run reaps it (`docs/architecture.md`
//! section 8.1).
//!
//! This binary holds this one test: the SIGCHLD disposition belongs to the whole process,
//! and the reset happens once per process.

#![cfg(unix)]

use std::ffi::OsString;
use std::time::{Duration, Instant};

use cardinal::os::child::{Limits, git_env, run};

/// `shell -c script` in `dir`, with only `PATH` from this process and `HOME` in `dir`.
fn shell(shell: &str, dir: &std::path::Path, script: &str) -> cardinal::os::child::Output {
    let mut inherited: Vec<(OsString, OsString)> =
        std::env::vars_os().filter(|(k, _)| k == "PATH").collect();
    inherited.push(("HOME".into(), dir.into()));
    let limits = Limits {
        deadline: Instant::now() + Duration::from_secs(10),
        max_output: 1024,
    };
    run(
        shell,
        &["-c".into(), script.into()],
        &git_env(inherited),
        dir,
        limits,
    )
    .unwrap()
}

#[test]
fn a_child_stays_unreaped_until_its_run_reaps_it_when_sigchld_is_ignored() {
    // SAFETY: `signal` takes no pointers; no other thread of this process starts children.
    unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) };
    let dir = tempfile::tempdir().unwrap();

    // The status comes from the run's own wait: the kernel did not reap the child first.
    let out = shell("sh", dir.path(), "exit 3");
    assert_eq!(out.code, Some(3), "{}", out.stderr_line());

    // The leader stayed a zombie until the group was killed, so the kill reached the
    // descendant it left in its group.
    let pid_file = dir.path().join("pid");
    let script = format!(
        "sleep 30 >/dev/null 2>&1 & echo $! > '{}'; exit 0",
        pid_file.display()
    );
    let out = shell("sh", dir.path(), &script);
    assert_eq!(out.code, Some(0), "{}", out.stderr_line());
    let pid: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let gone = (0..250).any(|_| {
        std::thread::sleep(Duration::from_millis(20));
        cardinal::os::start_time_s(pid).is_none()
    });
    assert!(gone, "the descendant {pid} was killed with the group");

    // Children start with SIGCHLD at its default: bash lists a signal ignored on entry.
    let out = shell("bash", dir.path(), "trap -p CHLD");
    assert_eq!(out.stdout, b"", "{}", String::from_utf8_lossy(&out.stdout));
}
