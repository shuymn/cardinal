//! The checks of `docs/architecture.md` section 9.2, evaluated the same way by `plan` and by
//! `act`. Every check is computed from facts gathered by the caller: an observation, a walk
//! (or `act`'s snapshot) of the target, and the in-use facts. Missing facts give `unknown`,
//! which is never treated as safe.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::config::{DataList, Managed, Selection, data_pattern_covers};
use crate::contract::{
    Approval, Check, CheckResult, CheckRole, IgnoredClass, InUse, MergeEvidence, PlanKind, Tri,
};
use crate::observe::WorktreeObservation;
use crate::os::{Detail, ProcessTable};

use super::walk::{Mount, Walk};

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

/// Every check for the plan kind, in the order of section 9.2.
#[must_use]
pub fn evaluate(i: &Inspection<'_>) -> Vec<Check> {
    let worktree = i.kind == PlanKind::ReclaimWorktree;
    let mut checks = vec![observed(i), not_in_use(i), not_managed(i)];
    if worktree {
        let facts = i.observation.ok().map(|o| &o.worktree.facts);
        checks.push(tri(
            "clean",
            facts.map(|f| f.dirty),
            Tri::No,
            "uncommitted changes",
        ));
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
        checks.push(merge_evidence(i));
    }
    checks
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

/// The PIDs of live processes whose working directory is `target` or inside it.
pub fn processes_inside(target: &Path) -> Result<Vec<u32>, String> {
    let table = ProcessTable::snapshot(Detail {
        cmd: false,
        cwd: true,
    })
    .map_err(|err| err.to_string())?;
    Ok(table
        .iter()
        .filter(|p| !p.zombie && p.cwd.as_deref().is_some_and(|cwd| cwd.starts_with(target)))
        .map(|p| p.pid)
        .collect())
}

fn check(name: &str, role: CheckRole, result: CheckResult, detail: impl Into<String>) -> Check {
    Check {
        name: name.to_owned(),
        role,
        result,
        detail: detail.into(),
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
    if facts.dirty == Tri::Unknown {
        unknown.push("dirty");
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
            failed.push(format!("reserved by jobs {}", in_use.jobs.join(", ")));
        }
        Ok(_) => {}
        Err(err) => unknown.push(format!("reservations: {err}")),
    }
    match &i.in_use.processes {
        Ok(pids) if !pids.is_empty() => {
            let pids: Vec<String> = pids.iter().map(u32::to_string).collect();
            failed.push(format!(
                "processes with their working directory inside: {}",
                pids.join(", ")
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
    let listed = observation.worktree.ignored.iter().find_map(|e| {
        let entry = ignored_rel(i.worktree, &e.path)?;
        rel.starts_with(&entry).then_some((entry, e.class))
    });
    match listed {
        None => invariant("regenerable", CheckResult::Fail, "not an ignored entry"),
        Some((entry, class)) if class != IgnoredClass::Regenerable => invariant(
            "regenerable",
            CheckResult::Fail,
            format!(
                "the ignored entry {} is {}",
                entry.display(),
                class_name(class)
            ),
        ),
        Some(_) if !i.patterns.covers(DataList::Regenerable, &rel) => invariant(
            "regenerable",
            CheckResult::Fail,
            "no regenerable pattern matches it",
        ),
        Some(_) => pass("regenerable"),
    }
}

fn class_name(class: IgnoredClass) -> &'static str {
    match class {
        IgnoredClass::Regenerable => "regenerable",
        IgnoredClass::Protected => "protected",
        IgnoredClass::Unclassifiable => "unclassifiable",
        IgnoredClass::NestedRepository => "a nested repository",
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
    match (walk.root.kind, &walk.mount) {
        (super::EntryKind::Symlink, _) => {
            invariant("plain_target", CheckResult::Fail, "a symbolic link")
        }
        (_, Mount::Boundary) => invariant(
            "plain_target",
            CheckResult::Fail,
            "a mount point, or on another device than its parent",
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

fn merge_evidence(i: &Inspection<'_>) -> Check {
    let condition =
        |result, detail: &str| check("merge_evidence", CheckRole::Condition, result, detail);
    match i.observation.map(|o| o.worktree.facts.merge_evidence) {
        Err(_) => condition(CheckResult::Unknown, "not observed"),
        Ok(MergeEvidence::BaseAncestor) => {
            condition(CheckResult::Pass, "HEAD is an ancestor of the base")
        }
        Ok(MergeEvidence::MergedPr) => condition(
            CheckResult::Pass,
            "HEAD is the head of a merged pull request",
        ),
        Ok(MergeEvidence::None) => condition(CheckResult::Fail, "no evidence that HEAD was merged"),
        Ok(MergeEvidence::Unknown) => condition(CheckResult::Unknown, "unknown"),
    }
}
