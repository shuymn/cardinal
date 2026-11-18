use serde::{Deserialize, Serialize};

use super::{
    ArbiterPass, DiskState, HostInfo, InboxItem, Record, Repo, ReservationView, SnapshotSchema,
};

/// The last full observation, with its time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotObservation {
    pub observed_ms: u64,
    pub repos: Vec<Repo>,
}

/// The reservations with their derived state, with the time they were read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotReservations {
    pub observed_ms: u64,
    pub items: Vec<ReservationView>,
}

/// An open or escalated inbox item, marked when only the owner can act on it (section 10.6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnapshotItem {
    #[serde(flatten)]
    pub item: InboxItem,
    pub needs_owner: bool,
}

/// `cardinal.snapshot.v1` (`ui/snapshot.json`, embedded in `ui/index.html`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema: SnapshotSchema,
    pub generated_ms: u64,
    pub host: HostInfo,
    pub disk: Vec<DiskState>,
    pub observation: Option<SnapshotObservation>,
    pub reservations: SnapshotReservations,
    pub inbox: Vec<SnapshotItem>,
    /// The last 50 `act`, `planned`, and `inbox_*` records.
    pub records: Vec<Record>,
    pub arbiter: Option<ArbiterPass>,
}
