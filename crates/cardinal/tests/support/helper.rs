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
//! The mode and its paths travel only as command-line arguments that [`command`] builds,
//! never as environment variables, so nothing a test run inherits can turn a test into a
//! helper or point a helper elsewhere. Helpers start through the spawn API, so the scope that
//! started one kills it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cardinal_test_support::{Child, Command, Scope};

/// Marks the arguments after libtest's `--` as a helper request. libtest reads them as name
/// filters, which with `--exact` match no test but `helper`.
const TAG: &str = "cardinal-test-helper:";

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
    /// Optionally take an exclusive lock on `lock`, start a sleeping grandchild in a process
    /// group of its own, write its PID to `pid_file`, create `ready`, then sleep.
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
    /// Write SIGCHLD's disposition as this process found it (`ignored`, `default`, or
    /// `handled`, read with `sigaction`) to `out`, then exit 3.
    #[cfg(unix)]
    ChildSignal { out: PathBuf },
    /// Exec `argv` as the leader of a new process group, without the helper's variables.
    #[cfg(unix)]
    LeadGroup { argv: Vec<String> },
}

/// Runs the helper mode named on the command line, then exits; returns at once in a normal
/// test run.
pub fn run_if_requested() {
    let mut request = std::env::args().filter_map(|arg| arg.strip_prefix(TAG).map(str::to_owned));
    let Some(mode) = request.next() else {
        return;
    };
    let args: Vec<String> = request.collect();
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
            // The grandchild has a group of its own, so killing this helper leaves it running;
            // a test that kills the helper kills the grandchild with a `PidGuard`. If this
            // helper lives out its lifetime, its scope kills the grandchild.
            let scope = Scope::new();
            let grandchild = spawn(&scope, &Mode::Sleep, scope.root());
            std::fs::write(&args[1], grandchild.id().to_string()).expect("write the pid");
            std::fs::write(&args[0], b"").expect("write the ready file");
            std::thread::sleep(LIFETIME);
            drop((lock, grandchild, scope));
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
        #[cfg(unix)]
        "child-signal" => {
            std::fs::write(&args[0], child_signal()).expect("write the disposition");
            std::process::exit(3);
        }
        #[cfg(unix)]
        "lead-group" => {
            use std::os::unix::process::CommandExt as _;
            let err = Command::new(&args[0])
                .args(&args[1..])
                .env_remove(MODE)
                .env_remove(ARGS)
                .process_group(0)
                .exec();
            panic!("exec {}: {err}", args[0]);
        }
        other => panic!("unknown helper mode {other}"),
    }
    std::process::exit(0);
}

/// SIGCHLD's disposition in this process, read without changing it.
#[cfg(unix)]
fn child_signal() -> &'static str {
    let mut current = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
    // SAFETY: without a new action, `sigaction` only writes the current one into `current`,
    // which is valid for writes.
    let read = unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), current.as_mut_ptr()) };
    assert_eq!(read, 0, "sigaction: {}", std::io::Error::last_os_error());
    // SAFETY: zero-initialized, a valid `sigaction`, and filled on success.
    match unsafe { current.assume_init() }.sa_sigaction {
        libc::SIG_IGN => "ignored",
        libc::SIG_DFL => "default",
        _ => "handled",
    }
}

/// A command that runs this test binary in `mode`, in `cwd` inside `scope`.
pub fn command(scope: &Scope, mode: &Mode, cwd: &Path) -> Command {
    let exe = std::env::current_exe().expect("current test binary");
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
        #[cfg(unix)]
        Mode::ChildSignal { out } => ("child-signal", vec![out.to_string_lossy().into_owned()]),
        #[cfg(unix)]
        Mode::LeadGroup { argv } => ("lead-group", argv.clone()),
    };
    let mut command = scope.command(exe, cwd);
    command
        .args(["helper", "--exact", "--nocapture", "--test-threads=1", "--"])
        .arg(format!("{TAG}{name}"))
        .args(args.iter().map(|arg| format!("{TAG}{arg}")))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
    command
}

/// Starts a helper in `cwd` inside `scope`.
pub fn spawn(scope: &Scope, mode: &Mode, cwd: &Path) -> Child {
    command(scope, mode, cwd)
        .spawn()
        .expect("start a helper process")
}
