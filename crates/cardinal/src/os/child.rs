//! Short-lived child processes whose output the caller needs: an environment the caller
//! constructs, an absolute deadline on every wait, output read on threads, and cleanup on
//! every early exit (`docs/architecture.md` sections 2.2 and 8.1).
//!
//! The environment is always the caller's choice. [`git_env`] and [`gh_env`] build the
//! constructed environments of section 8.1 from separate allowlists.
//!
//! No process cardinal starts here outlives what started it: the child's process group
//! (Unix) or Job Object (Windows) is killed before the child is reaped, after a normal exit
//! too. A terminated process takes its live children along. On Unix [`prepare`], or else the
//! first [`run`] in a process, installs handlers for SIGTERM, SIGINT, and SIGHUP that end the
//! process by the same signal (see `guard_termination`), so a process that handles those
//! signals itself must not use [`run`]; when that fails, no child starts, and this and every
//! later [`run`] in the process return [`ChildError::Setup`]. On Windows each child's job is
//! killed when its only handle, held by this process, closes; other processes this process
//! starts are not affected. On Unix the waits, group signals, and signal handling are those of
//! [`super::unix`]; this module adds only what bounds a child: the deadline, the readers, and
//! the cleanup.
//!
//! For short-lived callers only (a CLI command, a hook): a descendant that left the child's
//! process group (`setsid`) is not killed, and a reader thread blocked on a pipe it holds
//! open outlives a [`ChildError::Timeout`] until that descendant closes the pipe or the
//! process ends. A long-lived caller would collect such threads.

#[cfg(windows)]
mod windows;

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
#[cfg(unix)]
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

#[cfg(unix)]
use crate::os::unix;

/// The test harness's run marker (section 13.4), passed to every git and gh child so the
/// harness's sweep finds what such a child leaves running. Only the harness sets it, so
/// outside tests it is unset and passing it changes nothing.
const RUN_MARKER: &str = "CARDINAL_TEST_RUN";

/// Variables git children copy from the caller's environment (section 8.1). `LC_*` and
/// [`RUN_MARKER`] are copied too.
const GIT_INHERITED: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "TMPDIR",
    "XDG_CONFIG_HOME",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_HOST",
    "GH_CONFIG_DIR",
    "SSH_AUTH_SOCK",
    RUN_MARKER,
];

/// Variables gh children copy: where gh keeps its configuration, the tokens it authenticates
/// with, and [`RUN_MARKER`]. `GH_HOST` and `GH_REPO` are left out, because they would point
/// gh at another host or repository than the `--repo HOST/OWNER/NAME` observation passes.
const GH_INHERITED: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "TMPDIR",
    "XDG_CONFIG_HOME",
    "GH_CONFIG_DIR",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    RUN_MARKER,
];

/// Variables Windows programs need, copied on Windows only.
const INHERITED_WINDOWS: &[&str] = &[
    "SystemRoot",
    "SystemDrive",
    "ComSpec",
    "PATHEXT",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "TEMP",
    "TMP",
];

/// Variables every git and gh child gets: no optional locks, no lazy fetch of a partial
/// clone's missing objects (git 2.45 and later), no prompts, no colors.
const ADDED: &[(&str, &str)] = &[
    ("GIT_OPTIONAL_LOCKS", "0"),
    ("GIT_NO_LAZY_FETCH", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GH_PROMPT_DISABLED", "1"),
    ("GH_NO_UPDATE_NOTIFIER", "1"),
    ("NO_COLOR", "1"),
];

/// The environment of a git child, built from `inherited`: only the allowlisted variables
/// pass, so `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, and every other
/// repository-location variable of the caller never reach git, and `LC_ALL` is `C`.
pub fn git_env(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    let mut env = build_env(inherited, GIT_INHERITED);
    // git's messages in one locale, since a read compares one of them (section 8.1).
    env.retain(|(name, _)| !same_name(name, "LC_ALL"));
    env.push(("LC_ALL".into(), "C".into()));
    env
}

/// The environment of a gh child, built from `inherited` like [`git_env`] with the gh
/// allowlist.
pub fn gh_env(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    build_env(inherited, GH_INHERITED)
}

fn build_env(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    allowed: &[&str],
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = inherited
        .into_iter()
        .filter(|(name, _)| passes(name, allowed))
        .collect();
    env.retain(|(name, _)| !ADDED.iter().any(|(added, _)| same_name(name, added)));
    env.extend(ADDED.iter().map(|(k, v)| ((*k).into(), (*v).into())));
    env
}

fn passes(name: &OsStr, allowed: &[&str]) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    allowed.iter().any(|keep| same_name(name, keep))
        || (cfg!(windows) && INHERITED_WINDOWS.iter().any(|keep| same_name(name, keep)))
        || name.starts_with("LC_")
}

/// Variable names compare case-insensitively on Windows, exactly elsewhere.
fn same_name(name: impl AsRef<OsStr>, other: &str) -> bool {
    let name = name.as_ref();
    if cfg!(windows) {
        name.eq_ignore_ascii_case(other)
    } else {
        name == other
    }
}

/// The value of `name` in a constructed environment.
#[must_use]
pub fn lookup<'a>(env: &'a [(OsString, OsString)], name: &str) -> Option<&'a OsStr> {
    env.iter()
        .rev()
        .find(|(k, _)| same_name(k, name))
        .map(|(_, v)| v.as_os_str())
}

/// What a child that ran to completion within its deadline produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    /// The exit code; `None` when a signal ended the child.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    /// `self` if the child exited 0, else a message naming `what` and the first stderr line.
    pub fn success(self, what: &str) -> Result<Self, String> {
        match self.code {
            Some(0) => Ok(self),
            Some(code) => Err(format!("{what} exited with {code}: {}", self.stderr_line())),
            None => Err(format!(
                "{what} was ended by a signal: {}",
                self.stderr_line()
            )),
        }
    }

    /// The first line of stderr, for error messages.
    #[must_use]
    pub fn stderr_line(&self) -> String {
        let text = String::from_utf8_lossy(&self.stderr);
        let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        let mut line = line.trim().to_owned();
        line.truncate(line.floor_char_boundary(200));
        line
    }
}

/// Why a child produced no usable output. Every case makes the fact `unknown`.
#[derive(Debug, thiserror::Error)]
pub enum ChildError {
    #[error("cannot start {program}: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    /// This process could not make sure its children end with it, so no child was started.
    #[error("cannot start {program}: cannot set up signal handling: {source}")]
    Setup {
        program: String,
        source: std::io::Error,
    },
    #[error("{program} did not finish by its deadline")]
    Timeout { program: String },
    #[error("{program} wrote more than {limit} bytes to stdout or stderr")]
    OutputTooLarge { program: String, limit: u64 },
    #[error("{program}: {source}")]
    Io {
        program: String,
        source: std::io::Error,
    },
}

/// The bounds of one [`run`]: the absolute deadline every wait ends at, and the most bytes
/// read from each of stdout and stderr.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub deadline: Instant,
    pub max_output: u64,
}

/// What a reader thread read from one stream.
enum Stream {
    Read(Vec<u8>),
    TooLarge,
    Failed(std::io::Error),
}

/// Where the child's `execvp` would find `program`, a name without a `/`, when that is known
/// without trying it: the first directory of the `PATH` in `env` that holds an executable
/// regular file of that name, every directory before it absolute and without the name
/// (`ENOENT` or `ENOTDIR`). `None`, which leaves the name to `execvp`, when `program` has a
/// `/`, `env` has no `PATH`, no directory holds such a file, or a directory up to the file is
/// empty or relative (taken in the child's working directory), ends in `/` (to which `execvp`
/// adds another, as a script then sees in `$0`), or answers anything else: a
/// name `exec` refuses (`EACCES`, which `execvp` passes over to the next directory), a loop of
/// symbolic links (which glibc's `execvp` stops at and Apple's passes over), or another error.
///
/// A child started by such a path is spawned with `posix_spawn`. Started by a name that the
/// `PATH` of its own environment must resolve, std forks this process instead, and the forks
/// of a process with many threads and much memory, as an observation running git for several
/// worktrees at a time is, are slow and wait for each other in the kernel (section 8.1).
#[cfg(unix)]
fn on_path(program: &OsStr, env: &[(OsString, OsString)]) -> Option<std::path::PathBuf> {
    use rustix::fs::{Access, FileType};
    use rustix::io::Errno;
    use std::os::unix::ffi::OsStrExt;

    if program.is_empty() || program.as_bytes().contains(&b'/') {
        return None;
    }
    for dir in std::env::split_paths(lookup(env, "PATH")?) {
        if !dir.is_absolute() || dir.as_os_str().as_bytes().ends_with(b"/") {
            return None;
        }
        let candidate = dir.join(program);
        match rustix::fs::stat(&candidate) {
            Err(Errno::NOENT | Errno::NOTDIR) => {}
            Ok(st)
                if FileType::from_raw_mode(st.st_mode) == FileType::RegularFile
                    && rustix::fs::access(&candidate, Access::EXEC_OK).is_ok() =>
            {
                return Some(candidate);
            }
            _ => return None,
        }
    }
    None
}

/// The command that runs `program`, found at `found` when that is given (with `program` as its
/// `argv[0]`, as `execvp` passes it), with `args` and exactly `env` in `cwd`, stdin closed,
/// stdout and stderr piped.
fn command(
    program: &OsStr,
    found: Option<&Path>,
    args: &[OsString],
    env: &[(OsString, OsString)],
    cwd: &Path,
) -> Command {
    #[expect(
        clippy::disallowed_methods,
        reason = "os::child's spawn site: a bounded child in a constructed environment"
    )]
    let mut command = Command::new(found.map_or(program, Path::as_os_str));
    #[cfg(unix)]
    if found.is_some() {
        std::os::unix::process::CommandExt::arg0(&mut command, program);
    }
    command
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(cwd);
    command
}

/// Starts the child as the leader of its own process group.
#[cfg(unix)]
fn spawn(mut command: Command) -> std::io::Result<Reaper> {
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    #[expect(
        clippy::disallowed_methods,
        reason = "os::child's spawn site: a bounded child in a constructed environment"
    )]
    let child = command.spawn()?;
    Ok(Reaper {
        child,
        complete: false,
    })
}

/// Starts the child in a Job Object of its own, which every process it starts joins.
#[cfg(windows)]
fn spawn(command: Command) -> std::io::Result<Reaper> {
    let (child, job) = windows::spawn(command)?;
    Ok(Reaper {
        child,
        job,
        complete: false,
    })
}

/// The process groups of the children alive now (Unix). Held while a child is spawned and
/// added, while one is killed, reaped, and removed, and by the signal handler while it kills
/// them, so every group it names is pinned by an unreaped leader.
#[cfg(unix)]
static LIVE: Mutex<Vec<u32>> = Mutex::new(Vec::new());

#[cfg(unix)]
fn live() -> MutexGuard<'static, Vec<u32>> {
    LIVE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Sets up, now, what [`run`] needs before this process's first child. On Unix that is the
/// termination handling of `guard_termination`, set up best before the process starts any
/// thread other than the one calling this: a termination signal sent to the process while
/// the handlers go in is lost if it reaches a thread that does not block it. A command that
/// runs children calls this first. A failure is not lost: every [`run`] returns it.
pub fn prepare() {
    #[cfg(unix)]
    let _ = guard_termination();
}

/// Makes this process take its children along when it is terminated, once per process.
/// SIGCHLD goes to its default action first, so every child stays unreaped, and the process
/// group it leads pinned, until [`Reaper::finish`] reaps it, also when the caller ignored
/// SIGCHLD. Then SIGTERM, SIGINT, and SIGHUP (unless ignored) kill every live child's process
/// group and end the process by the same signal with its default action. A failure installs
/// no handler and is returned by this and every later call: signal-hook keeps a slot for a
/// signal it once took, so a second attempt would not install its handler again.
///
/// Both change process-wide signal state, which cardinal owns: it is a command line program,
/// and this library serves only its binary and tests.
#[cfg(unix)]
fn guard_termination() -> std::io::Result<()> {
    static GUARD: OnceLock<Result<(), String>> = OnceLock::new();
    GUARD
        .get_or_init(|| {
            unix::reset_child_signal();
            unix::watch_signals(
                &[unix::SIGTERM, unix::SIGINT, unix::SIGHUP],
                end_with_children,
            )
            .map_err(|err| err.to_string())
        })
        .clone()
        .map_err(std::io::Error::other)
}

/// Kills every live child's process group, then ends this process by `sig`.
#[cfg(unix)]
fn end_with_children(sig: i32) {
    // Held until the process ends: no child starts or is reaped after this.
    let live = live();
    for &pgid in live.iter() {
        // Pinned: the group's leader is an unreaped child while it is listed.
        let _ = unix::signal_group(pgid, unix::SIGKILL);
    }
    unix::die_by_signal(sig)
}

/// Owns the child until it is reaped. On every exit from [`run`], complete or early
/// (deadline, output over the limit, read error, a reader thread that could not start), it
/// kills whatever is left of the child's process group (Unix) or Job Object (Windows)
/// before it reaps the child, so no process cardinal started outlives the run.
struct Reaper {
    child: Child,
    /// The job the child runs in, which ends with this process.
    #[cfg(windows)]
    job: windows::Job,
    complete: bool,
}

impl Reaper {
    /// Whether the child has ended. On Unix the child stays unreaped, so the ID of the
    /// process group it leads cannot pass to another group.
    fn ended(&mut self) -> std::io::Result<bool> {
        #[cfg(unix)]
        {
            unix::wait_unreaped(self.child.id(), unix::Until::Now)
                .map(|state| state == unix::ChildState::Ended)
        }
        #[cfg(windows)]
        {
            Ok(self.child.try_wait()?.is_some())
        }
    }

    /// Kills the child and everything left in its process group or job, then reaps the
    /// child. A child that had already exited keeps its own status.
    fn finish(&mut self) -> std::io::Result<ExitStatus> {
        self.complete = true;
        #[cfg(unix)]
        {
            let mut live = live();
            let id = self.child.id();
            // The group is the child's own (`process_group(0)`), and its ID is pinned until
            // the child is reaped below.
            let _ = unix::signal_group(id, unix::SIGKILL);
            let _ = self.child.kill();
            let status = self.child.wait();
            live.retain(|&pgid| pgid != id);
            status
        }
        #[cfg(windows)]
        {
            let _ = self.job.terminate();
            self.child.wait()
        }
    }
}

impl Drop for Reaper {
    fn drop(&mut self) {
        if !self.complete {
            let _ = self.finish();
        }
    }
}

/// Reads `stream` to its end on a thread of its own, stopping one byte past `max`, and
/// sends `index` with what it read.
fn start_reader<R: Read + Send + 'static>(
    stream: R,
    index: usize,
    max: u64,
    sender: mpsc::Sender<(usize, Stream)>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("child-output".into())
        .spawn(move || {
            let mut bytes = Vec::new();
            let read = match stream.take(max.saturating_add(1)).read_to_end(&mut bytes) {
                Ok(_) if bytes.len() as u64 > max => Stream::TooLarge,
                Ok(_) => Stream::Read(bytes),
                Err(err) => Stream::Failed(err),
            };
            let _ = sender.send((index, read));
        })?;
    Ok(())
}

/// Runs `program` with `args` and exactly `env` in `cwd`, stdin closed, within `limits`.
///
/// Output is read on two threads, each stopping one byte past `limits.max_output`. On Unix
/// the child leads its own process group; on Windows it runs in its own Job Object. The run
/// completes once the child has exited and both streams are at end of file; whatever is
/// left in the group or job is then killed before the child is reaped. On the deadline the
/// group or job is killed and the child reaped, and the result is
/// [`ChildError::Timeout`]; that includes a child that exited while a descendant still
/// holds its output open. A stream over the limit ends the same way at once, with
/// [`ChildError::OutputTooLarge`]. Once the group or job is gone the reader threads see end
/// of file and end; nothing waits for them.
pub fn run(
    program: impl AsRef<OsStr>,
    args: &[OsString],
    env: &[(OsString, OsString)],
    cwd: &Path,
    limits: Limits,
) -> Result<Output, ChildError> {
    #[cfg(unix)]
    let guard = guard_termination;
    // Each child's job ends with this process (`windows::Job`): nothing to set up.
    #[cfg(windows)]
    let guard = || Ok(());
    run_guarded(guard, program.as_ref(), args, env, cwd, limits)
}

/// [`run`], with `guard` run before the child is started.
fn run_guarded(
    guard: impl FnOnce() -> std::io::Result<()>,
    program: &OsStr,
    args: &[OsString],
    env: &[(OsString, OsString)],
    cwd: &Path,
    limits: Limits,
) -> Result<Output, ChildError> {
    let name = || program.to_string_lossy().into_owned();
    guard().map_err(|source| ChildError::Setup {
        program: name(),
        source,
    })?;
    #[cfg(unix)]
    let found = on_path(program, env);
    #[cfg(not(unix))]
    let found: Option<std::path::PathBuf> = None;
    #[cfg(unix)]
    let mut live = live();
    // Nothing starts once the deadline has passed, which the lookup and the wait for `LIVE`
    // can reach. The lookup has no deadline of its own, as `execvp`'s had none.
    let past = || Instant::now() >= limits.deadline;
    if past() {
        return Err(ChildError::Timeout { program: name() });
    }
    let started = match &found {
        // What `execvp` does beyond finding the file (`sh` running a script without a `#!`
        // line, the next directory tried when an `exec` fails) is left to it: the name is
        // started again the way it was before it was looked up, unless the deadline has
        // passed meanwhile.
        Some(path) => match spawn(command(program, Some(path), args, env, cwd)) {
            Err(_) if past() => return Err(ChildError::Timeout { program: name() }),
            Err(_) => spawn(command(program, None, args, env, cwd)),
            started => started,
        },
        None => spawn(command(program, None, args, env, cwd)),
    };
    let mut child = started.map_err(|source| ChildError::Spawn {
        program: name(),
        source,
    })?;
    #[cfg(unix)]
    {
        live.push(child.child.id());
        drop(live);
    }

    let io_err = |source| ChildError::Io {
        program: name(),
        source,
    };
    let (Some(stdout), Some(stderr)) = (child.child.stdout.take(), child.child.stderr.take())
    else {
        return Err(io_err(std::io::Error::other(
            "the child's output is not piped",
        )));
    };
    let (sender, receiver) = mpsc::channel();
    start_reader(stdout, 0, limits.max_output, sender.clone()).map_err(io_err)?;
    start_reader(stderr, 1, limits.max_output, sender).map_err(io_err)?;
    let mut pending = 2;
    let mut streams = [Vec::new(), Vec::new()];
    let mut ended = false;
    let mut delay = Duration::from_millis(1);
    loop {
        if !ended {
            ended = child.ended().map_err(io_err)?;
        }
        if ended && pending == 0 {
            let status = child.finish().map_err(io_err)?;
            let [stdout, stderr] = streams;
            return Ok(Output {
                code: status.code(),
                stdout,
                stderr,
            });
        }
        let now = Instant::now();
        if now >= limits.deadline {
            return Err(ChildError::Timeout { program: name() });
        }
        let wait = delay.min(limits.deadline - now);
        delay = (delay * 2).min(Duration::from_millis(20));
        if pending == 0 {
            std::thread::sleep(wait);
            continue;
        }
        match receiver.recv_timeout(wait) {
            Ok((index, Stream::Read(bytes))) => {
                streams[index] = bytes;
                pending -= 1;
            }
            Ok((_, Stream::TooLarge)) => {
                return Err(ChildError::OutputTooLarge {
                    program: name(),
                    limit: limits.max_output,
                });
            }
            Ok((_, Stream::Failed(err))) => return Err(io_err(err)),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io_err(std::io::Error::other(
                    "an output reader ended without a result",
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::time::{Duration, Instant};

    use cardinal_test_support::Scope;

    use super::{ChildError, Limits, git_env, lookup, run, run_guarded};

    fn pairs(items: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        items
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    #[test]
    fn environment_keeps_only_the_allowlist_and_adds_the_fixed_variables() {
        let env = git_env(pairs(&[
            ("PATH", "/bin"),
            ("HOME", "/h"),
            ("LC_ALL", "C"),
            ("GIT_DIR", "/decoy/.git"),
            ("GIT_WORK_TREE", "/decoy"),
            ("GIT_INDEX_FILE", "/decoy/.git/index"),
            ("GIT_CONFIG_PARAMETERS", "'core.bare'='true'"),
            ("GIT_CONFIG_GLOBAL", "/h/.gitconfig"),
            ("GIT_OPTIONAL_LOCKS", "1"),
            ("GH_TOKEN", "t"),
            ("CARDINAL_TEST_RUN", "run-1"),
            ("SECRET", "s"),
        ]));
        let names: Vec<_> = env.iter().map(|(k, _)| k.to_str().unwrap()).collect();
        for absent in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_CONFIG_PARAMETERS",
            "SECRET",
        ] {
            assert!(!names.contains(&absent), "{absent} passed: {names:?}");
        }
        for present in [
            "PATH",
            "HOME",
            "LC_ALL",
            "GIT_CONFIG_GLOBAL",
            "GH_TOKEN",
            "CARDINAL_TEST_RUN",
        ] {
            assert!(names.contains(&present), "{present} missing: {names:?}");
        }
        assert_eq!(lookup(&env, "GIT_OPTIONAL_LOCKS").unwrap(), "0");
        assert_eq!(
            names.iter().filter(|n| **n == "GIT_OPTIONAL_LOCKS").count(),
            1
        );
        assert_eq!(lookup(&env, "GIT_NO_LAZY_FETCH").unwrap(), "1");
        assert_eq!(lookup(&env, "GIT_TERMINAL_PROMPT").unwrap(), "0");
        assert_eq!(lookup(&env, "GH_PROMPT_DISABLED").unwrap(), "1");
    }

    #[test]
    fn gh_gets_its_own_allowlist() {
        let env = super::gh_env(pairs(&[
            ("PATH", "/bin"),
            ("GH_TOKEN", "t"),
            ("GH_ENTERPRISE_TOKEN", "e"),
            ("GH_HOST", "evil.example.com"),
            ("GH_REPO", "evil/repo"),
            ("GIT_DIR", "/decoy/.git"),
            ("CARDINAL_TEST_RUN", "run-1"),
        ]));
        let names: Vec<_> = env.iter().map(|(k, _)| k.to_str().unwrap()).collect();
        for absent in ["GH_HOST", "GH_REPO", "GIT_DIR"] {
            assert!(!names.contains(&absent), "{absent} passed: {names:?}");
        }
        for present in [
            "PATH",
            "GH_TOKEN",
            "GH_ENTERPRISE_TOKEN",
            "GH_PROMPT_DISABLED",
            "CARDINAL_TEST_RUN",
        ] {
            assert!(names.contains(&present), "{present} missing: {names:?}");
        }
    }

    /// `scope`'s environment with `PATH` set to `dirs`.
    #[cfg(unix)]
    fn with_path(scope: &Scope, dirs: &[std::path::PathBuf]) -> Vec<(OsString, OsString)> {
        let mut env = scope.env();
        env.retain(|(name, _)| name != "PATH");
        env.push(("PATH".into(), std::env::join_paths(dirs).unwrap()));
        env
    }

    /// The child's standard output, trimmed.
    #[cfg(unix)]
    fn text(out: &super::Output) -> String {
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// `<dir>/tool` in `scope`, a script that runs `body`, started once before a test runs it
    /// within a deadline (`Scope::started_script`).
    #[cfg(unix)]
    fn started_tool(scope: &Scope, dir: &str, body: &str) {
        let path = scope.path(dir).join("tool");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        scope.started_script(&path, body);
    }

    /// Writes `body` to `<dir>/tool` in `scope` with the permission bits `mode`.
    #[cfg(unix)]
    fn tool(scope: &Scope, dir: &str, body: &str, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        let path = scope.path(dir).join("tool");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A program named without a path runs from where the child's `execvp` would find it: the
    /// first directory of the child's `PATH` holding an executable file of that name, a file
    /// without the execute permission and a directory of that name passed over, and an empty
    /// directory standing for the child's working directory.
    #[cfg(unix)]
    #[test]
    fn a_name_runs_from_the_first_match_on_the_childs_path() {
        let scope = Scope::new();
        tool(&scope, "plain", "#!/bin/sh\necho plain\n", 0o644);
        std::fs::create_dir_all(scope.path("dir/tool")).unwrap();
        started_tool(&scope, "first", "echo \"$0\"\n");
        tool(&scope, "second", "#!/bin/sh\necho second\n", 0o755);
        started_tool(&scope, "cwd", "echo cwd\n");
        let dirs = ["missing", "plain", "dir", "first", "second"].map(|d| scope.path(d));
        let limits = || limits(Duration::from_secs(10), 1024);
        let out = run(
            "tool",
            &[],
            &with_path(&scope, &dirs),
            scope.root(),
            limits(),
        )
        .unwrap();
        assert_eq!(text(&out), scope.path("first/tool").display().to_string());
        let dirs = [std::path::PathBuf::new(), scope.path("second")];
        let out = run(
            "tool",
            &[],
            &with_path(&scope, &dirs),
            &scope.path("cwd"),
            limits(),
        );
        assert_eq!(text(&out.unwrap()), "cwd");
    }

    /// What `execvp` does beyond finding the file still happens: a script without a `#!` line
    /// is run by `sh`.
    #[cfg(unix)]
    #[test]
    fn a_found_script_without_an_interpreter_line_runs_in_sh() {
        let scope = Scope::new();
        tool(&scope, "bin", "echo \"run by sh\"\n", 0o755);
        // Started once before the deadline, as `Scope::started_script` does but without the
        // `#!` line it writes: `sh` execs the file and, refused, runs it as `execvp` does.
        let started = scope
            .command("sh", scope.root())
            .args(["-c", "exec \"$0\" > /dev/null"])
            .arg(scope.path("bin").join("tool"))
            .status()
            .expect("start the script once");
        assert!(started.success(), "{started}");
        let env = with_path(&scope, &[scope.path("bin")]);
        let out = run(
            "tool",
            &[],
            &env,
            scope.root(),
            limits(Duration::from_secs(10), 1024),
        )
        .unwrap();
        assert_eq!(text(&out), "run by sh");
    }

    /// A name is taken from the child's `PATH` only where `execvp` would surely find the same
    /// file: every directory up to it absolute and without the name. A relative or empty
    /// directory, a file `exec` refuses, a directory of that name, a directory that cannot be
    /// searched, or a loop of symbolic links on the way leaves the name to `execvp`.
    #[cfg(unix)]
    #[test]
    fn a_name_is_taken_from_the_path_only_where_execvp_would_surely_find_it() {
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;

        let scope = Scope::new();
        tool(&scope, "bin", "#!/bin/sh\n", 0o755);
        tool(&scope, "plain", "#!/bin/sh\n", 0o644);
        tool(&scope, "locked", "#!/bin/sh\n", 0o755);
        std::fs::create_dir_all(scope.path("dir/tool")).unwrap();
        std::fs::write(scope.path("file"), b"").unwrap();
        std::os::unix::fs::symlink("loop", scope.path("loop")).unwrap();
        let bin = scope.path("bin");
        let found = |before: PathBuf| {
            super::on_path("tool".as_ref(), &with_path(&scope, &[before, bin.clone()]))
        };
        assert_eq!(found(scope.path("missing")), Some(bin.join("tool")));
        assert_eq!(found(scope.path("file")), Some(bin.join("tool")));
        let taken: Vec<_> = [
            PathBuf::from("bin"),
            PathBuf::new(),
            scope.path("plain"),
            scope.path("dir"),
            scope.path("loop"),
        ]
        .into_iter()
        .filter_map(|before| Some((found(before.clone())?, before)))
        .collect();
        assert_eq!(taken, []);
        let locked = scope.path("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let through_locked = found(locked.clone());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Root searches any directory, and so does its `execvp`.
        let searchable = rustix::process::geteuid().is_root();
        assert_eq!(through_locked, searchable.then(|| locked.join("tool")));
    }

    /// A directory of the child's `PATH` that ends in `/` is left to `execvp`, which joins it to
    /// the name with another `/`: the path a script sees as `$0` keeps both.
    #[cfg(unix)]
    #[test]
    fn a_directory_ending_in_a_slash_is_left_to_execvp() {
        let scope = Scope::new();
        started_tool(&scope, "bin", "echo \"$0\"\n");
        let mut dir = scope.path("bin").into_os_string();
        dir.push("/");
        let env = with_path(&scope, &[dir.into()]);
        let out = run(
            "tool",
            &[],
            &env,
            scope.root(),
            limits(Duration::from_secs(10), 1024),
        )
        .unwrap();
        let doubled = format!("{}//tool", scope.path("bin").display());
        assert_eq!(text(&out), doubled);
    }

    /// A found file that fails to start is started again by name, so `execvp` goes on to the
    /// next directory, but not once the deadline has passed: that run ends as a timeout.
    #[cfg(unix)]
    #[test]
    fn a_failed_start_by_path_is_retried_by_name_only_before_the_deadline() {
        let scope = Scope::new();
        tool(&scope, "broken", "#!/nonexistent/interpreter\n", 0o755);
        started_tool(&scope, "next", "echo next\n");
        let env = with_path(&scope, &[scope.path("broken"), scope.path("next")]);
        let out = run(
            "tool",
            &[],
            &env,
            scope.root(),
            limits(Duration::from_secs(10), 1024),
        )
        .unwrap();
        assert_eq!(text(&out), "next");
        // Started again by name, it would fail to start: its `execvp` finds nothing else.
        let env = with_path(&scope, &[scope.path("broken")]);
        let passed = Limits {
            deadline: Instant::now(),
            max_output: 1024,
        };
        let err = run("tool", &[], &env, scope.root(), passed).unwrap_err();
        assert!(matches!(err, ChildError::Timeout { .. }), "{err}");
    }

    /// A run whose deadline has passed by the time it would start the child starts nothing
    /// and ends as a timeout: a program that cannot be started is not even tried.
    #[test]
    fn a_run_past_its_deadline_starts_nothing() {
        let scope = Scope::new();
        let passed = Limits {
            deadline: Instant::now(),
            max_output: 1024,
        };
        let err = run("cardinal-no-such-program", &[], &[], scope.root(), passed).unwrap_err();
        assert!(matches!(err, ChildError::Timeout { .. }), "{err}");
    }

    #[test]
    fn a_missing_program_is_a_spawn_error() {
        let scope = Scope::new();
        let err = run(
            "cardinal-no-such-program",
            &[],
            &[],
            scope.root(),
            limits(Duration::from_secs(5), 1024),
        )
        .unwrap_err();
        assert!(matches!(err, ChildError::Spawn { .. }), "{err}");
    }

    /// A failed guard fails the run before the child is started: a missing program is not
    /// even looked up.
    #[test]
    fn a_failed_guard_starts_no_child() {
        let scope = Scope::new();
        let err = run_guarded(
            || Err(std::io::Error::other("no reader thread")),
            "cardinal-no-such-program".as_ref(),
            &[],
            &[],
            scope.root(),
            limits(Duration::from_secs(5), 1024),
        )
        .unwrap_err();
        assert!(matches!(err, ChildError::Setup { .. }), "{err}");
        assert!(err.to_string().contains("no reader thread"), "{err}");
    }

    fn limits(timeout: Duration, max_output: u64) -> Limits {
        Limits {
            deadline: Instant::now() + timeout,
            max_output,
        }
    }

    /// `sh -c script` in the root of `scope`, with the scope's environment.
    #[cfg(unix)]
    fn sh(scope: &Scope, script: &str, limits: Limits) -> Result<super::Output, ChildError> {
        let args = ["-c".into(), script.into()];
        run("sh", &args, &scope.env(), scope.root(), limits)
    }

    /// The process ID a child wrote to `file`.
    #[cfg(unix)]
    fn written_pid(file: &std::path::Path) -> u32 {
        std::fs::read_to_string(file)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn output_and_status_are_returned() {
        let scope = Scope::new();
        let out = sh(
            &scope,
            "printf out; printf err >&2; exit 3",
            limits(Duration::from_secs(10), 1024),
        )
        .unwrap();
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stdout, b"out");
        assert_eq!(out.stderr_line(), "err");
    }

    #[cfg(unix)]
    #[test]
    fn the_deadline_kills_and_reaps_the_child() {
        let scope = Scope::new();
        let pid_file = scope.path("pid");
        let script = format!("echo $$ > '{}'; exec sleep 30", pid_file.display());
        let start = Instant::now();
        let err = sh(&scope, &script, limits(Duration::from_millis(300), 1024)).unwrap_err();
        assert!(matches!(err, ChildError::Timeout { .. }), "{err}");
        assert!(start.elapsed() < Duration::from_secs(5));
        let pid = written_pid(&pid_file);
        assert!(
            crate::os::start_time_s(pid).is_none(),
            "the child was killed and reaped"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_descendant_holding_the_output_open_ends_in_a_timeout_and_is_killed() {
        let scope = Scope::new();
        let pid_file = scope.path("pid");
        let script = format!(
            "sleep 30 & echo $! > '{}'; printf partial; exit 0",
            pid_file.display()
        );
        let start = Instant::now();
        let err = sh(&scope, &script, limits(Duration::from_millis(300), 1024)).unwrap_err();
        assert!(matches!(err, ChildError::Timeout { .. }), "{err}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        let pid = written_pid(&pid_file);
        let gone = (0..250).any(|_| {
            std::thread::sleep(Duration::from_millis(20));
            crate::os::start_time_s(pid).is_none()
        });
        assert!(gone, "the descendant {pid} was killed with the group");
    }

    /// A stream exactly at the limit is read; one byte more is refused, on stderr too.
    #[cfg(unix)]
    #[test]
    fn output_at_the_limit_passes_and_one_byte_more_fails() {
        let scope = Scope::new();
        let out = sh(&scope, "printf abcd", limits(Duration::from_secs(10), 4)).unwrap();
        assert_eq!(out.stdout, b"abcd");
        for script in ["printf abcd", "printf abcd >&2"] {
            let err = sh(&scope, script, limits(Duration::from_secs(10), 3)).unwrap_err();
            assert!(
                matches!(err, ChildError::OutputTooLarge { limit: 3, .. }),
                "{script}: {err}"
            );
        }
    }

    /// A child that writes past the limit and keeps its output open ends as soon as a stream
    /// passes the limit, long before the deadline, and is killed and reaped.
    #[cfg(unix)]
    #[test]
    fn output_past_the_limit_ends_the_run_at_once_and_kills_the_child() {
        let scope = Scope::new();
        let pid_file = scope.path("pid");
        let script = format!(
            "echo $$ > '{}'; head -c 4000000 /dev/zero; exec sleep 30",
            pid_file.display()
        );
        let start = Instant::now();
        let err = sh(&scope, &script, limits(Duration::from_secs(20), 64 * 1024)).unwrap_err();
        assert!(matches!(err, ChildError::OutputTooLarge { .. }), "{err}");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{:?}",
            start.elapsed()
        );
        let pid = written_pid(&pid_file);
        assert!(
            crate::os::start_time_s(pid).is_none(),
            "the child was killed and reaped"
        );
    }

    /// A descendant that let go of the output and outlives a normal exit is killed with the
    /// group before the child is reaped.
    #[cfg(unix)]
    #[test]
    fn a_descendant_left_after_a_normal_exit_is_killed() {
        let scope = Scope::new();
        let pid_file = scope.path("pid");
        let script = format!(
            "sleep 30 >/dev/null 2>&1 & echo $! > '{}'; exit 0",
            pid_file.display()
        );
        let out = sh(&scope, &script, limits(Duration::from_secs(10), 1024)).unwrap();
        assert_eq!(out.code, Some(0));
        let pid = written_pid(&pid_file);
        let gone = (0..250).any(|_| {
            std::thread::sleep(Duration::from_millis(20));
            crate::os::start_time_s(pid).is_none()
        });
        assert!(gone, "the descendant {pid} was killed with the group");
    }

    /// A loopback address unique to this test process, for finding its `ping`.
    #[cfg(windows)]
    fn ping_target(n: u32) -> String {
        let id = std::process::id();
        format!("127.{}.{}.{n}", (id >> 8) & 0xff, id & 0xff)
    }

    /// The constructed git environment from this process, which carries the variables `cmd`
    /// needs.
    #[cfg(windows)]
    fn windows_env() -> Vec<(OsString, OsString)> {
        git_env(std::env::vars_os())
    }

    /// Finds the `ping` of `target` while it runs, so a later "gone" check cannot pass for a
    /// `ping` that never started.
    #[cfg(windows)]
    fn watch_ping(target: &str) -> std::thread::JoinHandle<Option<(u32, u64)>> {
        use crate::os::{Detail, ProcessTable};

        let wanted = target.to_owned();
        std::thread::spawn(move || {
            let detail = Detail {
                cmd: true,
                cwd: false,
            };
            for _ in 0..600 {
                if let Ok(table) = ProcessTable::snapshot(detail) {
                    let found = table.iter().find(|p| {
                        p.name.to_ascii_lowercase().starts_with("ping")
                            && p.cmd.iter().any(|arg| arg == &wanted)
                    });
                    if let Some(p) = found {
                        return Some((p.pid, p.start_s));
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            None
        })
    }

    #[cfg(windows)]
    fn assert_gone(found: Option<(u32, u64)>) {
        let (pid, start_s) = found.expect("ping started");
        let gone = (0..250).any(|_| {
            std::thread::sleep(Duration::from_millis(20));
            !crate::os::is_alive(pid, start_s)
        });
        assert!(gone, "ping {pid} was killed with the job");
    }

    /// On the deadline the whole job goes on Windows: `cmd` waits for `ping`, which holds
    /// the output open; killing `cmd` alone would leave `ping` running.
    #[cfg(windows)]
    #[test]
    fn the_deadline_kills_the_whole_tree_on_windows() {
        let scope = Scope::new();
        let target = ping_target(37);
        let watcher = watch_ping(&target);
        let err = run(
            "cmd",
            &[
                "/D".into(),
                "/C".into(),
                "ping".into(),
                "-n".into(),
                "31".into(),
                target.clone().into(),
            ],
            &windows_env(),
            scope.root(),
            limits(Duration::from_secs(5), 1024 * 1024),
        )
        .unwrap_err();
        assert!(matches!(err, ChildError::Timeout { .. }), "{err}");
        assert_gone(watcher.join().unwrap());
    }
}
