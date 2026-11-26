//! `cardinal reconcile` and `cardinal arbiter run` (`docs/architecture.md` sections 10.3–10.5):
//! one pass and exit, nothing resident between passes.
//!
//! [`run`] calls one function per step of section 10.3, in order. A step that fails is
//! recorded in the pass's `errors` and the next one runs; only step 1, which takes
//! `reconcile.lock`, ends the pass. The recovery of executions that died and the trash
//! listing are in `recovery.rs`, the disk states and the reserve file in `disk.rs`,
//! `disk-reclaim`'s order, listing, and execution in `reclaim.rs`, the items raised from
//! observation, disk states, and the trash in [`raise`], and the arbiter's trigger and launcher
//! in `arbiter.rs` (`docs/architecture.md` section 14.2). Everything a pass writes under the
//! state dir is written under `reconcile.lock`. Recovery and executions share one budget,
//! `[reconcile] reclaim_budget_s` ([`Budget`]).

mod arbiter;
mod disk;
pub mod raise;
mod reclaim;
#[cfg(unix)]
mod recovery;

pub use arbiter::run as run_arbiter;
pub use disk::{TIMER_INTERVAL_MS, read_history};

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

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
    let mut budget = Budget::new(config);
    #[cfg(unix)]
    recover_and_list_trash(config, state, &mut budget, &mut pass);
    // 3.
    if let Some(changes) = pass.step("inbox", refresh_inbox(config, state)) {
        pass.add(changes);
    }
    // 4. The items as they were before the readings: the raisers end only these.
    let before = pass
        .step("inbox_before", raise::snapshot(state))
        .unwrap_or_default();
    let mut watched = disk::read_watched(config, state, &mut pass.errors);
    disk::write_reserve(config, state, &watched, &mut pass.errors);
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
        &mut watched,
        observation.as_ref(),
        &mut budget,
        &mut pass.errors,
    );
    let disks: Vec<DiskState> = watched
        .iter()
        .filter(|d| d.reading.is_some())
        .map(|d| d.state.clone())
        .collect();
    let passed: Vec<_> = watched
        .iter()
        .map(|d| (d.state.volume.clone(), d.passed))
        .collect();

    if let Some(changes) = pass.step(
        "disk_items",
        raise::disk_items(config, state, &before, &disks, &reports, &passed),
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

/// The time a pass may spend on recovery and executions, `[reconcile] reclaim_budget_s`
/// (section 9.7, "Starters"): once it is spent, the pass starts neither, and the one under way
/// finishes.
pub struct Budget {
    limit: Duration,
    spent: Duration,
}

impl Budget {
    fn new(config: &Config) -> Self {
        Self {
            limit: Duration::from_secs(config.host.reconcile.reclaim_budget_s),
            spent: Duration::ZERO,
        }
    }

    /// Whether the pass may start another recovery or execution.
    #[must_use]
    pub fn left(&self) -> bool {
        self.spent < self.limit
    }

    /// Runs `work`, counting its time as spent.
    pub fn spend<T>(&mut self, work: impl FnOnce() -> T) -> T {
        self.spend_unless(work, |_| false)
    }

    /// Runs `work`, counting its time as spent unless `free` says what it did costs nothing.
    pub fn spend_unless<T>(
        &mut self,
        work: impl FnOnce() -> T,
        free: impl FnOnce(&T) -> bool,
    ) -> T {
        let start = Instant::now();
        let done = work();
        if !free(&done) {
            self.spent = self.spent.saturating_add(start.elapsed());
        }
        done
    }
}

/// Step 2, after the temp files: the executions that died, recovered within `budget`, then
/// `<state>/trash/` listed, raising `reclaim_stranded` for an entry no execution names and
/// ending each one whose trash entry is gone, from the items read after the recoveries.
#[cfg(unix)]
fn recover_and_list_trash(
    config: &Config,
    state: &StateDir,
    budget: &mut Budget,
    pass: &mut Reconcile,
) {
    pass.recovered = recovery::recover_executions(config, state, budget, &mut pass.errors);
    let Some(before) = pass.step("trash", raise::snapshot(state)) else {
        return;
    };
    let listed = recovery::list_trash(state, &mut pass.errors).map_err(|err| {
        Error::new(
            ErrorCode::StateUnavailable,
            format!("{}: {err}", state.trash_dir().display()),
        )
    });
    let Some(trash) = pass.step("trash", listed) else {
        return;
    };
    if let Some(changes) = pass.step(
        "trash",
        raise::stranded_items(config, state, &before, &trash, &|plan| {
            recovery::claim_unreadable(state, plan)
        }),
    ) {
        pass.add(changes);
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
    inbox::refresh_with(config, state, &admission::recovery_pass)
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
    // A pass that executes on the state dir's volume writes its plans through the reserve's
    // retry (section 9.7, "Space").
    let state_key = disk::key_of(state.root());
    let executing =
        reclaim::executes(config) && state_key.is_some_and(|key| pressured.contains(&key.as_str()));
    let reserve = state.reserve_file();
    let built = plan::create_from(
        config,
        state,
        &inventory,
        &on_pressure,
        executing.then_some(reserve.as_path()),
        errors,
    );
    let own_file_bytes = built
        .iter()
        .filter_map(|b| Some((b.plan.id.clone(), b.own_file_bytes?)))
        .collect();
    let plans: Vec<Plan> = built.into_iter().map(|b| b.plan).collect();
    fill_sizes(&mut inventory, &plans);
    // Reclamation does not wait for this write: a test's fake fails it as `observation`
    // (section 13.7), and the pass goes on from the observation in memory.
    match testpoint::fake_write("observation") {
        Ok(()) => {
            write_json(
                state,
                &state.reconcile_observation(),
                &inventory,
                "observe",
                errors,
            );
        }
        Err(err) => errors.push(scoped("observe", err.to_string())),
    }
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

/// The sizes of the `reclaim_dir` plans' walks, their bounds included, in the observation's
/// ignored entries.
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
            entry.exclusive_bytes = plan.exclusive_bytes;
            entry.exclusive_measure = plan.exclusive_measure;
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
) -> bool {
    let written = serde_json::to_vec(value)
        .map_err(|err| err.to_string())
        .and_then(|bytes| {
            state
                .write_atomic(path, &bytes, Durability::Volatile)
                .map_err(|err| err.to_string())
        });
    match written {
        Ok(()) => true,
        Err(message) => {
            errors.push(scoped(scope, message));
            false
        }
    }
}

fn scoped(scope: &str, message: String) -> ScopedError {
    ScopedError {
        scope: scope.to_owned(),
        message,
    }
}
