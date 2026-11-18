//! The read-only web UI (`docs/architecture.md` section 10.6): `cardinal.snapshot.v1` and the
//! self-contained page `ui/index.html` that shows it, written by every `reconcile` pass and
//! by `cardinal ui` from what the state directory holds, with no git, gh, or network call.
//!
//! The snapshot is read from the files a pass has already written: the disk states, the last
//! full observation, the inbox, the records, and the last arbiter pass. Only the reservations
//! are read again, under the admission lock (section 4.3), with their states derived from the
//! process table once it is released, because their derived state is never stored. A part that cannot be read, or is read only in part,
//! is recorded in the snapshot's `errors` under the part's name and shown as incomplete; it
//! never ends the snapshot. [`page`] renders it.

mod page;

pub use page::{CONTENT_SECURITY_POLICY, PageOptions, render};

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;

use crate::admission;
use crate::config::Config;
use crate::contract::{
    ArbiterPass, DiskState, Error, ErrorCode, InboxItem, InboxStatus, Inventory, Record,
    RecordKind, ScopedError, Snapshot, SnapshotItem, SnapshotObservation, SnapshotReservations,
    SnapshotSchema,
};
use crate::observe::Observer;
use crate::record;
use crate::state::{self, Durability, LockMode, StateDir};
use crate::testpoint;

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

/// The parts that could not be read, or were read only in part, as they are found.
#[derive(Default)]
struct Errors(Vec<ScopedError>);

impl Errors {
    fn add(&mut self, scope: &str, message: impl Into<String>) {
        self.0.push(ScopedError {
            scope: scope.to_owned(),
            message: message.into(),
        });
    }
}

/// The snapshot of this host's state: disk states, the last full observation,
/// reservations, open and escalated inbox items, recent records, and the last arbiter pass.
/// A part that cannot be read is empty or absent, with its error in `errors`.
pub fn build(config: &Config, state: &StateDir) -> Result<Snapshot, Error> {
    let mut errors = Errors::default();
    let host = Observer::host_info(state, &mut errors.0);
    let disk = disk_states(state, &mut errors);
    let observation =
        read_part::<Inventory>(&state.reconcile_observation(), "observation", &mut errors).map(
            |inventory| SnapshotObservation {
                observed_ms: inventory.observed_ms,
                repos: inventory.repos,
                errors: inventory.errors,
            },
        );
    let reservations = reservations(config, state, &mut errors);
    let inbox = pending_items(config, state, &mut errors);
    let records = recent_records(state, &mut errors);
    let arbiter = read_part::<ArbiterPass>(&state.arbiter_last(), "arbiter", &mut errors);
    Ok(Snapshot {
        schema: SnapshotSchema,
        generated_ms: state::now_ms(),
        host,
        disk,
        observation,
        reservations,
        inbox,
        records,
        arbiter,
        errors: errors.0,
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

/// Every disk state in `disk/` that reads, by mount.
fn disk_states(state: &StateDir, errors: &mut Errors) -> Vec<DiskState> {
    let mut states: Vec<DiskState> = json_files(&state.disk_dir(), "disk", errors)
        .into_iter()
        .filter_map(|path| read_listed::<DiskState>(&path, "disk", errors))
        .collect();
    states.sort_by(|a, b| (&a.mount, &a.volume).cmp(&(&b.mount, &b.volume)));
    states
}

/// The reservations with their derived state, and each reservation file that does not read
/// as an error, both from one listing of `reservations/` under the admission lock (section
/// 4.3). The states are derived once the lock is released.
fn reservations(config: &Config, state: &StateDir, errors: &mut Errors) -> SnapshotReservations {
    const SCOPE: &str = "reservations";
    let read = admission::snapshot_with_unread(state, &config.host);
    let observed_ms = state::now_ms();
    let mut found = match read {
        Ok((found, unread)) => {
            for message in unread {
                errors.add(SCOPE, message);
            }
            found
        }
        Err(err) => {
            errors.add(SCOPE, format!("the reservations could not be read: {err}"));
            Vec::new()
        }
    };
    if let Err(err) = testpoint::hit(testpoint::SNAPSHOT_RESERVATIONS_READ) {
        errors.add(SCOPE, err.to_string());
        found.clear();
    }
    SnapshotReservations {
        observed_ms,
        items: admission::views(state, found),
    }
}

/// The open and escalated items, those for the owner first, then by creation. `inbox/` is
/// listed under `inbox/.lock`, held shared, so no upsert renames an item during the listing.
fn pending_items(config: &Config, state: &StateDir, errors: &mut Errors) -> Vec<SnapshotItem> {
    const SCOPE: &str = "inbox";
    let deadline = Instant::now() + Duration::from_millis(config.host.admission.lock_timeout_ms);
    let _no_upsert = match state::lock(&state.inbox_lock(), LockMode::Shared, deadline) {
        Ok(lock) => lock,
        Err(err) => {
            errors.add(SCOPE, format!("the inbox could not be read: {err}"));
            return Vec::new();
        }
    };
    let mut items: Vec<SnapshotItem> = json_files(&state.inbox_dir(), SCOPE, errors)
        .into_iter()
        .filter_map(|path| read_listed::<InboxItem>(&path, SCOPE, errors))
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
    items
}

/// The newest [`RECORDS_SHOWN`] `act`, `planned`, and `inbox_*` records, oldest first, from
/// the record files `records/` holds, newest first; no older file is read once enough are
/// found. The files are opened with their lengths taken under `records/.lock`, held shared as
/// an appender holds it, so no record is read twice, and read up to those lengths once it is
/// released, so no appender or rotation waits for the scan. A listed file that cannot be read
/// is an error, as is one that could not be opened past the newest records, and a line that
/// does not read is counted.
fn recent_records(state: &StateDir, errors: &mut Errors) -> Vec<Record> {
    const SCOPE: &str = "records";
    let files = match record::open_files(state, Instant::now() + RECORDS_WAIT) {
        Ok(files) => files,
        Err(err) => {
            errors.add(SCOPE, format!("the records could not be read: {err}"));
            return Vec::new();
        }
    };
    if let Err(err) = testpoint::hit(testpoint::SNAPSHOT_RECORDS_OPENED) {
        errors.add(SCOPE, err.to_string());
        return Vec::new();
    }
    let mut newest_first = Vec::with_capacity(RECORDS_SHOWN);
    let mut files = files.iter();
    for file in files.by_ref() {
        let (path, bytes) = match file {
            Ok(file) => match file.read() {
                Ok(bytes) => (&file.path, bytes),
                Err(err) => {
                    errors.add(SCOPE, format!("{}: {err}", file.path.display()));
                    continue;
                }
            },
            Err(err) => {
                errors.add(SCOPE, err.to_string());
                continue;
            }
        };
        let mut unreadable = 0;
        let mut shown = Vec::new();
        for line in bytes.split(|&b| b == b'\n') {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match serde_json::from_slice::<Record>(line) {
                Ok(record) if is_shown(&record) => shown.push(record),
                Ok(_) => {}
                Err(_) => unreadable += 1,
            }
        }
        if unreadable > 0 {
            let lines = if unreadable == 1 { "line" } else { "lines" };
            errors.add(
                SCOPE,
                format!("{}: {unreadable} {lines} could not be read", path.display()),
            );
        }
        let missing = RECORDS_SHOWN - newest_first.len();
        newest_first.extend(shown.into_iter().rev().take(missing));
        if newest_first.len() == RECORDS_SHOWN {
            break;
        }
    }
    // An older file is not read once enough records are found, but one that could not be
    // opened is still known not to read.
    for err in files.filter_map(|file| file.as_ref().err()) {
        errors.add(SCOPE, err.to_string());
    }
    newest_first.reverse();
    newest_first
}

fn is_shown(record: &Record) -> bool {
    matches!(
        record.kind,
        RecordKind::Act | RecordKind::Planned | RecordKind::InboxRaised | RecordKind::InboxResolved
    )
}

/// The `*.json` files in `dir`, in name order; a missing directory has none.
fn json_files(dir: &Path, scope: &str, errors: &mut Errors) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            errors.add(scope, format!("{}: {err}", dir.display()));
            return Vec::new();
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) if entry.path().extension().is_some_and(|ext| ext == "json") => {
                paths.push(entry.path());
            }
            Ok(_) => {}
            Err(err) => errors.add(scope, format!("{}: {err}", dir.display())),
        }
    }
    paths.sort();
    paths
}

/// A document that may not exist yet (the observation, the arbiter's last pass): `None` when
/// it is missing, and `None` with an error under `scope` when it cannot be read or does not
/// parse.
fn read_part<T: DeserializeOwned>(path: &Path, scope: &str, errors: &mut Errors) -> Option<T> {
    match std::fs::read(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        read => parse(path, read, scope, errors),
    }
}

/// A document a listing of its directory found: `None` with an error under `scope` when it
/// cannot be read, a missing one included, or does not parse.
fn read_listed<T: DeserializeOwned>(path: &Path, scope: &str, errors: &mut Errors) -> Option<T> {
    parse(path, std::fs::read(path), scope, errors)
}

fn parse<T: DeserializeOwned>(
    path: &Path,
    read: io::Result<Vec<u8>>,
    scope: &str,
    errors: &mut Errors,
) -> Option<T> {
    let parsed = read
        .map_err(|err| err.to_string())
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|err| err.to_string()));
    match parsed {
        Ok(doc) => Some(doc),
        Err(err) => {
            errors.add(scope, format!("{}: {err}", path.display()));
            None
        }
    }
}
