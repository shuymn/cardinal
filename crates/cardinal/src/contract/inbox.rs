use serde::{Deserialize, Serialize};

use super::{InboxItemSchema, InboxSchema};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxKind {
    RecoveryNeeded,
    OutlivedProcesses,
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

/// What an item is about. Every field is optional and omitted when absent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxSubject {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    pub outcome: ResolutionOutcome,
    pub by: String,
    pub note: String,
    pub at_ms: u64,
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
    pub evidence: serde_json::Value,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub seen: u64,
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
    use super::InboxSubject;

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
}
