use serde::{Deserialize, Serialize};

use super::{ActStatus, DiskStateSchema, present};

/// How free space was measured (`docs/architecture.md` section 10.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskMeasure {
    /// macOS: `kCFURLVolumeAvailableCapacityForImportantUsageKey`, purgeable space included.
    ImportantUsage,
    /// Linux: `f_bavail × f_frsize`.
    Statvfs,
}

/// The state of one watched volume. `Unknown` before a first successful reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskLevel {
    Ok,
    Warning,
    Critical,
    Unknown,
}

/// A volume's lines in bytes of free space: below `warning` it is under pressure, below
/// `critical` critical, and pressure ends above `recovery`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskLines {
    pub warning_bytes: u64,
    pub recovery_bytes: u64,
    pub critical_bytes: u64,
}

/// One reading of a volume; the sizes are null when the read failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskReading {
    pub at_ms: u64,
    pub free_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
}

/// `cardinal.disk-state.v1` (`disk/<volume>.json`); `DiskState` in inventory and snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskState {
    pub schema: DiskStateSchema,
    /// The FNV-1a key of the volume's identity (section 10.4).
    pub volume: String,
    pub mount: String,
    pub measure: DiskMeasure,
    pub state: DiskLevel,
    /// When the volume entered `state`.
    pub since_ms: u64,
    pub lines: DiskLines,
    /// The last 12 readings, oldest first.
    pub readings: Vec<DiskReading>,
}

/// The pass's last successful reading of a volume, which a disk report is computed from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportReading {
    pub at_ms: u64,
    pub free_bytes: u64,
}

/// A target `disk-reclaim` lists, in its reclamation order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportTarget {
    pub plan_id: String,
    pub worktree: String,
    pub target: String,
    /// `allocated_bytes` less `aliased_bytes`.
    pub exclusive_bytes: u64,
    /// The status of the `act` that ran on the target when `disk-reclaim` executes (unit X,
    /// section 10.4); absent when it only lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ActStatus>,
    /// That `act`'s `freed_bytes`, null or not; absent when it only lists.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub freed_bytes: Option<Option<u64>>,
}

/// An entry matching a regenerable pattern whose plan failed, with its first failed check
/// (or `"aliased"` for a target whose regular files' bytes are all aliased).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportBlocked {
    pub worktree: String,
    pub target: String,
    pub check: String,
    pub bytes: u64,
}

/// `disk-reclaim`'s report on one volume (section 10.4): `reconcile` prints it, the disk
/// items carry it, and the page shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskReport {
    pub volume: String,
    pub reading: ReportReading,
    pub lines: DiskLines,
    /// When the plans' checks and sizes were evaluated.
    pub checks_ms: u64,
    pub targets: Vec<ReportTarget>,
    /// The bytes still missing to the recovery line; 0 when the targets reach it.
    pub shortfall_bytes: u64,
    pub blocked: Vec<ReportBlocked>,
    /// The worktrees on the volume that the pass's observation read only in part, so that
    /// what is listed there is not all there is. Absent in reports made before it was
    /// recorded, which read as empty.
    #[serde(default)]
    pub incomplete: Vec<ReportIncomplete>,
}

/// A worktree on the volume whose observation failed in part, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportIncomplete {
    pub worktree: String,
    pub reason: String,
}
