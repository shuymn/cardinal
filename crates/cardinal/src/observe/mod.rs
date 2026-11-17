//! Observation: git and gh with constructed environments and deadlines, repository and
//! worktree facts, merge evidence, ignored data, disk usage, and outlived processes
//! (`docs/architecture.md` section 8).
//!
//! Owned by the observation work unit. The bodies are not written yet; every declared
//! function returns `not_implemented`.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::contract::{Error, Inventory, Repo, Worktree};
use crate::state::StateDir;

/// `cardinal inventory` arguments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InventoryOptions {
    /// `--repo`: main worktrees to observe in addition to the configured ones.
    pub repos: Vec<PathBuf>,
    /// `--sizes`: walk worktrees for `allocated_bytes`.
    pub sizes: bool,
}

/// One worktree observed from scratch, with its repository (whose `worktrees` is empty).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeObservation {
    pub repo: Repo,
    pub worktree: Worktree,
}

/// `cardinal inventory`. Read-only: never deletes or signals anything.
pub fn inventory(
    config: &Config,
    state: &StateDir,
    options: &InventoryOptions,
) -> Result<Inventory, Error> {
    let _ = (config, state, options);
    Err(Error::not_implemented("cardinal inventory"))
}

/// Observes the worktree containing `path` from scratch; `plan` and `act` use it.
pub fn observe_worktree(
    config: &Config,
    state: &StateDir,
    path: &Path,
) -> Result<WorktreeObservation, Error> {
    let _ = (config, state, path);
    Err(Error::not_implemented("observe::observe_worktree"))
}
