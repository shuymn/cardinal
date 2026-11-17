//! On Windows `os::child` ends the child's whole Job Object at the end of every run: a process
//! the child leaves behind goes with it after a normal exit, and one that holds the child's
//! output keeps the run from completing until its deadline (`docs/architecture.md`
//! section 8.1). The child is a helper that starts a sleeper and exits.

#![cfg(windows)]

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use cardinal::os::child::{ChildError, Limits, Output, git_env, run};
use cardinal_test_support::Scope;
use support::helper::{self, Mode};

#[test]
fn helper() {
    helper::run_if_requested();
}

/// What `watch` saw of a process the helper reports: its ID and start time once reported,
/// and when it was last seen alive.
#[derive(Clone, Copy, Debug)]
struct Seen {
    name: &'static str,
    process: Option<(u32, u64)>,
    alive_at: Option<Duration>,
}

impl std::fmt::Display for Seen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = self.name;
        match (self.process, self.alive_at) {
            (None, _) => write!(f, "{name} not started"),
            (Some((pid, _)), None) => write!(f, "{name} {pid} never seen alive"),
            (Some((pid, _)), Some(at)) => write!(f, "{name} {pid} last seen alive at {at:?}"),
        }
    }
}

/// Until `done` is set, notes when the helper and its sleeper were last seen alive, from
/// `start`.
fn watch(
    dir: PathBuf,
    start: Instant,
    done: Arc<AtomicBool>,
) -> std::thread::JoinHandle<[Seen; 2]> {
    std::thread::spawn(move || {
        let mut seen = ["helper", "sleeper"].map(|name| Seen {
            name,
            process: None,
            alive_at: None,
        });
        while !done.load(Ordering::SeqCst) {
            for seen in &mut seen {
                if seen.process.is_none() {
                    seen.process = helper::reported(&dir, seen.name);
                }
                if let Some((pid, start_s)) = seen.process
                    && cardinal::os::is_alive(pid, start_s)
                {
                    seen.alive_at = Some(start.elapsed());
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        seen
    })
}

/// Runs the helper through `os::child` in `dir`, within `timeout`, and returns the result, how
/// long the run took, and what `watch` saw, with every process the helper reported.
fn run_helper(
    dir: &Path,
    keep_output: bool,
    timeout: Duration,
) -> (Result<Output, ChildError>, Duration, [Seen; 2]) {
    let mode = Mode::Leave {
        dir: dir.to_owned(),
        keep_output,
    };
    let args: Vec<_> = helper::args(&mode).into_iter().map(Into::into).collect();
    let limits = Limits {
        deadline: Instant::now() + timeout,
        max_output: 1024 * 1024,
    };
    let done = Arc::default();
    let start = Instant::now();
    let watcher = watch(dir.to_owned(), start, Arc::clone(&done));
    let result = run(
        std::env::current_exe().unwrap(),
        &args,
        &git_env(std::env::vars_os()),
        dir,
        limits,
    );
    let returned = start.elapsed();
    done.store(true, Ordering::SeqCst);
    let mut seen = watcher.join().unwrap();
    for seen in &mut seen {
        seen.process = seen.process.or_else(|| helper::reported(dir, seen.name));
    }
    (result, returned, seen)
}

fn assert_gone(found: Option<(u32, u64)>) {
    let (pid, start_s) = found.expect("the sleeper started");
    let gone = (0..250).any(|_| {
        std::thread::sleep(Duration::from_millis(20));
        !cardinal::os::is_alive(pid, start_s)
    });
    assert!(gone, "sleeper {pid} was killed with the job");
}

/// After a normal exit the rest of the job goes: the helper leaves a sleeper that holds none
/// of the run's pipes, so the run completes while the sleeper runs, well before the deadline
/// and the minute the sleeper takes, and the job takes it along.
///
/// A failed run names the stage it stopped at: a helper that never started was not resumed or
/// never reached its body; one still alive at the deadline did not exit; one gone long before
/// the deadline exited while a process kept its output open.
#[test]
fn a_process_left_after_a_normal_exit_is_killed() {
    let scope = Scope::new();
    let (result, returned, seen) = run_helper(scope.root(), false, Duration::from_secs(20));
    let seen_text = seen.map(|s| s.to_string()).join(", ");
    let out = result.unwrap_or_else(|err| panic!("{err} after {returned:?}: {seen_text}"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.code, Some(0), "{stdout}{}", out.stderr_line());
    let sleeper = seen[1].process;
    assert!(
        sleeper.is_some(),
        "the helper left no sleeper: {seen_text}\n{stdout}"
    );
    assert_gone(sleeper);
}

/// A process the child leaves holding its output keeps the run from completing: the helper
/// exits at once and leaves a sleeper with its stdout and stderr, so the run ends in a timeout
/// at its deadline, not in success, and the job takes the sleeper along.
#[test]
fn a_process_left_holding_the_output_ends_in_a_timeout() {
    let timeout = Duration::from_secs(10);
    let scope = Scope::new();
    let (result, returned, [helper, sleeper]) = run_helper(scope.root(), true, timeout);
    let seen_text = format!("{helper}, {sleeper}");
    assert!(
        matches!(result, Err(ChildError::Timeout { .. })),
        "{result:?} after {returned:?}: {seen_text}"
    );
    assert!(
        returned < timeout + Duration::from_secs(5),
        "{returned:?}: {seen_text}"
    );
    assert!(
        helper.process.is_some()
            && helper
                .alive_at
                .is_none_or(|at| at + Duration::from_secs(1) < timeout),
        "the helper exited well before the deadline: {seen_text}"
    );
    assert_gone(sleeper.process);
}
