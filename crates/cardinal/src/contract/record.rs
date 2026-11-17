use serde::{Deserialize, Serialize};

use super::RecordSchema;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    JobAdmitted,
    JobStarted,
    JobFinished,
    JobFailOpen,
    JobInfraFailure,
    JobRecovered,
    InUseUnprotected,
    TtyPassthrough,
    SessionEnded,
    SubagentStopped,
    PlanCreated,
    Act,
    Planned,
    InboxRaised,
    InboxResolved,
    HookError,
    ConfigError,
}

/// `cardinal.record.v1`: one line in `records/*.jsonl`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub schema: RecordSchema,
    pub ts_ms: u64,
    pub host_id: String,
    pub kind: RecordKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inbox_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Kind-specific. `job_finished` has `exit`, `signal`, `waited_ms`, `admission`, and
    /// `measurements` (`[{ "kind": "wall_time", "ms": … }]` in phase 0).
    pub data: serde_json::Value,
}

impl Record {
    /// A record with no IDs and empty data.
    pub fn new(kind: RecordKind, host_id: impl Into<String>, ts_ms: u64) -> Self {
        Self {
            schema: RecordSchema,
            ts_ms,
            host_id: host_id.into(),
            kind,
            job_id: None,
            plan_id: None,
            inbox_key: None,
            session_id: None,
            data: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Record, RecordKind};

    #[test]
    fn absent_ids_are_omitted() {
        let mut record = Record::new(RecordKind::JobFinished, "00000000000000aa", 5);
        record.job_id = Some("j".into());
        assert_eq!(
            serde_json::to_value(&record).unwrap(),
            serde_json::json!({
                "schema": "cardinal.record.v1", "ts_ms": 5, "host_id": "00000000000000aa",
                "kind": "job_finished", "job_id": "j", "data": {}
            })
        );
    }
}
