use serde::{Deserialize, Serialize};

use super::SessionSnapshotSchema;

/// The agent process of a session: the nearest ancestor of the hook that is not a shell.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotAgent {
    pub pid: u32,
    pub start_s: u64,
    pub name: String,
}

/// A process listed in a session snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionProcess {
    pub pid: u32,
    pub start_s: u64,
    pub name: String,
    /// The first 256 bytes of the command line.
    pub command: String,
}

/// `cardinal.session-snapshot.v1` (`sessions/<session>.json`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub schema: SessionSnapshotSchema,
    pub session_id: String,
    pub reason: String,
    pub ended_ms: u64,
    pub agent: SnapshotAgent,
    pub processes: Vec<SessionProcess>,
}
