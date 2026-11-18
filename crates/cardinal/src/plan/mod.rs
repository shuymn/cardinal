//! Plans: generation and identity binding (`docs/architecture.md` sections 9.1–9.4).
//!
//! A plan binds a target to its identity (host, repository, worktree, worktree root, HEAD,
//! branch, path) and carries the checks of section 9.2 as they were when it was made; `act`
//! evaluates them again and never trusts the stored ones. Observation and admission are
//! reached through [`Real`], which calls the `observe` and `admission` modules.

mod checks;
mod merge;
mod walk;

use std::path::{Path, PathBuf};
use std::time::Duration;

pub use checks::{
    DataLists, FACT_ENTRIES, InUseFacts, Inspection, approval, bound_facts, evaluate,
    invariants_ok, processes_inside, provenance,
};
pub use walk::{Entry, EntryKind, Mount, Walk, root_identity, walk};

use crate::admission;
use crate::config::Config;
use crate::contract::{
    Error, ErrorCode, InUse, Inventory, Plan, PlanKind, PlanList, PlanListSchema, PlanSchema,
    PlanTarget, Record, RecordKind, Repo, ScopedError, Worktree,
};
use crate::observe::{self, InventoryOptions, WorktreeObservation};
use crate::record::Recorder;
use crate::state::{self, Durability, Probe, StateDir};

/// Plan directories older than this are removed by `plan` runs.
const PLAN_MAX_AGE: Duration = Duration::from_hours(30 * 24);

/// What plans and `act` read from observation and admission: the `observe` and `admission`
/// modules.
#[derive(Clone, Copy, Debug)]
pub struct Real<'a> {
    pub config: &'a Config,
    pub state: &'a StateDir,
}

impl Real<'_> {
    /// Observes the worktree containing `path` from scratch.
    pub fn observe_worktree(&self, path: &Path) -> Result<WorktreeObservation, Error> {
        observe::observe_worktree(self.config, self.state, path)
    }

    /// Observes the configured repositories and `repos`.
    pub fn inventory(&self, repos: &[PathBuf]) -> Result<Inventory, Error> {
        let options = InventoryOptions {
            repos: repos.to_vec(),
            sizes: false,
        };
        observe::inventory(self.config, self.state, &options)
    }

    /// Live and recovery-needed reservations naming the worktree with key `worktree_key`,
    /// read under the admission lock.
    pub fn in_use(&self, worktree_key: &str) -> Result<InUse, Error> {
        admission::in_use(self.state, &self.config.host, worktree_key)
    }
}

/// `cardinal plan reclaim-worktree PATH | reclaim-dir PATH`.
pub fn create(
    config: &Config,
    state: &StateDir,
    kind: PlanKind,
    target: &Path,
) -> Result<Plan, Error> {
    let sources = Real { config, state };
    housekeeping(state);
    let target = canonical_target(kind, target)?;
    // A target that is a symbolic link is planned as the link it is (its checks refuse it),
    // so its worktree is the one of the directory the link is in, not of what it points to.
    let inside = match std::fs::symlink_metadata(&target) {
        Ok(meta) if meta.is_symlink() => target.parent().unwrap_or(&target),
        _ => &target,
    };
    let observation = sources.observe_worktree(inside)?;
    let worktree = PathBuf::from(&observation.worktree.path);
    match kind {
        PlanKind::ReclaimWorktree if target != worktree => {
            return Err(invalid(format!(
                "{} is not a worktree root (its worktree is {})",
                target.display(),
                worktree.display()
            )));
        }
        PlanKind::ReclaimWorktree if observation.worktree.main => {
            return Err(invalid(format!(
                "{} is a main worktree, which is never reclaimed",
                target.display()
            )));
        }
        PlanKind::ReclaimDir if target == worktree || !target.starts_with(&worktree) => {
            return Err(invalid(format!(
                "{} is not inside its worktree {}",
                target.display(),
                worktree.display()
            )));
        }
        _ => {}
    }
    let built = build(config, state, &sources, kind, &target, &observation);
    store(config, state, built.plan)
}

/// `cardinal plan [--repo PATH]...`: a plan for every linked worktree and for every ignored
/// entry matching a regenerable pattern, whatever their checks find (section 9.2).
pub fn create_all(config: &Config, state: &StateDir, repos: &[PathBuf]) -> Result<PlanList, Error> {
    let sources = Real { config, state };
    housekeeping(state);
    let inventory = sources.inventory(repos)?;
    let mut errors = inventory.errors.clone();
    let plans = create_from(config, state, &inventory, &|_| true, &mut errors)
        .into_iter()
        .map(|built| built.plan)
        .collect();
    Ok(PlanList {
        schema: PlanListSchema,
        plans,
        errors,
    })
}

/// A plan, with what its walk measured that the plan does not keep.
#[derive(Clone, Debug, PartialEq)]
pub struct Built {
    pub plan: Plan,
    /// [`Walk::own_file_bytes`]; `None` when the walk failed.
    pub own_file_bytes: Option<u64>,
}

/// Plans, stored and recorded, for every linked worktree of `inventory` and for every ignored
/// entry matching a regenerable pattern in the worktrees `entries_in` picks, whatever their
/// checks find; a plan that cannot be stored is reported in `errors` (section 9.2). `reconcile`
/// builds its plans with this from the observation it holds (section 10.3).
pub fn create_from(
    config: &Config,
    state: &StateDir,
    inventory: &Inventory,
    entries_in: &dyn Fn(&Worktree) -> bool,
    errors: &mut Vec<ScopedError>,
) -> Vec<Built> {
    let sources = Real { config, state };
    let mut plans = Vec::new();
    for repo in &inventory.repos {
        let bare = Repo {
            worktrees: Vec::new(),
            ..repo.clone()
        };
        let repo_scope = format!("repo:{}", repo.main_worktree);
        for worktree in &repo.worktrees {
            let scope = format!("worktree:{}", worktree.path);
            let observation = WorktreeObservation {
                repo: bare.clone(),
                worktree: worktree.clone(),
                errors: inventory
                    .errors
                    .iter()
                    .filter(|e| e.scope == scope || e.scope == repo_scope)
                    .cloned()
                    .collect(),
            };
            let whole = (!worktree.main)
                .then(|| (PlanKind::ReclaimWorktree, PathBuf::from(&worktree.path)));
            let entries = worktree
                .ignored
                .iter()
                .filter(|e| e.regenerable_match && entries_in(worktree))
                .map(|e| {
                    (
                        PlanKind::ReclaimDir,
                        Path::new(&worktree.path).join(&e.path),
                    )
                });
            let targets: Vec<(PlanKind, PathBuf)> = whole.into_iter().chain(entries).collect();
            for (kind, target) in targets {
                let built = build(config, state, &sources, kind, &target, &observation);
                match store(config, state, built.plan) {
                    Ok(plan) => plans.push(Built { plan, ..built }),
                    Err(err) => errors.push(ScopedError {
                        scope: target.display().to_string(),
                        message: err.message,
                    }),
                }
            }
        }
    }
    plans
}

/// Loads the plan whose ID starts with `prefix` (at least 4 characters, unique).
pub fn load(state: &StateDir, prefix: &str) -> Result<Plan, Error> {
    let ids = plan_ids(state)?;
    let id = state::resolve_prefix(&ids, prefix)?;
    let path = state.plan_file(&id);
    let bytes = std::fs::read(&path)
        .map_err(|err| Error::new(ErrorCode::NotFound, format!("{}: {err}", path.display())))?;
    serde_json::from_slice(&bytes).map_err(|err| {
        Error::new(
            ErrorCode::StateUnavailable,
            format!("{}: {err}", path.display()),
        )
    })
}

/// The data patterns of the profiles observation selected for the repository, plus every
/// generic profile.
#[must_use]
pub fn patterns(config: &Config, repo: &Repo) -> DataLists {
    DataLists::from_selection(&config.select(|p| repo.profiles.contains(&p.name)))
}

/// The in-use facts of `target` when nothing is held: the lock probed, reservations read
/// under the admission lock (an error when it is not free in time), and processes.
fn probe_in_use(state: &StateDir, sources: &Real<'_>, worktree: &Path) -> InUseFacts {
    let key = state::worktree_key(worktree);
    InUseFacts {
        lock_held: state::probe(&state.worktree_lock(&key))
            .map(|probe| probe == Probe::Held)
            .map_err(|err| err.to_string()),
        reservations: sources.in_use(&key).map_err(|err| err.message),
        // The whole worktree for both kinds: a process working anywhere in it can use the
        // target (section 9.2).
        processes: processes_inside(worktree),
    }
}

/// Builds a plan for `target` from an observation of its worktree. Plans are built even
/// when invariants fail, so the reason is visible.
fn build(
    config: &Config,
    state: &StateDir,
    sources: &Real<'_>,
    kind: PlanKind,
    target: &Path,
    observation: &WorktreeObservation,
) -> Built {
    let worktree = PathBuf::from(&observation.worktree.path);
    let walked = walk(&worktree, target).map_err(|err| format!("{}: {err}", target.display()));
    let (root_id, root_created_ms) = root_identity(&worktree).unwrap_or((None, None));
    let in_use = probe_in_use(state, sources, &worktree);
    let patterns = patterns(config, &observation.repo);
    let inspection = Inspection {
        kind,
        worktree: &worktree,
        target,
        observation: Ok(observation),
        walk: walked.as_ref().map_err(String::as_str),
        in_use: &in_use,
        patterns: &patterns,
        managed: &config.host.observe.managed,
        squash_merged: false,
    };
    let checks = evaluate(&inspection);
    let provenance = provenance(config, &inspection);
    let plan = Plan {
        schema: PlanSchema,
        id: String::new(),
        created_ms: state::now_ms(),
        host_id: state.host_id().to_owned(),
        kind,
        target: PlanTarget {
            path: target.display().to_string(),
            repo: observation.repo.common_dir.clone(),
            worktree: observation.worktree.path.clone(),
            head: observation.worktree.head.clone(),
            branch: observation.worktree.branch.clone(),
            git_dir: observation.worktree.git_dir.clone(),
            root_id,
            root_created_ms,
        },
        allocated_bytes: walked.as_ref().ok().map(Walk::allocated),
        aliased_bytes: walked.as_ref().ok().map(Walk::aliased),
        invariants_ok: invariants_ok(&checks),
        approval: approval(&checks),
        checks,
        provenance,
    };
    Built {
        plan,
        own_file_bytes: walked.as_ref().ok().map(Walk::own_file_bytes),
    }
}

/// Claims a new plan ID by creating its directory, writes `plan.json`, and records it.
fn store(config: &Config, state: &StateDir, mut plan: Plan) -> Result<Plan, Error> {
    let id = loop {
        let id = state::new_id();
        match std::fs::create_dir(state.plan_dir(&id)) {
            Ok(()) => break id,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(state_error(&state.plan_dir(&id), &err)),
        }
    };
    plan.id = id;
    let bytes = serde_json::to_vec(&plan)
        .map_err(|err| Error::new(ErrorCode::Internal, err.to_string()))?;
    state.write_atomic(&state.plan_file(&plan.id), &bytes, Durability::Volatile)?;
    let recorder = recorder(config, state);
    let mut record: Record = recorder.record(RecordKind::PlanCreated);
    record.plan_id = Some(plan.id.clone());
    record.data = serde_json::json!({
        "kind": plan.kind,
        "target": plan.target.path,
        "invariants_ok": plan.invariants_ok,
        "approval": plan.approval,
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    let _ = recorder.append(&record);
    Ok(plan)
}

/// The recorder configured by `[records]`.
#[must_use]
pub fn recorder(config: &Config, state: &StateDir) -> Recorder {
    Recorder::new(
        state,
        config.host.records.max_bytes,
        config.host.records.keep,
    )
}

/// Every plan ID in the state directory.
fn plan_ids(state: &StateDir) -> Result<Vec<String>, Error> {
    let dir = state.plans_dir();
    let entries = std::fs::read_dir(&dir).map_err(|err| state_error(&dir, &err))?;
    Ok(entries
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| state::is_id(name))
        .collect())
}

/// Removes old temporary files and plan directories older than 30 days. Failures are
/// ignored: housekeeping never fails a command.
fn housekeeping(state: &StateDir) {
    let _ = state.gc_tmp();
    let Ok(ids) = plan_ids(state) else { return };
    let now = std::time::SystemTime::now();
    for id in ids {
        let dir = state.plan_dir(&id);
        let old = std::fs::metadata(&dir)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > PLAN_MAX_AGE);
        if old {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

/// The canonical target. A `reclaim_dir` target's own name is kept, not followed, so a
/// symbolic link is planned (and refused) as the link it is.
fn canonical_target(kind: PlanKind, target: &Path) -> Result<PathBuf, Error> {
    let not_found = |err: std::io::Error| {
        Error::new(ErrorCode::NotFound, format!("{}: {err}", target.display()))
    };
    let resolved = match (kind, target.parent(), target.file_name()) {
        (PlanKind::ReclaimDir, Some(parent), Some(name)) => {
            let parent = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            dunce::canonicalize(parent).map_err(not_found)?.join(name)
        }
        _ => dunce::canonicalize(target).map_err(not_found)?,
    };
    std::fs::symlink_metadata(&resolved).map_err(not_found)?;
    Ok(resolved)
}

fn invalid(message: String) -> Error {
    Error::new(ErrorCode::InvalidArgument, message)
}

fn state_error(path: &Path, err: &std::io::Error) -> Error {
    Error::new(
        ErrorCode::StateUnavailable,
        format!("{}: {err}", path.display()),
    )
}
