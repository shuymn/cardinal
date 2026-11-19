//! The checks of `docs/architecture.md` section 9.2, evaluated the same way by `plan` and by
//! `act`. Every check is computed from facts gathered by the caller: an observation, a walk
//! (or `act`'s snapshot) of the target, and the in-use facts. Missing facts give `unknown`,
//! which is never treated as safe. Each check carries the facts it read, and [`provenance`]
//! names the observation errors and configuration keys behind them (section 10.2).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::{Config, DataList, Managed, Selection, data_patterns_cover};
use crate::contract::{
    Approval, Check, CheckFact, CheckResult, CheckRole, ConfigKey, IgnoredClass, InUse,
    MergeEvidence, ObservedError, PlanKind, Provenance, Tri,
};
use crate::observe::WorktreeObservation;
use crate::os::fds::{Scan, Use, Way};
use crate::os::{Detail, ProcessTable};

use super::walk::{EntryKind, Mount, Walk};

/// What makes a worktree in use (`docs/architecture.md` section 4.6). `Err` holds why a fact
/// could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InUseFacts {
    /// Whether another process holds the worktree's in-use lock.
    pub lock_held: Result<bool, String>,
    /// Live and recovery-needed reservations that name the worktree.
    pub reservations: Result<InUse, String>,
    /// Processes whose working directory is inside the target.
    pub processes: Result<Vec<u32>, String>,
    /// The descriptor scan of section 9.7, read for a `reclaim_dir` target on Unix
    /// ([`descriptor_scan`]).
    pub descriptors: Descriptors,
}

/// The descriptor scan read for a target, or why it is not complete; `None` where it is not
/// read.
pub type Descriptors = Option<Result<Arc<Scan>, String>>;

/// Everything the checks are computed from.
#[derive(Clone, Copy, Debug)]
pub struct Inspection<'a> {
    pub kind: PlanKind,
    /// The worktree root (absolute).
    pub worktree: &'a Path,
    /// The target (absolute): the worktree root, or a path inside it.
    pub target: &'a Path,
    pub observation: Result<&'a WorktreeObservation, &'a str>,
    pub walk: Result<&'a Walk, &'a str>,
    pub in_use: &'a InUseFacts,
    pub patterns: &'a DataLists,
    /// `[observe] managed` roots.
    pub managed: &'a [Managed],
    /// The caller judged HEAD squash-merged (`act --squash-merged`, section 9.3).
    pub squash_merged: bool,
}

/// The `[data]` lists that apply to one repository, matched with
/// [`data_patterns_cover`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DataLists {
    pub protected: Vec<String>,
    pub regenerable: Vec<String>,
}

impl DataLists {
    /// The union of the `[data]` lists of the selected profiles.
    #[must_use]
    pub fn from_selection(selection: &Selection<'_>) -> Self {
        let owned =
            |set: std::collections::BTreeSet<&str>| set.into_iter().map(str::to_owned).collect();
        Self {
            protected: owned(selection.protected()),
            regenerable: owned(selection.regenerable()),
        }
    }

    /// Whether the worktree-relative path `rel` is, or is inside, a match of `list`.
    #[must_use]
    pub fn covers(&self, list: DataList, rel: &Path) -> bool {
        let patterns = match list {
            DataList::Protected => &self.protected,
            DataList::Regenerable => &self.regenerable,
        };
        data_patterns_cover(list, patterns, rel)
    }
}

/// How many paths a check detail names before it summarizes the rest.
const DETAIL_PATHS: usize = 5;

/// How many entries a list fact keeps where a command prints it bounded.
pub const FACT_ENTRIES: usize = 10;

/// How many entries `not_in_use` names of each list of processes it reads (those working in
/// the worktree, those using the target, those the scan could not read), the rest counted in
/// `omitted`: every plan of a worktree repeats them, while the check itself reads every one.
const PROCESS_ENTRIES: usize = 3;

/// Every check for the plan kind, in the order of section 9.2.
#[must_use]
pub fn evaluate(i: &Inspection<'_>) -> Vec<Check> {
    let worktree = i.kind == PlanKind::ReclaimWorktree;
    let users = descriptor_users(i);
    let mut checks = vec![observed(i), not_in_use(i, users.as_deref()), not_managed(i)];
    if worktree {
        let facts = i.observation.ok().map(|o| &o.worktree.facts);
        checks.push(clean(i));
        checks.push(tri(
            "no_unpushed",
            facts.map(|f| f.unpushed),
            Tri::No,
            "commits not on any remote",
        ));
        checks.push(tri(
            "no_open_pr",
            facts.map(|f| f.open_pr),
            Tri::No,
            "an open pull request",
        ));
        checks.push(tri(
            "has_upstream",
            facts.map(|f| f.has_upstream),
            Tri::Yes,
            "the branch has no upstream",
        ));
    } else {
        checks.push(regenerable(i));
    }
    checks.push(plain_target(i));
    checks.push(no_protected(i));
    checks.push(no_unclassifiable(i));
    checks.push(no_nested_repository(i));
    if !worktree && cfg!(unix) {
        checks.push(local_volume(i));
    }
    if worktree {
        checks.push(super::merge::merge_evidence(i));
    }
    for check in &mut checks {
        check.facts = facts(&check.name, i, users.as_deref());
    }
    checks
}

/// One fact a check read, with what produced it; null when it was not read. Kept whole: plans,
/// `act` records, and `explain` hold every entry, and only a command a timer pass runs prints
/// it cut ([`bound_facts`]).
fn fact(name: &str, value: Option<impl serde::Serialize>, source: &str) -> CheckFact {
    CheckFact {
        name: name.to_owned(),
        value: value
            .and_then(|v| serde_json::to_value(v).ok())
            .unwrap_or_default(),
        source: source.to_owned(),
        omitted: None,
    }
}

/// Cuts every list fact of `checks` to its first [`FACT_ENTRIES`] entries, counting the rest
/// in `omitted`, for the commands a timer pass runs, whose output must not grow with the
/// worktree (section 10.5): `plan reclaim-worktree` and `act` print their checks so.
pub fn bound_facts(checks: &mut [Check]) {
    for fact in checks.iter_mut().flat_map(|check| &mut check.facts) {
        if let serde_json::Value::Array(entries) = &mut fact.value
            && entries.len() > FACT_ENTRIES
        {
            fact.omitted = Some((entries.len() - FACT_ENTRIES) as u64);
            entries.truncate(FACT_ENTRIES);
        }
    }
}

/// The facts `merge_evidence` reads.
fn merge_facts(i: &Inspection<'_>) -> Vec<CheckFact> {
    let repo = i.observation.ok().map(|o| &o.repo);
    let facts = i.observation.ok().map(|o| &o.worktree.facts);
    vec![
        fact(
            "worktree.facts.merge_evidence",
            facts.map(|f| f.merge_evidence),
            "git, gh",
        ),
        fact(
            "worktree.facts.merged_pr",
            facts.map(|f| &f.merged_pr),
            "gh",
        ),
        fact("repo.base", repo.map(|r| &r.base), "git"),
        fact(
            "worktree.facts.content_in_base",
            facts.map(|f| f.content_in_base),
            "git",
        ),
        fact(
            "worktree.facts.content_diff",
            facts.map(|f| &f.content_diff),
            "git",
        ),
        fact("act.squash_merged", Some(i.squash_merged), "cardinal act"),
    ]
}

/// The fact `name` holding the first [`PROCESS_ENTRIES`] of `entries`, the rest counted in
/// `omitted`.
pub(crate) fn processes_fact<T: serde::Serialize>(
    name: &str,
    entries: Option<&[T]>,
    source: &str,
) -> CheckFact {
    let mut fact = fact(
        name,
        entries.map(|all| &all[..all.len().min(PROCESS_ENTRIES)]),
        source,
    );
    fact.omitted = entries
        .map(|all| all.len().saturating_sub(PROCESS_ENTRIES) as u64)
        .filter(|&rest| rest > 0);
    fact
}

/// The facts `not_in_use` reads, with `users`, what [`descriptor_users`] found.
fn in_use_facts(i: &Inspection<'_>, users: Option<&[DescriptorUser]>) -> Vec<CheckFact> {
    let in_use = i.in_use;
    let unscanned = scan(i).map(|scan| {
        scan.unscanned
            .iter()
            .map(|(pid, error)| serde_json::json!({ "pid": pid, "error": error }))
            .collect::<Vec<_>>()
    });
    vec![
        fact(
            "in_use.lock_held",
            in_use.lock_held.as_ref().ok(),
            "worktree in-use lock",
        ),
        fact(
            "in_use.reservations",
            in_use.reservations.as_ref().ok(),
            "reservations",
        ),
        processes_fact(
            "in_use.processes",
            in_use.processes.as_deref().ok(),
            "process table",
        ),
        processes_fact("in_use.descriptors.users", users, "descriptor scan"),
        processes_fact(
            "in_use.descriptors.unscanned",
            unscanned.as_deref(),
            "descriptor scan",
        ),
        fact(
            "in_use.descriptors.others",
            scan(i).map(|scan| scan.others),
            "descriptor scan",
        ),
    ]
}

/// The facts the check `name` reads from `i`, each with what produced it; `users` is what
/// [`descriptor_users`] found.
fn facts(name: &str, i: &Inspection<'_>, users: Option<&[DescriptorUser]>) -> Vec<CheckFact> {
    let repo = i.observation.ok().map(|o| &o.repo);
    let wt = i.observation.ok().map(|o| &o.worktree);
    let facts = wt.map(|w| &w.facts);
    let walk = i.walk.ok();
    match name {
        "observed" => vec![fact("repo.gh", repo.map(|r| r.gh), "gh")],
        "not_in_use" => in_use_facts(i, users),
        "not_managed" => vec![fact(
            "worktree.managed_by",
            wt.map(|w| &w.managed_by),
            "git",
        )],
        "clean" => vec![
            fact("worktree.facts.dirty", facts.map(|f| f.dirty), "git"),
            fact(
                "worktree.facts.dirty_unknown",
                facts.map(|f| &f.dirty_unknown),
                "git",
            ),
        ],
        "no_unpushed" => vec![fact(
            "worktree.facts.unpushed",
            facts.map(|f| f.unpushed),
            "git",
        )],
        "no_open_pr" => vec![fact(
            "worktree.facts.open_pr",
            facts.map(|f| f.open_pr),
            "gh",
        )],
        "has_upstream" => vec![
            fact(
                "worktree.facts.has_upstream",
                facts.map(|f| f.has_upstream),
                "git",
            ),
            fact("worktree.upstream", wt.map(|w| &w.upstream), "git"),
        ],
        "merge_evidence" => merge_facts(i),
        "regenerable" | "no_unclassifiable" | "no_nested_repository" | "no_protected" => {
            vec![fact(
                "worktree.ignored",
                ignored_in_target(i).map(|found| {
                    found
                        .into_iter()
                        .map(|(path, class)| serde_json::json!({ "path": path, "class": class }))
                        .collect::<Vec<_>>()
                }),
                "git",
            )]
        }
        "plain_target" => vec![
            fact(
                "walk.root.kind",
                walk.map(|w| format!("{:?}", w.root.kind).to_lowercase()),
                "lstat",
            ),
            fact(
                "walk.mount",
                walk.map(|w| format!("{:?}", w.mount)),
                "stat, statx, statfs",
            ),
            fact("walk.link_in_path", walk.map(|w| &w.link_in_path), "lstat"),
            fact(
                "walk.boundaries",
                walk.map(|w| &w.boundaries),
                "stat, statx, statfs",
            ),
            fact("walk.special", special(i), "lstat"),
        ],
        "local_volume" => vec![fact(
            "walk.fs_type",
            walk.and_then(|w| w.fs_type.as_ref().ok()),
            "statfs",
        )],
        _ => Vec::new(),
    }
}

/// What the checks of `i` were evaluated from beyond their facts: what could not be read,
/// and the configuration keys used (the `[data]` lists of the repository's profiles and the
/// managed roots of `host.toml`).
#[must_use]
pub fn provenance(config: &Config, i: &Inspection<'_>) -> Provenance {
    let error = |source: &str, message: &str| ObservedError {
        source: source.to_owned(),
        message: message.to_owned(),
    };
    let mut errors = Vec::new();
    match i.observation {
        Err(err) => errors.push(error("observation", err)),
        Ok(o) => {
            errors.extend(o.errors.iter().map(|e| error("observation", &e.message)));
            if let Some(err) = &o.repo.gh_error {
                errors.push(error("gh", err));
            }
        }
    }
    if let Err(err) = i.walk {
        errors.push(error("walk", err));
    }
    if let Err(err) = &i.in_use.lock_held {
        errors.push(error("worktree in-use lock", err));
    }
    if let Err(err) = &i.in_use.reservations {
        errors.push(error("reservations", err));
    }
    if let Err(err) = &i.in_use.processes {
        errors.push(error("process table", err));
    }
    if let Some(Err(err)) = &i.in_use.descriptors {
        errors.push(error("descriptor scan", err));
    }
    let mut keys = Vec::new();
    if let Ok(o) = i.observation {
        // The profiles `plan::patterns` reads: the repository's, then the generic ones.
        let selection = config.select(|p| o.repo.profiles.contains(&p.name));
        for profile in selection.profiles {
            for (key, list) in [
                ("data.protected", &profile.protected),
                ("data.regenerable", &profile.regenerable),
            ] {
                if !list.is_empty() {
                    keys.push(ConfigKey {
                        file: profile.file.display().to_string(),
                        key: key.to_owned(),
                        value: list.clone().into(),
                    });
                }
            }
        }
    }
    if !i.managed.is_empty() {
        keys.push(ConfigKey {
            file: config.dir.join("host.toml").display().to_string(),
            key: "observe.managed".to_owned(),
            value: i
                .managed
                .iter()
                .map(|m| serde_json::json!({ "app": m.app, "root": m.root }))
                .collect(),
        });
    }
    Provenance {
        errors,
        config: keys,
    }
}

/// Whether every invariant passed.
#[must_use]
pub fn invariants_ok(checks: &[Check]) -> bool {
    checks
        .iter()
        .filter(|c| c.role == CheckRole::Invariant)
        .all(|c| c.result == CheckResult::Pass)
}

/// Owner approval is needed when a condition did not pass.
#[must_use]
pub fn approval(checks: &[Check]) -> Approval {
    if checks
        .iter()
        .any(|c| c.role == CheckRole::Condition && c.result != CheckResult::Pass)
    {
        Approval::Owner
    } else {
        Approval::None
    }
}

/// The PIDs of live processes whose working directory is `target` or inside it, this one
/// left out: a `plan` or `act` run from inside the worktree reads the target, never uses it.
/// `plan` and `act` ask it of the whole worktree for both plan kinds (section 9.2).
pub fn processes_inside(target: &Path) -> Result<Vec<u32>, String> {
    let table = ProcessTable::snapshot(Detail {
        cmd: false,
        cwd: true,
    })
    .map_err(|err| err.to_string())?;
    let own = std::process::id();
    Ok(table
        .iter()
        .filter(|p| p.pid != own && !p.zombie)
        .filter(|p| p.cwd.as_deref().is_some_and(|cwd| cwd.starts_with(target)))
        .map(|p| p.pid)
        .collect())
}

/// The descriptor scan `not_in_use` reads for a target of `kind`: a `reclaim_dir` target's on
/// Unix (section 9.2), `None` for any other.
#[must_use]
pub fn descriptor_scan(kind: PlanKind) -> Descriptors {
    #[cfg(unix)]
    {
        (kind == PlanKind::ReclaimDir).then(|| crate::os::fds::scan().map(Arc::new))
    }
    #[cfg(not(unix))]
    {
        let _ = kind;
        None
    }
}

/// The complete descriptor scan of `i`, if one was read.
fn scan<'a>(i: &Inspection<'a>) -> Option<&'a Scan> {
    i.in_use.descriptors.as_ref()?.as_deref().ok()
}

/// One use of the target the descriptor scan found.
#[derive(Debug, serde::Serialize)]
struct DescriptorUser {
    pid: u32,
    way: Way,
    /// Worktree-relative when inside the worktree.
    path: Option<PathBuf>,
}

/// What the descriptor-use decision reads of an entry of a target with an identity
/// (`dev`, `ino`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Linked {
    /// How many names the target's entries give it.
    pub(crate) names: u64,
    /// Its link count, as the entries read it.
    pub(crate) nlink: u64,
    /// Whether it is a directory.
    pub(crate) dir: bool,
}

/// The uses that `scan` found of a target at `root` whose entries `ids` holds (section 9.7),
/// each with the PID of its process: a working or root directory, executable, or descriptor
/// that is one of the entries, by `dev` and `ino`, or whose path lies at or under `root`, as
/// given or as the OS names it, through no symbolic link. A file with more links than the
/// entries have names for it does not count when the use's path is the name it was opened
/// through ([`PATH_IS_OPENED_NAME`]), outside `root`, and still names the file: removing the
/// target leaves it to that name. A use through a name inside, or whose path cannot be read or
/// no longer names the file, counts. `plan` and `act` ask it of the target before the move,
/// and the deletion of the trash entry after it. An error when the path the OS names for
/// `root` cannot be read, so nothing decides without it.
pub(crate) fn descriptor_uses<'s>(
    scan: &'s Scan,
    ids: &HashMap<(u64, u64), Linked>,
    root: &Path,
) -> Result<Vec<(u32, &'s Use)>, String> {
    let canonical =
        std::fs::canonicalize(root).map_err(|err| format!("{}: {err}", root.display()))?;
    let under = |path: &Path| path.starts_with(root) || path.starts_with(&canonical);
    // A use's path that establishes it holds the file `id` through a name outside `root`.
    let outside = |path: &Path, id: (u64, u64)| {
        PATH_IS_OPENED_NAME && path.is_absolute() && !under(path) && names_file(path, id)
    };
    let mut found = Vec::new();
    for process in &scan.processes {
        for used in &process.uses {
            let counts = match used.id.and_then(|id| Some((id, ids.get(&id)?))) {
                Some((id, linked)) => {
                    linked.dir
                        || linked.nlink <= linked.names
                        || !used.path.as_deref().is_some_and(|path| outside(path, id))
                }
                None => used.path.as_deref().is_some_and(under),
            };
            if counts {
                found.push((process.pid, used));
            }
        }
    }
    Ok(found)
}

/// The uses of the target that the descriptor scan found ([`descriptor_uses`]), each named by
/// its path relative to the worktree, or by the entry it uses. `None` without a complete scan
/// and a walk, or when the target's path as the OS names it cannot be read.
fn descriptor_users(i: &Inspection<'_>) -> Option<Vec<DescriptorUser>> {
    let scan = scan(i)?;
    let walk = i.walk.ok()?;
    let mut ids: HashMap<(u64, u64), Linked> = HashMap::new();
    let mut entries: HashMap<(u64, u64), &super::Entry> = HashMap::new();
    for entry in walk.all() {
        let id = (entry.dev, entry.ino);
        ids.entry(id)
            .and_modify(|linked| linked.names += 1)
            .or_insert(Linked {
                names: 1,
                nlink: entry.nlink,
                dir: entry.kind == EntryKind::Dir,
            });
        entries.entry(id).or_insert(entry);
    }
    let uses = descriptor_uses(scan, &ids, i.target).ok()?;
    let canonical_worktree = std::fs::canonicalize(i.worktree).ok();
    let relative = |path: &Path| {
        path.strip_prefix(i.worktree)
            .ok()
            .or_else(|| path.strip_prefix(canonical_worktree.as_deref()?).ok())
            .unwrap_or(path)
            .to_path_buf()
    };
    Some(
        uses.into_iter()
            .map(|(pid, used)| DescriptorUser {
                pid,
                way: used.way,
                path: used.path.as_deref().map(relative).or_else(|| {
                    let entry = entries.get(&used.id?)?;
                    Some(entry.rel.clone())
                }),
            })
            .collect(),
    )
}

/// Whether the path the descriptor scan reads for a use is the name the file was opened
/// through: on Linux, `/proc/<pid>/fd/<n>` names the opened link. macOS names the link any
/// process looked up last (`PROC_PIDFDVNODEPATHINFO`; on APFS a descriptor opened through
/// `out/f` read `in/f` after a `stat` of `in/f`, and `out/f` again after one of `out/f`), so
/// there a file with names outside the target counts however it was opened.
const PATH_IS_OPENED_NAME: bool = cfg!(target_os = "linux");

/// Whether `path` names the file `id` (`dev`, `ino`), not following a final symbolic link.
#[cfg(unix)]
fn names_file(path: &Path, id: (u64, u64)) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path).is_ok_and(|meta| (meta.dev(), meta.ino()) == id)
}

/// Windows reads no descriptor scan (section 9.7), so no use is found to name a file.
#[cfg(not(unix))]
fn names_file(_path: &Path, _id: (u64, u64)) -> bool {
    false
}

/// The FIFOs, sockets, and devices of a `reclaim_dir` target, worktree-relative (section 9.2);
/// `None` without a walk.
fn special<'a>(i: &Inspection<'a>) -> Option<Vec<&'a Path>> {
    let walk = i.walk.ok()?;
    if i.kind != PlanKind::ReclaimDir {
        return Some(Vec::new());
    }
    Some(
        walk.all()
            .filter(|e| e.kind.is_special())
            .map(|e| e.rel.as_path())
            .collect(),
    )
}

pub(super) fn check(
    name: &str,
    role: CheckRole,
    result: CheckResult,
    detail: impl Into<String>,
) -> Check {
    Check {
        name: name.to_owned(),
        role,
        result,
        detail: detail.into(),
        facts: Vec::new(),
    }
}

fn invariant(name: &str, result: CheckResult, detail: impl Into<String>) -> Check {
    check(name, CheckRole::Invariant, result, detail)
}

fn pass(name: &str) -> Check {
    invariant(name, CheckResult::Pass, "")
}

/// `paths` as a short list for a check detail.
fn list<P: AsRef<Path>>(paths: &[P]) -> String {
    let mut text = paths
        .iter()
        .take(DETAIL_PATHS)
        .map(|p| p.as_ref().display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > DETAIL_PATHS {
        let _ = write!(text, " and {} more", paths.len() - DETAIL_PATHS);
    }
    text
}

/// The target relative to the worktree (empty for the worktree itself).
fn target_rel(i: &Inspection<'_>) -> Option<PathBuf> {
    i.target
        .strip_prefix(i.worktree)
        .ok()
        .map(Path::to_path_buf)
}

/// An ignored entry's path relative to the worktree; observation reports absolute paths.
fn ignored_rel(worktree: &Path, path: &str) -> Option<PathBuf> {
    let path = Path::new(path);
    if path.is_absolute() {
        path.strip_prefix(worktree).ok().map(Path::to_path_buf)
    } else {
        Some(path.to_path_buf())
    }
}

/// Ignored entries of the observation at or below the target, with their class.
fn ignored_in_target(i: &Inspection<'_>) -> Option<Vec<(PathBuf, IgnoredClass)>> {
    let observation = i.observation.ok()?;
    let target = target_rel(i)?;
    Some(
        observation
            .worktree
            .ignored
            .iter()
            .filter_map(|e| ignored_rel(i.worktree, &e.path).map(|rel| (rel, e.class)))
            .filter(|(rel, _)| rel.starts_with(&target))
            .collect(),
    )
}

fn observed(i: &Inspection<'_>) -> Check {
    let observation = match i.observation {
        Ok(o) => o,
        Err(err) => return invariant("observed", CheckResult::Fail, err.to_owned()),
    };
    let wt = &observation.worktree;
    let facts = &wt.facts;
    let mut unknown = Vec::new();
    // `dirty` is unknown because status could not be read, or because only filtered paths
    // changed (`dirty_unknown`), which is no failed step: it fails `clean`, and leaves a
    // `reclaim_dir` plan, which needs status but not `dirty`, observed (section 8.2).
    if facts.dirty == Tri::Unknown && facts.dirty_unknown.is_none() {
        unknown.push(if i.kind == PlanKind::ReclaimWorktree {
            "dirty"
        } else {
            "status"
        });
    }
    // An unread `origin` leaves the profiles it would select, and their protections, unknown.
    if observation.remote.is_err() {
        unknown.push("origin");
    }
    if i.kind == PlanKind::ReclaimWorktree {
        for (name, value) in [
            ("has_upstream", facts.has_upstream),
            ("unpushed", facts.unpushed),
            ("open_pr", facts.open_pr),
        ] {
            if value == Tri::Unknown {
                unknown.push(name);
            }
        }
        if facts.merge_evidence == MergeEvidence::Unknown {
            unknown.push("merge_evidence");
        }
        if wt.head.is_none() {
            unknown.push("head");
        }
    }
    if unknown.is_empty() {
        pass("observed")
    } else {
        let mut detail = format!("unknown: {}", unknown.join(", "));
        if let Some(err) = &observation.repo.gh_error {
            let _ = write!(detail, " (gh: {err})");
        }
        invariant("observed", CheckResult::Fail, detail)
    }
}

/// `not_in_use`, with `users`, what [`descriptor_users`] found.
fn not_in_use(i: &Inspection<'_>, users: Option<&[DescriptorUser]>) -> Check {
    let mut failed = Vec::new();
    let mut unknown = Vec::new();
    match &i.in_use.lock_held {
        Ok(true) => failed.push("the in-use lock is held".to_owned()),
        Ok(false) => {}
        Err(err) => unknown.push(format!("in-use lock: {err}")),
    }
    match &i.in_use.reservations {
        Ok(in_use) if in_use.value => {
            failed.push(format!("reserved by jobs {}", list(&in_use.jobs)));
        }
        Ok(_) => {}
        Err(err) => unknown.push(format!("reservations: {err}")),
    }
    match &i.in_use.processes {
        Ok(pids) if !pids.is_empty() => {
            let pids: Vec<String> = pids.iter().map(u32::to_string).collect();
            failed.push(format!(
                "processes with their working directory inside: {}",
                list(&pids)
            ));
        }
        Ok(_) => {}
        Err(err) => unknown.push(format!("processes: {err}")),
    }
    match (&i.in_use.descriptors, users) {
        (Some(Err(err)), _) => unknown.push(format!("descriptor scan: {err}")),
        (Some(Ok(_)), None) => unknown
            .push("descriptor scan: the target was not walked or its path did not resolve".into()),
        (Some(Ok(_)), Some(users)) if !users.is_empty() => {
            let users: Vec<String> = users
                .iter()
                .map(|u| match &u.path {
                    Some(path) => format!("{} ({}, {})", u.pid, way(u.way), path.display()),
                    None => format!("{} ({})", u.pid, way(u.way)),
                })
                .collect();
            failed.push(format!(
                "the user's processes using the target: {}",
                list(&users)
            ));
        }
        (None, _) | (Some(Ok(_)), Some(_)) => {}
    }
    if !failed.is_empty() {
        invariant("not_in_use", CheckResult::Fail, failed.join("; "))
    } else if !unknown.is_empty() {
        invariant("not_in_use", CheckResult::Unknown, unknown.join("; "))
    } else {
        pass("not_in_use")
    }
}

/// How a process uses a file, for a check detail.
fn way(way: Way) -> String {
    match way {
        Way::WorkingDirectory => "working directory".to_owned(),
        Way::RootDirectory => "root directory".to_owned(),
        Way::Executable => "executable".to_owned(),
        Way::Descriptor(fd) => format!("descriptor {fd}"),
    }
}

fn not_managed(i: &Inspection<'_>) -> Check {
    if let Some(managed) = i.managed.iter().find(|m| i.worktree.starts_with(&m.root)) {
        return invariant(
            "not_managed",
            CheckResult::Fail,
            format!(
                "under the root of {} ({})",
                managed.app,
                managed.root.display()
            ),
        );
    }
    match i.observation {
        Ok(o) => match &o.worktree.managed_by {
            Some(app) => invariant(
                "not_managed",
                CheckResult::Fail,
                format!("managed by {app}"),
            ),
            None => pass("not_managed"),
        },
        Err(_) => invariant("not_managed", CheckResult::Unknown, "not observed"),
    }
}

/// `clean`: `dirty` is `no`. Unknown because only filtered paths changed, it names them
/// with the reason (section 8.2).
fn clean(i: &Inspection<'_>) -> Check {
    let Ok(observation) = i.observation else {
        return invariant("clean", CheckResult::Unknown, "not observed");
    };
    let facts = &observation.worktree.facts;
    match (facts.dirty, &facts.dirty_unknown) {
        (Tri::No, _) => pass("clean"),
        (Tri::Yes, _) => invariant("clean", CheckResult::Fail, "uncommitted changes"),
        (Tri::Unknown, Some(unknown)) => invariant(
            "clean",
            CheckResult::Unknown,
            format!(
                "filtered_paths: status cannot tell whether these changed, because their filter does not run: {}",
                list(&unknown.paths)
            ),
        ),
        (Tri::Unknown, None) => invariant("clean", CheckResult::Unknown, "unknown"),
    }
}

fn tri(name: &str, fact: Option<Tri>, good: Tri, bad: &str) -> Check {
    match fact {
        None => invariant(name, CheckResult::Unknown, "not observed"),
        Some(Tri::Unknown) => invariant(name, CheckResult::Unknown, "unknown"),
        Some(value) if value == good => pass(name),
        Some(_) => invariant(name, CheckResult::Fail, bad),
    }
}

fn regenerable(i: &Inspection<'_>) -> Check {
    let Some(rel) = target_rel(i).filter(|rel| !rel.as_os_str().is_empty()) else {
        return invariant(
            "regenerable",
            CheckResult::Fail,
            "the target is the worktree root",
        );
    };
    if rel.starts_with(".git") {
        return invariant("regenerable", CheckResult::Fail, "the target is git data");
    }
    let Ok(observation) = i.observation else {
        return invariant("regenerable", CheckResult::Unknown, "not observed");
    };
    // A pattern match, whatever the entry's class: what is inside is for `no_protected`,
    // `no_unclassifiable`, and `no_nested_repository` (section 9.2).
    let listed = observation
        .worktree
        .ignored
        .iter()
        .any(|e| ignored_rel(i.worktree, &e.path).is_some_and(|entry| rel.starts_with(&entry)));
    if !listed {
        invariant("regenerable", CheckResult::Fail, "not an ignored entry")
    } else if !i.patterns.covers(DataList::Regenerable, &rel) {
        invariant(
            "regenerable",
            CheckResult::Fail,
            "no regenerable pattern matches it",
        )
    } else {
        pass("regenerable")
    }
}

fn plain_target(i: &Inspection<'_>) -> Check {
    let walk = match i.walk {
        Ok(walk) => walk,
        Err(err) => return invariant("plain_target", CheckResult::Unknown, err.to_owned()),
    };
    if let Some(link) = &walk.link_in_path {
        return invariant(
            "plain_target",
            CheckResult::Fail,
            format!("{} is a symbolic link", link.display()),
        );
    }
    if !walk.boundaries.is_empty() {
        return invariant(
            "plain_target",
            CheckResult::Fail,
            format!(
                "on another device or mount, so not walked: {}",
                list(&walk.boundaries)
            ),
        );
    }
    match (walk.root.kind, &walk.mount) {
        (super::EntryKind::Symlink, _) => {
            invariant("plain_target", CheckResult::Fail, "a symbolic link")
        }
        (_, Mount::Boundary) => invariant(
            "plain_target",
            CheckResult::Fail,
            "a mount point, on another device than its parent, or replaced while it was read",
        ),
        (_, Mount::Unknown(err)) => invariant(
            "plain_target",
            CheckResult::Unknown,
            format!("the mount cannot be read: {err}"),
        ),
        (_, Mount::Same) => match special(i) {
            Some(found) if !found.is_empty() => invariant(
                "plain_target",
                CheckResult::Fail,
                format!("FIFOs, sockets, or devices inside: {}", list(&found)),
            ),
            _ => pass("plain_target"),
        },
    }
}

/// The file systems whose users the descriptor scan sees: local ones (section 9.2).
const LOCAL_VOLUMES: &[&str] = if cfg!(target_os = "macos") {
    &["apfs", "hfs"]
} else {
    &["ext2/ext3/ext4", "xfs", "btrfs", "tmpfs"]
};

/// `local_volume`: the target's file system is a local one (section 9.2).
fn local_volume(i: &Inspection<'_>) -> Check {
    let walk = match i.walk {
        Ok(walk) => walk,
        Err(err) => return invariant("local_volume", CheckResult::Unknown, err.to_owned()),
    };
    match &walk.fs_type {
        Ok(ty) if LOCAL_VOLUMES.contains(&ty.as_str()) => pass("local_volume"),
        Ok(ty) => invariant(
            "local_volume",
            CheckResult::Fail,
            format!("on a {ty} file system, whose users the descriptor scan cannot see"),
        ),
        Err(err) => invariant(
            "local_volume",
            CheckResult::Unknown,
            format!("the file system type cannot be read: {err}"),
        ),
    }
}

fn no_protected(i: &Inspection<'_>) -> Check {
    let Some(target) = target_rel(i) else {
        return invariant(
            "no_protected",
            CheckResult::Unknown,
            "the target is outside the worktree",
        );
    };
    if !target.as_os_str().is_empty() && i.patterns.covers(DataList::Protected, &target) {
        return invariant(
            "no_protected",
            CheckResult::Fail,
            "the target is protected data, or inside it",
        );
    }
    let walk = match i.walk {
        Ok(walk) => walk,
        Err(err) => return invariant("no_protected", CheckResult::Unknown, err.to_owned()),
    };
    // The outermost protected entries: what is inside them is covered too.
    let protected = |rel: &Path| i.patterns.covers(DataList::Protected, rel);
    let mut found: Vec<PathBuf> = walk
        .all()
        .filter(|e| protected(&e.rel) && !e.rel.parent().is_some_and(protected))
        .map(|e| e.rel.clone())
        .collect();
    if let Some(ignored) = ignored_in_target(i) {
        add_class(&mut found, ignored, IgnoredClass::Protected);
    }
    if found.is_empty() {
        pass("no_protected")
    } else {
        invariant(
            "no_protected",
            CheckResult::Fail,
            format!("protected data: {}", list(&found)),
        )
    }
}

/// Adds the ignored entries of `class` not yet in `found`.
fn add_class(found: &mut Vec<PathBuf>, ignored: Vec<(PathBuf, IgnoredClass)>, class: IgnoredClass) {
    for (rel, entry_class) in ignored {
        if entry_class == class && !found.contains(&rel) {
            found.push(rel);
        }
    }
}

fn no_unclassifiable(i: &Inspection<'_>) -> Check {
    let Some(ignored) = ignored_in_target(i) else {
        return invariant("no_unclassifiable", CheckResult::Unknown, "not observed");
    };
    let found: Vec<PathBuf> = ignored
        .into_iter()
        .filter(|(rel, class)| {
            *class == IgnoredClass::Unclassifiable
                || (*class != IgnoredClass::Protected
                    && *class != IgnoredClass::NestedRepository
                    && !i.patterns.covers(DataList::Regenerable, rel))
        })
        .map(|(rel, _)| rel)
        .collect();
    if found.is_empty() {
        pass("no_unclassifiable")
    } else {
        invariant(
            "no_unclassifiable",
            CheckResult::Fail,
            format!("unclassifiable ignored data: {}", list(&found)),
        )
    }
}

fn no_nested_repository(i: &Inspection<'_>) -> Check {
    let walk = match i.walk {
        Ok(walk) => walk,
        Err(err) => {
            return invariant("no_nested_repository", CheckResult::Unknown, err.to_owned());
        }
    };
    let own = (i.kind == PlanKind::ReclaimWorktree).then(|| PathBuf::from(".git"));
    let mut found: Vec<PathBuf> = walk
        .all()
        .filter(|e| e.rel.file_name().is_some_and(|n| n == ".git"))
        .filter(|e| Some(&e.rel) != own.as_ref())
        .filter_map(|e| e.rel.parent().map(Path::to_path_buf))
        .collect();
    // A git directory itself: `HEAD`, `objects/`, and `refs/`, as git recognizes a bare
    // repository (section 9.2).
    let kinds: std::collections::HashMap<&Path, EntryKind> =
        walk.all().map(|e| (e.rel.as_path(), e.kind)).collect();
    let has = |dir: &Path, name: &str, kind: EntryKind| {
        kinds.get(dir.join(name).as_path()) == Some(&kind)
    };
    for entry in walk.all().filter(|e| e.kind == EntryKind::Dir) {
        let dir = entry.rel.as_path();
        if Some(dir) != own.as_deref()
            && has(dir, "HEAD", EntryKind::File)
            && has(dir, "objects", EntryKind::Dir)
            && has(dir, "refs", EntryKind::Dir)
            && !found.iter().any(|f| f == dir)
        {
            found.push(dir.to_path_buf());
        }
    }
    if let Some(ignored) = ignored_in_target(i) {
        add_class(&mut found, ignored, IgnoredClass::NestedRepository);
    }
    if found.is_empty() {
        pass("no_nested_repository")
    } else {
        invariant(
            "no_nested_repository",
            CheckResult::Fail,
            format!("repositories inside: {}", list(&found)),
        )
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::MetadataExt as _;
    use std::path::Path;
    use std::sync::Arc;

    use super::{DataLists, InUseFacts, Inspection, evaluate};
    use crate::contract::{CheckResult, InUse, PlanKind};
    use crate::os::fds::{Process, Scan, Use, Way};

    /// `not_in_use` and its facts for the target `t` of `wt` under `scan`.
    fn not_in_use(wt: &Path, scan: Option<Result<Scan, String>>) -> crate::contract::Check {
        not_in_use_with(wt, Vec::new(), scan)
    }

    /// [`not_in_use`] with `working`, the processes working in `wt`.
    fn not_in_use_with(
        wt: &Path,
        working: Vec<u32>,
        scan: Option<Result<Scan, String>>,
    ) -> crate::contract::Check {
        let target = wt.join("t");
        let walk = super::super::walk(wt, &target).unwrap();
        let in_use = InUseFacts {
            lock_held: Ok(false),
            reservations: Ok(InUse::default()),
            processes: Ok(working),
            descriptors: scan.map(|scan| scan.map(Arc::new)),
        };
        let checks = evaluate(&Inspection {
            kind: PlanKind::ReclaimDir,
            worktree: wt,
            target: &target,
            observation: Err("not observed"),
            walk: Ok(&walk),
            in_use: &in_use,
            patterns: &DataLists::default(),
            managed: &[],
            squash_merged: false,
        });
        checks.into_iter().find(|c| c.name == "not_in_use").unwrap()
    }

    fn holding(pid: u32, way: Way, id: Option<(u64, u64)>, path: Option<&Path>) -> Scan {
        Scan {
            processes: vec![Process {
                pid,
                uses: vec![Use {
                    way,
                    id,
                    path: path.map(Path::to_path_buf),
                }],
            }],
            ..Scan::default()
        }
    }

    /// A scan that is not complete leaves `not_in_use` unknown, never passing; the processes
    /// the scan could not read do not count, and the first three are listed, the rest counted;
    /// a use of an entry of the target, by `dev` and `ino` or by a path under it, fails the
    /// check. A file with a name outside the target does not count on Linux when the use's
    /// path is that name, and counts when the path is a name inside, cannot be read, or no
    /// longer names the file, and on macOS, whose path is the link looked up last.
    #[test]
    fn the_descriptor_scan_decides_only_when_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = dunce::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir_all(wt.join("t")).unwrap();
        std::fs::write(wt.join("outside"), b"x").unwrap();
        std::fs::hard_link(wt.join("outside"), wt.join("t/linked")).unwrap();
        std::fs::write(wt.join("t/own"), b"x").unwrap();
        let id = |rel: &str| {
            let meta = std::fs::symlink_metadata(wt.join(rel)).unwrap();
            Some((meta.dev(), meta.ino()))
        };

        assert_eq!(not_in_use(&wt, None).result, CheckResult::Pass);
        let incomplete = not_in_use(&wt, Some(Err("the process table does not list".into())));
        assert_eq!(incomplete.result, CheckResult::Unknown, "{incomplete:#?}");
        assert!(
            incomplete.detail.contains("descriptor scan"),
            "{incomplete:#?}"
        );

        let unscanned = Scan {
            unscanned: (42..47)
                .map(|pid| (pid, "PROC_PIDVNODEPATHINFO: EPERM".into()))
                .collect(),
            others: 3,
            ..Scan::default()
        };
        let check = not_in_use(&wt, Some(Ok(unscanned)));
        assert_eq!(check.result, CheckResult::Pass, "{check:#?}");
        let fact = check
            .facts
            .iter()
            .find(|f| f.name == "in_use.descriptors.unscanned")
            .unwrap();
        assert_eq!(fact.value[0]["pid"], 42, "{fact:#?}");
        assert_eq!(
            (fact.value.as_array().map(Vec::len), fact.omitted),
            (Some(3), Some(2)),
            "{fact:#?}"
        );

        let held = not_in_use(
            &wt,
            Some(Ok(holding(7, Way::Descriptor(3), id("t/own"), None))),
        );
        assert_eq!(held.result, CheckResult::Fail, "{held:#?}");
        assert!(held.detail.contains("7 (descriptor 3, t/own)"), "{held:#?}");
        let rooted = not_in_use(&wt, Some(Ok(holding(8, Way::RootDirectory, id("t"), None))));
        assert!(
            rooted.detail.contains("8 (root directory, t)"),
            "{rooted:#?}"
        );
        let by_path = holding(9, Way::Executable, None, Some(&wt.join("t/new/bin")));
        let by_path = not_in_use(&wt, Some(Ok(by_path)));
        assert!(
            by_path.detail.contains("9 (executable, t/new/bin)"),
            "{by_path:#?}"
        );

        let linked = |path: Option<&Path>| {
            not_in_use(
                &wt,
                Some(Ok(holding(10, Way::Descriptor(0), id("t/linked"), path))),
            )
            .result
        };
        let through_outside = if cfg!(target_os = "linux") {
            CheckResult::Pass
        } else {
            CheckResult::Fail
        };
        assert_eq!(linked(Some(&wt.join("outside"))), through_outside);
        for path in [Some(wt.join("t/linked")), None, Some(wt.join("removed"))] {
            assert_eq!(linked(path.as_deref()), CheckResult::Fail, "{path:?}");
        }
        let elsewhere = holding(11, Way::WorkingDirectory, id("."), Some(&wt));
        assert_eq!(
            not_in_use(&wt, Some(Ok(elsewhere))).result,
            CheckResult::Pass
        );
    }

    /// Every process working in the worktree and every use of the target fails the check,
    /// while each list's fact names the first three and counts the rest, as every plan of the
    /// worktree repeats it.
    #[test]
    fn process_facts_name_three_and_count_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = dunce::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir_all(wt.join("t")).unwrap();
        std::fs::write(wt.join("t/own"), b"x").unwrap();
        let meta = std::fs::symlink_metadata(wt.join("t/own")).unwrap();
        let holders = Scan {
            processes: (20..25)
                .map(|pid| Process {
                    pid,
                    uses: vec![Use {
                        way: Way::Descriptor(3),
                        id: Some((meta.dev(), meta.ino())),
                        path: None,
                    }],
                })
                .collect(),
            ..Scan::default()
        };
        let check = not_in_use_with(&wt, (10..15).collect(), Some(Ok(holders)));
        assert_eq!(check.result, CheckResult::Fail, "{check:#?}");
        for pid in (10..15).chain(20..25) {
            assert!(check.detail.contains(&pid.to_string()), "{pid}: {check:#?}");
        }
        for name in ["in_use.processes", "in_use.descriptors.users"] {
            let fact = check.facts.iter().find(|f| f.name == name).unwrap();
            assert_eq!(
                (fact.value.as_array().map(Vec::len), fact.omitted),
                (Some(3), Some(2)),
                "{fact:#?}"
            );
        }
    }
}
