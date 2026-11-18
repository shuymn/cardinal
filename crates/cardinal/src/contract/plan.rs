use serde::{Deserialize, Serialize};

use super::{Check, PlanListSchema, PlanSchema, ScopedError};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanKind {
    ReclaimWorktree,
    ReclaimDir,
}

/// Owner approval a plan needs. Informational in the plan; `act` recomputes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    None,
    Owner,
}

/// The identity of a directory: `dev` and `ino` on Unix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootId {
    pub dev: u64,
    pub ino: u64,
}

/// The target a plan is bound to (`docs/architecture.md` section 9.4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanTarget {
    pub path: String,
    pub repo: String,
    pub worktree: String,
    pub head: Option<String>,
    pub branch: Option<String>,
    /// The worktree's administrative entry (section 8.2). Absent in plans made before it was
    /// recorded, which read as null.
    #[serde(default)]
    pub git_dir: Option<String>,
    pub root_id: Option<RootId>,
    pub root_created_ms: Option<u64>,
}

/// `cardinal.plan.v1` (`plans/<plan>/plan.json`, immutable after creation).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub schema: PlanSchema,
    pub id: String,
    pub created_ms: u64,
    pub host_id: String,
    pub kind: PlanKind,
    pub target: PlanTarget,
    pub allocated_bytes: Option<u64>,
    /// The part of `allocated_bytes` held by files with links outside the target (section
    /// 8.5). Absent in plans made before it was recorded, which read as null.
    #[serde(default)]
    pub aliased_bytes: Option<u64>,
    pub checks: Vec<Check>,
    pub invariants_ok: bool,
    pub approval: Approval,
    /// What the checks were evaluated from beyond their facts.
    #[serde(default)]
    pub provenance: Provenance,
}

/// What a plan's or an act's checks were evaluated from beyond each check's facts
/// (`docs/architecture.md` section 10.2).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// What could not be observed, and why.
    pub errors: Vec<ObservedError>,
    /// The configuration keys the checks used.
    pub config: Vec<ConfigKey>,
}

/// An observation that failed: what was being read, and the error.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedError {
    pub source: String,
    pub message: String,
}

/// A configuration key and its value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigKey {
    pub file: String,
    pub key: String,
    pub value: serde_json::Value,
}

/// `cardinal.plan-list.v1`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanList {
    pub schema: PlanListSchema,
    pub plans: Vec<Plan>,
    pub errors: Vec<ScopedError>,
}

/// `plans/<plan>/claim`, reserved for the execution design (`docs/architecture.md` section
/// 9.7): nothing creates it in phase 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanClaim {
    pub pid: u32,
    pub claimed_ms: u64,
}
