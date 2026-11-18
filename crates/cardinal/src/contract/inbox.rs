use serde::{Deserialize, Serialize};

use super::{InboxItemSchema, InboxSchema, PassOutcome, present};

/// The item kinds of `docs/architecture.md` section 10.1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxKind {
    RecoveryNeeded,
    OutlivedProcesses,
    Reclaimable,
    MergeUndecided,
    ClosedPrDirty,
    ManagedMerged,
    UnclassifiableData,
    DiskLow,
    DiskCritical,
    ArbiterFailing,
    /// An execution left data in the trash (section 9.7).
    ReclaimStranded,
}

impl InboxKind {
    /// Kinds only the owner can act on: they are never due for an arbiter pass and are
    /// marked for the owner on the page (sections 10.5 and 10.6).
    #[must_use]
    pub fn is_owners(self) -> bool {
        matches!(
            self,
            Self::DiskLow | Self::DiskCritical | Self::ArbiterFailing | Self::ReclaimStranded
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxStatus {
    Open,
    Escalated,
    Resolved,
    Dismissed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionOutcome {
    Resolved,
    Escalated,
    Dismissed,
}

/// What an item is about. Every field is optional and omitted when absent; `head` and
/// `branch`, a worktree item's identity as raised, may also be present and null (a detached
/// or unborn HEAD).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxSubject {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub head: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub branch: Option<Option<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    pub outcome: ResolutionOutcome,
    pub by: String,
    pub note: String,
    pub at_ms: u64,
}

/// The last arbiter pass an item was assigned to (section 10.5); `outcome` and `ended_ms`
/// are set when that pass ends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemArbiter {
    pub pass: String,
    pub assigned_ms: u64,
    pub outcome: Option<PassOutcome>,
    pub ended_ms: Option<u64>,
}

/// `cardinal.inbox-item.v1` (`inbox/<key-hash>.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InboxItem {
    pub schema: InboxItemSchema,
    pub key: String,
    pub kind: InboxKind,
    pub status: InboxStatus,
    pub subject: InboxSubject,
    /// The observation that raised the item.
    pub evidence: serde_json::Map<String, serde_json::Value>,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub seen: u64,
    /// When the occurrence ended: a raiser observed its clearing condition (section 10.1).
    /// Absent in items stored before occurrences, which read as null.
    #[serde(default)]
    pub ended_ms: Option<u64>,
    /// Absent in items stored before arbiter passes, which read as null.
    #[serde(default)]
    pub arbiter: Option<ItemArbiter>,
    pub resolution: Option<Resolution>,
}

/// `cardinal.inbox.v1`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Inbox {
    pub schema: InboxSchema,
    pub items: Vec<InboxItem>,
}

#[cfg(test)]
mod tests {
    use super::{InboxKind, InboxSubject};

    #[test]
    fn absent_subject_fields_are_omitted() {
        let subject = InboxSubject {
            job_id: Some("j".into()),
            ..InboxSubject::default()
        };
        assert_eq!(
            serde_json::to_value(subject).unwrap(),
            serde_json::json!({"job_id": "j"})
        );
    }

    #[test]
    fn a_null_head_is_kept_apart_from_an_absent_one() {
        let subject: InboxSubject =
            serde_json::from_value(serde_json::json!({"worktree": "/w", "head": null})).unwrap();
        assert_eq!(subject.head, Some(None));
        assert_eq!(subject.branch, None);
        assert_eq!(
            serde_json::to_value(&subject).unwrap(),
            serde_json::json!({"worktree": "/w", "head": null})
        );
    }

    #[test]
    fn only_disk_arbiter_and_stranded_items_are_the_owners() {
        assert!(InboxKind::DiskLow.is_owners());
        assert!(InboxKind::DiskCritical.is_owners());
        assert!(InboxKind::ArbiterFailing.is_owners());
        assert!(InboxKind::ReclaimStranded.is_owners());
        assert!(!InboxKind::Reclaimable.is_owners());
        assert!(!InboxKind::RecoveryNeeded.is_owners());
    }
}
