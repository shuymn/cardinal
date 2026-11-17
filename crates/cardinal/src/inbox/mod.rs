//! The inbox: store, dedup, and refresh (`docs/architecture.md` sections 4.8 and 10.1).
//!
//! One file per item, `inbox/<key-hash>.json`, written durably under `inbox/.lock`. Raising
//! a key that exists updates the item while it is open or escalated and never reopens it once
//! resolved or dismissed (C19). The refresh that `cardinal inbox` runs first is the only
//! raiser in phase 0: it runs the recovery pass, then raises and clears `recovery_needed` and
//! `outlived_processes` items. A refresh clears only items it saw before it started observing,
//! and only while they are unchanged, so it never closes an item another refresh raised or
//! updated from a newer observation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::admission::{self, RecoveryReport};
use crate::config::{Config, HostConfig};
use crate::contract::{
    DerivedState, Error, ErrorCode, Inbox, InboxItem, InboxItemSchema, InboxKind, InboxSchema,
    InboxStatus, InboxSubject, RecordKind, Resolution, ResolutionOutcome, SessionSnapshot,
};
use crate::os::{Detail, ProcessTable};
use crate::record::Recorder;
use crate::state::{self, Durability, LockMode, StateDir};
use crate::testpoint;

/// How long an upsert waits for `inbox/.lock`.
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// How many collision slots of one key hash are probed.
const MAX_PROBES: u32 = 64;

/// Who resolves items through `cardinal inbox resolve`.
const BY_CLI: &str = "cli";

/// Who resolves items the refresh clears.
const BY_CARDINAL: &str = "cardinal";

/// An item to raise. Raising a key that exists updates `seen` and the evidence of an open or
/// escalated item and leaves resolved and dismissed items alone.
#[derive(Clone, Debug, PartialEq)]
pub struct NewItem {
    pub key: String,
    pub kind: InboxKind,
    pub subject: InboxSubject,
    pub evidence: serde_json::Map<String, serde_json::Value>,
}

/// What a raise did to the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Raised {
    /// A new item was created.
    New,
    /// An open or escalated item got its `seen` and evidence updated.
    Updated,
    /// A resolved or dismissed item was left alone.
    Unchanged,
}

/// Raises or updates an item; the one function refresh and (from phase 1) units raise through.
pub fn raise(state: &StateDir, item: &NewItem) -> Result<InboxItem, Error> {
    raise_item(state, item).map(|(item, _)| item)
}

/// [`raise`], also saying what changed.
pub fn raise_item(state: &StateDir, item: &NewItem) -> Result<(InboxItem, Raised), Error> {
    let _lock = lock(state)?;
    let (path, existing) = find(state, &item.key)?;
    let now = state::now_ms();
    let (stored, raised) = match existing {
        Some(existing)
            if matches!(
                existing.status,
                InboxStatus::Resolved | InboxStatus::Dismissed
            ) =>
        {
            return Ok((existing, Raised::Unchanged));
        }
        Some(mut existing) => {
            existing.seen += 1;
            existing.evidence.clone_from(&item.evidence);
            existing.updated_ms = now;
            (existing, Raised::Updated)
        }
        None => (
            InboxItem {
                schema: InboxItemSchema,
                key: item.key.clone(),
                kind: item.kind,
                status: InboxStatus::Open,
                subject: item.subject.clone(),
                evidence: item.evidence.clone(),
                created_ms: now,
                updated_ms: now,
                seen: 1,
                resolution: None,
            },
            Raised::New,
        ),
    };
    write(state, &path, &stored)?;
    Ok((stored, raised))
}

/// `--status`: one status, or `all`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusFilter {
    Only(InboxStatus),
    All,
}

/// `cardinal inbox [--status …] [--no-refresh]`. `status` is `None` without `--status`,
/// which lists the pending items: open and escalated.
pub fn list(
    config: &Config,
    state: &StateDir,
    status: Option<StatusFilter>,
    refresh: bool,
) -> Result<Inbox, Error> {
    list_with(config, state, status, refresh, &admission::recovery_pass)
}

/// The recovery pass the refresh runs first.
pub type RecoveryPass<'a> = dyn Fn(&StateDir, &HostConfig) -> Result<RecoveryReport, Error> + 'a;

/// [`list`] with the given recovery pass.
pub fn list_with(
    config: &Config,
    state: &StateDir,
    status: Option<StatusFilter>,
    refresh: bool,
    recovery: &RecoveryPass<'_>,
) -> Result<Inbox, Error> {
    let _ = state.gc_tmp();
    if refresh {
        refresh_with(config, state, recovery)?;
    }
    let mut items: Vec<InboxItem> = read_all(state)?
        .into_iter()
        .map(|(_, item)| item)
        .filter(|item| match status {
            None => matches!(item.status, InboxStatus::Open | InboxStatus::Escalated),
            Some(StatusFilter::All) => true,
            Some(StatusFilter::Only(wanted)) => item.status == wanted,
        })
        .collect();
    items.sort_by(|a, b| (a.created_ms, &a.key).cmp(&(b.created_ms, &b.key)));
    Ok(Inbox {
        schema: InboxSchema,
        items,
    })
}

/// `cardinal inbox resolve KEY --outcome … [--note TEXT]`. Only open and escalated items
/// can be resolved.
pub fn resolve(
    config: &Config,
    state: &StateDir,
    key: &str,
    outcome: ResolutionOutcome,
    note: Option<&str>,
) -> Result<InboxItem, Error> {
    let item = close(state, key, outcome, BY_CLI, note.unwrap_or_default(), None)?;
    record_resolution(config, state, &item);
    Ok(item)
}

/// Runs the recovery pass, then raises and clears the built-in items (section 10.1).
pub fn refresh_with(
    config: &Config,
    state: &StateDir,
    recovery: &RecoveryPass<'_>,
) -> Result<(), Error> {
    // The items as they were before anything was observed: only these may be cleared.
    let before: BTreeMap<String, InboxItem> = read_all(state)?
        .into_iter()
        .map(|(_, item)| (item.key.clone(), item))
        .collect();
    let report = recovery(state, &config.host)?;
    let reserved: BTreeSet<&str> = report
        .reservations
        .iter()
        .map(|view| view.reservation.job_id.as_str())
        .collect();
    for view in &report.reservations {
        if view.derived != DerivedState::RecoveryNeeded {
            continue;
        }
        let reservation = &view.reservation;
        let evidence = serde_json::to_value(view)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        let item = NewItem {
            key: format!("recovery_needed:{}", reservation.job_id),
            kind: InboxKind::RecoveryNeeded,
            subject: InboxSubject {
                job_id: Some(reservation.job_id.clone()),
                session_id: reservation.session.as_ref().map(|s| s.id.clone()),
                worktree: reservation.worktree.as_ref().map(|w| w.path.clone()),
            },
            evidence,
        };
        raise_recorded(config, state, &item)?;
    }

    let table = ProcessTable::snapshot(Detail::default())
        .map_err(|err| Error::new(ErrorCode::Internal, err.to_string()))?;
    let snapshots = read_sessions(state)?;
    testpoint::hit(testpoint::INBOX_OBSERVED)
        .map_err(|err| Error::new(ErrorCode::Internal, err.to_string()))?;
    let mut sessions = BTreeSet::new();
    for (path, snapshot) in snapshots {
        sessions.insert(snapshot.session_id.clone());
        let key = outlived_key(&snapshot.session_id);
        // `clear` and `resume` keep the agent running: nothing has outlived it yet.
        if table.is_alive(snapshot.agent.pid, snapshot.agent.start_s) {
            continue;
        }
        let live: Vec<_> = snapshot
            .processes
            .iter()
            .filter(|p| table.is_alive(p.pid, p.start_s))
            .collect();
        if live.is_empty() {
            let _ = std::fs::remove_file(&path);
            clear(
                config,
                state,
                &before,
                &key,
                "none of the session's processes is alive",
            )?;
            continue;
        }
        let evidence = serde_json::json!({
            "session_id": snapshot.session_id,
            "reason": snapshot.reason,
            "ended_ms": snapshot.ended_ms,
            "agent": snapshot.agent,
            "processes": live,
        });
        let item = NewItem {
            key,
            kind: InboxKind::OutlivedProcesses,
            subject: InboxSubject {
                session_id: Some(snapshot.session_id.clone()),
                ..InboxSubject::default()
            },
            evidence: evidence.as_object().cloned().unwrap_or_default(),
        };
        raise_recorded(config, state, &item)?;
    }

    clear_gone(config, state, &before, &reserved, &sessions)
}

/// Clears the open and escalated items of `before` whose reservation or session snapshot the
/// refresh no longer saw.
fn clear_gone(
    config: &Config,
    state: &StateDir,
    before: &BTreeMap<String, InboxItem>,
    reserved: &BTreeSet<&str>,
    sessions: &BTreeSet<String>,
) -> Result<(), Error> {
    for item in before.values() {
        if !matches!(item.status, InboxStatus::Open | InboxStatus::Escalated) {
            continue;
        }
        match item.kind {
            InboxKind::RecoveryNeeded
                if item
                    .subject
                    .job_id
                    .as_deref()
                    .is_some_and(|job| !reserved.contains(job)) =>
            {
                clear(config, state, before, &item.key, "the reservation is gone")?;
            }
            InboxKind::OutlivedProcesses
                if item
                    .subject
                    .session_id
                    .as_ref()
                    .is_some_and(|session| !sessions.contains(session)) =>
            {
                clear(
                    config,
                    state,
                    before,
                    &item.key,
                    "the session snapshot is gone",
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn outlived_key(session: &str) -> String {
    format!("outlived:{session}")
}

/// Raises `item` and records it when it is new.
fn raise_recorded(config: &Config, state: &StateDir, item: &NewItem) -> Result<(), Error> {
    let (stored, raised) = raise_item(state, item)?;
    if raised == Raised::New {
        let recorder = recorder(config, state);
        let mut record = recorder.record(RecordKind::InboxRaised);
        record.inbox_key = Some(stored.key.clone());
        record.session_id.clone_from(&stored.subject.session_id);
        record.job_id.clone_from(&stored.subject.job_id);
        record.data = serde_json::json!({ "kind": stored.kind })
            .as_object()
            .cloned()
            .unwrap_or_default();
        let _ = recorder.append(&record);
    }
    Ok(())
}

/// Resolves an open or escalated item as cardinal if it is still as `before` holds it, the
/// store before the refresh observed anything; anything else is left alone.
fn clear(
    config: &Config,
    state: &StateDir,
    before: &BTreeMap<String, InboxItem>,
    key: &str,
    note: &str,
) -> Result<(), Error> {
    // Raised after the observation started: the observation may predate what raised it.
    let Some(seen) = before.get(key) else {
        return Ok(());
    };
    match close(
        state,
        key,
        ResolutionOutcome::Resolved,
        BY_CARDINAL,
        note,
        Some(seen),
    ) {
        Ok(item) => {
            record_resolution(config, state, &item);
            Ok(())
        }
        Err(err) if matches!(err.code, ErrorCode::NotFound | ErrorCode::InvalidArgument) => Ok(()),
        Err(err) => Err(err),
    }
}

/// Sets the outcome of an open or escalated item; with `expected`, only while the item still
/// equals it (checked under the inbox lock).
fn close(
    state: &StateDir,
    key: &str,
    outcome: ResolutionOutcome,
    by: &str,
    note: &str,
    expected: Option<&InboxItem>,
) -> Result<InboxItem, Error> {
    let _lock = lock(state)?;
    let (path, existing) = find(state, key)?;
    let Some(mut item) = existing else {
        return Err(Error::new(
            ErrorCode::NotFound,
            format!("no inbox item has the key `{key}`"),
        ));
    };
    if !matches!(item.status, InboxStatus::Open | InboxStatus::Escalated) {
        return Err(Error::new(
            ErrorCode::InvalidArgument,
            format!("the inbox item `{key}` is already closed"),
        ));
    }
    if expected.is_some_and(|expected| *expected != item) {
        return Err(Error::new(
            ErrorCode::InvalidArgument,
            format!("the inbox item `{key}` changed since it was observed"),
        ));
    }
    let now = state::now_ms();
    item.status = match outcome {
        ResolutionOutcome::Resolved => InboxStatus::Resolved,
        ResolutionOutcome::Escalated => InboxStatus::Escalated,
        ResolutionOutcome::Dismissed => InboxStatus::Dismissed,
    };
    item.resolution = Some(Resolution {
        outcome,
        by: by.to_owned(),
        note: note.to_owned(),
        at_ms: now,
    });
    item.updated_ms = now;
    write(state, &path, &item)?;
    Ok(item)
}

fn record_resolution(config: &Config, state: &StateDir, item: &InboxItem) {
    let recorder = recorder(config, state);
    let mut record = recorder.record(RecordKind::InboxResolved);
    record.inbox_key = Some(item.key.clone());
    record.data = serde_json::to_value(&item.resolution)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let _ = recorder.append(&record);
}

fn recorder(config: &Config, state: &StateDir) -> Recorder {
    Recorder::new(
        state,
        config.host.records.max_bytes,
        config.host.records.keep,
    )
}

fn lock(state: &StateDir) -> Result<state::FileLock, Error> {
    Ok(state::lock(
        &state.inbox_lock(),
        LockMode::Exclusive,
        Instant::now() + LOCK_WAIT,
    )?)
}

/// The slot of `key`: its file if it exists, else the first free probe.
fn find(state: &StateDir, key: &str) -> Result<(PathBuf, Option<InboxItem>), Error> {
    for probe in 0..MAX_PROBES {
        let path = state.inbox_item(key, probe);
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<InboxItem>(&bytes) {
                Ok(item) if item.key == key => return Ok((path, Some(item))),
                // Another key with the same hash, or a file that does not parse: never
                // overwritten.
                _ => {}
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok((path, None)),
            Err(err) => return Err(state_error(&path, &err)),
        }
    }
    Err(Error::new(
        ErrorCode::StateUnavailable,
        format!("no free inbox slot for `{key}`"),
    ))
}

fn write(state: &StateDir, path: &Path, item: &InboxItem) -> Result<(), Error> {
    let bytes =
        serde_json::to_vec(item).map_err(|err| Error::new(ErrorCode::Internal, err.to_string()))?;
    Ok(state.write_atomic(path, &bytes, Durability::Durable)?)
}

/// Every item that parses, with its file.
pub fn read_all(state: &StateDir) -> Result<Vec<(PathBuf, InboxItem)>, Error> {
    read_dir_json(&state.inbox_dir())
}

fn read_sessions(state: &StateDir) -> Result<Vec<(PathBuf, SessionSnapshot)>, Error> {
    read_dir_json(&state.sessions_dir())
}

/// Every `*.json` file in `dir` that parses as `T`, in file-name order.
fn read_dir_json<T: serde::de::DeserializeOwned>(dir: &Path) -> Result<Vec<(PathBuf, T)>, Error> {
    let entries = std::fs::read_dir(dir).map_err(|err| state_error(dir, &err))?;
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    Ok(paths
        .into_iter()
        .filter_map(|path| {
            let bytes = std::fs::read(&path).ok()?;
            let doc = serde_json::from_slice(&bytes).ok()?;
            Some((path, doc))
        })
        .collect())
}

fn state_error(path: &Path, err: &std::io::Error) -> Error {
    Error::new(
        ErrorCode::StateUnavailable,
        format!("{}: {err}", path.display()),
    )
}
