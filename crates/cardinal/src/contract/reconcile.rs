use serde::{Deserialize, Serialize};

use super::{ActStatus, DiskReport, DiskState, ReconcileSchema, ReconcileStateSchema, ScopedError};

/// What started a `reconcile` pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Trigger {
    #[serde(rename = "timer")]
    Timer,
    /// The runner's job-end disk check (section 5.2).
    #[serde(rename = "job-end")]
    JobEnd,
    #[serde(rename = "manual")]
    Manual,
}

/// Why a pass did nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Skipped {
    /// Another pass holds `reconcile.lock`.
    Busy,
}

/// What the arbiter trigger of a pass did (section 10.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArbiterStart {
    Started,
    Running,
    NotNeeded,
    /// An item is due, but `min_interval_s` has not passed since the last pass started.
    Interval,
}

/// The inbox keys a pass raised and cleared.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxChanges {
    pub raised: Vec<String>,
    pub cleared: Vec<String>,
}

/// An execution a pass recovered, with the status of its result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveredExecution {
    pub plan_id: String,
    pub status: ActStatus,
}

/// `cardinal.reconcile.v1`: the stdout of `cardinal reconcile` (section 10.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reconcile {
    pub schema: ReconcileSchema,
    pub trigger: Trigger,
    pub skipped: Option<Skipped>,
    pub disk: Vec<DiskState>,
    pub observed: bool,
    /// One per volume `disk-reclaim` ran on.
    pub disk_reports: Vec<DiskReport>,
    /// The executions that died, finished or undone at step 2 (unit X, section 9.7).
    pub recovered: Vec<RecoveredExecution>,
    pub inbox: InboxChanges,
    pub arbiter: ArbiterStart,
    pub errors: Vec<ScopedError>,
}

/// `cardinal.reconcile-state.v1` (`reconcile/state.json`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileState {
    pub schema: ReconcileStateSchema,
    pub last_pass_ms: u64,
    pub last_observation_ms: Option<u64>,
    /// The worktrees whose `not_in_use` was `unknown` at the last `streak` full observations
    /// (section 9.8), which raise `in_use_unknown` at 3. Absent in states written before it
    /// and when there is none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub in_use_unknown: Vec<InUseUnknownStreak>,
}

/// A worktree's run of full observations whose `not_in_use` was `unknown`, with the input the
/// last one could not read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InUseUnknownStreak {
    pub worktree: String,
    pub streak: u32,
    pub input: String,
}
