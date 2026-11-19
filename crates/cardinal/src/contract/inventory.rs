use serde::{Deserialize, Serialize};

use super::{
    ArbiterPass, DiskState, InventorySchema, ReservationView, Resources, ScopedError,
    SessionProcess, Tri, WorktreeSchema,
};

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
    /// The disk states of `disk/`, read, not measured (section 8.5).
    pub disk: Vec<DiskState>,
    pub arbiter: ArbiterView,
    pub errors: Vec<ScopedError>,
}

/// `cardinal.worktree.v1` (`cardinal inventory --worktree PATH`): one worktree observed from
/// scratch, in a bounded size whatever the host holds (section 11.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeView {
    pub schema: WorktreeSchema,
    pub observed_ms: u64,
    /// The worktree's repository, its `worktrees` empty.
    pub repo: Repo,
    /// The worktree, its `ignored` cut to its first entries in path order.
    pub worktree: Worktree,
    /// How many ignored entries the worktree has, before the cut.
    pub ignored_entries: u64,
    /// The `<set>` an `unclassifiable_data` item for the worktree's unclassifiable entries has
    /// in its key (section 10.1); null when it has none or they were not listed.
    pub unclassifiable_set: Option<String>,
    /// What observation of the worktree, or of its repository, could not read.
    pub errors: Vec<ScopedError>,
}

/// Whether an arbiter pass runs, and the last one (section 10.5).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArbiterView {
    pub running: bool,
    pub last: Option<ArbiterPass>,
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

/// Why `dirty` is `unknown` although status listed only modifications.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirtyUnknownReason {
    /// Every entry is a worktree-side modification of a path whose `filter` attribute names
    /// a configured driver, which observation does not run (section 8.1).
    FilteredPaths,
}

/// The paths behind a `dirty` of `unknown` (section 8.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirtyUnknown {
    pub reason: DirtyUnknownReason,
    /// Up to 20 worktree-relative paths.
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    pub dirty: Tri,
    pub dirty_unknown: Option<DirtyUnknown>,
    pub has_upstream: Tri,
    pub unpushed: Tri,
    pub open_pr: Tri,
    pub merge_evidence: MergeEvidence,
    pub merged_pr: Option<PrRef>,
    /// Whether every file the branch changed has the same content in the base; `unknown`
    /// unless `merge_evidence` is `none`.
    pub content_in_base: Tri,
    /// Up to 20 paths the branch changed that differ from the base.
    pub content_diff: Vec<String>,
    /// The newest pull request of the branch closed without merging.
    pub closed_pr: Option<PrRef>,
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
    /// Whether the entry matches a regenerable pattern, whatever its class (section 8.3).
    pub regenerable_match: bool,
    pub allocated_bytes: Option<u64>,
    /// The part of `allocated_bytes` held by files with links outside the entry.
    pub aliased_bytes: Option<u64>,
    /// A lower bound of what deleting the entry frees (section 8.5). Absent in observations
    /// made before it was recorded, which read as null.
    #[serde(default)]
    pub exclusive_bytes: Option<u64>,
    /// How `exclusive_bytes` was read; null without sizes. Absent in observations made before
    /// it was recorded, which read as null.
    #[serde(default)]
    pub exclusive_measure: Option<ExclusiveMeasure>,
}

/// How a lower bound of the freed bytes was read (`docs/architecture.md` section 8.5): from
/// the file system's private sizes (APFS), data extents (ext2–4), or allocated blocks with
/// the link rule (HFS+, tmpfs); or why there is none, in which case the bound is null.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExclusiveMeasure {
    PrivateSize,
    Extents,
    Links,
    /// A read the measure needs failed.
    ReadFailed,
    /// Btrfs and XFS, whose shared extents are not read.
    SharingUnread,
    /// Windows, every other file system type, and an APFS read that returned no private size
    /// for a regular file.
    NotMeasured,
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
    /// The worktree's administrative entry (`git rev-parse --absolute-git-dir`).
    pub git_dir: Option<String>,
    /// The modification time of the worktree's index.
    pub last_activity_ms: Option<u64>,
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
