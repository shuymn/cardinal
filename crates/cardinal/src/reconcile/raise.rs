//! Inbox items raised from a pass's observation and disk states (`docs/architecture.md`
//! section 10.1): `reclaimable`, `merge_undecided`, `closed_pr_dirty`, `managed_merged`, and
//! `unclassifiable_data` from the full observation, `disk_low` and `disk_critical` from the
//! disk states with their disk reports. Each raises and ends occurrences through the inbox.

use crate::config::Config;
use crate::contract::{DiskReport, DiskState, Error, InboxChanges};
use crate::state::StateDir;

use super::Observation;

/// The items as they were before the pass observed anything: the raisers end only these.
#[derive(Clone, Debug, Default)]
pub struct Before;

/// The items before step 4.
pub fn snapshot(_state: &StateDir) -> Result<Before, Error> {
    Err(Error::not_implemented(
        "reconcile: reading the items before the pass",
    ))
}

/// Raises and ends the worktree items of section 10.1 from step 5's observation and plans.
pub fn worktree_items(
    _config: &Config,
    _state: &StateDir,
    _before: &Before,
    _observation: &Observation,
) -> Result<InboxChanges, Error> {
    Err(Error::not_implemented("reconcile: raising worktree items"))
}

/// Raises, updates, and ends the disk items of every watched volume from its state, each
/// with the volume's disk report from `disk-reclaim` (step 6).
pub fn disk_items(
    _config: &Config,
    _state: &StateDir,
    _before: &Before,
    _disks: &[DiskState],
    _reports: &[DiskReport],
) -> Result<InboxChanges, Error> {
    Err(Error::not_implemented("reconcile: raising disk items"))
}
