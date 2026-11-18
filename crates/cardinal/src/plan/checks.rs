//! The checks of `docs/architecture.md` section 9.2, evaluated the same way by `plan` and by
//! `act`. Every check is computed from facts gathered by the caller: an observation, a walk
//! (or `act`'s snapshot) of the target, and the in-use facts. Missing facts give `unknown`,
//! which is never treated as safe. Each check carries the facts it read, and [`provenance`]
//! names the observation errors and configuration keys behind them (section 10.2).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::config::{Config, DataList, Managed, Selection, data_pattern_covers};
use crate::contract::{
    Approval, Check, CheckFact, CheckResult, CheckRole, ConfigKey, IgnoredClass, InUse,
    MergeEvidence, ObservedError, PlanKind, Provenance, Tri,
};
use crate::observe::WorktreeObservation;
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
}

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
/// [`data_pattern_covers`].
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
        patterns
            .iter()
            .any(|pattern| data_pattern_covers(list, pattern, rel))
    }
}

/// How many paths a check detail names before it summarizes the rest.
const DETAIL_PATHS: usize = 5;

/// How many entries a list fact keeps where a command prints it bounded.
pub const FACT_ENTRIES: usize = 10;

/// Every check for the plan kind, in the order of section 9.2.
#[must_use]
pub fn evaluate(i: &Inspection<'_>) -> Vec<Check> {
    let worktree = i.kind == PlanKind::ReclaimWorktree;
    let mut checks = vec![observed(i), not_in_use(i), not_managed(i)];
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
    if worktree {
        checks.push(super::merge::merge_evidence(i));
    }
    for check in &mut checks {
        check.facts = facts(&check.name, i);
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

/// The facts the check `name` reads from `i`, each with what produced it.
fn facts(name: &str, i: &Inspection<'_>) -> Vec<CheckFact> {
    let repo = i.observation.ok().map(|o| &o.repo);
    let wt = i.observation.ok().map(|o| &o.worktree);
    let facts = wt.map(|w| &w.facts);
    let walk = i.walk.ok();
    let in_use = i.in_use;
    match name {
        "observed" => vec![fact("repo.gh", repo.map(|r| r.gh), "gh")],
        "not_in_use" => vec![
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
            fact(
                "in_use.processes",
                in_use.processes.as_ref().ok(),
                "process table",
            ),
        ],
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
        ],
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

fn not_in_use(i: &Inspection<'_>) -> Check {
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
    if !failed.is_empty() {
        invariant("not_in_use", CheckResult::Fail, failed.join("; "))
    } else if !unknown.is_empty() {
        invariant("not_in_use", CheckResult::Unknown, unknown.join("; "))
    } else {
        pass("not_in_use")
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
        (_, Mount::Same) => pass("plain_target"),
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
