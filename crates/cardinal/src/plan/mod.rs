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
pub use walk::{Entry, EntryKind, Mount, Tree, Walk, root_identity, walk};

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::admission;
use crate::config::Config;
use crate::contract::{
    Error, ErrorCode, InUse, Inventory, ObservedError, Plan, PlanKind, PlanList, PlanListSchema,
    PlanSchema, PlanTarget, Record, RecordKind, Repo, ScopedError, Worktree,
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
    /// read under the admission lock, one read at a time in this process
    /// ([`ADMISSION_READS`]).
    pub fn in_use(&self, worktree_key: &str) -> Result<InUse, Error> {
        let _one_at_a_time = ADMISSION_READS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        admission::in_use(self.state, &self.config.host, worktree_key)
    }
}

/// Held by each [`Real::in_use`] of this process. Plans built at once (section 9.2) would
/// otherwise meet on the locks a read takes: one read would find the admission lock held by
/// another and, past `admission.lock_timeout_ms`, fail, and one read's probe of a job's live
/// lock would find it held by another's probe and count a gone job as live. One at a time,
/// each read is the read a plan built alone makes, at the moment its plan makes it. A wait
/// here ends once the reads before it end, each bounded by the admission lock's deadline.
static ADMISSION_READS: Mutex<()> = Mutex::new(());

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
///
/// The plans of one worktree are built together, those of up to [`BUILD_WORKERS`] worktrees at
/// a time, and stored in the order of `inventory`, so the plans and their records come out as
/// one worktree after another makes them.
pub fn create_from(
    config: &Config,
    state: &StateDir,
    inventory: &Inventory,
    entries_in: &dyn Fn(&Worktree) -> bool,
    errors: &mut Vec<ScopedError>,
) -> Vec<Built> {
    let sources = Real { config, state };
    let mut jobs = Vec::new();
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
            jobs.push((observation, targets));
        }
    }
    let built = parallel(&jobs, &|(observation, targets)| {
        build_worktree(config, state, &sources, observation, targets)
    });
    let mut plans = Vec::new();
    for ((_, targets), built) in jobs.iter().zip(built) {
        for ((_, target), built) in targets.iter().zip(built) {
            match store(config, state, built.plan) {
                Ok(plan) => plans.push(Built { plan, ..built }),
                Err(err) => errors.push(ScopedError {
                    scope: target.display().to_string(),
                    message: err.message,
                }),
            }
        }
    }
    plans
}

/// The most worktrees whose plans are built at a time. Their walks are metadata reads in this
/// process, which contend in the kernel: more threads cost more kernel time and memory than
/// they save (section 9.2).
const BUILD_WORKERS: usize = 4;

/// `build` of each of `jobs`, in their order, on one thread per logical CPU, at least one and
/// at most [`BUILD_WORKERS`]. Once a `build` panics no worker takes another job, and the panic
/// goes on, with its own payload, when the builds under way have ended.
fn parallel<J: Sync, T: Send>(jobs: &[J], build: &(dyn Fn(&J) -> T + Sync)) -> Vec<T> {
    let results: Mutex<Vec<(usize, T)>> = Mutex::new(Vec::with_capacity(jobs.len()));
    let next = AtomicUsize::new(0);
    let panicked = AtomicBool::new(false);
    let workers = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .clamp(1, BUILD_WORKERS)
        .min(jobs.len().max(1));
    let panic = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let _set = SetOnPanic(&panicked);
                    while !panicked.load(Ordering::Relaxed) {
                        let n = next.fetch_add(1, Ordering::Relaxed);
                        let Some(job) = jobs.get(n) else { break };
                        let result = build(job);
                        results
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push((n, result));
                    }
                })
            })
            .collect();
        let mut panic = None;
        for handle in handles {
            if let Err(payload) = handle.join() {
                panic.get_or_insert(payload);
            }
        }
        panic
    });
    if let Some(payload) = panic {
        std::panic::resume_unwind(payload);
    }
    let mut results = results
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    results.sort_by_key(|(n, _)| *n);
    results.into_iter().map(|(_, result)| result).collect()
}

/// Sets its flag when a panic drops it.
struct SetOnPanic<'a>(&'a AtomicBool);

impl Drop for SetOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.store(true, Ordering::Relaxed);
        }
    }
}

/// The plans of `targets`, all in the worktree of `observation`, built in their order and not
/// stored. The worktree's own walk, taken first when its plan is among them, gives each
/// directory below it its walk when it listed that directory and the path holds it still
/// ([`Tree::walk_of`]); any other target is walked on its own.
fn build_worktree(
    config: &Config,
    state: &StateDir,
    sources: &Real<'_>,
    observation: &WorktreeObservation,
    targets: &[(PlanKind, PathBuf)],
) -> Vec<Built> {
    let worktree = Path::new(&observation.worktree.path);
    let whole =
        |kind: PlanKind, target: &Path| kind == PlanKind::ReclaimWorktree && target == worktree;
    // The worktree's plan comes first, so its walk is taken where it was.
    let tree = targets
        .first()
        .filter(|(kind, target)| whole(*kind, target))
        .map(|_| Tree::walk(worktree).map_err(|err| walk_error(worktree, &err)));
    let mut plans = Vec::with_capacity(targets.len());
    for (kind, target) in targets {
        let own: Result<Walk, String>;
        let walked = match &tree {
            Some(tree) if whole(*kind, target) => {
                tree.as_ref().map(|tree| &tree.walk).map_err(String::as_str)
            }
            _ => {
                let taken = tree.as_ref().and_then(|tree| {
                    let rel = target.strip_prefix(worktree).ok()?;
                    tree.as_ref().ok()?.walk_of(worktree, rel)
                });
                own = taken.map_or_else(
                    || walk(worktree, target).map_err(|err| walk_error(target, &err)),
                    Ok,
                );
                own.as_ref().map_err(String::as_str)
            }
        };
        plans.push(build_with(
            config,
            state,
            sources,
            (*kind, target),
            observation,
            walked,
        ));
    }
    plans
}

fn walk_error(target: &Path, err: &std::io::Error) -> String {
    format!("{}: {err}", target.display())
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
    let worktree = Path::new(&observation.worktree.path);
    let walked = walk(worktree, target).map_err(|err| walk_error(target, &err));
    build_with(
        config,
        state,
        sources,
        (kind, target),
        observation,
        walked.as_ref().map_err(String::as_str),
    )
}

/// [`build`] from `walked`, the walk of `target` or why it failed.
fn build_with(
    config: &Config,
    state: &StateDir,
    sources: &Real<'_>,
    (kind, target): (PlanKind, &Path),
    observation: &WorktreeObservation,
    walked: Result<&Walk, &str>,
) -> Built {
    let worktree = PathBuf::from(&observation.worktree.path);
    let (root_id, root_created_ms) = root_identity(&worktree).unwrap_or((None, None));
    let in_use = probe_in_use(state, sources, &worktree);
    let patterns = patterns(config, &observation.repo);
    let inspection = Inspection {
        kind,
        worktree: &worktree,
        target,
        observation: Ok(observation),
        walk: walked,
        in_use: &in_use,
        patterns: &patterns,
        managed: &config.host.observe.managed,
        squash_merged: false,
    };
    let checks = evaluate(&inspection);
    let mut provenance = provenance(config, &inspection);
    let exclusive = walked.ok().map(Walk::exclusive);
    if let Some(error) = exclusive.as_ref().and_then(|e| e.error.clone()) {
        provenance.errors.push(ObservedError {
            source: "exclusive_bytes".to_owned(),
            message: error,
        });
    }
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
        allocated_bytes: walked.ok().map(Walk::allocated),
        aliased_bytes: walked.ok().map(Walk::aliased),
        exclusive_bytes: exclusive.as_ref().and_then(|e| e.bytes),
        exclusive_measure: exclusive.map(|e| e.measure),
        invariants_ok: invariants_ok(&checks),
        approval: approval(&checks),
        checks,
        provenance,
    };
    Built {
        plan,
        own_file_bytes: walked.ok().map(Walk::own_file_bytes),
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

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::panic::AssertUnwindSafe;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::{Config, Real, parallel};
    use crate::config::HostConfig;
    use crate::contract::{
        AdmissionResult, GroupIdentity, Priority, ProcessIdentity, Reservation, ReservationSchema,
        ReservationState, ReservationWorktree,
    };
    use crate::state::StateDir;

    /// Plans built at once read reservations as a plan built alone does: with
    /// `admission.lock_timeout_ms = 0`, no read finds the admission lock held by another, and
    /// no read finds the live lock of a gone job held by another's probe of it.
    #[test]
    fn plans_built_at_once_read_reservations_one_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::open(&dir.path().join("state")).unwrap();
        let mut host = HostConfig::defaults();
        host.admission.lock_timeout_ms = 0;
        let config = Config {
            dir: dir.path().join("config"),
            host,
            profiles: Vec::new(),
        };
        let sources = Real {
            config: &config,
            state: &state,
        };
        // A job of another boot whose live lock is free: stale, so the worktree is not in use.
        let job = "aaaa000000000001";
        let gone = Reservation {
            schema: ReservationSchema,
            job_id: job.into(),
            state: ReservationState::Running,
            boot_s: 1,
            created_ms: 1,
            updated_ms: 1,
            runner: ProcessIdentity {
                pid: 999_999,
                start_s: 1,
            },
            group: GroupIdentity::Pgid {
                pgid: 999_999,
                leader_start_s: 1,
            },
            child: None,
            stop_signal_ms: None,
            priority: Priority::Normal,
            resources: [("cpu".to_owned(), NonZeroU64::MIN)].into_iter().collect(),
            admission: AdmissionResult::WithinBudget,
            class: None,
            rule: None,
            worktree: Some(ReservationWorktree {
                key: "key".into(),
                path: "/w".into(),
                protected: true,
            }),
            session: None,
            cwd: "/w".into(),
            argv: vec!["x".into()],
        };
        std::fs::write(state.reservation(job), serde_json::to_vec(&gone).unwrap()).unwrap();
        std::fs::write(state.live_lock(job), b"").unwrap();
        let reads = parallel(&[(); 4], &|()| {
            (0..500).map(|_| sources.in_use("key")).collect::<Vec<_>>()
        });
        for read in reads.into_iter().flatten() {
            let in_use = read.unwrap();
            assert!(!in_use.value, "{:?}", in_use.jobs);
        }
        // The reservation is read: with its live lock held, the job counts.
        let held = crate::state::try_lock(&state.live_lock(job), crate::state::LockMode::Exclusive)
            .unwrap()
            .unwrap();
        assert!(sources.in_use("key").unwrap().value);
        drop(held);
    }

    /// Once a build panics, no worker takes another job, and the panic goes on with its own
    /// payload.
    #[test]
    fn a_panicking_build_stops_the_others_and_goes_on() {
        let built = AtomicUsize::new(0);
        let jobs: Vec<usize> = (0..100).collect();
        let panic = std::panic::catch_unwind(AssertUnwindSafe(|| {
            parallel(&jobs, &|&n| {
                assert!(n != 0, "build 0 failed");
                std::thread::sleep(Duration::from_millis(20));
                built.fetch_add(1, Ordering::Relaxed);
            })
        }))
        .unwrap_err();
        let message = panic
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
        assert_eq!(message, Some("build 0 failed"));
        let built = built.into_inner();
        assert!(built < jobs.len() - 1, "{built} builds ran");
    }
}
