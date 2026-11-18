//! `cardinal reconcile` and `cardinal arbiter run` (`docs/architecture.md` sections 10.3–10.5):
//! one pass and exit, nothing resident between passes.
//!
//! [`run`] calls one function per step of section 10.3, in order. A step that fails is
//! recorded in the pass's `errors` and the next one runs; only step 1, which takes
//! `reconcile.lock`, ends the pass. The steps' bodies come with their work units
//! (`docs/architecture.md` section 14.2): the disk states, the observation, `disk-reclaim`,
//! and the state file here, the items raised from observation and disk states in
//! [`raise`], and the arbiter's trigger and launcher in [`arbiter`]. Until then a step returns
//! `not_implemented`.

mod arbiter;
pub mod raise;

pub use arbiter::run as run_arbiter;

use crate::config::Config;
use crate::contract::{
    ArbiterStart, DiskReport, DiskState, Error, InboxChanges, Inventory, Plan, Reconcile,
    ReconcileSchema, ScopedError, Skipped, Trigger,
};
use crate::snapshot;
use crate::state::{FileLock, StateDir};
use crate::testpoint;

/// What step 5 of a pass observed: the full observation, kept in
/// `reconcile/observation.json`, and the plans built from it, whose checks and exclusive
/// bytes were evaluated together at `checks_ms`. `disk-reclaim` reports from this one set.
#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    pub inventory: Inventory,
    pub plans: Vec<Plan>,
    pub checks_ms: u64,
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
    let mut disks = pass
        .step("disk", read_volumes(config, state))
        .unwrap_or_default();
    // 5.
    let observation = pass
        .step("observe", observe(config, state, &disks))
        .flatten();
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
    // the disk items of every watched volume with those reports.
    pass.step("disk", reread_volumes(config, state, &mut disks));
    let reports = pass
        .step(
            "disk_reclaim",
            disk_reclaim(config, state, &disks, observation.as_ref()),
        )
        .unwrap_or_default();
    if let Some(changes) = pass.step(
        "disk_items",
        raise::disk_items(config, state, &before, &disks, &reports),
    ) {
        pass.add(changes);
    }
    pass.disk = disks;
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
fn lock(_state: &StateDir) -> Result<Option<FileLock>, Error> {
    Err(Error::not_implemented("reconcile: taking reconcile.lock"))
}

/// Step 2: temp files older than one hour removed.
fn remove_temp_files(_state: &StateDir) -> Result<(), Error> {
    Err(Error::not_implemented("reconcile: removing temp files"))
}

/// Step 3: the inbox refresh of section 10.1, with the keys it raised and cleared.
fn refresh_inbox(_config: &Config, _state: &StateDir) -> Result<InboxChanges, Error> {
    Err(Error::not_implemented("reconcile: the inbox refresh"))
}

/// Step 4: the free space of every watched volume read and its disk state updated
/// (section 10.4).
fn read_volumes(_config: &Config, _state: &StateDir) -> Result<Vec<DiskState>, Error> {
    Err(Error::not_implemented("reconcile: reading the volumes"))
}

/// Step 5: the full observation and its plans, when the last one is older than
/// `observe_every_s` or a volume is under pressure; `None` when none was due.
fn observe(
    _config: &Config,
    _state: &StateDir,
    _disks: &[DiskState],
) -> Result<Option<Observation>, Error> {
    Err(Error::not_implemented("reconcile: the full observation"))
}

/// Step 6, first part: each volume under pressure read again; the pass's last successful
/// reading updates its disk state.
fn reread_volumes(
    _config: &Config,
    _state: &StateDir,
    _disks: &mut [DiskState],
) -> Result<(), Error> {
    Err(Error::not_implemented(
        "reconcile: reading the volumes again",
    ))
}

/// Step 6, second part: `disk-reclaim`'s report on every volume under pressure, from the
/// plans of step 5 and the pass's last successful reading.
fn disk_reclaim(
    _config: &Config,
    _state: &StateDir,
    _disks: &[DiskState],
    _observation: Option<&Observation>,
) -> Result<Vec<DiskReport>, Error> {
    Err(Error::not_implemented("reconcile: disk-reclaim"))
}

/// Step 8, second part: `reconcile/state.json`.
fn write_state(_state: &StateDir, _observation: Option<&Observation>) -> Result<(), Error> {
    Err(Error::not_implemented(
        "reconcile: writing reconcile/state.json",
    ))
}
