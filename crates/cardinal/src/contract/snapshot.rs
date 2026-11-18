use serde::{Deserialize, Serialize};

use super::{
    ArbiterPass, DiskState, HostInfo, InboxItem, Record, Repo, ReservationView, ScopedError,
    SnapshotSchema,
};

/// The last full observation, with its time and what it could not observe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotObservation {
    pub observed_ms: u64,
    pub repos: Vec<Repo>,
    /// The observation's own errors (`Inventory.errors`). Absent in a snapshot written before
    /// it was recorded, which reads as none.
    #[serde(default)]
    pub errors: Vec<ScopedError>,
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
    /// The parts that could not be read, or were read only in part, each under the part's
    /// name: `host`, `disk`, `observation`, `reservations`, `inbox`, `records`, or
    /// `arbiter`. A part with an error is incomplete, never empty for lack of data. Absent in a
    /// snapshot written before it was recorded, which reads as none.
    #[serde(default)]
    pub errors: Vec<ScopedError>,
}
