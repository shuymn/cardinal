//! Plans: generation and identity binding (`docs/architecture.md` sections 9.1–9.4).
//!
//! Owned by the plan, act, inbox, and explain work unit. The bodies are not written yet;
//! every declared function returns `not_implemented`.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::contract::{Error, Plan, PlanKind, PlanList};
use crate::state::StateDir;

/// `cardinal plan reclaim-worktree PATH | reclaim-dir PATH`.
pub fn create(
    config: &Config,
    state: &StateDir,
    kind: PlanKind,
    target: &Path,
) -> Result<Plan, Error> {
    let _ = (config, state, kind, target);
    Err(Error::not_implemented("cardinal plan"))
}

/// `cardinal plan [--repo PATH]...`: plans for every whole-worktree candidate and every
/// regenerable entry of worktrees that are not candidates.
pub fn create_all(config: &Config, state: &StateDir, repos: &[PathBuf]) -> Result<PlanList, Error> {
    let _ = (config, state, repos);
    Err(Error::not_implemented("cardinal plan"))
}
