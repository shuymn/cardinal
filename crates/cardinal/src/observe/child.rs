//! Child processes for git and gh (`docs/architecture.md` section 8.1): a constructed
//! environment, a deadline on every wait, and output read on threads.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
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
pub fn constructed_env(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    build_env(inherited, GIT_INHERITED)
}

/// The environment of a gh child, built from `inherited` like [`constructed_env`] with the
/// gh allowlist.
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
    #[error("{program} did not finish within {} ms", timeout.as_millis())]
    Timeout { program: String, timeout: Duration },
    #[error("{program}: {source}")]
    Io {
        program: String,
        source: std::io::Error,
    },
}

/// Owns the child until its output is complete. Dropped before that (deadline, read error,
/// a reader thread that could not start), it kills the child's process group on Unix, so
/// descendants holding the output open go too, then kills and reaps the child. On Windows
/// only the child is killed; a descendant can outlive it until it closes the pipe.
struct Reaper {
    child: Child,
    complete: bool,
}

impl Reaper {
    fn wait_until(&mut self, deadline: Instant) -> std::io::Result<Option<ExitStatus>> {
        let mut delay = Duration::from_millis(1);
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(Some(status));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            std::thread::sleep(delay.min(deadline - now));
            delay = (delay * 2).min(Duration::from_millis(20));
        }
    }
}

impl Drop for Reaper {
    fn drop(&mut self) {
        if self.complete {
            return;
        }
        // The group is the child's own (`process_group(0)`); while the child is unreaped or a
        // member still holds the output open, its ID cannot belong to another group.
        #[cfg(unix)]
        crate::os::kill_process_group(self.child.id());
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs `program` with `args` and exactly `env`, stdin closed, until `timeout`.
///
/// Output is read on two threads. On Unix the child leads its own process group. On the
/// deadline the group is killed and the child reaped, and the result is
/// [`ChildError::Timeout`]; that includes a child that exited while a descendant still
/// holds its output open. Once the group is gone the reader threads see end of file and
/// end; nothing waits for them.
pub fn run(
    program: &str,
    args: &[OsString],
    env: &[(OsString, OsString)],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<Output, ChildError> {
    let deadline = Instant::now() + timeout;
    let name = || program.to_owned();
    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = Reaper {
        child: command.spawn().map_err(|source| ChildError::Spawn {
            program: name(),
            source,
        })?,
        complete: false,
    };

    let (sender, receiver) = mpsc::channel();
    let stdout = child
        .child
        .stdout
        .take()
        .map(|s| Box::new(s) as Box<dyn Read + Send>);
    let stderr = child
        .child
        .stderr
        .take()
        .map(|s| Box::new(s) as Box<dyn Read + Send>);
    for (index, stream) in [stdout, stderr].into_iter().enumerate() {
        let Some(mut stream) = stream else { continue };
        let sender = sender.clone();
        std::thread::Builder::new()
            .name("observe-output".into())
            .spawn(move || {
                let mut bytes = Vec::new();
                let result = stream.read_to_end(&mut bytes).map(|_| bytes);
                let _ = sender.send((index, result));
            })
            .map_err(|source| ChildError::Io {
                program: name(),
                source,
            })?;
    }
    drop(sender);

    let io_err = |source| ChildError::Io {
        program: name(),
        source,
    };
    let timed_out = || ChildError::Timeout {
        program: name(),
        timeout,
    };
    let Some(status) = child.wait_until(deadline).map_err(io_err)? else {
        return Err(timed_out());
    };
    let mut streams = [Vec::new(), Vec::new()];
    for _ in 0..2 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok((index, result)) => streams[index] = result.map_err(io_err)?,
            Err(mpsc::RecvTimeoutError::Timeout) => return Err(timed_out()),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    child.complete = true;
    let [stdout, stderr] = streams;
    Ok(Output {
        code: status.code(),
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::time::{Duration, Instant};

    use super::{ChildError, constructed_env, lookup, run};

    fn pairs(items: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        items
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    #[test]
    fn environment_keeps_only_the_allowlist_and_adds_the_fixed_variables() {
        let env = constructed_env(pairs(&[
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
        let err = run(
            "cardinal-no-such-program",
            &[],
            &[],
            None,
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(matches!(err, ChildError::Spawn { .. }), "{err}");
    }

    #[cfg(unix)]
    fn sh(script: &str, timeout: Duration) -> Result<super::Output, ChildError> {
        let env = constructed_env(std::env::vars_os());
        run("sh", &["-c".into(), script.into()], &env, None, timeout)
    }

    #[cfg(unix)]
    #[test]
    fn output_and_status_are_returned() {
        let out = sh(
            "printf out; printf err >&2; exit 3",
            Duration::from_secs(10),
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
        let err = sh(&script, Duration::from_millis(300)).unwrap_err();
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
        let err = sh(&script, Duration::from_millis(300)).unwrap_err();
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
}
