//! Plan evaluation (`docs/architecture.md` sections 9.5 and 9.6), and the execution of
//! `reclaim_dir` plans for the callers a host grants (section 9.7).
//!
//! Every `act` loads the plan, inspects a snapshot of the target, re-observes identity, and
//! evaluates the checks. An `act` that does not execute reports `expired`, `rejected`, or
//! `dry_run`, with the reason it did not execute; it writes nothing but its record. Whether it
//! executes is the grant decision ([`Run::grant`]): a `reclaim_dir` plan on macOS or Linux
//! whose caller `[act] execute` grants that kind, without `--dry-run`. Only the owner executes
//! until unit X2 brings `disk-reclaim`'s pass; the arbiter never does. An execution
//! ([`execute`]) takes the plan's `act.lock` and the worktree's in-use lock, checks the target
//! again after it moved it out of the worktree, and deletes it from the trash, or restores it.

#[cfg(unix)]
pub mod delete;
#[cfg(unix)]
mod execute;
#[cfg(unix)]
pub mod recover;
#[cfg(unix)]
mod unix;

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::contract::{
    ActResult, ActResultSchema, ActStatus, Approval, Caller, Check, CheckResult, CheckRole,
    ConfigKey, Error, InUse, Plan, PlanKind, Provenance, RecordKind, RootId,
};
use crate::observe::WorktreeObservation;
use crate::plan::{self, DataLists, InUseFacts, Inspection, Real, Walk};
use crate::state::{self, Probe, StateDir};
use crate::testpoint;

#[cfg(unix)]
pub use execute::{
    BOUND_ELSEWHERE, CHANGED_DURING_ACT, CLAIM_UNREADABLE, IN_PROGRESS, IN_USE_WHILE_DELETING,
    NO_SPACE, NOT_DIRECTORY_EXCLUDED, NOT_ON_STATE_MOUNT, SPARSE_CHECKOUT, TRACKED_OVERLAP,
    TRASH_OCCUPIED,
};

/// Why a plan whose checks pass and whose approval is met reports `dry_run` on a path that has
/// no execution: a whole worktree, every plan on Windows, and, until unit X2, a unit's
/// (section 9.3).
pub const EXECUTION_NOT_DESIGNED: &str = "execution_not_designed";

/// Why such a plan reports `dry_run` when `[act] execute` does not grant its caller the plan's
/// kind (section 9.7).
pub const EXECUTION_NOT_GRANTED: &str = "execution_not_granted";

/// Why such a plan reports `dry_run` when a granted caller passed `--dry-run`.
pub const DRY_RUN_REQUESTED: &str = "dry_run_requested";

/// `cardinal act` flags.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActOptions {
    /// Report what a granted caller would execute, executing nothing.
    pub dry_run: bool,
    /// Accepted only with `caller` `owner` (a usage error otherwise).
    pub owner_approved: bool,
    /// The caller judged HEAD squash-merged: merge evidence counts as present when
    /// `content_in_base` is `yes` in `act`'s own observation (section 9.3).
    pub squash_merged: bool,
    pub caller: Caller,
}

/// `cardinal act PLAN`: `plan_id` is a unique prefix of at least 4 characters.
pub fn execute(
    config: &Config,
    state: &StateDir,
    plan_id: &str,
    options: &ActOptions,
) -> Result<ActResult, Error> {
    let plan = plan::load(state, plan_id)?;
    let run = Run {
        config,
        state,
        sources: Real { config, state },
        plan: &plan,
        options,
    };
    if plan.host_id != state.host_id() {
        let outcome = Outcome::refused(ActStatus::Expired, vec!["identity_changed:host_id".into()]);
        return Ok(run.report(outcome));
    }
    match run.grant() {
        // Executes the plan (section 9.7); off Unix the grant refuses every plan.
        #[cfg(unix)]
        Ok(()) => execute::run(&run),
        #[cfg(not(unix))]
        Ok(()) => Ok(run.report(run.evaluate_report(EXECUTION_NOT_DESIGNED))),
        Err(reason) => {
            // An owner's `act` that would execute but for the grant recovers its plan's claim
            // first (section 9.7, "Recovery"); `--dry-run` only reports.
            #[cfg(unix)]
            if reason == EXECUTION_NOT_GRANTED
                && matches!(run.options.caller, Caller::Owner)
                && !run.options.dry_run
                && let Some(result) = execute::recover_claim(&run)?
            {
                return Ok(result);
            }
            Ok(run.report(run.evaluate_report(reason)))
        }
    }
}

/// What an evaluation, or an execution, decided.
#[derive(Clone, Debug)]
struct Outcome {
    status: ActStatus,
    reasons: Vec<String>,
    checks: Vec<Check>,
    provenance: Provenance,
    freed_bytes: Option<u64>,
    /// Entries an execution kept, relative to the target.
    skipped_changed: Vec<String>,
    /// The trash entry holding what an execution kept or could not put back.
    trash: Option<String>,
    /// Whether recovery wrote the result.
    recovered: bool,
    error: Option<String>,
}

impl Default for Outcome {
    /// `rejected` until an evaluation decides otherwise.
    fn default() -> Self {
        Self {
            status: ActStatus::Rejected,
            reasons: Vec::new(),
            checks: Vec::new(),
            provenance: Provenance::default(),
            freed_bytes: None,
            skipped_changed: Vec::new(),
            trash: None,
            recovered: false,
            error: None,
        }
    }
}

impl Outcome {
    fn refused(status: ActStatus, reasons: Vec<String>) -> Self {
        Self {
            status,
            reasons,
            ..Self::default()
        }
    }
}

/// What steps 2 to 5 of section 9.5 read and decided.
struct Evaluated {
    /// `rejected` or `expired` with its reasons, or, when every invariant passed and approval
    /// is met, `dry_run` without a reason yet and the snapshot's bound in `freed_bytes`.
    outcome: Outcome,
    /// Every invariant passed and approval is met.
    passed: bool,
    /// What an execution goes on from.
    #[cfg_attr(not(unix), expect(dead_code, reason = "only an execution reads it"))]
    snapshot: Snapshot,
    #[cfg_attr(not(unix), expect(dead_code, reason = "only an execution reads it"))]
    observation: Option<WorktreeObservation>,
}

struct Run<'a> {
    config: &'a Config,
    state: &'a StateDir,
    sources: Real<'a>,
    plan: &'a Plan,
    options: &'a ActOptions,
}

impl Run<'_> {
    fn worktree(&self) -> PathBuf {
        PathBuf::from(&self.plan.target.worktree)
    }

    fn target(&self) -> PathBuf {
        PathBuf::from(&self.plan.target.path)
    }

    /// The grant decision (section 9.7, "Grants"): `Ok` when this run executes, else the
    /// reason a plan whose checks pass reports `dry_run`. A whole worktree, every plan on
    /// Windows, and a granted unit, whose execution comes with unit X2's pass, are not
    /// designed; a caller `[act] execute` does not grant, the arbiter always, is not granted;
    /// and a granted caller's `--dry-run` asks for the report.
    fn grant(&self) -> Result<(), &'static str> {
        if cfg!(not(unix)) || self.plan.kind != PlanKind::ReclaimDir {
            return Err(EXECUTION_NOT_DESIGNED);
        }
        let caller = &self.options.caller;
        if !self.config.host.grants(caller, PlanKind::ReclaimDir) {
            return Err(EXECUTION_NOT_GRANTED);
        }
        if matches!(caller, Caller::Unit(_)) {
            return Err(EXECUTION_NOT_DESIGNED);
        }
        if self.options.dry_run {
            return Err(DRY_RUN_REQUESTED);
        }
        Ok(())
    }

    /// The evaluation of an `act` that does not execute, `reason` saying why a plan whose
    /// checks pass reports `dry_run`. The in-use lock is probed, never taken or waited for, so
    /// a report never makes a command wait.
    fn evaluate_report(&self, reason: &str) -> Outcome {
        let key = state::worktree_key(&self.worktree());
        let lock_held = state::probe(&self.state.worktree_lock(&key))
            .map(|probe| probe == Probe::Held)
            .map_err(|err| err.to_string());
        let mut evaluated = self.evaluate(lock_held);
        if evaluated.passed {
            evaluated.outcome.reasons = vec![reason.to_owned()];
        }
        evaluated.outcome
    }

    /// The in-use facts with `lock_held`, the snapshot, a new observation with its identity
    /// comparison, and every check of section 9.2.
    fn evaluate(&self, lock_held: Result<bool, String>) -> Evaluated {
        let worktree = self.worktree();
        let target = self.target();
        let key = state::worktree_key(&worktree);
        let reservations = self.reservations(&key);

        let snapshot = Snapshot::take(&worktree, &target);
        let refused = |outcome: Outcome, snapshot: Snapshot, observation| Evaluated {
            outcome,
            passed: false,
            snapshot,
            observation,
        };
        if let Err(err) = testpoint::hit(testpoint::ACT_SNAPSHOTTED) {
            let outcome = Outcome {
                error: Some(err.to_string()),
                ..Outcome::default()
            };
            return refused(outcome, snapshot, None);
        }

        let observation = self.sources.observe_worktree(&worktree);
        let changed = self.identity_changes(observation.as_ref().ok(), &snapshot);
        if !changed.is_empty() {
            let outcome = Outcome::refused(ActStatus::Expired, changed);
            return refused(outcome, snapshot, observation.ok());
        }
        let in_use = InUseFacts {
            lock_held,
            reservations,
            // The whole worktree for both kinds (section 9.2).
            processes: plan::processes_inside(&worktree),
            descriptors: plan::descriptor_scan(self.plan.kind),
        };
        let patterns = observation.as_ref().map_or_else(
            |_| DataLists::default(),
            |o| plan::patterns(self.config, &o.repo),
        );
        let inspection = Inspection {
            kind: self.plan.kind,
            worktree: &worktree,
            target: &target,
            observation: observation.as_ref().map_err(|err| err.message.as_str()),
            walk: snapshot.walk.as_ref().map_err(String::as_str),
            in_use: &in_use,
            patterns: &patterns,
            managed: &self.config.host.observe.managed,
            squash_merged: self.options.squash_merged,
        };
        let checks = plan::evaluate(&inspection);
        let failed: Vec<String> = checks
            .iter()
            .filter(|c| c.role == CheckRole::Invariant && c.result != CheckResult::Pass)
            .map(|c| c.name.clone())
            .collect();
        let mut outcome = Outcome {
            checks,
            provenance: plan::provenance(self.config, &inspection),
            ..Outcome::default()
        };
        let mut passed = false;
        if !failed.is_empty() {
            outcome.status = ActStatus::Rejected;
            outcome.reasons = failed;
        } else if plan::approval(&outcome.checks) == Approval::Owner && !self.options.owner_approved
        {
            outcome.status = ActStatus::Rejected;
            outcome.reasons = vec!["owner_approval_required".into()];
        } else {
            passed = true;
            outcome.status = ActStatus::DryRun;
            // The lower bound of section 8.5, null where it is unknown.
            outcome.freed_bytes = snapshot
                .walk
                .as_ref()
                .ok()
                .and_then(|walk| walk.exclusive().bytes);
        }
        Evaluated {
            outcome,
            passed,
            snapshot,
            observation: observation.ok(),
        }
    }

    /// Reservations naming the worktree, read under the admission lock.
    fn reservations(&self, key: &str) -> Result<InUse, String> {
        self.sources.in_use(key).map_err(|err| err.message)
    }

    /// The identity fields of section 9.4 that differ from the plan, as
    /// `identity_changed:<field>` reasons. An observation that failed leaves the git facts to
    /// the `observed` check.
    fn identity_changes(
        &self,
        observation: Option<&WorktreeObservation>,
        snapshot: &Snapshot,
    ) -> Vec<String> {
        let target = &self.plan.target;
        let mut changed = Vec::new();
        if let Some(o) = observation {
            if o.repo.common_dir != target.repo {
                changed.push("repo");
            }
            if o.worktree.path != target.worktree {
                changed.push("worktree");
            }
            if o.worktree.head != target.head {
                changed.push("head");
            }
            if o.worktree.branch != target.branch {
                changed.push("branch");
            }
            // A git dir observation could not read is left to the `observed` check.
            if o.worktree.git_dir.is_some() && o.worktree.git_dir != target.git_dir {
                changed.push("git_dir");
            }
            if self.plan.kind == PlanKind::ReclaimWorktree && o.worktree.main {
                changed.push("main");
            }
        }
        if !self.root_unchanged(snapshot.root_id) {
            changed.push("root_id");
        }
        if snapshot.missing {
            changed.push("path");
        }
        changed
            .into_iter()
            .map(|field| format!("identity_changed:{field}"))
            .collect()
    }

    /// Whether the worktree root is the one the plan was made for. `opened` is the root as
    /// the snapshot opened it, when it did.
    fn root_unchanged(&self, opened: Option<RootId>) -> bool {
        let Ok((id, created_ms)) = plan::root_identity(&self.worktree()) else {
            return false;
        };
        let expected = &self.plan.target;
        let same_created = match (expected.root_created_ms, created_ms) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        };
        id == expected.root_id
            && same_created
            && opened.is_none_or(|opened| Some(opened) == expected.root_id)
    }

    /// Builds the result of `outcome` and appends its record: `planned` for a unit whose `act`
    /// is off, `act` otherwise.
    fn report(&self, outcome: Outcome) -> ActResult {
        let result = self.result(&outcome);
        self.append(&result, outcome.provenance);
        result
    }

    /// The result of `outcome`.
    fn result(&self, outcome: &Outcome) -> ActResult {
        ActResult {
            schema: ActResultSchema,
            plan_id: self.plan.id.clone(),
            caller: self.options.caller.to_string(),
            status: outcome.status,
            reasons: outcome.reasons.clone(),
            checks: outcome.checks.clone(),
            freed_bytes: outcome.freed_bytes,
            skipped_changed: outcome.skipped_changed.clone(),
            trash: outcome.trash.clone(),
            recovered: outcome.recovered,
            error: outcome.error.clone(),
            finished_ms: state::now_ms(),
        }
    }

    /// Appends the record of `result`: `planned` for a unit whose `act` is off, naming the key,
    /// `act` otherwise, with the provenance of its checks.
    fn append(&self, result: &ActResult, mut provenance: Provenance) {
        let gated = matches!(&self.options.caller, Caller::Unit(name)
            if !self.config.host.unit_may_act(name));
        if let (true, Caller::Unit(unit)) = (gated, &self.options.caller) {
            provenance.config.push(ConfigKey {
                file: self.config.dir.join("host.toml").display().to_string(),
                key: format!("unit.{unit}.act"),
                value: false.into(),
            });
        }
        let recorder = plan::recorder(self.config, self.state);
        let mut record = recorder.record(if gated {
            RecordKind::Planned
        } else {
            RecordKind::Act
        });
        record.plan_id = Some(self.plan.id.clone());
        let data = serde_json::json!({
            "caller": result.caller,
            "kind": self.plan.kind,
            "target": self.plan.target.path,
            "status": result.status,
            "reasons": result.reasons,
            "freed_bytes": result.freed_bytes,
            "skipped_changed": result.skipped_changed,
            "trash": result.trash,
            "recovered": result.recovered,
            "error": result.error,
            "checks": result.checks,
            "provenance": provenance,
        });
        record.data = data.as_object().cloned().unwrap_or_default();
        let _ = recorder.append(&record);
    }
}

/// The target's entries and the worktree root as the snapshot opened it.
struct Snapshot {
    walk: Result<Walk, String>,
    root_id: Option<RootId>,
    /// The target does not exist.
    missing: bool,
    /// The target as opened through the chain from the worktree root, which an execution
    /// moves and restores through.
    #[cfg(unix)]
    opened: Option<unix::Target>,
}

impl Snapshot {
    #[cfg(unix)]
    fn take(worktree: &Path, target: &Path) -> Self {
        match unix::Target::open(worktree, target) {
            Ok(opened) => {
                let root_id = Some(opened.root_id());
                let walk = opened.snapshot();
                Self {
                    opened: Some(opened),
                    ..Self::from(walk, root_id, target)
                }
            }
            Err(err) => Self::from(Err(err), None, target),
        }
    }

    #[cfg(not(unix))]
    fn take(worktree: &Path, target: &Path) -> Self {
        Self::from(plan::walk(worktree, target), None, target)
    }

    fn from(walk: std::io::Result<Walk>, root_id: Option<RootId>, target: &Path) -> Self {
        let missing = matches!(&walk, Err(err) if err.kind() == std::io::ErrorKind::NotFound);
        Self {
            walk: walk.map_err(|err| format!("{}: {err}", target.display())),
            root_id,
            missing,
            #[cfg(unix)]
            opened: None,
        }
    }
}
