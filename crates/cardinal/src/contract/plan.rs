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
    pub checks: Vec<Check>,
    pub invariants_ok: bool,
    pub approval: Approval,
}

/// `cardinal.plan-list.v1`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanList {
    pub schema: PlanListSchema,
    pub plans: Vec<Plan>,
    pub errors: Vec<ScopedError>,
}

/// `plans/<plan>/claim`, created once by the `act` that executes the plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanClaim {
    pub pid: u32,
    pub claimed_ms: u64,
}
