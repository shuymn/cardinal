//! Helper processes: the test binary re-runs itself in a helper mode, so tests can start
//! real processes on every OS without shell scripts. A test crate that uses helpers defines
//!
//! ```ignore
//! #[test]
//! fn helper() {
//!     support::helper::run_if_requested();
//! }
//! ```
//!
//! Every process a test starts is killed by its PID in a drop guard.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const MODE: &str = "CARDINAL_TEST_HELPER";
const ARGS: &str = "CARDINAL_TEST_HELPER_ARGS";

/// How long a helper sleeps before exiting on its own, so a crashed test leaks nothing for
/// long.
const LIFETIME: Duration = Duration::from_secs(60);

/// What a helper process does.
#[derive(Clone, Debug)]
pub enum Mode {
    /// Sleep, then exit.
    Sleep,
    /// Exit with status 0 after half a second, long enough to be seen running.
    Exit,
    /// Optionally take an exclusive lock on `lock`, start a sleeping grandchild, write its PID
    /// to `pid_file`, create `ready`, then sleep.
    Spawn {
        ready: PathBuf,
        pid_file: PathBuf,
        lock: Option<PathBuf>,
    },
    /// Call `cardinal::testpoint::hit(point)` and exit 0, or exit 3 on an injected failure.
    Testpoint { point: String },
    /// Append one record with `data.seq = seq` to the state dir `state` through a
    /// `Recorder` with `max_bytes` and `keep`; exit 3 if the append fails.
    Append {
        state: PathBuf,
        seq: u64,
        max_bytes: u64,
        keep: u32,
    },
}

/// Runs the helper mode requested through the environment, then exits; returns at once in a
/// normal test run.
pub fn run_if_requested() {
    let Ok(mode) = std::env::var(MODE) else {
        return;
    };
    let args: Vec<String> = std::env::var(ARGS)
        .unwrap_or_default()
        .split('\n')
        .map(str::to_owned)
        .collect();
    match mode.as_str() {
        "sleep" => std::thread::sleep(LIFETIME),
        "exit" => std::thread::sleep(Duration::from_millis(500)),
        "spawn" => {
            let lock = (!args[2].is_empty()).then(|| {
                cardinal::state::lock(
                    Path::new(&args[2]),
                    cardinal::state::LockMode::Exclusive,
                    Instant::now() + Duration::from_secs(5),
                )
                .expect("take the lock")
            });
            // Killed only if this helper lives out its lifetime; a test that kills the
            // helper kills the grandchild by PID.
            let grandchild = spawn(&Mode::Sleep, None);
            std::fs::write(&args[1], grandchild.id().to_string()).expect("write the pid");
            std::fs::write(&args[0], b"").expect("write the ready file");
            std::thread::sleep(LIFETIME);
            drop((lock, grandchild));
        }
        "testpoint" => {
            let point: &'static str = Box::leak(args[0].clone().into_boxed_str());
            if cardinal::testpoint::hit(point).is_err() {
                std::process::exit(3);
            }
        }
        "append" => {
            let state =
                cardinal::state::StateDir::open(Path::new(&args[0])).expect("open the state dir");
            let recorder = cardinal::record::Recorder::new(
                &state,
                args[2].parse().expect("max_bytes"),
                args[3].parse().expect("keep"),
            );
            let mut record = recorder.record(cardinal::contract::RecordKind::JobFinished);
            let seq: u64 = args[1].parse().expect("seq");
            record.data.insert("seq".into(), seq.into());
            if recorder.append(&record).is_err() {
                std::process::exit(3);
            }
        }
        other => panic!("unknown helper mode {other}"),
    }
    std::process::exit(0);
}

/// A command that runs this test binary in `mode`.
pub fn command(mode: &Mode) -> Command {
    let exe = std::env::current_exe().expect("current test binary");
    let mut command = Command::new(exe);
    command
        .args(["helper", "--exact", "--nocapture", "--test-threads=1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    let (name, args): (&str, Vec<String>) = match mode {
        Mode::Sleep => ("sleep", vec![]),
        Mode::Exit => ("exit", vec![]),
        Mode::Spawn {
            ready,
            pid_file,
            lock,
        } => (
            "spawn",
            vec![
                ready.to_string_lossy().into_owned(),
                pid_file.to_string_lossy().into_owned(),
                lock.as_ref()
                    .map_or_else(String::new, |l| l.to_string_lossy().into_owned()),
            ],
        ),
        Mode::Testpoint { point } => ("testpoint", vec![point.clone()]),
        Mode::Append {
            state,
            seq,
            max_bytes,
            keep,
        } => (
            "append",
            vec![
                state.to_string_lossy().into_owned(),
                seq.to_string(),
                max_bytes.to_string(),
                keep.to_string(),
            ],
        ),
    };
    command.env(MODE, name).env(ARGS, args.join("\n"));
    command
}

/// Starts a helper in `cwd` (or this process's cwd).
pub fn spawn(mode: &Mode, cwd: Option<&Path>) -> ChildGuard {
    let mut command = command(mode);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    ChildGuard(Some(command.spawn().expect("start a helper process")))
}

/// Kills and reaps its child when dropped.
pub struct ChildGuard(Option<Child>);

impl ChildGuard {
    pub fn new(child: Child) -> Self {
        Self(Some(child))
    }

    pub fn id(&self) -> u32 {
        self.0.as_ref().expect("child").id()
    }

    pub fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("child")
    }

    /// Hands the child over without killing it.
    pub fn into_child(mut self) -> Child {
        self.0.take().expect("child")
    }

    /// Kills the child (SIGKILL on Unix, `TerminateProcess` on Windows) and reaps it.
    pub fn kill(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Kills a process that is not this process's child, by PID, when dropped.
pub struct PidGuard(pub u32);

impl Drop for PidGuard {
    fn drop(&mut self) {
        kill_pid(self.0);
    }
}

/// Kills `pid` (SIGKILL on Unix, `TerminateProcess` on Windows) if it exists.
pub fn kill_pid(pid: u32) {
    let Ok(mut system) = sysinfo::System::new() else {
        return;
    };
    let pid = sysinfo::Pid::from_u32(pid);
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    if let Some(process) = system.process(pid) {
        process.kill();
    }
}

/// Waits until `check` returns `Some`, polling every 10 ms; panics after `timeout`.
pub fn wait_for<T>(what: &str, timeout: Duration, mut check: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits until `path` exists.
pub fn wait_for_file(path: &Path, timeout: Duration) {
    wait_for(&path.display().to_string(), timeout, || {
        path.exists().then_some(())
    });
}
