//! Inbox items raised from a pass's observation and disk states (`docs/architecture.md`
//! section 10.1): `reclaimable`, `merge_undecided`, `closed_pr_dirty`, `managed_merged`, and
//! `unclassifiable_data` from the full observation, `disk_low` and `disk_critical` from the
//! disk states with their disk reports. Each raises and ends occurrences through the inbox.
//!
//! A raiser ends only the occurrences it read before it started observing (section 10.1):
//! the pass takes [`snapshot`] before step 4 and hands it to both raisers, so an item changed
//! while the pass read its volumes or observed, an escalation included, is left alone until
//! the next pass.
//!
//! An item about a worktree ends only on what the observation shows: a fact that is
//! `unknown`, a HEAD that was not read, or ignored entries that were not listed end nothing.
//! A worktree the observation does not list is gone only when its path no longer exists or is
//! no longer a worktree of the item's repository: a listing can miss a worktree without
//! failing (`git worktree list` skips what it cannot read), so a path whose `.git` entry still
//! leads into the repository's common dir, or that cannot be read, is not gone.

use std::collections::BTreeMap;
use std::path::Path;

use crate::config::Config;
use crate::contract::{
    Approval, CheckResult, DiskLevel, DiskReport, DiskState, Error, IgnoredClass, InboxChanges,
    InboxItem, InboxKind, InboxSubject, Inventory, MergeEvidence, Plan, PlanKind, Repo, Tri,
    Worktree,
};
use crate::inbox::{self, NewItem, Occurrence};
use crate::state::{self, StateDir};

use super::Observation;

/// How many unclassifiable paths an item's evidence lists.
const EVIDENCE_PATHS: usize = 50;

/// The items as they were before the pass observed: the only ones its raisers may end.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Before(BTreeMap<String, InboxItem>);

/// Reads the items' latest occurrences for [`worktree_items`] and [`disk_items`]; the pass
/// takes it before step 4. Without it, a pass passes [`Before::default`], which ends nothing.
pub fn snapshot(state: &StateDir) -> Result<Before, Error> {
    inbox::latest(state).map(Before)
}

/// Raises and ends the worktree items of section 10.1 from step 5's observation and plans,
/// ending only occurrences in `before`, with the keys raised anew and ended.
pub fn worktree_items(
    config: &Config,
    state: &StateDir,
    before: &Before,
    observation: &Observation,
) -> Result<InboxChanges, Error> {
    inbox::apply_recorded(config, state, &worktree_changes(&before.0, observation))
}

/// Raises, updates, and ends the disk items of every watched volume from its state, each
/// with the volume's disk report from `disk-reclaim` (step 6), with the keys raised anew and
/// ended. A volume whose state is `unknown` changes nothing; one under pressure without a
/// report this pass (its reading or `disk-reclaim` failed) keeps its open items as they were.
/// Only occurrences in `before` end.
pub fn disk_items(
    config: &Config,
    state: &StateDir,
    before: &Before,
    disks: &[DiskState],
    reports: &[DiskReport],
) -> Result<InboxChanges, Error> {
    inbox::apply_recorded(config, state, &disk_changes(&before.0, disks, reports))
}

/// The worktrees of an observation by path, each with its repository.
type Listed<'a> = BTreeMap<&'a str, (&'a Repo, &'a Worktree)>;

fn worktree_changes(
    before: &BTreeMap<String, InboxItem>,
    observation: &Observation,
) -> Vec<Occurrence> {
    let inventory = &observation.inventory;
    let found_ms = inventory.observed_ms;
    let listed: Listed<'_> = inventory
        .repos
        .iter()
        .flat_map(|repo| {
            repo.worktrees
                .iter()
                .map(move |w| (w.path.as_str(), (repo, w)))
        })
        .collect();
    let raise = |item: NewItem| Occurrence::Raise {
        item,
        since_ms: found_ms,
    };
    let mut changes: Vec<Occurrence> = observation
        .plans
        .iter()
        .filter(|plan| plan.kind == PlanKind::ReclaimWorktree)
        .filter_map(|plan| {
            let (_, worktree) = listed.get(plan.target.worktree.as_str())?;
            plan_item(plan, worktree, observation.checks_ms, found_ms)
        })
        .map(raise)
        .collect();
    for (repo, worktree) in listed.values() {
        changes.extend(fact_items(inventory, repo, worktree).into_iter().map(raise));
    }
    for (condition, item) in before {
        if item.ended_ms.is_some() {
            continue;
        }
        if let Some(note) = worktree_cleared(condition, item, &listed) {
            changes.push(Occurrence::End {
                key: condition.clone(),
                before: Some(Box::new(item.clone())),
                ended_ms: found_ms,
                note,
            });
        }
    }
    changes
}

/// `reclaimable` for a whole-worktree plan that passes every check without approval, else
/// `merge_undecided` for one whose invariants pass and whose `merge_evidence` fails.
fn plan_item(plan: &Plan, worktree: &Worktree, checks_ms: u64, found_ms: u64) -> Option<NewItem> {
    let head = plan.target.head.as_deref()?;
    let every_check_passes = !plan.checks.is_empty()
        && plan
            .checks
            .iter()
            .all(|check| check.result == CheckResult::Pass);
    let merge_evidence_fails = plan
        .checks
        .iter()
        .any(|check| check.name == "merge_evidence" && check.result == CheckResult::Fail);
    let mut evidence = serde_json::json!({
        "observed_ms": found_ms,
        "plan_id": plan.id,
        "repo": plan.target.repo,
        "checks_ms": checks_ms,
        "checks": plan.checks,
    });
    let (kind, prefix) =
        if plan.invariants_ok && plan.approval == Approval::None && every_check_passes {
            (InboxKind::Reclaimable, "reclaimable")
        } else if plan.invariants_ok && merge_evidence_fails {
            let facts = &worktree.facts;
            evidence["merge_evidence"] = serde_json::json!(facts.merge_evidence);
            evidence["content_in_base"] = serde_json::json!(facts.content_in_base);
            evidence["content_diff"] = serde_json::json!(facts.content_diff);
            evidence["closed_pr"] = serde_json::json!(facts.closed_pr);
            (InboxKind::MergeUndecided, "merge_undecided")
        } else {
            return None;
        };
    Some(NewItem {
        key: format!("{prefix}:{}@{head}", plan.target.worktree),
        kind,
        subject: subject(
            &plan.target.worktree,
            plan.target.head.as_ref(),
            plan.target.branch.as_ref(),
        ),
        evidence: object(evidence),
    })
}

/// `closed_pr_dirty`, `managed_merged`, and `unclassifiable_data` from a worktree's facts and
/// ignored entries.
fn fact_items(inventory: &Inventory, repo: &Repo, worktree: &Worktree) -> Vec<NewItem> {
    let facts = &worktree.facts;
    let path = worktree.path.as_str();
    let found_ms = inventory.observed_ms;
    let about = || subject(path, worktree.head.as_ref(), worktree.branch.as_ref());
    let mut items = Vec::new();
    if let (Tri::Yes, Some(pr)) = (facts.dirty, &facts.closed_pr) {
        items.push(NewItem {
            key: format!("closed_pr_dirty:{path}#{}", pr.number),
            kind: InboxKind::ClosedPrDirty,
            subject: about(),
            evidence: object(serde_json::json!({
                "observed_ms": found_ms,
                "repo": repo.common_dir,
                "closed_pr": pr,
                "facts": facts,
            })),
        });
    }
    if let (Some(app), Some(head)) = (&worktree.managed_by, &worktree.head)
        && !worktree.main
        && has_merge_evidence(facts.merge_evidence)
        && facts.dirty == Tri::No
        && !facts.in_use.value
        && !unknown(worktree, "not_in_use")
    {
        items.push(NewItem {
            key: format!("managed_merged:{path}@{head}"),
            kind: InboxKind::ManagedMerged,
            subject: about(),
            evidence: object(serde_json::json!({
                "observed_ms": found_ms,
                "repo": repo.common_dir,
                "managed_by": app,
                "facts": facts,
            })),
        });
    }
    if let Some((condition, mut paths)) = unclassifiable(worktree) {
        let entries = paths.len();
        paths.truncate(EVIDENCE_PATHS);
        let scope = format!("worktree:{path}");
        let errors: Vec<_> = inventory
            .errors
            .iter()
            .filter(|err| err.scope == scope)
            .collect();
        items.push(NewItem {
            key: condition,
            kind: InboxKind::UnclassifiableData,
            subject: about(),
            evidence: object(serde_json::json!({
                "observed_ms": found_ms,
                "repo": repo.common_dir,
                "entries": entries,
                "paths": paths
                    .iter()
                    .map(|entry| serde_json::json!({
                        "path": entry.path,
                        "allocated_bytes": entry.allocated_bytes,
                    }))
                    .collect::<Vec<_>>(),
                "errors": errors,
            })),
        });
    }
    items
}

/// The `unclassifiable_data` condition of a worktree whose ignored entries were listed, with
/// its unclassifiable entries in path order; `None` when it has none or they were not listed.
/// `<set>` is the FNV-1a of the sorted paths, each followed by a NUL.
fn unclassifiable(worktree: &Worktree) -> Option<(String, Vec<&crate::contract::IgnoredEntry>)> {
    if unknown(worktree, "no_unclassifiable") {
        return None;
    }
    let mut entries: Vec<_> = worktree
        .ignored
        .iter()
        .filter(|entry| entry.class == IgnoredClass::Unclassifiable)
        .collect();
    if entries.is_empty() {
        return None;
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let mut bytes = Vec::new();
    for entry in &entries {
        bytes.extend_from_slice(entry.path.as_bytes());
        bytes.push(0);
    }
    let set = state::key(&bytes);
    Some((format!("unclassifiable:{}:{set}", worktree.path), entries))
}

/// Why the worktree item `item`, the latest occurrence of `condition`, ended, or `None` while
/// its condition lasts or the observation cannot tell.
fn worktree_cleared(condition: &str, item: &InboxItem, listed: &Listed<'_>) -> Option<String> {
    if !matches!(
        item.kind,
        InboxKind::Reclaimable
            | InboxKind::MergeUndecided
            | InboxKind::ClosedPrDirty
            | InboxKind::ManagedMerged
            | InboxKind::UnclassifiableData
    ) {
        return None;
    }
    let path = item.subject.worktree.as_deref()?;
    let Some((_, worktree)) = listed.get(path) else {
        let repo = item.evidence.get("repo").and_then(|repo| repo.as_str());
        return gone(Path::new(path), repo.map(Path::new));
    };
    let facts = &worktree.facts;
    match item.kind {
        InboxKind::Reclaimable | InboxKind::ManagedMerged => head_changed(item, worktree),
        InboxKind::MergeUndecided => head_changed(item, worktree).or_else(|| {
            has_merge_evidence(facts.merge_evidence).then(|| {
                format!(
                    "merge evidence appeared: {}",
                    serde_json::json!(facts.merge_evidence)
                        .as_str()
                        .unwrap_or_default()
                )
            })
        }),
        InboxKind::ClosedPrDirty => {
            (facts.dirty == Tri::No).then(|| "the worktree is clean".to_owned())
        }
        _ => {
            if unknown(worktree, "no_unclassifiable") {
                return None;
            }
            match unclassifiable(worktree) {
                None => Some("no unclassifiable entries are left".to_owned()),
                Some((now, _)) if now != condition => Some(format!("superseded by {now}")),
                Some(_) => None,
            }
        }
    }
}

/// Why the worktree at `path`, which the observation does not list, is gone for an item raised
/// in the repository whose common dir is `repo`; `None` while it may still be one of that
/// repository's worktrees.
fn gone(path: &Path, repo: Option<&Path>) -> Option<String> {
    match std::fs::symlink_metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Some("the worktree is gone".to_owned());
        }
        Err(_) => return None,
        Ok(_) => {}
    }
    (linked(path, repo?) == Some(false))
        .then(|| "the path is no longer a worktree of its repository".to_owned())
}

/// Whether the `.git` entry of `path` leads into the common dir `repo`: a `.git` directory or
/// a `gitdir:` file whose git dir, resolved, is the common dir or names it in its `commondir`
/// file. `None` when that cannot be read; `Some(false)` when there is no `.git` entry, the git
/// dir it names no longer exists, or it leads elsewhere.
fn linked(path: &Path, repo: &Path) -> Option<bool> {
    let dot_git = path.join(".git");
    let git_dir = match std::fs::metadata(&dot_git) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Some(false),
        Err(_) => return None,
        Ok(meta) if meta.is_dir() => dot_git,
        Ok(_) => {
            let text = std::fs::read_to_string(&dot_git).ok()?;
            path.join(text.strip_prefix("gitdir:")?.trim())
        }
    };
    // `Some(None)`: nothing is there.
    let resolve = |dir: &Path| match std::fs::canonicalize(dir) {
        Ok(dir) => Some(Some(dir)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(None),
        Err(_) => None,
    };
    let Some(git_dir) = resolve(&git_dir)? else {
        return Some(false);
    };
    // A linked worktree's git dir names its common dir; without `commondir` it is one. Git
    // strips only the line end, so a path may start with a space.
    let common = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(rel) => resolve(&git_dir.join(rel.trim_end_matches(['\n', '\r'])))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(git_dir),
        Err(_) => return None,
    };
    match (common, resolve(repo)?) {
        (Some(common), Some(repo)) => Some(common == repo),
        _ => Some(false),
    }
}

/// `Some` when the observation read a HEAD other than the one the item was raised for.
fn head_changed(item: &InboxItem, worktree: &Worktree) -> Option<String> {
    let head = worktree.head.as_ref()?;
    (item.subject.head.as_ref() != Some(&Some(head.clone())))
        .then(|| format!("HEAD changed to {head}"))
}

fn has_merge_evidence(evidence: MergeEvidence) -> bool {
    matches!(
        evidence,
        MergeEvidence::BaseAncestor | MergeEvidence::MergedPr
    )
}

/// Whether observation could not evaluate the whole-worktree check `check`.
fn unknown(worktree: &Worktree, check: &str) -> bool {
    worktree.reclaim.unknown.iter().any(|name| name == check)
}

fn subject(path: &str, head: Option<&String>, branch: Option<&String>) -> InboxSubject {
    InboxSubject {
        worktree: Some(path.to_owned()),
        head: Some(head.cloned()),
        branch: Some(branch.cloned()),
        ..InboxSubject::default()
    }
}

fn object(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    match value {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    }
}

/// `disk_low` while a volume is under pressure and `disk_critical` while it is critical,
/// raised with the volume's disk report and ended at the reading that found the volume back
/// to `ok` or out of `critical`.
fn disk_changes(
    before: &BTreeMap<String, InboxItem>,
    disks: &[DiskState],
    reports: &[DiskReport],
) -> Vec<Occurrence> {
    let mut changes = Vec::new();
    for disk in disks {
        let (low, critical) = match disk.state {
            DiskLevel::Unknown => continue,
            DiskLevel::Ok => (false, false),
            DiskLevel::Warning => (true, false),
            DiskLevel::Critical => (true, true),
        };
        let report = reports.iter().find(|report| report.volume == disk.volume);
        for (kind, prefix, present, note) in [
            (
                InboxKind::DiskLow,
                "disk_low",
                low,
                "the volume is back to ok",
            ),
            (
                InboxKind::DiskCritical,
                "disk_critical",
                critical,
                "the volume left critical",
            ),
        ] {
            let condition = format!("{prefix}:{}", disk.volume);
            if present {
                let item = NewItem {
                    key: condition,
                    kind,
                    subject: InboxSubject::default(),
                    evidence: object(serde_json::json!({
                        "volume": disk.volume,
                        "mount": disk.mount,
                        "disk": disk,
                        "report": report,
                    })),
                };
                let since_ms = disk.since_ms;
                // Without a report this pass, an occurrence that has not ended keeps the one it
                // has; the store decides that under its lock.
                changes.push(match report {
                    Some(_) => Occurrence::Raise { item, since_ms },
                    None => Occurrence::RaiseUnlessActive { item, since_ms },
                });
            } else if let Some(item) = before
                .get(&condition)
                .filter(|item| item.ended_ms.is_none())
            {
                changes.push(Occurrence::End {
                    key: condition,
                    before: Some(Box::new(item.clone())),
                    ended_ms: disk.since_ms,
                    note: note.to_owned(),
                });
            }
        }
    }
    changes
}
