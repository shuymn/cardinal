//! Plans: generation and identity binding (`docs/architecture.md` sections 9.1–9.4).
//!
//! A plan binds a target to its identity (host, repository, worktree, worktree root, HEAD,
//! branch, path) and carries the checks of section 9.2 as they were when it was made; `act`
//! evaluates them again and never trusts the stored ones. Observation and admission are
//! reached through [`Sources`], so plans and `act` run against fakes in tests; [`Real`] calls
//! the `observe` and `admission` modules.

mod checks;
mod walk;

use std::path::{Path, PathBuf};
use std::time::Duration;

pub use checks::{
    DataLists, InUseFacts, Inspection, approval, evaluate, invariants_ok, processes_inside,
};
pub use walk::{Entry, EntryKind, Mount, Walk, root_identity, walk};

use crate::admission;
use crate::config::Config;
use crate::contract::{
    Error, ErrorCode, IgnoredClass, InUse, Inventory, Plan, PlanKind, PlanList, PlanListSchema,
    PlanSchema, PlanTarget, Record, RecordKind, Repo, ScopedError,
};
use crate::observe::{self, InventoryOptions, WorktreeObservation};
use crate::record::Recorder;
use crate::state::{self, Durability, Probe, StateDir};

/// Plan directories older than this are removed by `plan` runs.
const PLAN_MAX_AGE: Duration = Duration::from_hours(30 * 24);

/// HEAD and branch of a worktree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Head {
    pub head: Option<String>,
    pub branch: Option<String>,
}

/// What plans and `act` read from observation and admission, and the one git operation
/// `act` performs.
pub trait Sources {
    /// Observes the worktree containing `path` from scratch.
    fn observe_worktree(&self, path: &Path) -> Result<WorktreeObservation, Error>;
    /// Observes the configured repositories and `repos`.
    fn inventory(&self, repos: &[PathBuf]) -> Result<Inventory, Error>;
    /// Live and recovery-needed reservations naming the worktree with key `worktree_key`.
    fn in_use(&self, worktree_key: &str) -> Result<InUse, Error>;
    /// HEAD and branch of the worktree rooted at `worktree`, read right before deletion.
    fn head(&self, worktree: &Path) -> Result<Head, Error>;
    /// `git -C <main> worktree remove <worktree>`: drops the administrative entry of a
    /// worktree whose directory `act` has removed.
    fn remove_worktree(&self, main: &Path, worktree: &Path) -> Result<(), Error>;
}

/// [`Sources`] backed by the `observe` and `admission` modules.
#[derive(Clone, Copy, Debug)]
pub struct Real<'a> {
    pub config: &'a Config,
    pub state: &'a StateDir,
}

impl Sources for Real<'_> {
    fn observe_worktree(&self, path: &Path) -> Result<WorktreeObservation, Error> {
        observe::observe_worktree(self.config, self.state, path)
    }

    fn inventory(&self, repos: &[PathBuf]) -> Result<Inventory, Error> {
        let options = InventoryOptions {
            repos: repos.to_vec(),
            sizes: false,
        };
        observe::inventory(self.config, self.state, &options)
    }

    fn in_use(&self, worktree_key: &str) -> Result<InUse, Error> {
        admission::in_use(self.state, worktree_key)
    }

    fn head(&self, worktree: &Path) -> Result<Head, Error> {
        let observation = observe::observe_worktree(self.config, self.state, worktree)?;
        Ok(Head {
            head: observation.worktree.head,
            branch: observation.worktree.branch,
        })
    }

    fn remove_worktree(&self, main: &Path, worktree: &Path) -> Result<(), Error> {
        let _ = (main, worktree);
        Err(Error::not_implemented("observe::git"))
    }
}

/// `cardinal plan reclaim-worktree PATH | reclaim-dir PATH`.
pub fn create(
    config: &Config,
    state: &StateDir,
    kind: PlanKind,
    target: &Path,
) -> Result<Plan, Error> {
    create_with(config, state, &Real { config, state }, kind, target)
}

/// [`create`] with the given sources.
pub fn create_with(
    config: &Config,
    state: &StateDir,
    sources: &dyn Sources,
    kind: PlanKind,
    target: &Path,
) -> Result<Plan, Error> {
    housekeeping(state);
    let target = canonical_target(kind, target)?;
    let observation = sources.observe_worktree(&target)?;
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
    let plan = build(config, state, sources, kind, &target, &observation);
    store(config, state, plan)
}

/// `cardinal plan [--repo PATH]...`: plans for every whole-worktree candidate and every
/// regenerable entry of worktrees that are not candidates.
pub fn create_all(config: &Config, state: &StateDir, repos: &[PathBuf]) -> Result<PlanList, Error> {
    create_all_with(config, state, &Real { config, state }, repos)
}

/// [`create_all`] with the given sources.
pub fn create_all_with(
    config: &Config,
    state: &StateDir,
    sources: &dyn Sources,
    repos: &[PathBuf],
) -> Result<PlanList, Error> {
    housekeeping(state);
    let inventory = sources.inventory(repos)?;
    let mut plans = Vec::new();
    let mut errors = inventory.errors.clone();
    for repo in &inventory.repos {
        let bare = Repo {
            worktrees: Vec::new(),
            ..repo.clone()
        };
        for worktree in &repo.worktrees {
            let observation = WorktreeObservation {
                repo: bare.clone(),
                worktree: worktree.clone(),
            };
            let targets: Vec<(PlanKind, PathBuf)> = if !worktree.main && worktree.reclaim.candidate
            {
                vec![(PlanKind::ReclaimWorktree, PathBuf::from(&worktree.path))]
            } else {
                worktree
                    .ignored
                    .iter()
                    .filter(|e| e.class == IgnoredClass::Regenerable)
                    .map(|e| {
                        (
                            PlanKind::ReclaimDir,
                            Path::new(&worktree.path).join(&e.path),
                        )
                    })
                    .collect()
            };
            for (kind, target) in targets {
                let plan = build(config, state, sources, kind, &target, &observation);
                match store(config, state, plan) {
                    Ok(plan) => plans.push(plan),
                    Err(err) => errors.push(ScopedError {
                        scope: target.display().to_string(),
                        message: err.message,
                    }),
                }
            }
        }
    }
    Ok(PlanList {
        schema: PlanListSchema,
        plans,
        errors,
    })
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
/// without the admission lock, and processes.
fn probe_in_use(
    state: &StateDir,
    sources: &dyn Sources,
    worktree: &Path,
    target: &Path,
) -> InUseFacts {
    let key = state::worktree_key(worktree);
    InUseFacts {
        lock_held: state::probe(&state.worktree_lock(&key))
            .map(|probe| probe == Probe::Held)
            .map_err(|err| err.to_string()),
        reservations: sources.in_use(&key).map_err(|err| err.message),
        processes: processes_inside(target),
    }
}

/// Builds a plan for `target` from an observation of its worktree. Plans are built even
/// when invariants fail, so the reason is visible.
fn build(
    config: &Config,
    state: &StateDir,
    sources: &dyn Sources,
    kind: PlanKind,
    target: &Path,
    observation: &WorktreeObservation,
) -> Plan {
    let worktree = PathBuf::from(&observation.worktree.path);
    let walked = walk(&worktree, target).map_err(|err| format!("{}: {err}", target.display()));
    let (root_id, root_created_ms) = root_identity(&worktree).unwrap_or((None, None));
    let in_use = probe_in_use(state, sources, &worktree, target);
    let patterns = patterns(config, &observation.repo);
    let checks = evaluate(&Inspection {
        kind,
        worktree: &worktree,
        target,
        observation: Ok(observation),
        walk: walked.as_ref().map_err(String::as_str),
        in_use: &in_use,
        patterns: &patterns,
        managed: &config.host.observe.managed,
    });
    Plan {
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
            root_id,
            root_created_ms,
        },
        allocated_bytes: walked.as_ref().ok().map(Walk::allocated),
        invariants_ok: invariants_ok(&checks),
        approval: approval(&checks),
        checks,
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
