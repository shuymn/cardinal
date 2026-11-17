//! Plan evaluation (`docs/architecture.md` sections 9.5 and 9.6).
//!
//! Phase 0 `act` is report-only on every platform: it loads the plan, inspects a snapshot of
//! the target, re-observes identity, evaluates the checks, and reports `expired`, `rejected`,
//! or `dry_run`. It claims nothing, deletes or renames nothing, and writes nothing inside the
//! repository, whatever the caller, approval, or unit configuration. The snapshot and the
//! identity checks describe observed state; they do not exclude concurrent modification,
//! which is why deletion waits for phase 1.

#[cfg(unix)]
mod unix;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::contract::{
    ActResult, ActResultSchema, ActStatus, Approval, Caller, Check, CheckResult, CheckRole,
    ConfigKey, Error, InUse, Plan, PlanKind, Provenance, RecordKind, RootId,
};
use crate::observe::WorktreeObservation;
use crate::plan::{self, DataLists, InUseFacts, Inspection, Real, Sources, Walk};
use crate::state::{self, LockMode, Probe, StateDir};
use crate::testpoint;

/// `cardinal act` flags. In phase 0 neither `dry_run` nor `owner_approved` enables
/// deletion: every run reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActOptions {
    pub dry_run: bool,
    pub owner_approved: bool,
    pub caller: Caller,
}

/// `cardinal act PLAN`: `plan_id` is a unique prefix of at least 4 characters.
pub fn execute(
    config: &Config,
    state: &StateDir,
    plan_id: &str,
    options: &ActOptions,
) -> Result<ActResult, Error> {
    execute_with(config, state, &Real { config, state }, plan_id, options)
}

/// [`execute`] with the given sources.
pub fn execute_with(
    config: &Config,
    state: &StateDir,
    sources: &dyn Sources,
    plan_id: &str,
    options: &ActOptions,
) -> Result<ActResult, Error> {
    let plan = plan::load(state, plan_id)?;
    let run = Run {
        config,
        state,
        sources,
        plan: &plan,
        options,
    };
    let outcome = if plan.host_id == state.host_id() {
        run.evaluate()
    } else {
        Outcome::refused(ActStatus::Expired, vec!["identity_changed:host_id".into()])
    };
    Ok(run.report(outcome))
}

/// What an evaluation decided.
#[derive(Clone, Debug, Default)]
struct Outcome {
    status: Option<ActStatus>,
    reasons: Vec<String>,
    checks: Vec<Check>,
    provenance: Provenance,
    freed_bytes: Option<u64>,
    error: Option<String>,
}

impl Outcome {
    fn refused(status: ActStatus, reasons: Vec<String>) -> Self {
        Self {
            status: Some(status),
            reasons,
            ..Self::default()
        }
    }
}

struct Run<'a> {
    config: &'a Config,
    state: &'a StateDir,
    sources: &'a dyn Sources,
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

    /// The in-use facts, the snapshot, a new observation with its identity comparison, and
    /// every check of section 9.2.
    fn evaluate(&self) -> Outcome {
        let worktree = self.worktree();
        let target = self.target();
        // The in-use lock is probed, not taken: a report never makes a command wait.
        let key = state::worktree_key(&worktree);
        let lock_held = state::probe(&self.state.worktree_lock(&key))
            .map(|probe| probe == Probe::Held)
            .map_err(|err| err.to_string());
        let reservations = self.reservations(&key);

        let snapshot = Snapshot::take(&worktree, &target);
        if let Err(err) = testpoint::hit(testpoint::ACT_SNAPSHOTTED) {
            return Outcome {
                status: Some(ActStatus::Rejected),
                error: Some(err.to_string()),
                ..Outcome::default()
            };
        }

        let observation = self.sources.observe_worktree(&worktree);
        let changed = self.identity_changes(observation.as_ref().ok(), &snapshot);
        if !changed.is_empty() {
            return Outcome::refused(ActStatus::Expired, changed);
        }
        let in_use = InUseFacts {
            lock_held,
            reservations,
            processes: plan::processes_inside(&target),
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
        if !failed.is_empty() {
            outcome.status = Some(ActStatus::Rejected);
            outcome.reasons = failed;
        } else if plan::approval(&outcome.checks) == Approval::Owner && !self.options.owner_approved
        {
            outcome.status = Some(ActStatus::Rejected);
            outcome.reasons = vec!["owner_approval_required".into()];
        } else {
            outcome.status = Some(ActStatus::DryRun);
            outcome.freed_bytes = snapshot.walk.as_ref().ok().map(Walk::allocated);
        }
        outcome
    }

    /// Reservations naming the worktree, read under the admission lock.
    fn reservations(&self, key: &str) -> Result<InUse, String> {
        let deadline =
            Instant::now() + Duration::from_millis(self.config.host.admission.lock_timeout_ms);
        let _admission = state::lock(&self.state.admission_lock(), LockMode::Exclusive, deadline)
            .map_err(|err| err.to_string())?;
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

    /// Appends the record (`planned` for a unit whose `act` is off, `act` otherwise) and
    /// builds the result.
    fn report(&self, outcome: Outcome) -> ActResult {
        let gated = matches!(&self.options.caller, Caller::Unit(name)
            if !self.config.host.unit_may_act(name));
        let mut provenance = outcome.provenance;
        if let (true, Caller::Unit(unit)) = (gated, &self.options.caller) {
            provenance.config.push(ConfigKey {
                file: self.config.dir.join("host.toml").display().to_string(),
                key: format!("unit.{unit}.act"),
                value: false.into(),
            });
        }
        let result = ActResult {
            schema: ActResultSchema,
            plan_id: self.plan.id.clone(),
            caller: self.options.caller.to_string(),
            status: outcome.status.unwrap_or(ActStatus::Rejected),
            reasons: outcome.reasons,
            checks: outcome.checks,
            freed_bytes: outcome.freed_bytes,
            skipped_changed: Vec::new(),
            error: outcome.error,
            finished_ms: state::now_ms(),
        };
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
            "error": result.error,
            "checks": result.checks,
            "provenance": provenance,
        });
        record.data = data.as_object().cloned().unwrap_or_default();
        let _ = recorder.append(&record);
        result
    }
}

/// The target's entries and the worktree root as the snapshot opened it.
struct Snapshot {
    walk: Result<Walk, String>,
    root_id: Option<RootId>,
    /// The target does not exist.
    missing: bool,
}

impl Snapshot {
    #[cfg(unix)]
    fn take(worktree: &Path, target: &Path) -> Self {
        let opened = unix::Target::open(worktree, target);
        let root_id = opened.as_ref().ok().map(unix::Target::root_id);
        let walk = opened.and_then(|t| t.snapshot());
        Self::from(walk, root_id, target)
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
        }
    }
}
