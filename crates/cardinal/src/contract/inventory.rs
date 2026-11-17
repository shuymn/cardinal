use serde::{Deserialize, Serialize};

use super::{InventorySchema, ReservationView, Resources, ScopedError, SessionProcess, Tri};

/// `cardinal.inventory.v1`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inventory {
    pub schema: InventorySchema,
    pub observed_ms: u64,
    pub host: HostInfo,
    pub budget: Resources,
    /// Live plus recovery-needed reservations.
    pub reserved: Resources,
    pub reservations: Vec<ReservationView>,
    pub repos: Vec<Repo>,
    pub outlived: Vec<Outlived>,
    pub errors: Vec<ScopedError>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Os {
    Macos,
    Linux,
    Windows,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    pub id: String,
    pub hostname: String,
    pub os: Os,
    pub arch: String,
    pub boot_s: u64,
}

/// Processes of a session snapshot still alive after the session's agent process ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outlived {
    pub session_id: String,
    pub ended_ms: u64,
    pub processes: Vec<SessionProcess>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GhStatus {
    Ok,
    Unknown,
    NotApplicable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseRef {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub oid: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repo {
    pub common_dir: String,
    pub main_worktree: String,
    pub profiles: Vec<String>,
    /// `"owner/name"`
    pub github: Option<String>,
    pub base: Option<BaseRef>,
    pub gh: GhStatus,
    pub gh_error: Option<String>,
    pub worktrees: Vec<Worktree>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeEvidence {
    BaseAncestor,
    MergedPr,
    None,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrRef {
    pub number: u64,
    pub url: String,
}

/// Whether a worktree is in use, and by which jobs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InUse {
    pub value: bool,
    pub jobs: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    pub dirty: Tri,
    pub has_upstream: Tri,
    pub unpushed: Tri,
    pub open_pr: Tri,
    pub merge_evidence: MergeEvidence,
    pub merged_pr: Option<PrRef>,
    pub in_use: InUse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IgnoredClass {
    Regenerable,
    Protected,
    Unclassifiable,
    NestedRepository,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IgnoredEntry {
    pub path: String,
    pub class: IgnoredClass,
    pub allocated_bytes: Option<u64>,
}

/// Whether a worktree is a whole-worktree reclaim candidate, with failing and unknown checks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReclaimSummary {
    pub candidate: bool,
    pub failed: Vec<String>,
    pub unknown: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    pub path: String,
    pub main: bool,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub upstream: Option<String>,
    /// App name or `"git-lock"`.
    pub managed_by: Option<String>,
    pub facts: Facts,
    pub ignored: Vec<IgnoredEntry>,
    pub reclaim: ReclaimSummary,
    pub allocated_bytes: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::{BaseRef, MergeEvidence};

    #[test]
    fn field_names_follow_the_contract() {
        let base = BaseRef {
            ref_name: "origin/main".into(),
            oid: "abc".into(),
        };
        assert_eq!(
            serde_json::to_value(base).unwrap(),
            serde_json::json!({"ref": "origin/main", "oid": "abc"})
        );
        assert_eq!(
            serde_json::to_value(MergeEvidence::None).unwrap(),
            serde_json::json!("none")
        );
    }
}
