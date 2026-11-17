//! The repository a Bash call runs in, for profile matching (`docs/architecture.md` section
//! 3.3): its git common dir without a child process, and its origin remote through git only
//! when a rule could match a stage at all.
//!
//! git runs as section 8.1 requires: `git --no-optional-locks -C <root>` with a constructed
//! environment, so a hook started from a git hook (`GIT_DIR`, `GIT_WORK_TREE`, …) never
//! reads another repository, and under the hook's own deadline: past it the child is killed
//! and reaped, and the hook rewrites nothing.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::config;
use crate::state;

/// The most bytes read from git's output; a remote URL is far shorter.
const MAX_OUTPUT: u64 = 64 * 1024;

/// How often the child is polled for its exit after its output ended.
const POLL: Duration = Duration::from_millis(2);

/// Variables copied into git's environment (section 8.1).
const ALLOWED: &[&str] = &[
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

const ALLOWED_WINDOWS: &[&str] = &[
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

const ADDED: &[(&str, &str)] = &[
    ("GIT_OPTIONAL_LOCKS", "0"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GH_PROMPT_DISABLED", "1"),
    ("GH_NO_UPDATE_NOTIFIER", "1"),
    ("NO_COLOR", "1"),
];

/// The worktree a cwd is in and its repository identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Repo {
    /// The worktree root: the nearest directory with a `.git` entry.
    pub root: PathBuf,
    /// The canonical git common dir.
    pub common_dir: PathBuf,
}

impl Repo {
    /// The repository of `cwd`, read from `.git` entries; `None` outside a repository.
    pub fn locate(cwd: &Path) -> Option<Self> {
        let root = state::find_worktree_root(cwd)?;
        let common_dir = config::git_common_dir(&root)?;
        Some(Self { root, common_dir })
    }

    /// The main worktree: the parent of a `.git` common dir, else the common dir itself
    /// (a bare repository).
    pub fn main_worktree(&self) -> PathBuf {
        match (self.common_dir.file_name(), self.common_dir.parent()) {
            (Some(name), Some(parent)) if name == ".git" => parent.to_path_buf(),
            _ => self.common_dir.clone(),
        }
    }
}

/// What git said about the origin remote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Origin {
    Url(String),
    /// No origin, git failed or could not start, or its output was unusable.
    Unknown,
    /// The deadline passed; the child was killed and reaped.
    TimedOut,
}

/// The environment git gets: the allowlist of section 8.1 from `vars`, plus the fixed
/// settings. Every other variable, every other `GIT_*` included, is dropped.
pub(super) fn git_env(
    vars: impl Iterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = vars
        .filter(|(name, _)| {
            let Some(name) = name.to_str() else {
                return false;
            };
            ALLOWED.contains(&name)
                || name.starts_with("LC_")
                || (cfg!(windows)
                    && ALLOWED_WINDOWS
                        .iter()
                        .any(|allowed| allowed.eq_ignore_ascii_case(name)))
        })
        .collect();
    env.extend(ADDED.iter().map(|(k, v)| ((*k).into(), (*v).into())));
    env
}

/// `git --no-optional-locks -C <root> remote get-url origin`, killed at `deadline`.
pub(super) fn origin(git: &str, root: &Path, deadline: Instant) -> Origin {
    let mut command = Command::new(git);
    command
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args(["remote", "get-url", "origin"])
        .env_clear()
        .envs(git_env(std::env::vars_os()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    run(command, deadline)
}

/// Runs `command` (stdout piped) until it exits or `deadline` passes.
fn run(mut command: Command, deadline: Instant) -> Origin {
    if Instant::now() >= deadline {
        return Origin::TimedOut;
    }
    let Ok(mut child) = command.spawn() else {
        return Origin::Unknown;
    };
    let Some(stdout) = child.stdout.take() else {
        kill(&mut child);
        return Origin::Unknown;
    };
    // The reader ends at EOF, which a descendant holding the pipe can postpone, so the
    // result is awaited with the deadline and the thread is never joined.
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::Builder::new().spawn(move || {
        let mut out = Vec::new();
        let read = stdout.take(MAX_OUTPUT).read_to_end(&mut out);
        let _ = sender.send(read.map(|_| out));
    });
    if reader.is_err() {
        kill(&mut child);
        return Origin::Unknown;
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    let out = match receiver.recv_timeout(remaining) {
        Ok(Ok(out)) => out,
        Ok(Err(_)) => {
            kill(&mut child);
            return Origin::Unknown;
        }
        Err(_) => {
            kill(&mut child);
            return Origin::TimedOut;
        }
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            Ok(None) => {
                kill(&mut child);
                return Origin::TimedOut;
            }
            Err(_) => {
                kill(&mut child);
                return Origin::Unknown;
            }
        }
    };
    let url = String::from_utf8_lossy(&out).trim().to_owned();
    if status.success() && !url.is_empty() && !url.contains('\n') {
        Origin::Url(url)
    } else {
        Origin::Unknown
    }
}

/// Kills and reaps a child that has not exited.
fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::git_env;

    #[test]
    fn git_env_keeps_only_the_allowlist() {
        let vars = [
            ("PATH", "/bin"),
            ("HOME", "/h"),
            ("LC_ALL", "C"),
            ("GIT_DIR", "/decoy/.git"),
            ("GIT_WORK_TREE", "/decoy"),
            ("GIT_INDEX_FILE", "/decoy/index"),
            ("GIT_CONFIG_PARAMETERS", "'core.x'='y'"),
            ("GIT_CONFIG_GLOBAL", "/h/.gitconfig"),
            ("SECRET", "x"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let env = git_env(vars.into_iter());
        let names: Vec<&str> = env.iter().map(|(k, _)| k.to_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "PATH",
                "HOME",
                "LC_ALL",
                "GIT_CONFIG_GLOBAL",
                "GIT_OPTIONAL_LOCKS",
                "GIT_TERMINAL_PROMPT",
                "GH_PROMPT_DISABLED",
                "GH_NO_UPDATE_NOTIFIER",
                "NO_COLOR"
            ]
        );
    }
}
