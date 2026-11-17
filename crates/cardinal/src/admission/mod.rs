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
pub struct AdmissionLock {
    _lock: FileLock,
}

/// Takes the admission lock exclusively, waiting at most `timeout`. `act` takes it this way
/// before it calls [`in_use`].
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

/// One reservation file as read: the job ID from its name and the parsed document. A file
/// that does not parse is kept (and counted by nobody) rather than deleted.
fn read_reservations(state: &StateDir) -> Result<Vec<Reservation>, AdmissionError> {
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

fn write_reservation(state: &StateDir, reservation: &Reservation) -> Result<(), AdmissionError> {
    let bytes = serde_json::to_vec(reservation).map_err(state_error)?;
    state
        .write_atomic(
            &state.reservation(&reservation.job_id),
            &bytes,
            Durability::Volatile,
        )
        .map_err(state_error)
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

/// Every reservation with its derived state, group members, and verification. Read-only: it
/// never deletes or signals anything (`inventory` uses it).
pub fn list_reservations(state: &StateDir) -> Result<Vec<ReservationView>, Error> {
    let facts = HostFacts::new();
    Ok(read_reservations(state)?
        .into_iter()
        .map(|reservation| {
            let derived = derive(&reservation, probe_live(state, &reservation.job_id), &facts);
            view(reservation, derived, &facts)
        })
        .collect())
}

/// Whether a live or recovery-needed reservation names the worktree with key `worktree_key`,
/// and which jobs do. The caller holds the [`lock`]: `act` calls it under the admission lock
/// while it holds the worktree's in-use lock exclusively.
pub fn in_use(state: &StateDir, worktree_key: &str) -> Result<InUse, Error> {
    let facts = HostFacts::new();
    let jobs: Vec<String> = read_reservations(state)?
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
    _lock: &AdmissionLock,
    state: &StateDir,
    facts: &dyn ProcessFacts,
) -> Result<Recovery, AdmissionError> {
    let reservations = read_reservations(state)?;
    let mut kept = Vec::new();
    let mut released = Vec::new();
    for reservation in reservations {
        let derived = derive(&reservation, probe_live(state, &reservation.job_id), facts);
        if derived.state == DerivedState::Stale {
            let GroupIdentity::Pgid { pgid, .. } = reservation.group;
            remove_if_exists(&state.reservation(&reservation.job_id)).map_err(state_error)?;
            let _ = remove_if_exists(&state.live_lock(&reservation.job_id));
            released.push(Recovered {
                job_id: reservation.job_id,
                pgid,
            });
        } else {
            kept.push((reservation, derived));
        }
    }
    collect_free_live_locks(state, &kept);
    Ok(Recovery { kept, released })
}

/// Deletes live lock files that nobody holds and no reservation names: left by runners that
/// died before reserving. Runs only inside the admission lock, where live lock files are
/// created, so a file is never deleted between its creation and its locking.
fn collect_free_live_locks(state: &StateDir, kept: &[(Reservation, Derived)]) {
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
            let _ = std::fs::remove_file(&path);
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
    let guard = lock(state, Duration::from_millis(host.admission.lock_timeout_ms))?;
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
/// admitted deletes its live lock file.
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
            lock_timeout: Duration::from_millis(host.admission.lock_timeout_ms),
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
            write_reservation(&self.state, &reservation)?;
            Some(reservation)
        } else {
            None
        };
        drop(guard);
        append_recovered(self.recorder.as_ref(), &recovery.released);
        Ok(match admitted {
            Some(reservation) => Pass::Admitted(Box::new(Job {
                state: self.state.clone(),
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
        if let Some(live) = self.live.take() {
            let _ = std::fs::remove_file(live.path());
        }
    }
}

/// An admitted job: its reservation file and its live lock. Dropping it releases both, so a
/// runner that fails open or unwinds before the spawn never leaves a reservation behind;
/// after the spawn the runner keeps the job until the stage has ended.
pub struct Job {
    state: StateDir,
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
    /// immediately before the spawn.
    pub fn starting(&mut self) -> Result<(), AdmissionError> {
        self.reservation.state = ReservationState::Starting;
        self.reservation.updated_ms = now_ms();
        write_reservation(&self.state, &self.reservation)
    }

    /// Writes `running` with the child's identity.
    pub fn running(&mut self, child: ProcessIdentity) -> Result<(), AdmissionError> {
        self.reservation.state = ReservationState::Running;
        self.reservation.child = Some(child);
        self.reservation.updated_ms = now_ms();
        write_reservation(&self.state, &self.reservation)
    }

    /// Deletes the reservation, then the live lock file, then drops the live lock.
    pub fn release(mut self) -> Result<(), AdmissionError> {
        self.done = true;
        let result = remove_if_exists(&self.state.reservation(&self.reservation.job_id))
            .map_err(state_error);
        if let Some(live) = self.live.take() {
            let _ = std::fs::remove_file(live.path());
        }
        result
    }

    /// Keeps the reservation and the live lock file in place and drops the live lock: the
    /// reservation is then evaluated like that of a runner that died. Used when the stage's
    /// group did not empty before the runner gave up waiting.
    pub fn abandon(mut self) {
        self.done = true;
        drop(self.live.take());
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        if !self.done {
            let _ = remove_if_exists(&self.state.reservation(&self.reservation.job_id));
            if let Some(live) = self.live.take() {
                let _ = std::fs::remove_file(live.path());
            }
        }
    }
}

#[cfg(test)]
mod tests;
