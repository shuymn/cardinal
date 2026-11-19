//! An execution's records and the recovery of an execution that died
//! (`docs/architecture.md` section 9.7, "Records" and "Recovery").
//!
//! An execution keeps its records in `executions/<plan>/`: the manifest, written durably
//! before the claim; the claim, published complete by a rename from a file written and synced
//! under a temporary name, at each stage; and the result. Every one of them is written by
//! the `act` or the recovery holding the plan's `act.lock`, checked as section 4.3 says.
//!
//! Recovery takes the `act.lock` with one try; a result there, read whole and its directory
//! synced, means the execution finished. Otherwise it makes durable what it found (the claim,
//! its directory, the target's parent as far as the chain resolves, `<state>/trash/`, and the
//! trash entry with its `t` and `h`), and then acts on the claim: an absent claim means nothing
//! was changed, and an unreadable one leaves everything as it is. A `committed` claim is
//! finished, once the caller still holds its grant and unit gate, the configuration loads, and
//! the descriptor scan finds no process using the trash entry: a hold that a cleanup removed is
//! made again, each entry in the hold is put back through `t` by its manifest path, walking as
//! the deletion walks, `t` is deleted as live deletion deletes it, and the result is written. A
//! `claimed` claim is undone: `t` holding the claimed inode is restored into the parent the
//! claim's chain resolves to ([`restore`], which a live execution's restore is too), and a
//! trash entry with nothing of the target is removed; the result is `rejected` (`interrupted`).
//! Whatever recovery leaves in the trash, and a claim it cannot read, raises
//! `reclaim_stranded` (section 10.1).

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use super::delete::{self, Deletion, Fate, Guard as _, Watch};
use crate::config::Config;
use crate::contract::{
    ActClaim, ActManifest, ActManifestSchema, ActResult, ActResultSchema, ActStatus, Caller, Check,
    CheckResult, CheckRole, ClaimStage, EntryType, InboxKind, InboxSubject, ManifestEntry,
};
use crate::inbox::{self, NewItem};
use crate::observe::Measure;
use crate::os::dir::{self, Kind};
use crate::state::{self, Op, StateDir};
use crate::testpoint;

/// The reason of a `claimed` execution that recovery undid (section 9.7).
pub const INTERRUPTED: &str = "interrupted";

/// A held `executions/<plan>/act.lock`: dropping it releases the lock.
#[derive(Debug)]
pub struct ExecutionLock {
    _file: File,
}

/// What an attempt to take an execution's `act.lock` found.
#[derive(Debug)]
pub enum Locked {
    Held(ExecutionLock),
    /// Another `act` or recovery holds it.
    InProgress,
    /// No lock file, or the path names another file than the one locked: a `plan` run removed
    /// the execution directory meanwhile.
    NotFound,
}

/// Takes the `act.lock` of `plan`'s execution with one try, and checks, holding it, that the
/// path still names the file it locked (section 4.3).
pub fn lock_execution(state: &StateDir, plan: &str) -> io::Result<Locked> {
    let path = state.execution_lock(plan);
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Locked::NotFound),
        Err(err) => return Err(err),
    };
    // A failure injected here stands for a lock that cannot be tried.
    testpoint::hit(testpoint::ACT_LOCK_OPENED).map_err(io::Error::other)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(Locked::InProgress),
        Err(TryLockError::Error(err)) => return Err(err),
    }
    let opened = dir::stat_fd(&file)?;
    match dir::stat_at(
        &File::open(state.execution_dir(plan))?,
        OsStr::new("act.lock"),
    ) {
        Ok(now) if (now.dev, now.ino) == (opened.dev, opened.ino) => {
            Ok(Locked::Held(ExecutionLock { _file: file }))
        }
        Ok(_) => Ok(Locked::NotFound),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Locked::NotFound),
        Err(err) => Err(err),
    }
}

/// Writes `plan`'s manifest durably: a header line, then one line per entry in walk order,
/// through `tmp/` and the reserve's retry (section 9.7, "Space").
pub fn write_manifest(state: &StateDir, plan: &str, entries: &[ManifestEntry]) -> io::Result<()> {
    let header = ActManifest {
        schema: ActManifestSchema,
        plan_id: plan.to_owned(),
        entries: entries.len() as u64,
    };
    let mut bytes = serde_json::to_vec(&header).map_err(io::Error::other)?;
    bytes.push(b'\n');
    for entry in entries {
        serde_json::to_writer(&mut bytes, entry).map_err(io::Error::other)?;
        bytes.push(b'\n');
    }
    let target = state.execution_manifest(plan);
    state::retry_with_reserve(&state.reserve_file(), || {
        testpoint::fake_write("manifest")?;
        state::write_atomic(
            &state.tmp_dir(),
            &target,
            &bytes,
            state::Durability::Durable,
        )
    })
}

/// Reads `plan`'s manifest whole: the header and exactly as many entries as it names, each
/// line parsed to its end. Anything else is an error, so nothing decides from part of it.
pub fn read_manifest(state: &StateDir, plan: &str) -> Result<Vec<ManifestEntry>, String> {
    let path = state.execution_manifest(plan);
    let bytes = std::fs::read(&path).map_err(|err| format!("{}: {err}", path.display()))?;
    let bad = |what: String| format!("{}: {what}", path.display());
    let body = bytes
        .strip_suffix(b"\n")
        .ok_or_else(|| bad("does not end with a newline".into()))?;
    let mut lines = body.split(|&b| b == b'\n');
    let header: ActManifest = serde_json::from_slice(lines.next().unwrap_or_default())
        .map_err(|err| bad(format!("the header: {err}")))?;
    if header.plan_id != plan {
        return Err(bad(format!("names the plan {}", header.plan_id)));
    }
    let entries = lines
        .enumerate()
        .map(|(n, line)| {
            serde_json::from_slice::<ManifestEntry>(line)
                .map_err(|err| bad(format!("line {}: {err}", n + 2)))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if entries.len() as u64 != header.entries {
        return Err(bad(format!(
            "{} entries where the header names {}",
            entries.len(),
            header.entries
        )));
    }
    Ok(entries)
}

/// Writes `claim` under a temporary name and syncs it, through the reserve's retry: the first
/// half of its publication, which [`state::Staged::publish`] completes. A test's fake fails
/// the write of a `claimed` stage as `claim` and of `committed` as `commit` (section 13.7).
pub fn stage_claim(state: &StateDir, claim: &ActClaim) -> io::Result<state::Staged> {
    let bytes = serde_json::to_vec(claim).map_err(io::Error::other)?;
    let target = state.execution_claim(&claim.plan_id);
    let kind = match claim.stage {
        ClaimStage::Claimed => "claim",
        ClaimStage::Committed => "commit",
    };
    state::retry_with_reserve(&state.reserve_file(), || {
        testpoint::fake_write(kind)?;
        state::Staged::write(&state.tmp_dir(), &target, &bytes)
    })
}

/// How a claim's publication ended once its rename succeeded.
#[derive(Debug)]
pub enum Published {
    /// Renamed into place, and its directory synced.
    Durable,
    /// Renamed into place, which publishes it, and its directory's sync failed: the claim's
    /// stage stands, and the execution is left to recovery (section 9.7, durable order).
    NotSynced(io::Error),
}

/// Publishes `claim` complete (section 9.7, "Records"): written and synced under a temporary
/// name, paused at `act.publishing`, and published by [`publish_staged`]. An error means it
/// was not published.
pub fn publish_claim(state: &StateDir, claim: &ActClaim) -> io::Result<Published> {
    let mut staged = stage_claim(state, claim)?;
    testpoint::hit(testpoint::ACT_PUBLISHING).map_err(io::Error::other)?;
    publish_staged(state, &claim.plan_id, &mut staged, None)
}

/// Renames `staged`, a claim of `plan` that [`stage_claim`] wrote, into place through the
/// reserve's retry (section 9.7, "Space"), pauses at `point` when one is given, whose injected
/// failure stands for a failed sync, and syncs the execution directory. An error means the
/// rename failed and the claim was not published; the temporary file goes with `staged`.
pub fn publish_staged(
    state: &StateDir,
    plan: &str,
    staged: &mut state::Staged,
    point: Option<&'static str>,
) -> io::Result<Published> {
    state::retry_with_reserve(&state.reserve_file(), || {
        testpoint::fake_write("claim_rename")?;
        staged.publish()
    })?;
    let paused = point.map_or(Ok(()), |point| {
        testpoint::hit(point).map_err(io::Error::other)
    });
    let synced = paused
        .and_then(|()| testpoint::fake_write("claim_sync"))
        .and_then(|()| state::sync_dir(&state.execution_dir(plan)));
    Ok(match synced {
        Ok(()) => Published::Durable,
        Err(err) => Published::NotSynced(err),
    })
}

/// What reading a claim found.
#[derive(Debug)]
pub enum ClaimRead {
    Absent,
    Unreadable(String),
    Read(Box<ActClaim>),
}

/// Reads `plan`'s claim: absent only when its file is missing, unreadable when it cannot be
/// read or does not parse as `cardinal.act-claim.v1` for this plan.
#[must_use]
pub fn read_claim(state: &StateDir, plan: &str) -> ClaimRead {
    let path = state.execution_claim(plan);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return ClaimRead::Absent,
        Err(err) => return ClaimRead::Unreadable(format!("{}: {err}", path.display())),
    };
    match serde_json::from_slice::<ActClaim>(&bytes) {
        Ok(claim) if claim.plan_id == plan => ClaimRead::Read(Box::new(claim)),
        Ok(claim) => ClaimRead::Unreadable(format!(
            "{}: names the plan {}",
            path.display(),
            claim.plan_id
        )),
        Err(err) => ClaimRead::Unreadable(format!("{}: {err}", path.display())),
    }
}

/// Reads `plan`'s result whole, `None` when there is none: a result that cannot be read or
/// does not parse to its end as `cardinal.act-result.v2` for this plan is an error.
pub fn read_result(state: &StateDir, plan: &str) -> io::Result<Option<ActResult>> {
    let path = state.execution_result(plan);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let bad = |why: String| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: {why}", path.display()),
        )
    };
    match serde_json::from_slice::<ActResult>(&bytes) {
        Ok(result) if result.plan_id == plan => Ok(Some(result)),
        Ok(result) => Err(bad(format!("names the plan {}", result.plan_id))),
        Err(err) => Err(bad(err.to_string())),
    }
}

/// Writes `plan`'s result durably through the reserve's retry.
pub fn write_result(state: &StateDir, result: &ActResult) -> io::Result<()> {
    let bytes = serde_json::to_vec(result).map_err(io::Error::other)?;
    let target = state.execution_result(&result.plan_id);
    state::retry_with_reserve(&state.reserve_file(), || {
        testpoint::fake_write("result")?;
        state::write_atomic(
            &state.tmp_dir(),
            &target,
            &bytes,
            state::Durability::Durable,
        )
    })
}

/// Whether the caller of a committed execution still holds its grant, read from the
/// configuration recovery has just loaded: `Err` says why not. The grant is `[act] execute`,
/// which the unit that executes brings (X1c); recovery checks the unit gate itself.
pub type Grant<'a> = &'a dyn Fn(&Config, &Caller) -> Result<(), String>;

/// What recovery found and did.
#[derive(Debug)]
pub enum Recovered {
    /// Another `act` or recovery holds the plan's `act.lock`.
    InProgress,
    /// No execution to recover: no `act.lock`, or its path now names another file.
    NotFound,
    /// The execution has a result already, read whole, and its directory synced.
    Finished,
    /// No claim: nothing was changed, since the claim precedes every change.
    Absent,
    /// The claim cannot be read: nothing is touched; the plan's trash entry, if any.
    Unreadable {
        error: String,
        trash: Option<PathBuf>,
    },
    /// An execution whose data stays in the trash for now: why, and the trash entry. Under a
    /// `claimed` claim, `t` could not go back (its parent moved, its name taken, or it holds
    /// another inode) or the entry holds what the claim does not account for; under a
    /// `committed` one, nothing was deleted when the grant, the gate, the configuration, or
    /// the scan stopped it first, and a deletion that stopped kept what it had not taken. A
    /// later recovery goes on.
    Stranded { why: String, trash: PathBuf },
    /// The execution was finished: its result, written. A `committed` one reports `executed`
    /// or `partial`, a `claimed` one, undone, `rejected` (`interrupted`).
    Done(Box<ActResult>),
}

/// Recovers `plan`'s execution as section 9.7's table says, loading the configuration from
/// `config_dir`, and raises `reclaim_stranded` for what it leaves in the trash.
pub fn recover(
    state: &StateDir,
    config_dir: &Path,
    plan: &str,
    grant: Grant<'_>,
) -> io::Result<Recovered> {
    let lock = match lock_execution(state, plan)? {
        Locked::Held(lock) => lock,
        Locked::InProgress => return Ok(Recovered::InProgress),
        Locked::NotFound => return Ok(Recovered::NotFound),
    };
    testpoint::hit(testpoint::RECOVER_LOCKED).map_err(io::Error::other)?;
    recover_held(state, config_dir, plan, grant, &lock)
}

/// [`recover`] by the holder of `plan`'s `act.lock`: an `act` recovers its own plan's claim
/// before it starts another execution (section 9.7, step 1).
pub fn recover_held(
    state: &StateDir,
    config_dir: &Path,
    plan: &str,
    grant: Grant<'_>,
    _lock: &ExecutionLock,
) -> io::Result<Recovered> {
    // A result renamed into place whose directory's sync failed is made durable before it
    // counts.
    if read_result(state, plan)?.is_some() {
        state::sync_dir(&state.execution_dir(plan))?;
        return Ok(Recovered::Finished);
    }
    let claim = match read_claim(state, plan) {
        ClaimRead::Absent => return Ok(Recovered::Absent),
        ClaimRead::Unreadable(error) => {
            let entry = state.trash_entry(plan);
            let trash = std::fs::symlink_metadata(&entry).is_ok().then_some(entry);
            let mut evidence = serde_json::Map::new();
            evidence.insert(
                "execution".into(),
                state.execution_dir(plan).display().to_string().into(),
            );
            evidence.insert(
                "trash".into(),
                trash.as_ref().map(|t| t.display().to_string()).into(),
            );
            evidence.insert(
                "reason".into(),
                format!("the claim cannot be read: {error}").into(),
            );
            raise_stranded(state, plan, None, evidence);
            return Ok(Recovered::Unreadable { error, trash });
        }
        ClaimRead::Read(claim) => claim,
    };
    let entry = state.trash_entry(plan);
    let recovered = match make_found_durable(state, &claim) {
        Err(err) => Recovered::Stranded {
            why: format!("what recovery found is not durable: {err}"),
            trash: entry,
        },
        Ok(()) => match claim.stage {
            ClaimStage::Claimed => claimed(state, &claim)?,
            ClaimStage::Committed => committed(state, config_dir, &claim, grant)?,
        },
    };
    match &recovered {
        Recovered::Stranded { why, trash } => stranded(state, &claim, trash, why, &[]),
        Recovered::Done(result) => {
            if let Some(trash) = &result.trash {
                stranded(
                    state,
                    &claim,
                    Path::new(trash),
                    "entries kept",
                    &result.skipped_changed,
                );
            }
        }
        _ => {}
    }
    Ok(recovered)
}

/// Raises `reclaim_stranded` for `claim`'s execution, which leaves data in the trash entry
/// `trash` for `why`, with the entries it kept (section 10.1).
pub fn stranded(state: &StateDir, claim: &ActClaim, trash: &Path, why: &str, kept: &[String]) {
    let mut evidence = serde_json::Map::new();
    let target = Path::new(&claim.worktree.path).join(target_rel(claim));
    evidence.insert("target".into(), target.display().to_string().into());
    evidence.insert("trash".into(), trash.display().to_string().into());
    let (entries, bytes) = left(trash);
    evidence.insert("entries_left".into(), entries.into());
    evidence.insert("bytes_left".into(), bytes.into());
    evidence.insert(
        "kept".into(),
        kept.iter()
            .take(STRANDED_ENTRIES)
            .cloned()
            .collect::<Vec<_>>()
            .into(),
    );
    evidence.insert("reason".into(), why.into());
    raise_stranded(state, &claim.plan_id, Some(&claim.worktree.path), evidence);
}

/// How many entries of [`stranded`]'s kept list an item names.
const STRANDED_ENTRIES: usize = 20;

/// Raises the `reclaim_stranded:<plan>` occurrence with `evidence`. Best effort: an inbox that
/// cannot be written keeps the trash entry as it is, and a later recovery raises it again.
fn raise_stranded(
    state: &StateDir,
    plan: &str,
    worktree: Option<&str>,
    evidence: serde_json::Map<String, serde_json::Value>,
) {
    let item = NewItem {
        key: format!("reclaim_stranded:{plan}"),
        kind: InboxKind::ReclaimStranded,
        subject: InboxSubject {
            worktree: worktree.map(str::to_owned),
            ..InboxSubject::default()
        },
        evidence,
    };
    let _ = inbox::raise_occurrence(state, &item, state::now_ms());
}

/// The entries below `trash` and the bytes they allocate, read without following links; what
/// cannot be read is left out.
fn left(trash: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let mut entries = 0;
    let mut bytes = 0;
    let mut dirs = vec![trash.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(listed) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in listed.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            entries += 1;
            bytes += meta.blocks() * 512;
            if meta.is_dir() {
                dirs.push(entry.path());
            }
        }
    }
    (entries, bytes)
}

/// How a restore ([`restore`]) ended when it did not finish.
#[derive(Debug)]
pub enum RestoreError {
    /// `t` stays in the trash: the name was taken meanwhile, the rename failed, or it failed
    /// for lack of space after the reserve's retry.
    Stayed(io::Error),
    /// `t` went back, and a sync or the removal of the trash entry after the rename failed:
    /// the restore is left to recovery, which makes durable what it finds before it decides.
    NotDurable(io::Error),
}

impl std::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stayed(err) => write!(f, "the target stays in the trash: {err}"),
            Self::NotDurable(err) => write!(f, "the restore is not durable: {err}"),
        }
    }
}

/// Restores `plan`'s target (section 9.7, "Restore"): renames the trash entry's `t` back as
/// `name` into `parent`, without replacing, wherever `parent` now is, so a rollback stays
/// bound to the original parent, through the reserve's retry; pauses at `act.restored`; syncs
/// both directories; and removes the empty hold and the trash entry, syncing `<state>/trash/`.
pub fn restore(
    state: &StateDir,
    plan: &str,
    parent: &File,
    name: &OsStr,
) -> Result<(), RestoreError> {
    let entry_path = state.trash_entry(plan);
    let entry = match dir::open(&entry_path) {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            return Err(RestoreError::Stayed(io::Error::other(
                "the trash entry is not a directory",
            )));
        }
        Err(err) => return Err(RestoreError::Stayed(err)),
    };
    let to = || dir::current_path(parent).unwrap_or_default().join(name);
    state::retry_with_reserve(&state.reserve_file(), || {
        testpoint::fake_write("restore")?;
        let renamed = dir::rename_noreplace(&entry, OsStr::new("t"), parent, name);
        state::logged(Op::Rename, &entry_path.join("t"), Some(&to()), renamed)
    })
    .map_err(RestoreError::Stayed)?;
    let durable = (|| {
        testpoint::hit(testpoint::ACT_RESTORED).map_err(io::Error::other)?;
        let parent_path = dir::current_path(parent).unwrap_or_default();
        state::logged(Op::Sync, &parent_path, None, parent.sync_all())?;
        state::logged(Op::Sync, &entry_path, None, entry.sync_all())?;
        remove_empty_entry(state, plan)
    })();
    durable.map_err(RestoreError::NotDurable)
}

/// Removes `plan`'s trash entry when it holds nothing but an empty hold, and syncs
/// `<state>/trash/`. A missing entry or hold is fine; one that holds anything else is an error,
/// and the entry stays.
pub fn remove_empty_entry(state: &StateDir, plan: &str) -> io::Result<()> {
    let entry = state.trash_entry(plan);
    for path in [entry.join("h"), entry.clone()] {
        match state::logged(Op::Rmdir, &path, None, std::fs::remove_dir(&path)) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    state::sync_dir(&state.trash_dir())
}

/// Recovers a `claimed` execution (section 9.7's three `claimed` rows): `t` holding the
/// claimed inode goes back into the parent the chain resolves to, and a trash entry that holds
/// nothing of the target is removed when the target's name in that parent holds the claimed
/// inode, each ending `rejected` (`interrupted`); otherwise nothing is touched.
fn claimed(state: &StateDir, claim: &ActClaim) -> io::Result<Recovered> {
    let plan = &claim.plan_id;
    let entry = state.trash_entry(plan);
    let stranded = |why: String| {
        Ok(Recovered::Stranded {
            why,
            trash: entry.clone(),
        })
    };
    let mut names = Vec::new();
    match std::fs::read_dir(&entry) {
        Ok(listed) => {
            for name in listed {
                names.push(name?.file_name());
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    if names.iter().any(|name| name != "t" && name != "h") {
        return stranded("the trash entry holds what the claim does not account for".into());
    }
    match std::fs::read_dir(entry.join("h")) {
        Ok(mut listed) => {
            if listed.next().is_some() {
                return stranded("the hold holds what the claim does not account for".into());
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    if !names.iter().any(|name| name == "t") {
        // Nothing of the target was moved, or a restore renamed it back, when its name holds
        // the claimed inode: only the empty entry is left. Otherwise the target left `t` for
        // somewhere recovery cannot vouch for, and the entry stays as the evidence.
        if let Some(why) = not_at_its_name(claim)? {
            return stranded(why);
        }
        if let Err(err) = remove_empty_entry(state, plan) {
            return stranded(format!("the trash entry cannot be removed: {err}"));
        }
        return Ok(interrupted(state, claim));
    }
    let t = dir::stat_at(&File::open(&entry)?, OsStr::new("t"))?;
    let kind = match t.kind {
        Kind::Dir => Some(EntryType::Dir),
        Kind::File => Some(EntryType::File),
        Kind::Symlink => Some(EntryType::Symlink),
        _ => None,
    };
    if (Some(claim.target.kind), t.dev, t.ino) != (kind, claim.target.dev, claim.target.ino) {
        return stranded("t holds another inode than the claimed target".into());
    }
    let target = Path::new(&claim.worktree.path).join(target_rel(claim));
    let Some(parent) = resolve_parent(claim)? else {
        return stranded(format!(
            "the target's parent no longer resolves to {}; the target stays at {}",
            target.parent().unwrap_or(&target).display(),
            entry.join("t").display()
        ));
    };
    match restore(state, plan, &parent, OsStr::new(&claim.target.name)) {
        Ok(()) => Ok(interrupted(state, claim)),
        Err(err) => stranded(format!(
            "{} cannot go back to {}: {err}",
            entry.join("t").display(),
            target.display()
        )),
    }
}

/// Why the target's name, in the parent the claim's chain resolves to, does not hold the
/// claimed inode with its kind; `None` when it does.
fn not_at_its_name(claim: &ActClaim) -> io::Result<Option<String>> {
    let target = Path::new(&claim.worktree.path).join(target_rel(claim));
    let Some(parent) = resolve_parent(claim)? else {
        return Ok(Some(format!(
            "no t, and the target's parent no longer resolves to {}",
            target.parent().unwrap_or(&target).display()
        )));
    };
    let found = match dir::stat_at(&parent, OsStr::new(&claim.target.name)) {
        Ok(st) => st,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(Some(format!(
                "no t, and nothing at {}: the claimed target is elsewhere",
                target.display()
            )));
        }
        Err(err) => return Err(err),
    };
    let kind = match found.kind {
        Kind::Dir => Some(EntryType::Dir),
        Kind::File => Some(EntryType::File),
        Kind::Symlink => Some(EntryType::Symlink),
        _ => None,
    };
    Ok(((Some(claim.target.kind), found.dev, found.ino)
        != (kind, claim.target.dev, claim.target.ino))
        .then(|| {
            format!(
                "no t, and {} holds another inode than the claimed target",
                target.display()
            )
        }))
}

/// Writes the result of a `claimed` execution recovery undid: `rejected` (`interrupted`).
fn interrupted(state: &StateDir, claim: &ActClaim) -> Recovered {
    let result = ActResult {
        schema: ActResultSchema,
        plan_id: claim.plan_id.clone(),
        caller: claim.caller.clone(),
        status: ActStatus::Rejected,
        reasons: vec![INTERRUPTED.to_owned()],
        checks: Vec::new(),
        freed_bytes: None,
        skipped_changed: Vec::new(),
        trash: None,
        recovered: true,
        error: None,
        finished_ms: state::now_ms(),
    };
    match write_result(state, &result) {
        Ok(()) => Recovered::Done(Box::new(result)),
        Err(err) => Recovered::Stranded {
            why: format!("the result: {err}"),
            trash: state.trash_entry(&claim.plan_id),
        },
    }
}

/// Syncs what recovery found before it acts on it (section 9.7, durable order): the claim,
/// its directory, the target's parent as far as the chain resolves, `<state>/trash/`, and the
/// trash entry with its `t` and `h` where they exist.
fn make_found_durable(state: &StateDir, claim: &ActClaim) -> io::Result<()> {
    state::sync_file(&state.execution_claim(&claim.plan_id))?;
    state::sync_dir(&state.execution_dir(&claim.plan_id))?;
    if let Some(parent) = resolve_parent(claim)? {
        let synced = parent.sync_all();
        state::logged(
            Op::Sync,
            &dir::current_path(&parent).unwrap_or_default(),
            None,
            synced,
        )?;
    }
    let entry = state.trash_entry(&claim.plan_id);
    for path in [
        state.trash_dir(),
        entry.clone(),
        entry.join("t"),
        entry.join("h"),
    ] {
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => state::sync_dir(&path)?,
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// The target's parent, reached from the worktree root by the chain's names, each with the
/// claimed identity, without following a symbolic link; `None` when the chain does not
/// resolve: a name missing, not a directory, or not the one claimed. A read that fails
/// otherwise is an error (section 9.7, "Reads that fail").
pub fn resolve_parent(claim: &ActClaim) -> io::Result<Option<File>> {
    let opened = |opened: io::Result<Option<File>>| match opened {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        other => other,
    };
    let is = |fd: &File, id: (u64, u64)| dir::stat_fd(fd).map(|st| (st.dev, st.ino) == id);
    let root = (claim.worktree.root.dev, claim.worktree.root.ino);
    let Some(mut current) = opened(dir::open(Path::new(&claim.worktree.path)))? else {
        return Ok(None);
    };
    if !is(&current, root)? {
        return Ok(None);
    }
    for link in &claim.chain {
        let Some(next) = opened(dir::open_at(&current, OsStr::new(&link.name)))? else {
            return Ok(None);
        };
        if !is(&next, (link.dev, link.ino))? {
            return Ok(None);
        }
        current = next;
    }
    Ok(Some(current))
}

/// The target's path relative to its worktree: the chain's names and the target's.
#[must_use]
pub fn target_rel(claim: &ActClaim) -> PathBuf {
    claim
        .chain
        .iter()
        .map(|link| link.name.as_str())
        .chain(std::iter::once(claim.target.name.as_str()))
        .collect()
}

/// Recovers a `committed` execution (section 9.7's two `committed` rows).
fn committed(
    state: &StateDir,
    config_dir: &Path,
    claim: &ActClaim,
    grant: Grant<'_>,
) -> io::Result<Recovered> {
    let plan = &claim.plan_id;
    let entry = state.trash_entry(plan);
    let stranded = |why: String| {
        Ok(Recovered::Stranded {
            why,
            trash: entry.clone(),
        })
    };
    let measure = measure(state, &entry);
    match std::fs::symlink_metadata(&entry) {
        Ok(_) => {}
        // No trash entry: the deletion and its cleanup were done; only the result is missing.
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let freed = counted(&measure).then_some(0);
            return Ok(finish(state, claim, false, Vec::new(), Vec::new(), freed));
        }
        Err(err) => return Err(err),
    }
    let manifest = match read_manifest(state, plan) {
        Ok(manifest) if manifest.len() as u64 == claim.manifest_entries => manifest,
        Ok(manifest) => {
            return stranded(format!(
                "the manifest holds {} entries where the claim names {}",
                manifest.len(),
                claim.manifest_entries
            ));
        }
        Err(why) => return stranded(why),
    };
    let rel = target_rel(claim);
    let mut watch = Watch::new(config_dir, &claim.repo, &rel, &entry, &manifest);
    let loaded = watch.reload();
    let Some(config) = watch.config() else {
        return stranded(loaded.err().unwrap_or_default());
    };
    let caller = match claim.caller.parse::<Caller>() {
        Ok(caller) => caller,
        Err(err) => return stranded(err.to_string()),
    };
    if let Err(why) = grant(config, &caller) {
        return stranded(why);
    }
    if let Caller::Unit(name) = &caller
        && !config.host.unit_may_act(name)
    {
        return stranded(format!("unit.{name}.act is false"));
    }
    if let Err(why) = loaded {
        return stranded(why);
    }
    if let Err(why) = watch.before_batch() {
        return stranded(why);
    }
    if let Err(err) = make_hold(state, &entry) {
        return stranded(format!("the hold: {err}"));
    }
    let held = match put_back_held(state, &entry, &manifest) {
        Ok(held) => held,
        Err(err) => return stranded(format!("a put-back: {err}")),
    };
    let deletion = Deletion {
        entry: &entry,
        manifest: &manifest,
        target_rel: &rel,
        measure: &measure,
        reserve: &state.reserve_file(),
    };
    let mut deleted = deletion.run(&mut watch);
    for pos in held {
        deleted.fates[pos] = Fate::Kept;
    }
    if let Some(stop) = &deleted.stopped {
        return stranded(stop.to_string());
    }
    let kept = deleted.kept(&manifest);
    let remains = match clean_up(&entry) {
        Ok(remains) => remains,
        Err(err) => return stranded(format!("the cleanup: {err}")),
    };
    let checks = vec![unscanned_check(&watch.unscanned())];
    Ok(finish(
        state,
        claim,
        remains,
        kept,
        checks,
        deleted.freed_bytes,
    ))
}

/// The check the result lists for the descriptor scans the deletion made: `not_in_use`,
/// passed, since a use found stops the deletion, with the processes the scans could not read,
/// which do not count (section 9.7), cut as `act` cuts them, the rest counted in `omitted`.
fn unscanned_check(unscanned: &[(u32, String)]) -> Check {
    let entries: Vec<serde_json::Value> = unscanned
        .iter()
        .map(|(pid, error)| serde_json::json!({ "pid": pid, "error": error }))
        .collect();
    Check {
        name: "not_in_use".to_owned(),
        role: CheckRole::Invariant,
        result: CheckResult::Pass,
        detail: String::new(),
        facts: vec![crate::plan::processes_fact(
            "in_use.descriptors.unscanned",
            Some(&entries),
            "descriptor scan",
        )],
    }
}

/// Makes the trash entry's hold `h` again when a cleanup removed it (section 9.7, "Recovery":
/// any part of the trash entry), created without replacing through the reserve's retry, and
/// syncs the trash entry.
fn make_hold(state: &StateDir, entry: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    let hold = entry.join("h");
    if present(&hold)? {
        return Ok(());
    }
    state::retry_with_reserve(&state.reserve_file(), || {
        let made = std::fs::DirBuilder::new().mode(0o700).create(&hold);
        state::logged(Op::Mkdir, &hold, None, made)
    })?;
    state::sync_dir(entry)
}

/// Whether an entry is at `path`, not following a final symbolic link.
fn present(path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

/// Whether `measure` reads what an unlink frees.
fn counted(measure: &Measure) -> bool {
    matches!(
        measure,
        Measure::PrivateSize | Measure::Extents | Measure::Links
    )
}

/// The measure of the file system the trash is on: read from the trash entry, or from
/// `<state>/trash/` without one, a test's fake first.
fn measure(state: &StateDir, entry: &Path) -> Measure {
    let at = if entry.exists() {
        entry.to_path_buf()
    } else {
        state.trash_dir()
    };
    if let Some(ty) = testpoint::fake_fs_type(&at) {
        return Measure::of(&Ok(ty));
    }
    let read = match dir::open(&at) {
        Ok(Some(fd)) => crate::os::mount::fs_type(&fd),
        Ok(None) => Err("not a directory".to_owned()),
        Err(err) => Err(err.to_string()),
    };
    Measure::of(&read)
}

/// Puts each entry of the hold back through `t` by its manifest path, walking from `t` as the
/// deletion walks: each directory opened by name with its recorded identity. An entry stays in
/// the hold when the walk stops or its name is taken, and so does a name that is no manifest
/// index. Returns the manifest positions of the entries that stay; a write that fails for
/// lack of space after the reserve's retry is an error.
fn put_back_held(
    state: &StateDir,
    entry: &Path,
    manifest: &[ManifestEntry],
) -> io::Result<Vec<usize>> {
    let Some(entry_fd) = dir::open(entry)? else {
        return Ok(Vec::new());
    };
    let hold = match dir::open_at(&entry_fd, OsStr::new("h")) {
        Ok(Some(hold)) => hold,
        Ok(None) => return Ok(Vec::new()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(err) => return Err(err),
    };
    let tree = delete::Tree::of(manifest).map_err(io::Error::other)?;
    let by_index: HashMap<u64, usize> = manifest
        .iter()
        .enumerate()
        .map(|(pos, e)| (e.i, pos))
        .collect();
    let mut parents = vec![None; manifest.len()];
    for (pos, children) in tree.children.iter().enumerate() {
        for &child in children {
            parents[child] = Some(pos);
        }
    }
    let mut names: Vec<OsString> = std::fs::read_dir(entry.join("h"))?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<io::Result<_>>()?;
    names.sort();
    let mut stayed = Vec::new();
    for name in names {
        let Some(pos) = name
            .to_str()
            .and_then(|n| n.parse::<u64>().ok())
            .and_then(|i| by_index.get(&i).copied())
        else {
            continue;
        };
        // The directory the entry goes back into, and its name there.
        let into = if pos == tree.root {
            Some((entry_fd.try_clone()?, OsString::from("t")))
        } else {
            let mut chain = Vec::new();
            let mut at = parents[pos];
            while let Some(p) = at {
                chain.push(p);
                at = parents[p];
            }
            chain.reverse();
            walk_down(&entry_fd, manifest, &tree, &chain).map(|fd| (fd, tree.names[pos].clone()))
        };
        let Some((into, as_name)) = into else {
            stayed.push(pos);
            continue;
        };
        let back = state::retry_with_reserve(&state.reserve_file(), || {
            testpoint::fake_write("put_back")?;
            let renamed = dir::rename_noreplace(&hold, &name, &into, &as_name);
            state::logged(
                Op::Rename,
                &entry.join("h").join(&name),
                Some(&dir::current_path(&into).unwrap_or_default().join(&as_name)),
                renamed,
            )
        });
        match back {
            Ok(()) => {
                for (fd, path) in [
                    (&into, dir::current_path(&into)),
                    (&hold, Ok(entry.join("h"))),
                ] {
                    state::logged(Op::Sync, &path.unwrap_or_default(), None, fd.sync_all())?;
                }
            }
            Err(err) if state::is_no_space(&err) => return Err(err),
            Err(_) => stayed.push(pos),
        }
    }
    Ok(stayed)
}

/// The directory at the end of `chain`, manifest positions from the target down, opened from
/// the trash entry's `t` one name at a time with each directory's recorded identity; `None`
/// when one is missing or not the one recorded.
fn walk_down(
    entry: &File,
    manifest: &[ManifestEntry],
    tree: &delete::Tree,
    chain: &[usize],
) -> Option<File> {
    let mut current: Option<File> = None;
    for &pos in chain {
        let name = if pos == tree.root {
            OsStr::new("t")
        } else {
            tree.names[pos].as_os_str()
        };
        let parent = current.as_ref().unwrap_or(entry);
        let next = dir::open_at(parent, name).ok()??;
        let st = dir::stat_fd(&next).ok()?;
        let recorded = &manifest[pos];
        if recorded.kind != EntryType::Dir || (st.dev, st.ino) != (recorded.dev, recorded.ino) {
            return None;
        }
        current = Some(next);
    }
    current
}

/// Finishes the trash entry once the deletion took `t` (section 9.7, step 10): pauses at
/// `act.finishing`, removes `h` once it is empty and then the entry, and syncs what changed,
/// `<state>/trash/` once the entry is gone. While `t` stays, with what the deletion kept, the
/// entry and its hold stay too, and the hold is synced. Returns whether the entry remains.
pub fn clean_up(entry: &Path) -> io::Result<bool> {
    let hold = entry.join("h");
    if present(&entry.join("t"))? {
        if present(&hold)? {
            state::sync_dir(&hold)?;
        }
        return Ok(true);
    }
    testpoint::hit(testpoint::ACT_FINISHING).map_err(io::Error::other)?;
    let gone = |path: &Path| -> io::Result<bool> {
        match state::logged(Op::Rmdir, path, None, std::fs::remove_dir(path)) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(true),
            // Something stays: entries the hold kept, or what is not the deletion's.
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(false)
            }
            Err(err) => Err(err),
        }
    };
    if !gone(&hold)? {
        state::sync_dir(&hold)?;
    } else if gone(entry)? {
        state::sync_parent(entry)?;
        return Ok(false);
    }
    state::sync_dir(entry)?;
    Ok(true)
}

/// Writes the result of a committed execution that recovery finished, with `checks`:
/// `executed`, or `partial` with the kept entries and the trash entry while it remains.
fn finish(
    state: &StateDir,
    claim: &ActClaim,
    remains: bool,
    mut kept: Vec<String>,
    checks: Vec<Check>,
    freed_bytes: Option<u64>,
) -> Recovered {
    let entry = state.trash_entry(&claim.plan_id);
    if remains && kept.is_empty() {
        // The trash entry holds what the manifest does not name: the target is kept.
        kept.push(String::new());
    }
    let result = ActResult {
        schema: ActResultSchema,
        plan_id: claim.plan_id.clone(),
        caller: claim.caller.clone(),
        // An entry kept outside the trash (a directory moved out of `t` while it was
        // deleted) makes the result partial too.
        status: if remains || !kept.is_empty() {
            ActStatus::Partial
        } else {
            ActStatus::Executed
        },
        reasons: Vec::new(),
        checks,
        freed_bytes,
        skipped_changed: kept,
        trash: remains.then(|| entry.display().to_string()),
        recovered: true,
        error: None,
        finished_ms: state::now_ms(),
    };
    match write_result(state, &result) {
        Ok(()) => Recovered::Done(Box::new(result)),
        Err(err) => Recovered::Stranded {
            why: format!("the result: {err}"),
            trash: entry,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::unscanned_check;

    /// The result lists the processes the deletion's scans could not read as `act` lists
    /// them: the first three, the rest counted in `omitted`.
    #[test]
    fn the_result_lists_the_unscanned_processes() {
        let unscanned: Vec<(u32, String)> = (1..=5).map(|pid| (pid, format!("E{pid}"))).collect();
        let check = unscanned_check(&unscanned);
        assert_eq!(check.name, "not_in_use");
        let [fact] = &check.facts[..] else {
            panic!("{check:?}");
        };
        assert_eq!(fact.name, "in_use.descriptors.unscanned");
        assert_eq!(
            fact.value,
            serde_json::json!([
                {"pid": 1, "error": "E1"},
                {"pid": 2, "error": "E2"},
                {"pid": 3, "error": "E3"}
            ])
        );
        assert_eq!(fact.omitted, Some(2));
        assert_eq!(unscanned_check(&[]).facts[0].value, serde_json::json!([]));
    }
}
