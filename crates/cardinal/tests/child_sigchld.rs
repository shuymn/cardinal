//! `os::child` in a process that ignores SIGCHLD, which makes the kernel reap children
//! itself: Linux when SIGCHLD was ignored across `exec`, macOS when the process sets it.
//! `os::child` puts SIGCHLD back to its default before its first child, so the child stays
//! unreaped, and its process group pinned, until its run reaps it (`docs/architecture.md`
//! section 8.1).
//!
//! This binary holds this one test: the SIGCHLD disposition belongs to the whole process,
//! and the reset happens once per process.

#![cfg(unix)]

use std::time::{Duration, Instant};

use cardinal::os::child::{Limits, Output, run};
use cardinal_test_support::Scope;

/// `shell -c script` in the root of `scope`, with the scope's environment.
fn shell(shell: &str, scope: &Scope, script: &str) -> Output {
    let limits = Limits {
        deadline: Instant::now() + Duration::from_secs(10),
        max_output: 1024,
    };
    let args = ["-c".into(), script.into()];
    run(shell, &args, &scope.env(), scope.root(), limits).unwrap()
}

#[test]
fn a_child_stays_unreaped_until_its_run_reaps_it_when_sigchld_is_ignored() {
    // SAFETY: `signal` takes no pointers; no other thread of this process starts children.
    unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) };
    let scope = Scope::new();

    // The status comes from the run's own wait: the kernel did not reap the child first.
    let out = shell("sh", &scope, "exit 3");
    assert_eq!(out.code, Some(3), "{}", out.stderr_line());

    // The leader stayed a zombie until the group was killed, so the kill reached the
    // descendant it left in its group.
    let pid_file = scope.path("pid");
    let script = format!(
        "sleep 30 >/dev/null 2>&1 & echo $! > '{}'; exit 0",
        pid_file.display()
    );
    let out = shell("sh", &scope, &script);
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
    let out = shell("bash", &scope, "trap -p CHLD");
    assert_eq!(out.stdout, b"", "{}", String::from_utf8_lossy(&out.stdout));
}
