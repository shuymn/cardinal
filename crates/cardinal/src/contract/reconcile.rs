use serde::{Deserialize, Serialize};

use super::{DiskReport, DiskState, ReconcileSchema, ReconcileStateSchema, ScopedError};

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
    pub inbox: InboxChanges,
    pub arbiter: ArbiterStart,
    pub errors: Vec<ScopedError>,
}

/// `cardinal.reconcile-state.v1` (`reconcile/state.json`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileState {
    pub schema: ReconcileStateSchema,
    pub last_pass_ms: u64,
    pub last_observation_ms: Option<u64>,
}
