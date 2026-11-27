use serde::{Deserialize, Deserializer, Serialize};

use super::SessionLeaseSchema;

/// `cardinal.session-lease.v1` (`leases/<key>.json`, `docs/architecture.md` section 9.8): the
/// sessions that hold one worktree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLease {
    pub schema: SessionLeaseSchema,
    /// The worktree's canonical root.
    pub worktree: String,
    /// Live or undecidable entries alone passed [`MAX_ENTRIES`], so the file holds more; readers
    /// take the worktree as unknown. Written only as `true`.
    #[serde(
        default,
        skip_serializing_if = "std::ops::Not::not",
        deserialize_with = "only_true"
    )]
    pub overflow: bool,
    pub sessions: Vec<LeaseEntry>,
}

/// The most entries a lease file holds unless it is marked `overflow`.
pub const MAX_ENTRIES: usize = 1024;

/// One session's entry in a lease file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseEntry {
    pub session_id: String,
    /// The session's agent process, its generation; null when the hook found no Claude Code
    /// process, and then the entry only lingers. The key is required, null included.
    #[serde(deserialize_with = "Option::deserialize")]
    pub agent: Option<LeaseAgent>,
    /// Stands for the lost entries of a corrupt file: unknown until `linger_s` after
    /// `last_event_ms`, then dropped. Written only as `true`.
    #[serde(
        default,
        skip_serializing_if = "std::ops::Not::not",
        deserialize_with = "only_true"
    )]
    pub uncertain: bool,
    /// Required, null included.
    #[serde(deserialize_with = "Option::deserialize")]
    pub transcript_path: Option<String>,
    pub last_event: LeaseEvent,
    pub last_event_ms: u64,
    /// Required, null included.
    #[serde(deserialize_with = "Option::deserialize")]
    pub ended_ms: Option<u64>,
}

/// An agent process's generation: its PID and start time in the boot its boot ID names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseAgent {
    pub pid: u32,
    /// The start time in Unix seconds, as the process table reports it: with the PID, the
    /// process's identity.
    pub start_s: u64,
    /// The start time at the OS's finest resolution, in microseconds, which orders two starts
    /// within one second; compared only within one boot ID: since the epoch on macOS
    /// (`pbi_start_tvsec` and `pbi_start_tvusec`, 1 µs), since boot on Linux (`/proc/<pid>/stat`
    /// `starttime`, in clock ticks of usually 10 ms). Null when it cannot be read. The key is
    /// required, null included.
    #[serde(deserialize_with = "Option::deserialize")]
    pub start_us: Option<u64>,
    pub boot_id: String,
}

/// The hook event that wrote an entry last.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseEvent {
    SessionStart,
    SubagentStop,
    SessionEnd,
}

/// A marker field present only as `true`.
fn only_true<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    if bool::deserialize(deserializer)? {
        Ok(true)
    } else {
        Err(serde::de::Error::custom("expected true or an absent field"))
    }
}
