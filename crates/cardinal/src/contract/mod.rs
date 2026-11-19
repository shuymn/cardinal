//! serde types for every JSON document cardinal reads or writes (`docs/architecture.md`
//! section 11.3), plus the run spec, reservation, record, and session-snapshot files.
//!
//! Every top-level document carries a `schema` field `"cardinal.<name>.v<N>"`. The field is
//! a zero-sized marker type per document, so deserializing a document of another schema or
//! version fails instead of being misread. Readers ignore unknown fields.

mod act;
mod arbiter;
mod classify;
mod disk;
mod error;
mod execution;
mod explain;
mod inbox;
mod inventory;
mod plan;
mod reconcile;
mod record;
mod reservation;
mod run_spec;
mod session;
mod snapshot;

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use serde::{Deserialize, Deserializer, Serialize};

pub use act::{ActResult, ActStatus, Caller, CallerParseError};
pub use arbiter::{ArbiterPass, PassOutcome};
pub use classify::{
    ClassifyDecision, ClassifyRepo, ClassifyRequest, ClassifyResponse, ClassifyTool, ToolName,
};
pub use disk::{
    DiskLevel, DiskLines, DiskMeasure, DiskReading, DiskReport, DiskState, ReportBlocked,
    ReportIncomplete, ReportReading, ReportTarget,
};
pub use error::{Error, ErrorBody, ErrorCode, ErrorDoc};
pub use execution::{
    ActClaim, ActManifest, ChainDir, ClaimRepo, ClaimStage, ClaimTarget, ClaimWorktree, EntryPath,
    EntryType, ManifestEntry,
};
pub use explain::{Explain, ExplainKind, ExplainSource, ExplainSubject};
pub use inbox::{
    AnswerOutcome, BriefItem, Inbox, InboxBrief, InboxItem, InboxKind, InboxStatus, InboxSubject,
    ItemAnswer, ItemArbiter, Resolution, ResolutionOutcome,
};
pub use inventory::{
    ArbiterView, BaseRef, DirtyUnknown, DirtyUnknownReason, ExclusiveMeasure, Facts, GhStatus,
    HostInfo, IgnoredClass, IgnoredEntry, InUse, Inventory, MergeEvidence, Os, Outlived, PrRef,
    ReclaimSummary, Repo, Worktree, WorktreeView,
};
pub use plan::{
    Approval, ConfigKey, ObservedError, Plan, PlanKind, PlanList, PlanTarget, Provenance, RootId,
};
pub use reconcile::{
    ArbiterStart, InboxChanges, Reconcile, ReconcileState, RecoveredExecution, Skipped, Trigger,
};
pub use record::{Record, RecordKind};
pub use reservation::{
    AdmissionResult, DerivedState, GroupMember, Priority, ProcessIdentity, Reservation,
    ReservationSession, ReservationState, ReservationView, ReservationWorktree,
};
pub use run_spec::{RunSpec, SpecError};
pub use session::{SessionProcess, SessionSnapshot, SnapshotAgent};
pub use snapshot::{Snapshot, SnapshotItem, SnapshotObservation, SnapshotReservations};

/// Resource amounts by name (`cpu`, `memory_mb`, or any owner-defined name). Amounts are
/// positive integers; zero is rejected when a document or configuration is read.
pub type Resources = BTreeMap<String, NonZeroU64>;

/// Defines a zero-sized `schema` marker that serializes as its schema string and refuses to
/// deserialize any other value.
macro_rules! schema_marker {
    ($(#[$meta:meta])* $name:ident = $value:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
        pub struct $name;

        impl $name {
            /// The schema string this marker stands for.
            pub const NAME: &'static str = $value;
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str($value)
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let found = String::deserialize(deserializer)?;
                if found == $value {
                    Ok($name)
                } else {
                    Err(::serde::de::Error::custom(format!(
                        "expected schema {}, found {found}",
                        $value
                    )))
                }
            }
        }
    };
}

schema_marker!(
    /// `cardinal.error.v1`
    ErrorSchema = "cardinal.error.v1"
);
schema_marker!(
    /// `cardinal.run-spec.v1`
    RunSpecSchema = "cardinal.run-spec.v1"
);
schema_marker!(
    /// `cardinal.reservation.v1`
    ReservationSchema = "cardinal.reservation.v1"
);
schema_marker!(
    /// `cardinal.inventory.v1`
    InventorySchema = "cardinal.inventory.v1"
);
schema_marker!(
    /// `cardinal.plan.v1`
    PlanSchema = "cardinal.plan.v1"
);
schema_marker!(
    /// `cardinal.plan-list.v1`
    PlanListSchema = "cardinal.plan-list.v1"
);
schema_marker!(
    /// `cardinal.act-result.v2`: version 2 since a dry run's `freed_bytes` became the lower
    /// bound of section 8.5 (unit X1a)
    ActResultSchema = "cardinal.act-result.v2"
);
schema_marker!(
    /// `cardinal.act-claim.v1`
    ActClaimSchema = "cardinal.act-claim.v1"
);
schema_marker!(
    /// `cardinal.act-manifest.v1`
    ActManifestSchema = "cardinal.act-manifest.v1"
);
schema_marker!(
    /// `cardinal.inbox-item.v1`
    InboxItemSchema = "cardinal.inbox-item.v1"
);
schema_marker!(
    /// `cardinal.inbox.v1`
    InboxSchema = "cardinal.inbox.v1"
);
schema_marker!(
    /// `cardinal.inbox-brief.v1`
    InboxBriefSchema = "cardinal.inbox-brief.v1"
);
schema_marker!(
    /// `cardinal.worktree.v1`
    WorktreeSchema = "cardinal.worktree.v1"
);
schema_marker!(
    /// `cardinal.explain.v1`
    ExplainSchema = "cardinal.explain.v1"
);
schema_marker!(
    /// `cardinal.record.v1`
    RecordSchema = "cardinal.record.v1"
);
schema_marker!(
    /// `cardinal.session-snapshot.v1`
    SessionSnapshotSchema = "cardinal.session-snapshot.v1"
);
schema_marker!(
    /// `cardinal.disk-state.v1`
    DiskStateSchema = "cardinal.disk-state.v1"
);
schema_marker!(
    /// `cardinal.reconcile.v1`
    ReconcileSchema = "cardinal.reconcile.v1"
);
schema_marker!(
    /// `cardinal.reconcile-state.v1`
    ReconcileStateSchema = "cardinal.reconcile-state.v1"
);
schema_marker!(
    /// `cardinal.arbiter-pass.v1`
    ArbiterPassSchema = "cardinal.arbiter-pass.v1"
);
schema_marker!(
    /// `cardinal.snapshot.v1`
    SnapshotSchema = "cardinal.snapshot.v1"
);
schema_marker!(
    /// `cardinal.classify-request.v1`
    ClassifyRequestSchema = "cardinal.classify-request.v1"
);
schema_marker!(
    /// `cardinal.classify-response.v1`
    ClassifyResponseSchema = "cardinal.classify-response.v1"
);

/// A field that is present, null or not: `Some(None)` for null. An absent field is `None`
/// through `#[serde(default)]`.
#[expect(
    clippy::option_option,
    reason = "the contract tells an absent field from a null one (`head?: string | null`)"
)]
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(deserializer).map(Some)
}

/// A tri-state fact: `"yes" | "no" | "unknown"`. `Unknown` is never treated as safe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tri {
    Yes,
    No,
    Unknown,
}

/// Whether a failing check makes `act` refuse for every caller or only requires approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckRole {
    Invariant,
    Condition,
}

/// The outcome of one check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckResult {
    Pass,
    Fail,
    Unknown,
}

/// One plan or act check (`docs/architecture.md` section 9.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub role: CheckRole,
    pub result: CheckResult,
    pub detail: String,
    /// The facts the check read, each with where it came from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<CheckFact>,
}

/// A fact a check read (`docs/architecture.md` section 10.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckFact {
    /// The observation field or input, such as `worktree.facts.dirty`.
    pub name: String,
    /// Its value; null when it was not observed.
    pub value: serde_json::Value,
    /// What produced it: the tool behind the observation (`git`, `gh`) or the local read.
    pub source: String,
    /// For a list `value` printed cut to its first entries (`plan reclaim-worktree`, `act`),
    /// how many entries were left out; absent when nothing was, and in every stored plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted: Option<u64>,
}

/// The process group a job runs in. The leader is the runner. Phase 3 adds a Job Object kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GroupIdentity {
    Pgid { pgid: u32, leader_start_s: u64 },
}

/// An error attributed to a part of a larger observation (`{ scope, message }`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedError {
    pub scope: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::{ErrorSchema, GroupIdentity, Tri};

    #[test]
    fn schema_marker_round_trips_and_rejects_other_schemas() {
        assert_eq!(
            serde_json::to_string(&ErrorSchema).unwrap(),
            "\"cardinal.error.v1\""
        );
        assert!(serde_json::from_str::<ErrorSchema>("\"cardinal.error.v1\"").is_ok());
        let err = serde_json::from_str::<ErrorSchema>("\"cardinal.error.v2\"").unwrap_err();
        assert!(
            err.to_string()
                .contains("expected schema cardinal.error.v1")
        );
    }

    #[test]
    fn enums_are_snake_case() {
        assert_eq!(serde_json::to_string(&Tri::Unknown).unwrap(), "\"unknown\"");
        let group = GroupIdentity::Pgid {
            pgid: 42,
            leader_start_s: 7,
        };
        assert_eq!(
            serde_json::to_value(group).unwrap(),
            serde_json::json!({"kind": "pgid", "pgid": 42, "leader_start_s": 7})
        );
    }
}
