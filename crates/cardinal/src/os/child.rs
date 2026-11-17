//! Short-lived child processes whose output the caller needs: an environment the caller
//! constructs, an absolute deadline on every wait, output read on threads, and cleanup on
//! every early exit (`docs/architecture.md` sections 2.2 and 8.1).
//!
//! The environment is always the caller's choice. [`git_env`] and [`gh_env`] build the
//! constructed environments of section 8.1 from separate allowlists.
//!
//! No process cardinal starts here outlives what started it: the child's process group
//! (Unix) or Job Object (Windows) is killed before the child is reaped, after a normal exit
//! too. A terminated process takes its live children along. On Unix the first [`run`] in a
//! process installs handlers for SIGTERM, SIGINT, and SIGHUP that end the process by the
//! same signal (see `guard_termination`), so a process that handles those signals itself
//! must not use [`run`]; when that fails, no child starts, and this and every later [`run`]
//! in the process return [`ChildError::Setup`]. On Windows each child's job is killed when
//! its only handle, held by this process, closes; other processes this process starts are
//! not affected.
//!
//! For short-lived callers only (a CLI command, a hook): a descendant that left the child's
//! process group (`setsid`) is not killed, and a reader thread blocked on a pipe it holds
//! open outlives a [`ChildError::Timeout`] until that descendant closes the pipe or the
//! process ends. A long-lived caller would collect such threads.

#[cfg(unix)]
mod unix;
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

/// Variables git children copy from the caller's environment (section 8.1). `LC_*` is
/// copied too.
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
];

/// Variables gh children copy: where gh keeps its configuration and the tokens it
/// authenticates with. `GH_HOST` and `GH_REPO` are left out, because they would point gh at
/// another host or repository than the `--repo HOST/OWNER/NAME` observation passes.
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

/// Variables every git and gh child gets: no optional locks, no prompts, no colors.
const ADDED: &[(&str, &str)] = &[
    ("GIT_OPTIONAL_LOCKS", "0"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GH_PROMPT_DISABLED", "1"),
    ("GH_NO_UPDATE_NOTIFIER", "1"),
    ("NO_COLOR", "1"),
];

/// The environment of a git child, built from `inherited`: only the allowlisted variables
/// pass, so `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, and every other
/// repository-location variable of the caller never reach git.
pub fn git_env(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    build_env(inherited, GIT_INHERITED)
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

/// Starts the child as the leader of its own process group.
#[cfg(unix)]
fn spawn(mut command: Command) -> std::io::Result<Reaper> {
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    Ok(Reaper {
        child: command.spawn()?,
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

fn take_output(child: &mut Child) -> [Option<Box<dyn Read + Send>>; 2] {
    [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    ]
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

/// Makes this process take its children along when it is terminated, once per process.
/// SIGCHLD goes to its default action first, so every child stays unreaped, and the process
/// group it leads pinned, until [`Reaper::finish`] reaps it, also when the caller ignored
/// SIGCHLD. Then SIGTERM, SIGINT, and SIGHUP (unless ignored) kill every live child's process
/// group and end the process by the same signal with its default action. A failure installs
/// no handler and is returned by this and every later call: signal-hook keeps a slot for a
/// signal it once took, so a second attempt would not install its handler again.
#[cfg(unix)]
fn guard_termination() -> std::io::Result<()> {
    use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};

    static GUARD: OnceLock<Result<(), String>> = OnceLock::new();
    GUARD
        .get_or_init(|| {
            unix::reset_child_signal();
            unix::watch_signals(&[SIGTERM, SIGINT, SIGHUP], |sig| {
                // Held until the process ends: no child starts or is reaped after this.
                let live = live();
                for &pgid in live.iter() {
                    crate::os::kill_process_group(pgid);
                }
                unix::die_by_signal(sig)
            })
            .map_err(|err| err.to_string())
        })
        .clone()
        .map_err(std::io::Error::other)
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
            unix::ended(self.child.id())
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
            crate::os::kill_process_group(id);
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

/// Reads each present stream to its end on its own thread, stopping one byte past `max`.
/// Each thread sends its stream's index and what it read; returns the receiver and the
/// number of threads started.
fn read_streams(
    streams: [Option<Box<dyn Read + Send>>; 2],
    max: u64,
) -> std::io::Result<(mpsc::Receiver<(usize, Stream)>, usize)> {
    let (sender, receiver) = mpsc::channel();
    let mut started = 0;
    for (index, stream) in streams.into_iter().enumerate() {
        let Some(stream) = stream else { continue };
        let sender = sender.clone();
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
        started += 1;
    }
    Ok((receiver, started))
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
    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(cwd);
    #[cfg(unix)]
    let mut live = live();
    let mut child = spawn(command).map_err(|source| ChildError::Spawn {
        program: name(),
        source,
    })?;
    #[cfg(unix)]
    {
        live.push(child.child.id());
        drop(live);
    }

    let (receiver, mut pending) = read_streams(take_output(&mut child.child), limits.max_output)
        .map_err(|source| ChildError::Io {
            program: name(),
            source,
        })?;

    let io_err = |source| ChildError::Io {
        program: name(),
        source,
    };
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
        for present in ["PATH", "HOME", "LC_ALL", "GIT_CONFIG_GLOBAL", "GH_TOKEN"] {
            assert!(names.contains(&present), "{present} missing: {names:?}");
        }
        assert_eq!(lookup(&env, "GIT_OPTIONAL_LOCKS").unwrap(), "0");
        assert_eq!(
            names.iter().filter(|n| **n == "GIT_OPTIONAL_LOCKS").count(),
            1
        );
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
        ] {
            assert!(names.contains(&present), "{present} missing: {names:?}");
        }
    }

    #[test]
    fn a_missing_program_is_a_spawn_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(
            "cardinal-no-such-program",
            &[],
            &[],
            dir.path(),
            limits(Duration::from_secs(5), 1024),
        )
        .unwrap_err();
        assert!(matches!(err, ChildError::Spawn { .. }), "{err}");
    }

    /// A failed guard fails the run before the child is started: a missing program is not
    /// even looked up.
    #[test]
    fn a_failed_guard_starts_no_child() {
        let dir = tempfile::tempdir().unwrap();
        let err = run_guarded(
            || Err(std::io::Error::other("no reader thread")),
            "cardinal-no-such-program".as_ref(),
            &[],
            &[],
            dir.path(),
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

    /// `sh -c script` in `dir`, with only `PATH` from this process and `HOME` in `dir`.
    #[cfg(unix)]
    fn sh(
        dir: &std::path::Path,
        script: &str,
        limits: Limits,
    ) -> Result<super::Output, ChildError> {
        let mut inherited: Vec<(OsString, OsString)> =
            std::env::vars_os().filter(|(k, _)| k == "PATH").collect();
        inherited.push(("HOME".into(), dir.into()));
        let env = git_env(inherited);
        run("sh", &["-c".into(), script.into()], &env, dir, limits)
    }

    #[cfg(unix)]
    #[test]
    fn output_and_status_are_returned() {
        let dir = tempfile::tempdir().unwrap();
        let out = sh(
            dir.path(),
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
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let script = format!("echo $$ > '{}'; exec sleep 30", pid_file.display());
        let start = Instant::now();
        let err = sh(
            dir.path(),
            &script,
            limits(Duration::from_millis(300), 1024),
        )
        .unwrap_err();
        assert!(matches!(err, ChildError::Timeout { .. }), "{err}");
        assert!(start.elapsed() < Duration::from_secs(5));
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            crate::os::start_time_s(pid).is_none(),
            "the child was killed and reaped"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_descendant_holding_the_output_open_ends_in_a_timeout_and_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let script = format!(
            "sleep 30 & echo $! > '{}'; printf partial; exit 0",
            pid_file.display()
        );
        let start = Instant::now();
        let err = sh(
            dir.path(),
            &script,
            limits(Duration::from_millis(300), 1024),
        )
        .unwrap_err();
        assert!(matches!(err, ChildError::Timeout { .. }), "{err}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        let out = sh(
            dir.path(),
            "printf abcd",
            limits(Duration::from_secs(10), 4),
        )
        .unwrap();
        assert_eq!(out.stdout, b"abcd");
        for script in ["printf abcd", "printf abcd >&2"] {
            let err = sh(dir.path(), script, limits(Duration::from_secs(10), 3)).unwrap_err();
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
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let script = format!(
            "echo $$ > '{}'; head -c 4000000 /dev/zero; exec sleep 30",
            pid_file.display()
        );
        let start = Instant::now();
        let err = sh(
            dir.path(),
            &script,
            limits(Duration::from_secs(20), 64 * 1024),
        )
        .unwrap_err();
        assert!(matches!(err, ChildError::OutputTooLarge { .. }), "{err}");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{:?}",
            start.elapsed()
        );
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let script = format!(
            "sleep 30 >/dev/null 2>&1 & echo $! > '{}'; exit 0",
            pid_file.display()
        );
        let out = sh(dir.path(), &script, limits(Duration::from_secs(10), 1024)).unwrap();
        assert_eq!(out.code, Some(0));
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
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
    /// and `powershell` need.
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
        let dir = tempfile::tempdir().unwrap();
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
            dir.path(),
            limits(Duration::from_secs(5), 1024 * 1024),
        )
        .unwrap_err();
        assert!(matches!(err, ChildError::Timeout { .. }), "{err}");
        assert_gone(watcher.join().unwrap());
    }

    /// After a normal exit the rest of the job goes on Windows: `Start-Process` leaves a
    /// `ping` that inherits no handle, so the run completes while it still runs.
    #[cfg(windows)]
    #[test]
    fn a_process_left_after_a_normal_exit_is_killed_on_windows() {
        let dir = tempfile::tempdir().unwrap();
        let target = ping_target(38);
        let watcher = watch_ping(&target);
        let script = format!(
            "Start-Process -FilePath ping -ArgumentList '-n','31','{target}' -WindowStyle Hidden"
        );
        let out = run(
            "powershell",
            &[
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                script.into(),
            ],
            &windows_env(),
            dir.path(),
            limits(Duration::from_secs(60), 1024 * 1024),
        )
        .unwrap();
        assert_eq!(out.code, Some(0), "{}", out.stderr_line());
        assert_gone(watcher.join().unwrap());
    }
}
