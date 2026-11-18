//! `cardinal reconcile` and `cardinal arbiter run` (`docs/architecture.md` sections 10.3–10.5):
//! one pass and exit, nothing resident between passes.
//!
//! [`run`] calls one function per step of section 10.3, in order. A step that fails is
//! recorded in the pass's `errors` and the next one runs; only step 1, which takes
//! `reconcile.lock`, ends the pass. The disk states are in `disk.rs`, `disk-reclaim`'s
//! listing in `reclaim.rs`, the items raised from observation and disk states in [`raise`],
//! and the arbiter's trigger and launcher in `arbiter.rs` (`docs/architecture.md` section
//! 14.2). Everything a pass writes under the state dir is written under `reconcile.lock`.

mod arbiter;
mod disk;
pub mod raise;
mod reclaim;

pub use arbiter::run as run_arbiter;
pub use disk::TIMER_INTERVAL_MS;

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::admission;
use crate::config::Config;
use crate::contract::{
    ArbiterStart, DiskState, Error, ErrorCode, InboxChanges, Inventory, Plan, PlanKind, Reconcile,
    ReconcileSchema, ReconcileState, ReconcileStateSchema, ScopedError, Skipped, Trigger,
};
use crate::inbox;
use crate::observe::{self, InventoryOptions};
use crate::plan;
use crate::snapshot;
use crate::state::{self, Durability, FileLock, LockMode, StateDir};
use crate::testpoint;

use disk::Watched;

/// What step 5 of a pass observed: the full observation, kept in
/// `reconcile/observation.json`, and the plans built from it, whose checks and exclusive
/// bytes were evaluated together at `checks_ms`. `disk-reclaim` reports from this one set.
#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    pub inventory: Inventory,
    pub plans: Vec<Plan>,
    pub checks_ms: u64,
    /// By plan ID, the bytes of each walked target's regular files with no name outside it
    /// (`plan::Built`).
    pub own_file_bytes: BTreeMap<String, u64>,
}

/// `cardinal reconcile [--trigger …]`: one pass (section 10.3).
pub fn run(config: &Config, state: &StateDir, trigger: Trigger) -> Result<Reconcile, Error> {
    // 1. One pass at a time; a held lock ends this one as `busy`.
    let mut pass = Reconcile::new(trigger);
    let Some(_lock) = lock(state)? else {
        pass.skipped = Some(Skipped::Busy);
        return Ok(pass);
    };

    // 2.
    pass.step("tmp", remove_temp_files(state));
    // 3.
    if let Some(changes) = pass.step("inbox", refresh_inbox(config, state)) {
        pass.add(changes);
    }
    // 4. The items as they were before the readings: the raisers end only these.
    let before = pass
        .step("inbox_before", raise::snapshot(state))
        .unwrap_or_default();
    let mut watched = disk::read_watched(config, state, &mut pass.errors);
    // 5.
    let observation = observe(config, state, &watched, &mut pass.errors);
    if let Some(observation) = &observation {
        pass.observed = true;
        if let Err(err) = testpoint::hit(testpoint::RECONCILE_OBSERVED) {
            pass.fail("observe", err.to_string());
        }
        if let Some(changes) = pass.step(
            "worktree_items",
            raise::worktree_items(config, state, &before, observation),
        ) {
            pass.add(changes);
        }
    }
    // 6. Read the volumes under pressure again, report on them, then raise, update, and end
    // the disk items of every watched volume this pass read, with those reports. A volume
    // whose readings all failed is left out: a level carried from an earlier pass ends no
    // item (A16).
    reread_volumes(config, state, &mut watched, &mut pass.errors);
    let reports = reclaim::reports(
        config,
        state,
        &watched,
        observation.as_ref(),
        &mut pass.errors,
    );
    let disks: Vec<DiskState> = watched
        .iter()
        .filter(|d| d.reading.is_some())
        .map(|d| d.state.clone())
        .collect();

    if let Some(changes) = pass.step(
        "disk_items",
        raise::disk_items(config, state, &before, &disks, &reports),
    ) {
        pass.add(changes);
    }
    pass.disk = watched.into_iter().map(|d| d.state).collect();
    pass.disk_reports = reports;
    // 7.
    if let Some(start) = pass.step("arbiter", arbiter::trigger(config, state)) {
        pass.arbiter = start;
    }
    // 8.
    pass.step("ui", snapshot::write_page(config, state).map(drop));
    pass.step("state", write_state(state, observation.as_ref()));
    // 9.
    Ok(pass)
}

/// One pass's result, built as the steps run.
impl Reconcile {
    fn new(trigger: Trigger) -> Self {
        Self {
            schema: ReconcileSchema,
            trigger,
            skipped: None,
            disk: Vec::new(),
            observed: false,
            disk_reports: Vec::new(),
            recovered: Vec::new(),
            inbox: InboxChanges::default(),
            arbiter: ArbiterStart::NotNeeded,
            errors: Vec::new(),
        }
    }

    /// The step's value, or `None` with its error recorded under `scope`.
    fn step<T>(&mut self, scope: &str, result: Result<T, Error>) -> Option<T> {
        result.map_err(|err| self.fail(scope, err.message)).ok()
    }

    fn fail(&mut self, scope: &str, message: String) {
        self.errors.push(ScopedError {
            scope: scope.to_owned(),
            message,
        });
    }

    fn add(&mut self, changes: InboxChanges) {
        self.inbox.raised.extend(changes.raised);
        self.inbox.cleared.extend(changes.cleared);
    }
}

/// Step 1: `reconcile.lock` taken exclusively with one try; `None` when another pass holds
/// it.
fn lock(state: &StateDir) -> Result<Option<FileLock>, Error> {
    Ok(state::try_lock(
        &state.reconcile_lock(),
        LockMode::Exclusive,
    )?)
}

/// Step 2: temp files older than one hour removed.
fn remove_temp_files(state: &StateDir) -> Result<(), Error> {
    state.gc_tmp().map(drop).map_err(Error::from)
}

/// Step 3: the inbox refresh of section 10.1, with the keys it raised and cleared.
fn refresh_inbox(config: &Config, state: &StateDir) -> Result<InboxChanges, Error> {
    inbox::refresh_with(config, state, &admission::recovery_pass).map(|()| InboxChanges::default())
}

/// Step 5: the full observation and its plans, when the last one is older than
/// `observe_every_s` or a volume is under pressure; `None` when none was due or it failed,
/// which is reported in `errors`. Entries matching a regenerable pattern in worktrees on a
/// volume under pressure get `reclaim_dir` plans, whose walks give the observation their sizes,
/// so each target is walked once and the sizes and the checks are one set.
fn observe(
    config: &Config,
    state: &StateDir,
    disks: &[Watched],
    errors: &mut Vec<ScopedError>,
) -> Option<Observation> {
    let pressured: Vec<&str> = disks
        .iter()
        .filter(|d| d.under_pressure())
        .map(|d| d.state.volume.as_str())
        .collect();
    if pressured.is_empty() && !observation_due(config, state, errors) {
        return None;
    }
    let options = InventoryOptions {
        repos: Vec::new(),
        sizes: false,
    };
    let mut inventory = match observe::inventory(config, state, &options) {
        Ok(inventory) => inventory,
        Err(err) => {
            errors.push(scoped("observe", err.message));
            return None;
        }
    };
    let checks_ms = state::now_ms();
    let on_pressure = |worktree: &crate::contract::Worktree| {
        disk::key_of(Path::new(&worktree.path)).is_some_and(|key| pressured.contains(&key.as_str()))
    };
    let built = plan::create_from(config, state, &inventory, &on_pressure, errors);
    let own_file_bytes = built
        .iter()
        .filter_map(|b| Some((b.plan.id.clone(), b.own_file_bytes?)))
        .collect();
    let plans: Vec<Plan> = built.into_iter().map(|b| b.plan).collect();
    fill_sizes(&mut inventory, &plans);
    write_json(
        state,
        &state.reconcile_observation(),
        &inventory,
        "observe",
        errors,
    );
    Some(Observation {
        inventory,
        plans,
        checks_ms,
        own_file_bytes,
    })
}

/// Whether the last full observation is older than `observe_every_s`, or there is none.
fn observation_due(config: &Config, state: &StateDir, errors: &mut Vec<ScopedError>) -> bool {
    let last = read_state(state, errors).and_then(|s| s.last_observation_ms);
    let every_ms = config.host.reconcile.observe_every_s.saturating_mul(1000);
    last.is_none_or(|last| state::now_ms().saturating_sub(last) >= every_ms)
}

/// The sizes of the `reclaim_dir` plans' walks, in the observation's ignored entries.
fn fill_sizes(inventory: &mut Inventory, plans: &[Plan]) {
    let entries = inventory
        .repos
        .iter_mut()
        .flat_map(|r| &mut r.worktrees)
        .flat_map(|w| &mut w.ignored);
    for entry in entries {
        if let Some(plan) = plans
            .iter()
            .find(|p| p.kind == PlanKind::ReclaimDir && p.target.path == entry.path)
        {
            entry.allocated_bytes = plan.allocated_bytes;
            entry.aliased_bytes = plan.aliased_bytes;
        }
    }
}

/// Step 6, first part: each volume under pressure at step 4 read again; a successful reading
/// updates its disk state and becomes the pass's last successful reading.
fn reread_volumes(
    config: &Config,
    state: &StateDir,
    disks: &mut [Watched],
    errors: &mut Vec<ScopedError>,
) {
    for volume in disks.iter_mut().filter(|d| d.under_pressure()) {
        disk::read(config, state, volume, errors);
    }
}

/// `reconcile/state.json`, if there is one; one that cannot be read is reported in `errors`.
fn read_state(state: &StateDir, errors: &mut Vec<ScopedError>) -> Option<ReconcileState> {
    read_json(&state.reconcile_state(), "state", errors)
}

/// Step 8, second part: `reconcile/state.json`, with this pass's time and the time of the
/// last full observation.
fn write_state(state: &StateDir, observation: Option<&Observation>) -> Result<(), Error> {
    let last_observation_ms = match observation {
        Some(observation) => Some(observation.inventory.observed_ms),
        None => read_state(state, &mut Vec::new()).and_then(|s| s.last_observation_ms),
    };
    let file = ReconcileState {
        schema: ReconcileStateSchema,
        last_pass_ms: state::now_ms(),
        last_observation_ms,
    };
    let bytes = serde_json::to_vec(&file)
        .map_err(|err| Error::new(ErrorCode::Internal, err.to_string()))?;
    state.write_atomic(&state.reconcile_state(), &bytes, Durability::Volatile)?;
    Ok(())
}

/// The JSON document at `path`; `None` when there is none, or when it cannot be read or
/// parsed, which is reported in `errors` under `scope`.
fn read_json<T: DeserializeOwned>(
    path: &Path,
    scope: &str,
    errors: &mut Vec<ScopedError>,
) -> Option<T> {
    let failed = |err: &dyn std::fmt::Display| scoped(scope, format!("{}: {err}", path.display()));
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|err| errors.push(failed(&err)))
            .ok(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            errors.push(failed(&err));
            None
        }
    }
}

/// Writes `value` to `path` atomically, a failure reported in `errors` under `scope`.
fn write_json<T: Serialize>(
    state: &StateDir,
    path: &Path,
    value: &T,
    scope: &str,
    errors: &mut Vec<ScopedError>,
) {
    let written = serde_json::to_vec(value)
        .map_err(|err| err.to_string())
        .and_then(|bytes| {
            state
                .write_atomic(path, &bytes, Durability::Volatile)
                .map_err(|err| err.to_string())
        });
    if let Err(message) = written {
        errors.push(scoped(scope, message));
    }
}

fn scoped(scope: &str, message: String) -> ScopedError {
    ScopedError {
        scope: scope.to_owned(),
        message,
    }
}
