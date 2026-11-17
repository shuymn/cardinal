//! The repository a Bash call runs in, for profile matching (`docs/architecture.md` section
//! 3.3): its git common dir without a child process, and its origin remote through git only
//! when a rule could match a stage at all.
//!
//! git runs through `os::child` as section 8.1 requires: `git --no-optional-locks -c
//! core.fsmonitor=false -C <root>` in `root`, with the constructed environment of
//! `os::child::git_env`, so a hook started from a git hook (`GIT_DIR`, `GIT_WORK_TREE`, …)
//! never reads another repository, and under the hook's own deadline: past it git's process
//! group is killed, git is reaped, and the hook rewrites nothing.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::config;
use crate::os::child::{self, ChildError, Limits, Output};
use crate::state;

/// The most bytes read from each of git's stdout and stderr; a remote URL is far shorter.
const MAX_OUTPUT: u64 = 64 * 1024;

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
    /// The deadline passed: git was not started, or its process group was killed and git
    /// reaped.
    TimedOut,
}

/// `git remote get-url origin` in `root`, run as section 8.1 requires and ended at
/// `deadline`.
pub(super) fn origin(git: &str, root: &Path, deadline: Instant) -> Origin {
    // A child the deadline would end at once is not started.
    if Instant::now() >= deadline {
        return Origin::TimedOut;
    }
    let args: Vec<OsString> = vec![
        "--no-optional-locks".into(),
        "-c".into(),
        "core.fsmonitor=false".into(),
        "-C".into(),
        root.into(),
        "remote".into(),
        "get-url".into(),
        "origin".into(),
    ];
    let env = child::git_env(std::env::vars_os());
    let limits = Limits {
        deadline,
        max_output: MAX_OUTPUT,
    };
    match child::run(git, &args, &env, root, limits) {
        Ok(output) => url(&output),
        Err(ChildError::Timeout { .. }) => Origin::TimedOut,
        Err(_) => Origin::Unknown,
    }
}

/// The URL git printed: one non-empty line from a git that exited 0.
fn url(output: &Output) -> Origin {
    let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if output.code == Some(0) && !url.is_empty() && !url.contains('\n') {
        Origin::Url(url)
    } else {
        Origin::Unknown
    }
}
