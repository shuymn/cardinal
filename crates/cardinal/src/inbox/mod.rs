//! The inbox: store, dedup, occurrences, and refresh (`docs/architecture.md` sections 4.8
//! and 10.1), and the assignment of items to arbiter passes (section 10.5) in [`assign`].
//!
//! One file per item, `inbox/<key-hash>.json`, written durably under `inbox/.lock`. An item
//! is one occurrence of a condition: its key is the condition's key followed by
//! `@<since_ms>`, the time of the observation that found it. Raising a condition whose latest
//! occurrence has not ended updates an open or escalated item and leaves a resolved or
//! dismissed one alone (C19); a raiser that observes the clearing condition ends the
//! occurrence, resolving the item if it is still pending, and the condition found again
//! raises a new item. Raisers go through [`apply`], [`raise_occurrence`], and
//! [`end_occurrence`]. A raiser ends only occurrences it read before it started observing
//! ([`latest`]), and only while they are unchanged, so it never ends an item another raiser
//! raised or updated from a newer observation.
//!
//! The refresh that `cardinal inbox` and every `reconcile` pass run first runs the recovery
//! pass, then raises and ends `recovery_needed` and `outlived_processes`, one refresh at a time
//! under `inbox/.refresh.lock`. The items raised from a pass's observation and disk states come
//! from `reconcile`. [`raise`] and [`raise_item`] keep one item under a key as given, without
//! occurrences.

mod assign;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::admission::{self, RecoveryReport};
use crate::config::{Config, HostConfig};
use crate::contract::{
    DerivedState, Error, ErrorCode, Inbox, InboxChanges, InboxItem, InboxItemSchema, InboxKind,
    InboxSchema, InboxStatus, InboxSubject, RecordKind, Resolution, ResolutionOutcome,
    SessionSnapshot,
};
use crate::os::{Detail, ProcessTable};
use crate::record::Recorder;
use crate::state::{self, Durability, LockMode, StateDir};
use crate::testpoint;

pub use assign::{assign, due, list_assigned, record_outcome, resolve_assigned};

/// How long an upsert waits for `inbox/.lock`.
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// How many collision slots of one key hash are probed.
const MAX_PROBES: u32 = 64;

/// Who resolves items through `cardinal inbox resolve`.
const BY_CLI: &str = "cli";

/// Who resolves items whose occurrence a raiser ended.
const BY_CARDINAL: &str = "cardinal";

/// An item to raise: with [`raise`] under `key`, as an occurrence of the condition `key` with
/// [`Occurrence::Raise`].
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

/// Raises or updates the item `item.key` as given, without occurrences.
pub fn raise(state: &StateDir, item: &NewItem) -> Result<InboxItem, Error> {
    raise_item(state, item).map(|(item, _)| item)
}

/// [`raise`], also saying what changed.
pub fn raise_item(state: &StateDir, item: &NewItem) -> Result<(InboxItem, Raised), Error> {
    let _lock = lock(state)?;
    let (path, existing) = find(state, &item.key)?;
    let now = state::now_ms();
    let (stored, raised) = match existing {
        Some(mut existing) => {
            if !update_pending(&mut existing, item, now) {
                return Ok((existing, Raised::Unchanged));
            }
            (existing, Raised::Updated)
        }
        None => (new_item(item.key.clone(), item, now), Raised::New),
    };
    write(state, &path, &stored)?;
    Ok((stored, raised))
}

/// A new open item under `key`, raised from `item` at `now`.
fn new_item(key: String, item: &NewItem, now: u64) -> InboxItem {
    InboxItem {
        schema: InboxItemSchema,
        key,
        kind: item.kind,
        status: InboxStatus::Open,
        subject: item.subject.clone(),
        evidence: item.evidence.clone(),
        created_ms: now,
        updated_ms: now,
        seen: 1,
        ended_ms: None,
        arbiter: None,
        resolution: None,
    }
}

/// Raises `item` again over `existing`: an open or escalated item gets `seen` and the
/// evidence updated; a resolved or dismissed one is left alone (C19), and this returns false.
fn update_pending(existing: &mut InboxItem, item: &NewItem, now: u64) -> bool {
    if matches!(
        existing.status,
        InboxStatus::Resolved | InboxStatus::Dismissed
    ) {
        return false;
    }
    existing.seen += 1;
    existing.evidence.clone_from(&item.evidence);
    existing.updated_ms = now;
    true
}

/// Splits an item's key into the condition it is an occurrence of and the occurrence's time:
/// `<condition>@<since_ms>` (section 10.1). A key without that suffix, stored before
/// occurrences, is its own condition, with no time.
#[must_use]
pub fn occurrence_of(key: &str) -> (&str, Option<u64>) {
    // Digits only: `parse` alone would also take a leading `+`.
    key.rsplit_once('@')
        .filter(|(_, since)| !since.is_empty() && since.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|(condition, since)| Some((condition, Some(since.parse().ok()?))))
        .unwrap_or((key, None))
}

/// When an occurrence was found: its key's time, else (an item stored before occurrences)
/// its creation.
fn found_ms(item: &InboxItem) -> u64 {
    occurrence_of(&item.key).1.unwrap_or(item.created_ms)
}

/// The latest occurrence of every condition in the store, by condition. A raiser reads this
/// before it observes and passes each item to [`Occurrence::End`] as `before`.
pub fn latest(state: &StateDir) -> Result<BTreeMap<String, InboxItem>, Error> {
    Ok(latest_with_paths(read_all(state)?)
        .into_iter()
        .map(|(condition, (_, item))| (condition, item))
        .collect())
}

/// [`latest`] from items already read, each with its file.
fn latest_with_paths(items: Vec<(PathBuf, InboxItem)>) -> BTreeMap<String, (PathBuf, InboxItem)> {
    let mut latest: BTreeMap<String, (PathBuf, InboxItem)> = BTreeMap::new();
    for (path, item) in items {
        let condition = occurrence_of(&item.key).0.to_owned();
        let newer = latest.get(&condition).is_none_or(|(_, kept)| {
            (found_ms(&item), item.created_ms) > (found_ms(kept), kept.created_ms)
        });
        if newer {
            latest.insert(condition, (path, item));
        }
    }
    latest
}

/// A change to the occurrences of one condition (section 10.1), applied by [`apply`].
#[derive(Clone, Debug, PartialEq)]
pub enum Occurrence {
    /// The condition `item.key` (the key without its occurrence suffix) was found by an
    /// observation at `since_ms`. While its latest occurrence has not ended, an open or
    /// escalated item gets `seen` and the evidence updated and a resolved or dismissed one is
    /// left alone; otherwise a new item, `<key>@<since_ms>`, is raised.
    Raise { item: NewItem, since_ms: u64 },
    /// As [`Occurrence::Raise`], except that an occurrence that has not ended is left as it
    /// was, its `seen` and evidence included: for a raiser that found the condition but lacks
    /// the evidence to replace what the item holds.
    RaiseUnlessActive { item: NewItem, since_ms: u64 },
    /// The clearing condition of `key` was observed at `ended_ms`: its latest occurrence gets
    /// `ended_ms` and, if open or escalated, is resolved by cardinal with `note`. `before`
    /// is the item as the raiser read it before it started observing ([`latest`]); an item
    /// that changed since, or that the raiser did not read, is left alone.
    End {
        key: String,
        before: Option<Box<InboxItem>>,
        ended_ms: u64,
        note: String,
    },
}

/// What [`apply`] did for one [`Occurrence`].
#[derive(Clone, Debug, PartialEq)]
pub enum Applied {
    Raised(InboxItem, Raised),
    /// The ended item, and whether ending it resolved it (it was open or escalated); `None`
    /// when no occurrence was left to end.
    Ended(Option<(InboxItem, bool)>),
}

/// Applies `changes` in order under one hold of `inbox/.lock`, reading the store once. An item
/// file that cannot be read or parsed fails the whole call with `state_unavailable`, since the
/// occurrence it may hold could not be found.
pub fn apply(state: &StateDir, changes: &[Occurrence]) -> Result<Vec<Applied>, Error> {
    let _lock = lock(state)?;
    let mut latest = latest_with_paths(read_all_strict(state)?);
    let mut applied = Vec::with_capacity(changes.len());
    for change in changes {
        applied.push(match change {
            Occurrence::Raise { item, since_ms } => {
                let (stored, raised) = raise_in(state, &mut latest, item, *since_ms, true)?;
                Applied::Raised(stored, raised)
            }
            Occurrence::RaiseUnlessActive { item, since_ms } => {
                let (stored, raised) = raise_in(state, &mut latest, item, *since_ms, false)?;
                Applied::Raised(stored, raised)
            }
            Occurrence::End {
                key,
                before,
                ended_ms,
                note,
            } => Applied::Ended(end_in(
                state,
                &mut latest,
                key,
                before.as_deref(),
                *ended_ms,
                note,
            )?),
        });
    }
    Ok(applied)
}

/// [`apply`], then a record for each item raised anew and each one an end resolved, with
/// the keys raised anew and ended.
pub fn apply_recorded(
    config: &Config,
    state: &StateDir,
    changes: &[Occurrence],
) -> Result<InboxChanges, Error> {
    let mut summary = InboxChanges::default();
    for applied in apply(state, changes)? {
        match applied {
            Applied::Raised(item, Raised::New) => {
                record_raised(config, state, &item);
                summary.raised.push(item.key);
            }
            Applied::Raised(..) | Applied::Ended(None) => {}
            Applied::Ended(Some((item, resolved))) => {
                if resolved {
                    record_resolution(config, state, &item);
                }
                summary.cleared.push(item.key);
            }
        }
    }
    Ok(summary)
}

/// Raises one occurrence of a condition (section 10.1): `item.key` is the condition's key
/// without its occurrence suffix, and `since_ms` the time of the observation that found it.
/// While the condition's latest occurrence has not ended, this updates `seen` and the
/// evidence of an open or escalated item and leaves a resolved or dismissed one alone;
/// after it ended, it raises a new item, `<key>@<since_ms>`.
pub fn raise_occurrence(
    state: &StateDir,
    item: &NewItem,
    since_ms: u64,
) -> Result<(InboxItem, Raised), Error> {
    let change = Occurrence::Raise {
        item: item.clone(),
        since_ms,
    };
    match apply(state, &[change])?.pop() {
        Some(Applied::Raised(item, raised)) => Ok((item, raised)),
        _ => Err(Error::new(ErrorCode::Internal, "a raise applied nothing")),
    }
}

/// Ends the latest occurrence of the condition `key` (its key without the occurrence suffix)
/// once a raiser observed its clearing condition: sets the item's `ended_ms` and resolves it
/// if it is open or escalated. `before` holds the item as the raiser read it before it
/// started observing; an item changed since is left alone. `None` when no occurrence is
/// open.
pub fn end_occurrence(
    state: &StateDir,
    key: &str,
    before: Option<&InboxItem>,
    ended_ms: u64,
    note: &str,
) -> Result<Option<InboxItem>, Error> {
    let change = Occurrence::End {
        key: key.to_owned(),
        before: before.cloned().map(Box::new),
        ended_ms,
        note: note.to_owned(),
    };
    match apply(state, &[change])?.pop() {
        Some(Applied::Ended(ended)) => Ok(ended.map(|(item, _)| item)),
        _ => Err(Error::new(ErrorCode::Internal, "an end applied nothing")),
    }
}

/// One raise inside [`apply`]; `latest` is kept current. Without `update`, an occurrence that
/// has not ended is left as it was.
fn raise_in(
    state: &StateDir,
    latest: &mut BTreeMap<String, (PathBuf, InboxItem)>,
    item: &NewItem,
    since_ms: u64,
    update: bool,
) -> Result<(InboxItem, Raised), Error> {
    let now = state::now_ms();
    let mut since = since_ms;
    if let Some((path, current)) = latest.get_mut(&item.key) {
        if current.ended_ms.is_none() {
            let mut updated = current.clone();
            if !update || !update_pending(&mut updated, item, now) {
                return Ok((updated, Raised::Unchanged));
            }
            write(state, path, &updated)?;
            *current = updated.clone();
            return Ok((updated, Raised::Updated));
        }
        // A new occurrence comes after the one that ended, so the keys stay apart.
        since = since.max(found_ms(current) + 1);
    }
    let (path, key) = loop {
        let key = format!("{}@{since}", item.key);
        match find(state, &key)? {
            (path, None) => break (path, key),
            (_, Some(_)) => since += 1,
        }
    };
    let stored = new_item(key, item, now);
    write(state, &path, &stored)?;
    latest.insert(item.key.clone(), (path, stored.clone()));
    Ok((stored, Raised::New))
}

/// One end inside [`apply`]; `latest` is kept current.
fn end_in(
    state: &StateDir,
    latest: &mut BTreeMap<String, (PathBuf, InboxItem)>,
    key: &str,
    before: Option<&InboxItem>,
    ended_ms: u64,
    note: &str,
) -> Result<Option<(InboxItem, bool)>, Error> {
    let Some((path, current)) = latest.get_mut(key) else {
        return Ok(None);
    };
    // Already ended, not read before the observation, or changed since: left alone.
    if current.ended_ms.is_some() || before != Some(&*current) {
        return Ok(None);
    }
    let now = state::now_ms();
    let mut ended = current.clone();
    ended.ended_ms = Some(ended_ms);
    let resolves = matches!(ended.status, InboxStatus::Open | InboxStatus::Escalated);
    if resolves {
        ended.status = InboxStatus::Resolved;
        ended.resolution = Some(Resolution {
            outcome: ResolutionOutcome::Resolved,
            by: BY_CARDINAL.to_owned(),
            note: note.to_owned(),
            at_ms: now,
        });
    }
    ended.updated_ms = now;
    write(state, path, &ended)?;
    *current = ended.clone();
    Ok(Some((ended, resolves)))
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
    let item = close(state, key, outcome, BY_CLI, note.unwrap_or_default())?;
    record_resolution(config, state, &item);
    Ok(item)
}

/// Runs the recovery pass, then raises and ends the occurrences of `recovery_needed` and
/// `outlived_processes` (section 10.1), with the keys raised anew and ended. One refresh at a
/// time observes and applies, holding `inbox/.refresh.lock` throughout, so an observation that
/// a newer refresh superseded never raises or ends anything.
pub fn refresh_with(
    config: &Config,
    state: &StateDir,
    recovery: &RecoveryPass<'_>,
) -> Result<InboxChanges, Error> {
    let _refresh = refresh_lock(config, state)?;
    // The items as they were before anything was observed: only these may be ended.
    let before = latest(state)?;
    let report = recovery(state, &config.host)?;
    let found_ms = state::now_ms();
    let mut changes = Vec::new();
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
        changes.push(Occurrence::Raise {
            item: NewItem {
                key: format!("recovery_needed:{}", reservation.job_id),
                kind: InboxKind::RecoveryNeeded,
                subject: InboxSubject {
                    job_id: Some(reservation.job_id.clone()),
                    session_id: reservation.session.as_ref().map(|s| s.id.clone()),
                    worktree: reservation.worktree.as_ref().map(|w| w.path.clone()),
                    ..InboxSubject::default()
                },
                evidence,
            },
            since_ms: found_ms,
        });
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
            // The item ends with its snapshot: one that cannot be deleted is read again.
            match std::fs::remove_file(&path) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => continue,
                _ => {}
            }
            changes.push(Occurrence::End {
                before: before.get(&key).cloned().map(Box::new),
                key,
                ended_ms: found_ms,
                note: "none of the session's processes is alive".to_owned(),
            });
            continue;
        }
        let evidence = serde_json::json!({
            "session_id": snapshot.session_id,
            "reason": snapshot.reason,
            "ended_ms": snapshot.ended_ms,
            "agent": snapshot.agent,
            "processes": live,
        });
        changes.push(Occurrence::Raise {
            item: NewItem {
                key,
                kind: InboxKind::OutlivedProcesses,
                subject: InboxSubject {
                    session_id: Some(snapshot.session_id.clone()),
                    ..InboxSubject::default()
                },
                evidence: evidence.as_object().cloned().unwrap_or_default(),
            },
            since_ms: found_ms,
        });
    }

    changes.extend(ended_gone(state, &before, &reserved, &sessions, found_ms));
    apply_recorded(config, state, &changes)
}

/// `inbox/.refresh.lock`, taken before the admission lock and held from a refresh's first read
/// to its last write. The wait covers a refresh holding it: its recovery pass's admission lock
/// and its inbox writes.
fn refresh_lock(config: &Config, state: &StateDir) -> Result<state::FileLock, Error> {
    let path = state.inbox_dir().join(".refresh.lock");
    if let Some(held) = state::try_lock(&path, LockMode::Exclusive)? {
        return Ok(held);
    }
    testpoint::hit(testpoint::INBOX_REFRESH_WAITING)
        .map_err(|err| Error::new(ErrorCode::Internal, err.to_string()))?;
    let wait = Duration::from_millis(config.host.admission.lock_timeout_ms) + 2 * LOCK_WAIT;
    Ok(state::lock(
        &path,
        LockMode::Exclusive,
        Instant::now() + wait,
    )?)
}

/// Ends the occurrences in `before` whose reservation or session snapshot the refresh no
/// longer saw and whose file is absent: a file that is there but could not be read or parsed
/// proves nothing.
fn ended_gone(
    state: &StateDir,
    before: &BTreeMap<String, InboxItem>,
    reserved: &BTreeSet<&str>,
    sessions: &BTreeSet<String>,
    ended_ms: u64,
) -> Vec<Occurrence> {
    let gone = |item: &InboxItem| match item.kind {
        InboxKind::RecoveryNeeded => item
            .subject
            .job_id
            .as_deref()
            .is_some_and(|job| !reserved.contains(job) && absent(&state.reservation(job)))
            .then_some("the reservation is gone"),
        InboxKind::OutlivedProcesses => item
            .subject
            .session_id
            .as_ref()
            .is_some_and(|session| !sessions.contains(session) && absent(&state.session(session)))
            .then_some("the session snapshot is gone"),
        _ => None,
    };
    before
        .iter()
        .filter(|(_, item)| item.ended_ms.is_none())
        .filter_map(|(condition, item)| {
            gone(item).map(|note| Occurrence::End {
                key: condition.clone(),
                before: Some(Box::new(item.clone())),
                ended_ms,
                note: note.to_owned(),
            })
        })
        .collect()
}

/// Whether nothing is at `path`, as opposed to a file that is there, readable or not.
fn absent(path: &Path) -> bool {
    matches!(
        std::fs::symlink_metadata(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound
    )
}

fn outlived_key(session: &str) -> String {
    format!("outlived:{session}")
}

/// Appends `inbox_raised` for an item raised anew.
fn record_raised(config: &Config, state: &StateDir, item: &InboxItem) {
    let recorder = recorder(config, state);
    let mut record = recorder.record(RecordKind::InboxRaised);
    record.inbox_key = Some(item.key.clone());
    record.session_id.clone_from(&item.subject.session_id);
    record.job_id.clone_from(&item.subject.job_id);
    record.data = serde_json::json!({ "kind": item.kind })
        .as_object()
        .cloned()
        .unwrap_or_default();
    let _ = recorder.append(&record);
}

/// Sets the outcome of an open or escalated item.
fn close(
    state: &StateDir,
    key: &str,
    outcome: ResolutionOutcome,
    by: &str,
    note: &str,
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

/// Every item, with its file, read under `inbox/.lock`: an item file that is listed but
/// cannot be read or parsed is an error, one that is gone when read (a dangling link)
/// included.
fn read_all_strict(state: &StateDir) -> Result<Vec<(PathBuf, InboxItem)>, Error> {
    json_files(&state.inbox_dir())?
        .into_iter()
        .map(|path| {
            let bytes = std::fs::read(&path).map_err(|err| state_error(&path, &err))?;
            match serde_json::from_slice(&bytes) {
                Ok(item) => Ok((path, item)),
                Err(err) => Err(Error::new(
                    ErrorCode::StateUnavailable,
                    format!("{}: not an inbox item: {err}", path.display()),
                )),
            }
        })
        .collect()
}

/// The `*.json` files in `dir`, in file-name order; a directory that cannot be listed to the
/// end is an error.
fn json_files(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|err| state_error(dir, &err))? {
        let path = entry.map_err(|err| state_error(dir, &err))?.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

/// Every `*.json` file in `dir` that parses as `T`, in file-name order.
fn read_dir_json<T: serde::de::DeserializeOwned>(dir: &Path) -> Result<Vec<(PathBuf, T)>, Error> {
    Ok(json_files(dir)?
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
