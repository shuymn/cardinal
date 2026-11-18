//! The read-only web UI (`docs/architecture.md` section 10.6): `cardinal.snapshot.v1` and the
//! self-contained page `ui/index.html` that shows it, written by every `reconcile` pass and
//! by `cardinal ui` from what the state directory holds, with no git, gh, or network call.
//!
//! The snapshot is read from the files a pass has already written: the disk states, the last
//! full observation, the inbox, the records, and the last arbiter pass. Only the reservations
//! are read again, under the admission lock with the process table (section 4.3), because
//! their derived state is never stored. [`page`] renders it.

mod page;

pub use page::{CONTENT_SECURITY_POLICY, PageOptions, render};

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;

use crate::admission;
use crate::config::Config;
use crate::contract::{
    ArbiterPass, DiskState, Error, ErrorCode, HostInfo, InboxItem, InboxStatus, Inventory, Os,
    Record, RecordKind, Snapshot, SnapshotItem, SnapshotObservation, SnapshotReservations,
    SnapshotSchema,
};
use crate::inbox;
use crate::os;
use crate::record;
use crate::state::{self, Durability, LockMode, StateDir};

/// How many `act`, `planned`, and `inbox_*` records the snapshot holds, the newest.
pub const RECORDS_SHOWN: usize = 50;

/// How long the snapshot waits for a rotation of the record files to finish, as long as an
/// appender waits.
const RECORDS_WAIT: Duration = Duration::from_secs(1);

impl PageOptions {
    /// Twice the full-observation interval (`[reconcile] observe_every_s`): an observation
    /// is due once per interval, so one older than two has been missed.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let stale_after = Duration::from_secs(config.host.reconcile.observe_every_s)
            .saturating_mul(2)
            .as_millis();
        Self {
            stale_after_ms: u64::try_from(stale_after).unwrap_or(u64::MAX),
        }
    }
}

/// The snapshot of this host's state: disk states, the last full observation,
/// reservations, open and escalated inbox items, recent records, and the last arbiter pass.
pub fn build(config: &Config, state: &StateDir) -> Result<Snapshot, Error> {
    let disk = disk_states(state)?;
    let observation = read_doc::<Inventory>(&state.reconcile_observation())?.map(|inventory| {
        SnapshotObservation {
            observed_ms: inventory.observed_ms,
            repos: inventory.repos,
        }
    });
    let items = admission::list_reservations(state, &config.host)?;
    let reservations = SnapshotReservations {
        observed_ms: state::now_ms(),
        items,
    };
    let inbox = pending_items(state)?;
    let records = recent_records(state)?;
    let arbiter = read_doc::<ArbiterPass>(&state.arbiter_last())?;
    Ok(Snapshot {
        schema: SnapshotSchema,
        generated_ms: state::now_ms(),
        host: host_info(state),
        disk,
        observation,
        reservations,
        inbox,
        records,
        arbiter,
    })
}

/// Writes `ui/snapshot.json` and `ui/index.html` and returns the page's path.
pub fn write_page(config: &Config, state: &StateDir) -> Result<PathBuf, Error> {
    let snapshot = build(config, state)?;
    let json = serde_json::to_vec(&snapshot)
        .map_err(|err| Error::new(ErrorCode::Internal, err.to_string()))?;
    state.write_atomic(&state.ui_snapshot(), &json, Durability::Volatile)?;
    let page = render(&snapshot, PageOptions::from_config(config))?;
    let path = state.ui_page();
    state.write_atomic(&path, page.as_bytes(), Durability::Volatile)?;
    Ok(path)
}

/// Whether only the owner can answer an item: an escalated item, or one of the kinds only
/// the owner can act on (section 10.6).
fn needs_owner(item: &InboxItem) -> bool {
    item.status == InboxStatus::Escalated || item.kind.is_owners()
}

/// Every disk state in `disk/` that parses, by mount.
fn disk_states(state: &StateDir) -> Result<Vec<DiskState>, Error> {
    let dir = state.disk_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(state_error(&dir, &err)),
    };
    let mut states = Vec::new();
    for entry in entries {
        let path = entry.map_err(|err| state_error(&dir, &err))?.path();
        if path.extension().is_some_and(|ext| ext == "json")
            && let Some(disk) = read_doc::<DiskState>(&path)?
        {
            states.push(disk);
        }
    }
    states.sort_by(|a, b| (&a.mount, &a.volume).cmp(&(&b.mount, &b.volume)));
    Ok(states)
}

/// The open and escalated items, those for the owner first, then by creation.
fn pending_items(state: &StateDir) -> Result<Vec<SnapshotItem>, Error> {
    let mut items: Vec<SnapshotItem> = inbox::read_all(state)?
        .into_iter()
        .map(|(_, item)| item)
        .filter(|item| matches!(item.status, InboxStatus::Open | InboxStatus::Escalated))
        .map(|item| SnapshotItem {
            needs_owner: needs_owner(&item),
            item,
        })
        .collect();
    items.sort_by(|a, b| {
        (!a.needs_owner, a.item.created_ms, &a.item.key).cmp(&(
            !b.needs_owner,
            b.item.created_ms,
            &b.item.key,
        ))
    });
    Ok(items)
}

/// The newest [`RECORDS_SHOWN`] `act`, `planned`, and `inbox_*` records, oldest first, from
/// the record files `records/` holds, newest first; no older file is read once enough are
/// found. `records/.lock` is held shared, as an appender holds it, so no rotation renames a
/// file during the scan and no record is read twice.
fn recent_records(state: &StateDir) -> Result<Vec<Record>, Error> {
    let dir = state.records_dir();
    let _no_rotation = state::lock(
        &state.records_lock(),
        LockMode::Shared,
        Instant::now() + RECORDS_WAIT,
    )?;
    let files = record::files(state).map_err(|err| state_error(&dir, &err))?;
    let mut newest_first = Vec::with_capacity(RECORDS_SHOWN);
    for path in files.iter().rev() {
        let missing = RECORDS_SHOWN - newest_first.len();
        newest_first.extend(
            state::read_jsonl::<Record>(path)
                .map_err(|err| state_error(path, &err))?
                .into_iter()
                .rev()
                .filter(|record| {
                    matches!(
                        record.kind,
                        RecordKind::Act
                            | RecordKind::Planned
                            | RecordKind::InboxRaised
                            | RecordKind::InboxResolved
                    )
                })
                .take(missing),
        );
        if newest_first.len() == RECORDS_SHOWN {
            break;
        }
    }
    newest_first.reverse();
    Ok(newest_first)
}

/// This host, as `inventory` reports it; a boot time that cannot be read is 0.
fn host_info(state: &StateDir) -> HostInfo {
    let os = if cfg!(target_os = "macos") {
        Os::Macos
    } else if cfg!(windows) {
        Os::Windows
    } else {
        Os::Linux
    };
    HostInfo {
        id: state.host_id().to_owned(),
        hostname: os::hostname().unwrap_or_default(),
        os,
        arch: std::env::consts::ARCH.to_owned(),
        boot_s: os::boot_time_s().unwrap_or(0),
    }
}

/// A document of the state directory: `None` when it is missing or does not parse, as
/// readers skip what does not parse (section 4.7).
fn read_doc<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, Error> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(state_error(path, &err)),
    }
}

fn state_error(path: &Path, err: &io::Error) -> Error {
    Error::new(
        ErrorCode::StateUnavailable,
        format!("{}: {err}", path.display()),
    )
}
