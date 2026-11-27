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
    Activity, DataLists, Descriptors, FACT_ENTRIES, InUseFacts, Inspection, approval, bound_facts,
    descriptor_scan, entries_newest, evaluate, evaluate_only, invariants_ok, processes_inside,
    processes_inside_any, provenance, walk_newest,
};
#[cfg(unix)]
pub(crate) use checks::{Linked, descriptor_uses, processes_fact};
pub use walk::{Entry, EntryKind, Mount, Tree, Walk, root_identity, walk, walk_times};

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::admission;
use crate::admission::LeaseHold;
use crate::config::{Config, Sessions};
use crate::contract::{
    Error, ErrorCode, InUse, Inventory, ObservedError, Plan, PlanKind, PlanList, PlanListSchema,
    PlanSchema, PlanTarget, Record, RecordKind, Repo, ScopedError, Worktree,
};
use crate::observe::{self, InventoryOptions, Newest, Quiet, Signals, Window, WorktreeObservation};
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
    // Built even when invariants fail, so the reason is visible.
    let built = build_worktree(config, state, &sources, &observation, &[(kind, target)])
        .pop()
        .expect("one plan of one target");
    store(config, state, built.plan, None)
}

/// `cardinal plan [--repo PATH]...`: a plan for every linked worktree and for every ignored
/// entry matching a regenerable pattern, whatever their checks find (section 9.2).
pub fn create_all(config: &Config, state: &StateDir, repos: &[PathBuf]) -> Result<PlanList, Error> {
    let sources = Real { config, state };
    housekeeping(state);
    let inventory = sources.inventory(repos)?;
    let mut errors = inventory.errors.clone();
    let plans = create_from(config, state, &inventory, &|_| true, None, &mut errors)
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
/// builds its plans with this from the observation it holds (section 10.3). With `reserve`,
/// the path of the state dir's reserve file, a plan's write that fails for lack of space spends
/// the reserve and is tried again at once, as a pass that executes writes its plans (section
/// 9.7, "Space").
///
/// The plans of one worktree are built together, those of up to [`BUILD_WORKERS`] worktrees at
/// a time, and stored in the order of `inventory`, so the plans and their records come out as
/// one worktree after another makes them.
pub fn create_from(
    config: &Config,
    state: &StateDir,
    inventory: &Inventory,
    entries_in: &dyn Fn(&Worktree) -> bool,
    reserve: Option<&Path>,
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
                // Only whether `origin` was read matters here: an unread one fails `observed`,
                // and an execution's claim takes the URL from `act`'s own observation.
                remote: match &repo.gh_error {
                    Some(err) if err.starts_with(crate::observe::REMOTE_UNREAD) => Err(err.clone()),
                    _ => Ok(None),
                },
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
            match store(config, state, built.plan, reserve) {
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
///
/// Each `reclaim_dir` target's entries are a signal of every plan of the worktree (section
/// 9.8), so the plans are built in two passes. The first walks each target and reduces its
/// walk to what its plan keeps ([`Walked`]: every check but `not_in_use`, what that check reads
/// of the walk, its newest entry, and its sizes) before the next is walked, so the first pass
/// holds the worktree's walk and one target's walk at most, as a plan built right after its
/// walk does; the worktree's walk is dropped once every target has its own. The second reads
/// the lease and the signals once, then the regenerable targets no walk of the first pass read
/// unless those refuse, and the in-use facts, and finishes each plan with `not_in_use`.
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
    // One descriptor scan for the worktree's `reclaim_dir` targets, read with the first that
    // needs it, after its walk.
    let mut scan = None;
    let walked: Vec<Walked> = targets
        .iter()
        .map(|(kind, target)| {
            let mut descriptors = || match kind {
                PlanKind::ReclaimDir => scan.get_or_insert_with(|| descriptor_scan(*kind)).clone(),
                PlanKind::ReclaimWorktree => None,
            };
            if let (Some(tree), true) = (&tree, whole(*kind, target)) {
                let walk = tree.as_ref().map(|tree| &tree.walk).map_err(String::as_str);
                return reduce(config, (*kind, target), observation, walk, descriptors());
            }
            let taken = tree.as_ref().and_then(|tree| {
                let rel = target.strip_prefix(worktree).ok()?;
                tree.as_ref().ok()?.walk_of(worktree, rel)
            });
            let own = taken.map_or_else(
                || walk(worktree, target).map_err(|err| walk_error(target, &err)),
                Ok,
            );
            let walk = own.as_ref().map_err(String::as_str);
            reduce(config, (*kind, target), observation, walk, descriptors())
        })
        .collect();
    drop(tree);
    // The regenerable targets no walk above read, for the `reclaim_dir` plans; the worktree's
    // own walk read every entry.
    let dirs = |path: &Path| {
        targets
            .iter()
            .any(|(kind, target)| *kind == PlanKind::ReclaimDir && target == path)
    };
    let mut unwalked = None;
    let whole_reads: Vec<Option<Newest>> = targets
        .iter()
        .zip(&walked)
        .map(|((kind, _), walked)| (*kind == PlanKind::ReclaimDir).then(|| walked.whole.clone()))
        .collect();
    // One read of the lease and the signals, and one of the processes working in the
    // worktree, for its plans, each with the first plan that needs it, after the walks.
    let mut read = None;
    let mut working = None;
    targets
        .iter()
        .zip(walked)
        .enumerate()
        .map(|(n, ((kind, target), walked))| {
            let base = read
                .get_or_insert_with(|| {
                    activity(
                        state,
                        &config.host.sessions,
                        &observation.worktree,
                        (None, None),
                        false,
                    )
                })
                .clone();
            // The target walk's entries, the other targets' walks, and the targets no walk read.
            let mut targets = walked.own.clone();
            for (m, other) in whole_reads.iter().enumerate() {
                if let (true, Some(other)) = (m != n, other) {
                    targets = targets.merge(other);
                }
            }
            // Read with the first `reclaim_dir` plan, and only when the lease and the signals
            // leave the worktree free and quiet: a plan they refuse is refused without them.
            let idle = base.lease == LeaseHold::Free && base.quiet == Quiet::Quiet;
            if let (PlanKind::ReclaimDir, true) = (kind, idle) {
                let unwalked =
                    unwalked.get_or_insert_with(|| read_targets(&observation.worktree, &dirs));
                targets = targets.merge(unwalked);
            }
            let activity = Activity {
                targets: Some(targets),
                ..base
            };
            let processes = working
                .get_or_insert_with(|| processes_inside(worktree))
                .clone();
            finish(
                config,
                (state, sources),
                (*kind, target),
                observation,
                walked,
                (processes, activity),
            )
        })
        .collect()
}

/// What a target's plan keeps of its walk once the walk is dropped ([`build_worktree`]).
struct Walked {
    /// Every check but `not_in_use`, in their order.
    checks: Vec<crate::contract::Check>,
    /// The provenance of the observation and the walk; the in-use facts' errors follow.
    provenance: crate::contract::Provenance,
    /// What [`checks::descriptor_users`] found of the walk, which `not_in_use` reads.
    users: Option<Vec<checks::DescriptorUser>>,
    /// The descriptor scan read for the target, if any.
    descriptors: Descriptors,
    /// [`checks::entries_newest`] of the walk with what it found removed while it read (the
    /// target's root included), its plan's own reading, or why the walk failed. An execution
    /// reads its snapshot's removals in step 5's comparison instead.
    own: Newest,
    /// [`walk_newest`] of the walk, the other plans' reading, or why the walk failed.
    whole: Newest,
    allocated_bytes: Option<u64>,
    aliased_bytes: Option<u64>,
    exclusive: Option<crate::observe::Exclusive>,
    own_file_bytes: Option<u64>,
}

/// [`Walked`] of `walked`, the walk of `target` or why it failed, with `descriptors`, the
/// descriptor scan read for it, if any.
fn reduce(
    config: &Config,
    (kind, target): (PlanKind, &Path),
    observation: &WorktreeObservation,
    walked: Result<&Walk, &str>,
    descriptors: Descriptors,
) -> Walked {
    let worktree = Path::new(&observation.worktree.path);
    let unread = "read with not_in_use".to_owned();
    // The in-use facts but the scan are read later, with `not_in_use`, which alone reads them.
    let in_use = InUseFacts {
        lock_held: Err(unread.clone()),
        reservations: Err(unread.clone()),
        processes: Err(unread),
        descriptors,
        activity: Activity::unread(),
    };
    let patterns = patterns(config, &observation.repo);
    let inspection = Inspection {
        kind,
        worktree,
        target,
        observation: Ok(observation),
        walk: walked,
        in_use: &in_use,
        patterns: &patterns,
        managed: &config.host.observe.managed,
        squash_merged: false,
    };
    Walked {
        checks: evaluate_only(&inspection, |name| name != "not_in_use"),
        provenance: checks::walk_provenance(config, &inspection),
        users: checks::descriptor_users(&inspection),
        own: walked.map_or_else(checks::unwalked, |walk| {
            let mut own = checks::entries_newest(worktree, walk);
            if let Some(rel) = walk.vanished.first() {
                own.vanish(&worktree.join(rel));
            }
            own
        }),
        whole: walked.map_or_else(checks::unwalked, |walk| walk_newest(worktree, walk)),
        allocated_bytes: walked.ok().map(Walk::allocated),
        aliased_bytes: walked.ok().map(Walk::aliased),
        exclusive: walked.ok().map(Walk::exclusive),
        own_file_bytes: walked.ok().map(Walk::own_file_bytes),
        descriptors: in_use.descriptors,
    }
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

/// The session lease and the quiet period of the observed `worktree` (section 9.8), read now
/// under `sessions`: the lease file by name, and the signals of the observation's git dir,
/// status paths, and ignored entries, `skip` left out, by `deadline` when there is one. With
/// `walk_entries`, the checks read the target walk's entries as signals too.
#[must_use]
pub fn activity(
    state: &StateDir,
    sessions: &Sessions,
    worktree: &Worktree,
    (skip, deadline): (Option<&Path>, Option<std::time::Instant>),
    walk_entries: bool,
) -> Activity {
    let now_ms = state::now_ms();
    let window = Window {
        now_ms,
        quiet_ms: sessions.quiet_s.saturating_mul(1000),
    };
    Activity {
        lease: admission::lease_hold(
            state,
            &state::worktree_key(Path::new(&worktree.path)),
            now_ms,
            sessions.linger_s.saturating_mul(1000),
        ),
        quiet: observe::quiet::read(
            &Signals {
                deadline,
                ..Signals::of(worktree, skip)
            },
            window,
        ),
        window,
        walk_entries,
        targets: None,
    }
}

/// The newest entry below the regenerable targets of the observed `worktree` other than
/// `target`, never their own times (section 9.8), as [`read_targets`] reads them.
#[must_use]
pub fn other_targets(worktree: &Worktree, target: &Path) -> Newest {
    read_targets(worktree, &|path| path == target)
}

/// The newest entry below the regenerable targets of the observed `worktree` that `walked`
/// does not name, each walked by [`walk_times`], as `plan` walks a target but reading no sizes,
/// and reduced by [`walk_newest`] before the next is walked (section 9.8): a target the
/// observation listed and gone when its walk begins, or removed during it, is activity now, as
/// a status path gone is; a walk that fails otherwise, and a path git listed that was not kept,
/// is unknown. Its cost is the targets' walks, as `plan`'s without the sizes.
fn read_targets(worktree: &Worktree, walked: &dyn Fn(&Path) -> bool) -> Newest {
    let root = Path::new(&worktree.path);
    let mut newest = Newest::default();
    for entry in worktree.ignored.iter().filter(|e| e.regenerable_match) {
        let Some(path) = entry.native.as_deref() else {
            newest.fail(format!("{}: the path git listed was not kept", entry.path));
            continue;
        };
        if walked(path) {
            continue;
        }
        match walk_times(root, path) {
            Ok(walked) => newest = newest.merge(&walk_newest(root, &walked)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => newest.vanish(path),
            Err(err) => newest.fail(format!("the walk: {}", walk_error(path, &err))),
        }
    }
    newest
}

/// The in-use facts of `target` when nothing is held: the lock probed, reservations read
/// under the admission lock (an error when it is not free in time), and the processes read
/// for its worktree: those working in it ([`processes_inside`]) and the descriptor scan read
/// for the target, if any.
fn probe_in_use(
    state: &StateDir,
    sources: &Real<'_>,
    worktree: &Path,
    (processes, descriptors, activity): (Result<Vec<u32>, String>, Descriptors, Activity),
) -> InUseFacts {
    let key = state::worktree_key(worktree);
    InUseFacts {
        lock_held: state::probe(&state.worktree_lock(&key))
            .map(|probe| probe == Probe::Held)
            .map_err(|err| err.to_string()),
        reservations: sources.in_use(&key).map_err(|err| err.message),
        processes,
        descriptors,
        activity,
    }
}

/// The plan of `target` from `walked`, what it kept of its walk, `processes`, those working in
/// its worktree ([`processes_inside`]), and `activity`, the lease and the signals with the
/// regenerable targets' entries: the in-use facts are read now, and `not_in_use` is evaluated
/// from them.
fn finish(
    config: &Config,
    (state, sources): (&StateDir, &Real<'_>),
    (kind, target): (PlanKind, &Path),
    observation: &WorktreeObservation,
    walked: Walked,
    (processes, activity): (Result<Vec<u32>, String>, Activity),
) -> Built {
    let worktree = PathBuf::from(&observation.worktree.path);
    let (root_id, root_created_ms) = root_identity(&worktree).unwrap_or((None, None));
    let in_use = probe_in_use(
        state,
        sources,
        &worktree,
        (processes, walked.descriptors, activity),
    );
    let patterns = patterns(config, &observation.repo);
    let inspection = Inspection {
        kind,
        worktree: &worktree,
        target,
        observation: Ok(observation),
        walk: Err("reduced"),
        in_use: &in_use,
        patterns: &patterns,
        managed: &config.host.observe.managed,
        squash_merged: false,
    };
    let mut checks = walked.checks;
    // `not_in_use` follows `observed`, as `evaluate` orders them.
    let at = checks
        .iter()
        .position(|c| c.name == "observed")
        .map_or(0, |n| n + 1);
    checks.insert(
        at,
        checks::evaluate_not_in_use(&inspection, walked.users.as_deref()),
    );
    let mut provenance = walked.provenance;
    provenance.errors.extend(checks::in_use_errors(&in_use));
    if let Some(error) = walked.exclusive.as_ref().and_then(|e| e.error.clone()) {
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
        allocated_bytes: walked.allocated_bytes,
        aliased_bytes: walked.aliased_bytes,
        exclusive_bytes: walked.exclusive.as_ref().and_then(|e| e.bytes),
        exclusive_measure: walked.exclusive.map(|e| e.measure),
        invariants_ok: invariants_ok(&checks),
        approval: approval(&checks),
        checks,
        provenance,
    };
    Built {
        plan,
        own_file_bytes: walked.own_file_bytes,
    }
}

/// Claims a new plan ID by creating its directory, writes `plan.json`, and records it. With
/// `reserve`, each write goes through the reserve's retry; a test's fake fails it as `plan`
/// (section 13.7).
fn store(
    config: &Config,
    state: &StateDir,
    mut plan: Plan,
    reserve: Option<&Path>,
) -> Result<Plan, Error> {
    let written = |write: &mut dyn FnMut() -> std::io::Result<()>| match reserve {
        Some(reserve) => state::retry_with_reserve(reserve, || {
            crate::testpoint::fake_write("plan")?;
            write()
        }),
        None => write(),
    };
    let id = loop {
        let id = state::new_id();
        let dir = state.plan_dir(&id);
        match written(&mut || std::fs::create_dir(&dir)) {
            Ok(()) => break id,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(state_error(&dir, &err)),
        }
    };
    plan.id = id;
    let bytes = serde_json::to_vec(&plan)
        .map_err(|err| Error::new(ErrorCode::Internal, err.to_string()))?;
    let file = state.plan_file(&plan.id);
    let tmp = state.tmp_dir();
    if let Err(err) =
        written(&mut || state::write_atomic(&tmp, &file, &bytes, Durability::Volatile))
    {
        // The plan's directory without its file is no plan; it goes with the failure.
        let _ = std::fs::remove_dir(state.plan_dir(&plan.id));
        return Err(state_error(&file, &err));
    }
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
