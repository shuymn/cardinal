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
//! started one kills it. A test crate with a helper mode of its own starts it as
//! [`Mode::Custom`] and runs it from its `helper` test, before [`run_if_requested`], when
//! [`custom_request`] returns its argument.

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
    /// Write this process's PID to `out`, then sleep: once `out` holds the PID, this process
    /// is past its `execve`.
    Pid { out: PathBuf },
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
    /// Write this process's process group ID to `out`.
    #[cfg(unix)]
    Group { out: PathBuf },
    /// Stand in for an agent: start a sleeping helper (a process the agent leaves running)
    /// and write its PID to `dir/background.pid`; run `cardinal hook claude <event>` as this
    /// process's child, or with `via_shell` as the child of `sh -c` as Claude Code runs it,
    /// with `dir/input.json` on stdin and the state and config dirs `state` and `config`;
    /// write its exit status to `dir/hook.done`, then sleep. With `stray` (Unix), first start
    /// a sleep in the helper's group that is not this process's descendant (a shell in that
    /// group starts it and exits) and write its PID to `dir/stray.pid`.
    Agent {
        dir: PathBuf,
        event: String,
        state: PathBuf,
        config: PathBuf,
        via_shell: bool,
        stray: bool,
    },
    /// Run `cmd /D /C exit 0` through `cardinal::os::child`, start a sleeping helper through a
    /// scope of this process, write its PID to `out`, then sleep: the sleeper is a process
    /// started outside `os::child`.
    #[cfg(windows)]
    Outside { out: PathBuf },
    /// Write this process's PID and start time to `dir/helper`, start a sleeping helper
    /// through a scope of this process, write its PID and start time to `dir/sleeper`, and
    /// exit, leaving the sleeper running. The sleeper gets this process's stdout and stderr
    /// with `keep_output`; otherwise it holds no handle of them.
    #[cfg(windows)]
    Leave { dir: PathBuf, keep_output: bool },
    /// A mode the test crate runs itself: its name, and one argument such as a JSON request
    /// (see [`custom_request`]).
    Custom { name: &'static str, arg: String },
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
        "pid" => {
            std::fs::write(&args[0], std::process::id().to_string()).expect("write the pid");
            std::thread::sleep(LIFETIME);
        }
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
        "group" => {
            // SAFETY: `getpgrp` cannot fail and touches no memory of this process.
            let group = unsafe { libc::getpgrp() };
            std::fs::write(&args[0], group.to_string()).expect("write the group");
        }
        "agent" => agent(
            Path::new(&args[0]),
            &args[1],
            Path::new(&args[2]),
            Path::new(&args[3]),
            args[4] == "via-shell",
            args[5] == "stray",
        ),
        #[cfg(windows)]
        "outside" => {
            let limits = cardinal::os::child::Limits {
                deadline: Instant::now() + Duration::from_secs(30),
                max_output: 1024,
            };
            let env = cardinal::os::child::git_env(std::env::vars_os());
            let cwd = std::env::current_dir().expect("the working directory");
            let exit = ["/D".into(), "/C".into(), "exit 0".into()];
            let out = cardinal::os::child::run("cmd", &exit, &env, &cwd, limits)
                .expect("run a child through os::child");
            assert_eq!(out.code, Some(0), "{}", out.stderr_line());
            let scope = Scope::new();
            let sleeper = spawn(&scope, &Mode::Sleep, scope.root());
            std::fs::write(&args[0], sleeper.id().to_string()).expect("write the pid");
            std::thread::sleep(LIFETIME);
            drop((sleeper, scope));
        }
        #[cfg(windows)]
        "leave" => leave(Path::new(&args[0]), args[1] == "keep-output"),
        other => panic!("unknown helper mode {other}"),
    }
    std::process::exit(0);
}

/// The `agent` mode: see [`Mode::Agent`].
fn agent(dir: &Path, event: &str, state: &Path, config: &Path, via_shell: bool, stray: bool) {
    let scope = Scope::new();
    let background = spawn(&scope, &Mode::Sleep, scope.root());
    std::fs::write(dir.join("background.pid"), background.id().to_string()).expect("write the pid");
    #[cfg(unix)]
    if stray {
        let status = scope
            .shell(
                &format!(
                    "sleep 600 </dev/null >/dev/null 2>&1 & echo $! > '{}'",
                    dir.join("stray.pid").display()
                ),
                scope.root(),
            )
            .join_group(&background)
            .status()
            .expect("start the stray process");
        assert!(status.success(), "the stray process's shell failed");
    }
    #[cfg(not(unix))]
    let _ = stray;
    let input = dir.join("input.json");
    let cardinal = env!("CARGO_BIN_EXE_cardinal");
    let mut hook = if via_shell {
        // `; true` keeps the shell alive as the hook's parent.
        scope.shell(
            &format!(
                "'{cardinal}' hook claude {event} < '{}'; true",
                input.display()
            ),
            scope.root(),
        )
    } else {
        let mut hook = scope.command(cardinal, scope.root());
        hook.args(["hook", "claude", event])
            .stdin(std::fs::File::open(&input).expect("open the input"));
        hook
    };
    let status = hook
        .env("CARDINAL_STATE_DIR", state)
        .env("CARDINAL_CONFIG_DIR", config)
        .stdout(std::process::Stdio::null())
        .status()
        .expect("run the hook");
    let written = dir.join("hook.tmp");
    std::fs::write(&written, status.code().unwrap_or(-1).to_string()).expect("write the status");
    std::fs::rename(&written, dir.join("hook.done")).expect("publish the status");
    std::thread::sleep(LIFETIME);
    drop((background, scope));
}

/// The `leave` mode: see [`Mode::Leave`].
#[cfg(windows)]
fn leave(dir: &Path, keep_output: bool) {
    use std::os::windows::io::AsRawHandle as _;
    use std::process::Stdio;

    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

    report(dir, "helper", std::process::id());
    if !keep_output {
        // A child started to inherit handles gets every inheritable one, not only its own
        // standard streams, so stdout and stderr stop being inheritable first.
        for handle in [
            std::io::stdout().as_raw_handle(),
            std::io::stderr().as_raw_handle(),
        ] {
            // SAFETY: the handle is this process's stdout or stderr, open while it runs.
            let set = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
            assert_ne!(set, 0, "{}", std::io::Error::last_os_error());
        }
    }
    let scope = Scope::new();
    let mut sleeper = command(&scope, &Mode::Sleep, scope.root());
    if keep_output {
        sleeper.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    } else {
        sleeper.stderr(Stdio::null());
    }
    let sleeper = sleeper.spawn().expect("start the sleeper");
    report(dir, "sleeper", sleeper.id());
    // Left running on purpose: the scope's job does not kill its processes when its handle
    // closes, so the sleeper outlives this process, in the job of whatever started it.
    std::mem::forget(sleeper);
    std::mem::forget(scope);
}

/// Writes `pid`'s ID and start time to `dir/name`, atomically.
#[cfg(windows)]
fn report(dir: &Path, name: &str, pid: u32) {
    let start_s = cardinal::os::start_time_s(pid).expect("the process is listed");
    let written = dir.join(format!("{name}.tmp"));
    std::fs::write(&written, format!("{pid} {start_s}")).expect("write the report");
    std::fs::rename(&written, dir.join(name)).expect("publish the report");
}

/// The process ID and start time [`Mode::Leave`] wrote to `dir/name`, once written.
#[cfg(windows)]
pub fn reported(dir: &Path, name: &str) -> Option<(u32, u64)> {
    let text = std::fs::read_to_string(dir.join(name)).ok()?;
    let (pid, start_s) = text.split_once(' ')?;
    Some((pid.parse().ok()?, start_s.parse().ok()?))
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

/// The argument of helper mode `name` when this process was started in it as
/// [`Mode::Custom`]; `None` in a normal test run and in any other mode.
pub fn custom_request(name: &str) -> Option<String> {
    let mut request = std::env::args().filter_map(|arg| arg.strip_prefix(TAG).map(str::to_owned));
    if request.next()? != name {
        return None;
    }
    Some(
        request
            .next()
            .expect("a custom helper mode takes one argument"),
    )
}

/// The arguments that make this test binary run `mode`, after the program.
pub fn args(mode: &Mode) -> Vec<String> {
    let (name, args): (&str, Vec<String>) = match mode {
        Mode::Sleep => ("sleep", vec![]),
        Mode::Pid { out } => ("pid", vec![out.to_string_lossy().into_owned()]),
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
        Mode::Group { out } => ("group", vec![out.to_string_lossy().into_owned()]),
        Mode::Agent {
            dir,
            event,
            state,
            config,
            via_shell,
            stray,
        } => (
            "agent",
            vec![
                dir.to_string_lossy().into_owned(),
                event.clone(),
                state.to_string_lossy().into_owned(),
                config.to_string_lossy().into_owned(),
                if *via_shell { "via-shell" } else { "" }.to_owned(),
                if *stray { "stray" } else { "" }.to_owned(),
            ],
        ),
        #[cfg(windows)]
        Mode::Outside { out } => ("outside", vec![out.to_string_lossy().into_owned()]),
        #[cfg(windows)]
        Mode::Leave { dir, keep_output } => (
            "leave",
            vec![
                dir.to_string_lossy().into_owned(),
                if *keep_output { "keep-output" } else { "" }.to_owned(),
            ],
        ),
        Mode::Custom { name, arg } => (name, vec![arg.clone()]),
    };
    ["helper", "--exact", "--nocapture", "--test-threads=1", "--"]
        .into_iter()
        .map(str::to_owned)
        .chain(std::iter::once(format!("{TAG}{name}")))
        .chain(args.iter().map(|arg| format!("{TAG}{arg}")))
        .collect()
}

/// A command that runs this test binary in `mode`, in `cwd` inside `scope`.
pub fn command(scope: &Scope, mode: &Mode, cwd: &Path) -> Command {
    let exe = std::env::current_exe().expect("current test binary");
    let mut command = scope.command(exe, cwd);
    command
        .args(args(mode))
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
