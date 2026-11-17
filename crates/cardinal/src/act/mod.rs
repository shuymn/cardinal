//! Plan execution: claim, snapshot, checks, and descriptor-relative deletion
//! (`docs/architecture.md` sections 9.5 and 9.6).
//!
//! Data safety rests on the order snapshot, checks, deletion of unchanged entries: a change
//! made before the checks is seen by them, and an entry created, rewritten, or replaced after
//! the snapshot does not match it and survives. The in-use lock only keeps running commands
//! from losing files under them. `act` writes nothing inside the repository. On Windows it
//! always takes the dry-run path; handle-relative deletion is phase 3.

#[cfg(unix)]
mod unix;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::contract::{
    ActResult, ActResultSchema, ActStatus, Approval, Caller, Check, CheckResult, CheckRole, Error,
    ErrorCode, InUse, Plan, PlanClaim, PlanKind, RecordKind, RootId,
};
use crate::observe::WorktreeObservation;
use crate::plan::{self, DataLists, InUseFacts, Inspection, Real, Sources, Walk};
use crate::state::{self, Durability, LockMode, StateDir};
use crate::testpoint;

/// `cardinal act` flags.
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
    // Step 1: load the plan.
    let plan = plan::load(state, plan_id)?;
    // A unit whose `act` is off only reports (C16); Windows only dry-runs in phase 0.
    let gated = matches!(&options.caller, Caller::Unit(name) if !config.host.unit_may_act(name));
    let dry_run = options.dry_run || gated || cfg!(not(unix));
    let run = Run {
        config,
        state,
        sources,
        plan: &plan,
        options,
        check_only: dry_run,
    };
    if plan.host_id != state.host_id() {
        let outcome = Outcome::refused(ActStatus::Expired, vec!["identity_changed:host_id".into()]);
        return Ok(run.finish(outcome, false, gated));
    }
    // Step 2: claim, so the plan runs at most once, even across a crash.
    if !dry_run {
        let claim = PlanClaim {
            pid: std::process::id(),
            claimed_ms: state::now_ms(),
        };
        let bytes = serde_json::to_vec(&claim)
            .map_err(|err| Error::new(ErrorCode::Internal, err.to_string()))?;
        let path = state.plan_claim(&plan.id);
        match state::create_new_durable(&path, &bytes) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let outcome = Outcome::refused(ActStatus::AlreadyExecuted, Vec::new());
                return Ok(run.finish(outcome, false, gated));
            }
            Err(err) => {
                return Err(Error::new(
                    ErrorCode::StateUnavailable,
                    format!("{}: {err}", path.display()),
                ));
            }
        }
    }
    let outcome = run.steps();
    Ok(run.finish(outcome, !dry_run, gated))
}

/// What steps 3–7 decided.
#[derive(Clone, Debug, Default)]
struct Outcome {
    status: Option<ActStatus>,
    reasons: Vec<String>,
    checks: Vec<Check>,
    freed_bytes: Option<u64>,
    skipped_changed: Vec<String>,
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

    /// A failure before anything was deleted.
    fn failed(error: impl Into<String>) -> Self {
        Self {
            status: Some(ActStatus::Rejected),
            error: Some(error.into()),
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
    /// Steps 1 and 3–5 only: nothing is claimed or deleted.
    check_only: bool,
}

impl Run<'_> {
    fn worktree(&self) -> PathBuf {
        PathBuf::from(&self.plan.target.worktree)
    }

    fn target(&self) -> PathBuf {
        PathBuf::from(&self.plan.target.path)
    }

    /// Steps 3–7. The in-use lock is held from step 3 until this returns.
    fn steps(&self) -> Outcome {
        let worktree = self.worktree();
        let target = self.target();
        // Step 3: the exclusive in-use lock (no waiting), then reservations under the
        // admission lock.
        let key = state::worktree_key(&worktree);
        let (lock_held, _lock) =
            match state::try_lock(&self.state.worktree_lock(&key), LockMode::Exclusive) {
                Ok(Some(lock)) => (Ok(false), Some(lock)),
                Ok(None) => (Ok(true), None),
                Err(err) => (Err(err.to_string()), None),
            };
        if let Err(err) = testpoint::hit(testpoint::ACT_LOCKED) {
            return Outcome::failed(err.to_string());
        }
        let reservations = self.reservations(&key);

        // Step 4: the snapshot.
        let snapshot = Snapshot::take(&worktree, &target);
        if let Err(err) = testpoint::hit(testpoint::ACT_SNAPSHOTTED) {
            return Outcome::failed(err.to_string());
        }

        // Step 5: observe from scratch, compare identity, evaluate every check.
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
        let checks = plan::evaluate(&Inspection {
            kind: self.plan.kind,
            worktree: &worktree,
            target: &target,
            observation: observation.as_ref().map_err(|err| err.message.as_str()),
            walk: snapshot.walk.as_ref().map_err(String::as_str),
            in_use: &in_use,
            patterns: &patterns,
            managed: &self.config.host.observe.managed,
        });
        let failed: Vec<String> = checks
            .iter()
            .filter(|c| c.role == CheckRole::Invariant && c.result != CheckResult::Pass)
            .map(|c| c.name.clone())
            .collect();
        let would_free = snapshot.walk.as_ref().ok().map(Walk::allocated);
        let mut outcome = Outcome {
            checks,
            ..Outcome::default()
        };
        if !failed.is_empty() {
            outcome.status = Some(ActStatus::Rejected);
            outcome.reasons = failed;
            return outcome;
        }
        if plan::approval(&outcome.checks) == Approval::Owner && !self.options.owner_approved {
            outcome.status = Some(ActStatus::Rejected);
            outcome.reasons = vec!["owner_approval_required".into()];
            return outcome;
        }
        // Windows only dry-runs in phase 0.
        if self.check_only || cfg!(not(unix)) {
            outcome.status = Some(ActStatus::DryRun);
            outcome.freed_bytes = would_free;
            return outcome;
        }
        if let Err(err) = testpoint::hit(testpoint::ACT_CHECKED) {
            outcome.status = Some(ActStatus::Rejected);
            outcome.error = Some(err.to_string());
            return outcome;
        }
        #[cfg(unix)]
        let outcome = self.delete(&snapshot, observation.as_ref().ok(), outcome);
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

    /// Step 6 re-read: root identity, HEAD, and branch.
    #[cfg(unix)]
    fn head_unchanged(&self, head: &Result<plan::Head, Error>) -> Vec<String> {
        let mut changed = Vec::new();
        match head {
            Ok(head) => {
                if head.head != self.plan.target.head {
                    changed.push("identity_changed:head".to_owned());
                }
                if head.branch != self.plan.target.branch {
                    changed.push("identity_changed:branch".to_owned());
                }
            }
            Err(err) => changed.push(format!("identity_changed:head ({})", err.message)),
        }
        changed
    }

    /// Steps 6 and 7 (Unix).
    #[cfg(unix)]
    fn delete(
        &self,
        snapshot: &Snapshot,
        observation: Option<&WorktreeObservation>,
        mut outcome: Outcome,
    ) -> Outcome {
        let Ok(walk) = &snapshot.walk else {
            // The checks passed, so the snapshot exists.
            outcome.status = Some(ActStatus::Rejected);
            return outcome;
        };
        let main = observation.map_or_else(PathBuf::new, |o| PathBuf::from(&o.repo.main_worktree));
        let main = main.as_path();
        // Step 6: identity once more, then delete bottom-up.
        let mut changed = self.head_unchanged(&self.sources.head(&self.worktree()));
        if !self.root_unchanged(snapshot.root_id) {
            changed.push("identity_changed:root_id".into());
        }
        if !changed.is_empty() {
            outcome.status = Some(ActStatus::Expired);
            outcome.reasons = changed;
            return outcome;
        }
        let target = match unix::Target::open(&self.worktree(), &self.target()) {
            Ok(target) if Some(target.root_id()) == self.plan.target.root_id => target,
            Ok(_) => {
                outcome.status = Some(ActStatus::Expired);
                outcome.reasons = vec!["identity_changed:root_id".into()];
                return outcome;
            }
            Err(err) => {
                outcome.status = Some(ActStatus::Rejected);
                outcome.error = Some(format!("{}: {err}", self.plan.target.path));
                return outcome;
            }
        };
        let worktree = self.plan.kind == PlanKind::ReclaimWorktree;
        let mut deletion = target.delete(walk, worktree);
        let mut status = ActStatus::Executed;
        if worktree {
            // Step 7: `.git` goes last, and only when nothing else is left.
            status = ActStatus::Partial;
            if deletion.skipped.is_empty() && deletion.failed.is_empty() {
                let changed = self.head_unchanged(&self.sources.head(&self.worktree()));
                if changed.is_empty() {
                    match target.finish_worktree(walk, &mut deletion) {
                        Ok(true) => match self.sources.remove_worktree(main, &self.worktree()) {
                            Ok(()) => status = ActStatus::Executed,
                            Err(err) => deletion.failed.push(format!(
                                "git worktree remove {}: {}",
                                self.plan.target.worktree, err.message
                            )),
                        },
                        Ok(false) => {}
                        Err(err) => deletion
                            .failed
                            .push(format!("{}: {err}", self.plan.target.worktree)),
                    }
                } else {
                    status = ActStatus::Expired;
                    outcome.reasons = changed;
                }
            }
        } else if !deletion.root_removed {
            status = ActStatus::Partial;
        }
        if !deletion.failed.is_empty() && status == ActStatus::Executed {
            status = ActStatus::Partial;
        }
        outcome.status = Some(status);
        outcome.freed_bytes = Some(deletion.freed);
        outcome.skipped_changed = deletion
            .skipped
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        if !deletion.failed.is_empty() {
            outcome.error = Some(deletion.failed.join("; "));
        }
        outcome
    }

    /// Step 8: writes `result.json` when the plan was claimed, appends the record (`planned`
    /// for a unit that may not act), and builds the result.
    fn finish(&self, outcome: Outcome, claimed: bool, gated: bool) -> ActResult {
        let mut result = ActResult {
            schema: ActResultSchema,
            plan_id: self.plan.id.clone(),
            caller: self.options.caller.to_string(),
            status: outcome.status.unwrap_or(ActStatus::Rejected),
            reasons: outcome.reasons,
            checks: outcome.checks,
            freed_bytes: outcome.freed_bytes,
            skipped_changed: outcome.skipped_changed,
            error: outcome.error,
            finished_ms: state::now_ms(),
        };
        if claimed {
            let path = self.state.plan_result(&self.plan.id);
            let written = serde_json::to_vec(&result)
                .map_err(|err| err.to_string())
                .and_then(|bytes| {
                    self.state
                        .write_atomic(&path, &bytes, Durability::Durable)
                        .map_err(|err| err.to_string())
                });
            if let Err(err) = written {
                let note = format!("cannot write {}: {err}", path.display());
                result.error = Some(match result.error.take() {
                    Some(error) => format!("{error}; {note}"),
                    None => note,
                });
            }
        }
        let recorder = plan::recorder(self.config, self.state);
        let mut record = recorder.record(if gated {
            RecordKind::Planned
        } else {
            RecordKind::Act
        });
        record.plan_id = Some(self.plan.id.clone());
        let mut data = serde_json::json!({
            "caller": result.caller,
            "kind": self.plan.kind,
            "target": self.plan.target.path,
            "status": result.status,
            "reasons": result.reasons,
            "freed_bytes": result.freed_bytes,
            "skipped_changed": result.skipped_changed.len(),
            "error": result.error,
        });
        if gated {
            data["checks"] = serde_json::to_value(&result.checks).unwrap_or_default();
        }
        record.data = data.as_object().cloned().unwrap_or_default();
        let _ = recorder.append(&record);
        result
    }
}

/// Step 4: the target's entries and the worktree root as the snapshot opened it.
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
