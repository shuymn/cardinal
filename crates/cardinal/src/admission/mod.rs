//! Admission: budget math, the reservation store, liveness and the recovery pass, and the
//! worktree in-use query (`docs/architecture.md` sections 4.4–4.6).
//!
//! Phase 0 decides from reservations alone: the resources of every `live` and
//! `recovery_needed` reservation plus the new job's, compared per resource with the budgets
//! of `host.toml`, inside the admission lock. A job gets all of its resources or none.
//!
//! Admission never starts a process, never touches the network, and never signals: a
//! recovery-needed reservation counts until its group is empty or the boot changed, because
//! nothing pins a dead runner's process group ID between a check and a signal (section 4.5).

mod budget;
mod liveness;

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub use budget::{fits, resources, sum};
pub use liveness::{BOOT_TOLERANCE_S, Derived, HostFacts, ProcessFacts, derive};

use crate::config::HostConfig;
use crate::contract::{
    AdmissionResult, DerivedState, Error, ErrorCode, GroupIdentity, InUse, ProcessIdentity,
    RecordKind, Reservation, ReservationState, ReservationView, Resources,
};
use crate::record::Recorder;
use crate::state::{self, Durability, FileLock, LockError, LockMode, Probe, StateDir, now_ms};

/// What one recovery pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Jobs whose stale reservations were deleted.
    pub released: Vec<String>,
    /// The reservations left after the pass, with their derived state.
    pub reservations: Vec<ReservationView>,
}

/// Why admission could not decide. Before the spawn, every one of these makes the runner fail
/// open.
#[derive(Debug, thiserror::Error)]
pub enum AdmissionError {
    #[error("timed out waiting for the admission lock")]
    LockTimeout,
    #[error("{0}")]
    State(String),
}

impl From<AdmissionError> for Error {
    fn from(err: AdmissionError) -> Self {
        Error::new(ErrorCode::StateUnavailable, err.to_string())
    }
}

fn state_error(err: impl std::fmt::Display) -> AdmissionError {
    AdmissionError::State(err.to_string())
}

/// The admission lock: the critical section of section 4.4. Lock order is the worktree in-use
/// lock, then this lock, then the inbox or records lock.
///
/// Every creation, replacement, and deletion of a reservation or a live lock file, and every
/// listing of `reservations/`, happens while this lock is held (section 4.3): a directory
/// listing that runs while another process renames or deletes an entry of that directory may
/// omit entries, the unchanged ones included. The functions that touch those files take the
/// guard as an argument.
pub struct AdmissionLock {
    _lock: FileLock,
}

/// Takes the admission lock exclusively, waiting at most `timeout`.
pub fn lock(state: &StateDir, timeout: Duration) -> Result<AdmissionLock, AdmissionError> {
    match state::lock(
        &state.admission_lock(),
        LockMode::Exclusive,
        Instant::now() + timeout,
    ) {
        Ok(lock) => Ok(AdmissionLock { _lock: lock }),
        Err(LockError::TimedOut(_)) => Err(AdmissionError::LockTimeout),
        Err(err) => Err(state_error(err)),
    }
}

/// The deadline of every wait for the admission lock (`admission.lock_timeout_ms`).
fn lock_timeout(host: &HostConfig) -> Duration {
    Duration::from_millis(host.admission.lock_timeout_ms)
}

/// Every reservation, read under the admission lock, which is released before the caller
/// evaluates them. A lock that cannot be taken in time is an error, never an empty list.
pub fn snapshot(state: &StateDir, host: &HostConfig) -> Result<Vec<Reservation>, AdmissionError> {
    let guard = lock(state, lock_timeout(host))?;
    read_reservations(&guard, state)
}

/// Every reservation, and a message for each file `reservations/` lists that gave none: one
/// that cannot be read (a missing one included), does not parse, or names another job. Both
/// come from one listing under the admission lock, released before the caller evaluates
/// them. For a reader that shows the store as it is (the snapshot, section 10.6), where
/// [`snapshot`] stops at a file that cannot be read and skips the others.
pub fn snapshot_with_unread(
    state: &StateDir,
    host: &HostConfig,
) -> Result<(Vec<Reservation>, Vec<String>), AdmissionError> {
    let guard = lock(state, lock_timeout(host))?;
    let scan = scan_reservations(&guard, state)?;
    drop(guard);
    let unread = scan
        .unread
        .iter()
        .map(|(path, why)| format!("{}: {why}", path.display()))
        .collect();
    Ok((scan.found, unread))
}

/// Why a file `reservations/` lists gave no reservation.
enum Unread {
    Listing(io::Error),
    Read(io::Error),
    Parse(serde_json::Error),
    OtherJob,
}

impl std::fmt::Display for Unread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Listing(err) | Self::Read(err) => err.fmt(f),
            Self::Parse(err) => err.fmt(f),
            Self::OtherJob => f.write_str("names another job"),
        }
    }
}

/// One listing of `reservations/`: every reservation, by creation, and each listed file that
/// gave none, with why.
struct Scan {
    found: Vec<Reservation>,
    unread: Vec<(PathBuf, Unread)>,
}

fn scan_reservations(_lock: &AdmissionLock, state: &StateDir) -> Result<Scan, AdmissionError> {
    let dir = state.reservations_dir();
    let entries =
        std::fs::read_dir(&dir).map_err(|err| state_error(format!("{}: {err}", dir.display())))?;
    let mut scan = Scan {
        found: Vec::new(),
        unread: Vec::new(),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                scan.unread.push((dir.clone(), Unread::Listing(err)));
                continue;
            }
        };
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                scan.unread.push((path, Unread::Read(err)));
                continue;
            }
        };
        match serde_json::from_slice::<Reservation>(&bytes) {
            Ok(reservation)
                if path.file_stem().and_then(|s| s.to_str())
                    == Some(reservation.job_id.as_str()) =>
            {
                scan.found.push(reservation);
            }
            Ok(_) => scan.unread.push((path, Unread::OtherJob)),
            Err(err) => scan.unread.push((path, Unread::Parse(err))),
        }
    }
    scan.found
        .sort_by(|a, b| (a.created_ms, &a.job_id).cmp(&(b.created_ms, &b.job_id)));
    Ok(scan)
}

/// One reservation file as read: the job ID from its name and the parsed document. A file
/// that does not parse is kept (and counted by nobody) rather than deleted.
fn read_reservations(
    _lock: &AdmissionLock,
    state: &StateDir,
) -> Result<Vec<Reservation>, AdmissionError> {
    let dir = state.reservations_dir();
    let entries =
        std::fs::read_dir(&dir).map_err(|err| state_error(format!("{}: {err}", dir.display())))?;
    let mut found = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            // Released between the listing and the read.
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(state_error(format!("{}: {err}", path.display()))),
        };
        let Ok(reservation) = serde_json::from_slice::<Reservation>(&bytes) else {
            continue;
        };
        if path.file_stem().and_then(|s| s.to_str()) == Some(reservation.job_id.as_str()) {
            found.push(reservation);
        }
    }
    found.sort_by(|a, b| (a.created_ms, &a.job_id).cmp(&(b.created_ms, &b.job_id)));
    Ok(found)
}

fn write_reservation(
    _lock: &AdmissionLock,
    state: &StateDir,
    reservation: &Reservation,
) -> Result<(), AdmissionError> {
    let bytes = serde_json::to_vec(reservation).map_err(state_error)?;
    state
        .write_atomic(
            &state.reservation(&reservation.job_id),
            &bytes,
            Durability::Volatile,
        )
        .map_err(state_error)
}

/// Deletes the reservation of `job`; one that is already gone is not an error.
fn remove_reservation(_lock: &AdmissionLock, state: &StateDir, job: &str) -> io::Result<()> {
    remove_if_exists(&state.reservation(job))
}

/// Deletes a live lock file. Callers pass only a file that nobody holds or one they hold
/// themselves; one that is already gone is not an error.
fn remove_live_file(_lock: &AdmissionLock, path: &std::path::Path) {
    let _ = remove_if_exists(path);
}

fn remove_if_exists(path: &std::path::Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

fn probe_live(state: &StateDir, job: &str) -> Option<Probe> {
    state::probe(&state.live_lock(job)).ok()
}

fn view(reservation: Reservation, derived: Derived, facts: &dyn ProcessFacts) -> ReservationView {
    let GroupIdentity::Pgid { pgid, .. } = reservation.group;
    let members = if derived.state == DerivedState::Stale {
        // The group is empty or its ID now belongs to an unrelated group.
        Vec::new()
    } else {
        facts.members(pgid)
    };
    ReservationView {
        reservation,
        derived: derived.state,
        members,
        verified: derived.verified,
    }
}

/// Every reservation with its derived state, group members, and verification, from a
/// [`snapshot`]: the states are derived and the process table read after the admission lock is
/// released. Read-only: it never deletes or signals anything (`inventory` uses it).
pub fn list_reservations(
    state: &StateDir,
    host: &HostConfig,
) -> Result<Vec<ReservationView>, Error> {
    Ok(views(state, snapshot(state, host)?))
}

/// Each reservation with its derived state, group members, and verification. It probes the
/// live locks and reads the process table, so the caller holds no admission lock.
#[must_use]
pub fn views(state: &StateDir, reservations: Vec<Reservation>) -> Vec<ReservationView> {
    let facts = HostFacts::new();
    reservations
        .into_iter()
        .map(|reservation| {
            let derived = derive(&reservation, probe_live(state, &reservation.job_id), &facts);
            view(reservation, derived, &facts)
        })
        .collect()
}

/// Whether a live or recovery-needed reservation names the worktree with key `worktree_key`,
/// and which jobs do, from a [`snapshot`] (`plan` and `act` use it).
pub fn in_use(state: &StateDir, host: &HostConfig, worktree_key: &str) -> Result<InUse, Error> {
    let reservations = snapshot(state, host)?;
    let facts = HostFacts::new();
    let jobs: Vec<String> = reservations
        .into_iter()
        .filter(|r| r.worktree.as_ref().is_some_and(|w| w.key == worktree_key))
        .filter(|r| derive(r, probe_live(state, &r.job_id), &facts).state != DerivedState::Stale)
        .map(|r| r.job_id)
        .collect();
    Ok(InUse {
        value: !jobs.is_empty(),
        jobs,
    })
}

/// A reservation a recovery pass released, appended as a `job_recovered` record once the
/// admission lock is released.
#[derive(Clone, Debug)]
struct Recovered {
    job_id: String,
    pgid: u32,
}

/// The outcome of the recovery pass run inside the admission lock.
struct Recovery {
    /// Every reservation that is still there, with its derived state.
    kept: Vec<(Reservation, Derived)>,
    released: Vec<Recovered>,
}

/// The recovery pass of section 4.4, run while holding the admission lock: deletes `stale`
/// reservations and their live lock files, and deletes free live lock files without a
/// reservation. It keeps `live` and `recovery_needed` reservations as they are and signals
/// nothing.
fn recover(
    guard: &AdmissionLock,
    state: &StateDir,
    facts: &dyn ProcessFacts,
) -> Result<Recovery, AdmissionError> {
    let reservations = read_reservations(guard, state)?;
    let mut kept = Vec::new();
    let mut released = Vec::new();
    for reservation in reservations {
        let derived = derive(&reservation, probe_live(state, &reservation.job_id), facts);
        if derived.state == DerivedState::Stale {
            let GroupIdentity::Pgid { pgid, .. } = reservation.group;
            remove_reservation(guard, state, &reservation.job_id).map_err(state_error)?;
            remove_live_file(guard, &state.live_lock(&reservation.job_id));
            released.push(Recovered {
                job_id: reservation.job_id,
                pgid,
            });
        } else {
            kept.push((reservation, derived));
        }
    }
    collect_free_live_locks(guard, state, &kept);
    Ok(Recovery { kept, released })
}

/// Deletes live lock files that nobody holds and no reservation names: left by runners that
/// died before reserving. Runs only inside the admission lock, where live lock files are
/// created, so a file is never deleted between its creation and its locking.
fn collect_free_live_locks(
    guard: &AdmissionLock,
    state: &StateDir,
    kept: &[(Reservation, Derived)],
) {
    let Ok(entries) = std::fs::read_dir(state.live_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path: PathBuf = entry.path();
        let Some(job) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".lock"))
        else {
            continue;
        };
        if kept.iter().any(|(r, _)| r.job_id == job) {
            continue;
        }
        if state::probe(&path).ok() == Some(Probe::Free) {
            remove_live_file(guard, &path);
        }
    }
}

fn append_recovered(recorder: Option<&Recorder>, released: &[Recovered]) {
    let Some(recorder) = recorder else { return };
    for job in released {
        let mut record = recorder.record(RecordKind::JobRecovered);
        record.job_id = Some(job.job_id.clone());
        record.data.insert("action".into(), "released".into());
        record.data.insert("pgid".into(), job.pgid.into());
        let _ = recorder.append(&record);
    }
}

/// Runs the recovery pass of section 4.4 under the admission lock (taken with
/// `admission.lock_timeout_ms`). `cardinal inbox` runs it before raising items.
pub fn recovery_pass(state: &StateDir, host: &HostConfig) -> Result<RecoveryReport, Error> {
    let facts = HostFacts::new();
    let recorder = Recorder::new(state, host.records.max_bytes, host.records.keep);
    let guard = lock(state, lock_timeout(host))?;
    let recovery = recover(&guard, state, &facts)?;
    drop(guard);
    append_recovered(Some(&recorder), &recovery.released);
    Ok(RecoveryReport {
        released: recovery.released.into_iter().map(|r| r.job_id).collect(),
        reservations: recovery
            .kept
            .into_iter()
            .map(|(reservation, derived)| view(reservation, derived, &facts))
            .collect(),
    })
}

/// The result of one admission pass.
pub enum Pass {
    /// The reservation is written; the job owns it.
    Admitted(Box<Job>),
    /// The budget is full; the admission lock is released and the runner may try again.
    Full,
}

/// The runner's side of admission: one job, admitted in one or more passes.
///
/// The first pass claims the job ID by creating `live/<job>.lock` and holds that lock for the
/// runner's lifetime, before any reservation names the job. Dropping an `Admitter` that never
/// admitted deletes its live lock file under the admission lock; when that lock cannot be
/// taken in time, the free file is left for the recovery pass.
pub struct Admitter {
    state: StateDir,
    budget: Resources,
    lock_timeout: Duration,
    recorder: Option<Recorder>,
    job_id: String,
    live: Option<FileLock>,
}

impl Admitter {
    #[must_use]
    pub fn new(
        state: &StateDir,
        host: &HostConfig,
        job_id: String,
        recorder: Option<Recorder>,
    ) -> Self {
        Self {
            state: state.clone(),
            budget: host.budget.clone(),
            lock_timeout: lock_timeout(host),
            recorder,
            job_id,
            live: None,
        }
    }

    /// The job ID; it can change during the first pass if the ID was taken.
    #[must_use]
    pub fn job_id(&self) -> &str {
        &self.job_id
    }

    /// One pass of section 4.4 for `draft`, whose `job_id`, `state`, `admission`,
    /// `created_ms`, and `updated_ms` are filled in here. With `overflow`, the reservation is
    /// written even when the budget is full, with admission result `overflow` (the wait cap
    /// was reached, section 5.4).
    pub fn pass(&mut self, draft: &Reservation, overflow: bool) -> Result<Pass, AdmissionError> {
        let facts = HostFacts::new();
        let guard = lock(&self.state, self.lock_timeout)?;
        if self.live.is_none() {
            self.live = Some(self.claim_job_id()?);
        }
        let recovery = recover(&guard, &self.state, &facts)?;
        let reserved = sum(recovery.kept.iter().map(|(r, _)| &r.resources));
        let within = fits(&self.budget, &reserved, &draft.resources);
        let admitted = if within || overflow {
            let now = now_ms();
            let mut reservation = draft.clone();
            reservation.job_id.clone_from(&self.job_id);
            reservation.state = ReservationState::Reserved;
            reservation.admission = if within {
                AdmissionResult::WithinBudget
            } else {
                AdmissionResult::Overflow
            };
            reservation.created_ms = now;
            reservation.updated_ms = now;
            reservation.stop_signal_ms = None;
            reservation.child = None;
            write_reservation(&guard, &self.state, &reservation)?;
            Some(reservation)
        } else {
            None
        };
        drop(guard);
        append_recovered(self.recorder.as_ref(), &recovery.released);
        Ok(match admitted {
            Some(reservation) => Pass::Admitted(Box::new(Job {
                state: self.state.clone(),
                lock_timeout: self.lock_timeout,
                reservation,
                live: self.live.take(),
                done: false,
            })),
            None => Pass::Full,
        })
    }

    fn claim_job_id(&mut self) -> Result<FileLock, AdmissionError> {
        for _ in 0..8 {
            match state::create_locked(&self.state.live_lock(&self.job_id)) {
                Ok(lock) => return Ok(lock),
                Err(LockError::Io { source, .. })
                    if source.kind() == io::ErrorKind::AlreadyExists =>
                {
                    self.job_id = state::new_id();
                }
                Err(err) => return Err(state_error(err)),
            }
        }
        Err(state_error("cannot claim a job ID"))
    }
}

impl Drop for Admitter {
    fn drop(&mut self) {
        if let Some(live) = self.live.take()
            && let Ok(guard) = lock(&self.state, self.lock_timeout)
        {
            remove_live_file(&guard, live.path());
        }
    }
}

/// An admitted job: its reservation file and its live lock. Dropped while still `reserved`, it
/// releases both, so a runner that fails open or unwinds before the start boundary never
/// leaves a reservation behind. Once [`Job::starting`] has been called, the stage may be
/// running: dropped then, it is abandoned as by [`Job::abandon`], so no path that drops the
/// job (an unwinding panic included) deletes the reservation of a stage that may still run.
///
/// Every write and deletion takes the admission lock, waiting at most
/// `admission.lock_timeout_ms`. When the lock cannot be taken, the reservation stays as it
/// was: a release or a drop then abandons the job, which the recovery pass releases once its
/// group is empty (section 4.5), and nothing is ever deleted without the lock.
pub struct Job {
    state: StateDir,
    lock_timeout: Duration,
    reservation: Reservation,
    live: Option<FileLock>,
    done: bool,
}

impl Job {
    #[must_use]
    pub fn reservation(&self) -> &Reservation {
        &self.reservation
    }

    #[must_use]
    pub fn job_id(&self) -> &str {
        &self.reservation.job_id
    }

    /// Writes `starting`: the start boundary as seen from the state directory. Written
    /// immediately before the spawn. From here on, dropping the job abandons it. On an error
    /// the reservation stays `reserved`, in the file and here.
    pub fn starting(&mut self) -> Result<(), AdmissionError> {
        let mut next = self.reservation.clone();
        next.state = ReservationState::Starting;
        self.replace(next)
    }

    /// Writes `running` with the child's identity. On an error the reservation stays
    /// `starting`, in the file and here, which counts as possibly running.
    pub fn running(&mut self, child: ProcessIdentity) -> Result<(), AdmissionError> {
        let mut next = self.reservation.clone();
        next.state = ReservationState::Running;
        next.child = Some(child);
        self.replace(next)
    }

    /// Replaces the reservation with `next` under the admission lock.
    fn replace(&mut self, mut next: Reservation) -> Result<(), AdmissionError> {
        let guard = lock(&self.state, self.lock_timeout)?;
        next.updated_ms = now_ms();
        write_reservation(&guard, &self.state, &next)?;
        self.reservation = next;
        Ok(())
    }

    /// Under the admission lock, deletes the reservation, then the live lock file, then drops
    /// the live lock. When the lock cannot be taken in time, the job is abandoned instead.
    pub fn release(mut self) -> Result<(), AdmissionError> {
        self.done = true;
        let guard = match lock(&self.state, self.lock_timeout) {
            Ok(guard) => guard,
            Err(err) => {
                drop(self.live.take());
                return Err(err);
            }
        };
        self.delete(&guard).map_err(state_error)
    }

    /// Deletes the reservation, then the live lock file, then drops the live lock.
    fn delete(&mut self, guard: &AdmissionLock) -> io::Result<()> {
        let result = remove_reservation(guard, &self.state, &self.reservation.job_id);
        if let Some(live) = self.live.take() {
            remove_live_file(guard, live.path());
        }
        result
    }

    /// Keeps the reservation and the live lock file in place and drops the live lock: the
    /// reservation is then evaluated like that of a runner that died. Used when members of the
    /// stage's group are left once the stage has ended.
    pub fn abandon(mut self) {
        self.done = true;
        drop(self.live.take());
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if self.reservation.state == ReservationState::Reserved
            && let Ok(guard) = lock(&self.state, self.lock_timeout)
        {
            let _ = self.delete(&guard);
        }
        drop(self.live.take());
    }
}

#[cfg(test)]
mod tests;
