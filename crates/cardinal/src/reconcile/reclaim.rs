//! `disk-reclaim` listing (`docs/architecture.md` section 10.4, C10): for each volume under
//! pressure, from the plans of step 5 and the pass's last successful reading, the eligible
//! `reclaim_dir` targets in reclamation order until they would lift the reading above the
//! recovery line, the shortfall when they would not, the entries held by a failed check or
//! by aliased files, and the worktrees observed only in part. It runs no `act` and observes
//! nothing again; each listed target gets a `planned` record with the checks and bytes it was
//! listed by.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::json;

use crate::act::EXECUTION_NOT_DESIGNED;
use crate::config::Config;
use crate::contract::{
    ActStatus, Approval, CheckResult, ConfigKey, DiskReport, Plan, PlanKind, RecordKind,
    ReportBlocked, ReportIncomplete, ReportReading, ReportTarget, ScopedError,
};
use crate::plan;
use crate::state::StateDir;

use super::Observation;
use super::disk::{self, Watched};

/// The unit `disk-reclaim` runs as.
pub const UNIT: &str = "disk-reclaim";

/// The reports of every volume under pressure with a successful reading in this pass; a
/// volume whose readings all failed is skipped (A16), and one without an observation is
/// reported in `errors`.
pub fn reports(
    config: &Config,
    state: &StateDir,
    disks: &[Watched],
    observation: Option<&Observation>,
    errors: &mut Vec<ScopedError>,
) -> Vec<DiskReport> {
    let mut reports = Vec::new();
    for volume in disks.iter().filter(|d| d.under_pressure()) {
        let Some(reading) = volume.reading else {
            continue;
        };
        let Some(observation) = observation else {
            errors.push(ScopedError {
                scope: "disk_reclaim".into(),
                message: format!(
                    "{}: no observation in this pass, so nothing is listed",
                    volume.state.mount
                ),
            });
            continue;
        };
        let report = report(volume, reading, observation);
        if let Err(message) = record_planned(config, state, observation, &report) {
            errors.push(ScopedError {
                scope: "disk_reclaim".into(),
                message,
            });
        }
        reports.push(report);
    }
    reports
}

/// The report on `volume` from `observation`'s plans and `reading`.
fn report(volume: &Watched, reading: ReportReading, observation: &Observation) -> DiskReport {
    let on_volume = plans_on(&volume.state.volume, &observation.plans);
    let mut blocked = Vec::new();
    // Eligible targets with what removing them frees, by worktree.
    let mut by_worktree: BTreeMap<&str, Vec<(&Plan, u64)>> = BTreeMap::new();
    for plan in on_volume {
        let own_file_bytes = observation.own_file_bytes.get(&plan.id).copied();
        match listed(plan, own_file_bytes) {
            Ok(exclusive) => by_worktree
                .entry(plan.target.worktree.as_str())
                .or_default()
                .push((plan, exclusive)),
            Err(Some(entry)) => blocked.push(entry),
            Err(None) => {}
        }
    }
    // Oldest worktree first (one whose activity is unknown last), then the most exclusive
    // bytes first.
    let activity = |worktree: &str| {
        observation
            .inventory
            .repos
            .iter()
            .flat_map(|r| &r.worktrees)
            .find(|w| w.path == worktree)
            .and_then(|w| w.last_activity_ms)
    };
    let mut worktrees: Vec<(&str, Vec<(&Plan, u64)>)> = by_worktree.into_iter().collect();
    worktrees.sort_by_cached_key(|(path, _)| {
        let last = activity(path);
        (last.is_none(), last, *path)
    });
    let recovery = volume.state.lines.recovery_bytes;
    let mut lifted = reading.free_bytes;
    let mut targets = Vec::new();
    'listing: for (_, mut plans) in worktrees {
        plans.sort_by(|(a, x), (b, y)| y.cmp(x).then_with(|| a.target.path.cmp(&b.target.path)));
        for (plan, exclusive) in plans {
            if lifted > recovery {
                break 'listing;
            }
            lifted = lifted.saturating_add(exclusive);
            targets.push(ReportTarget {
                plan_id: plan.id.clone(),
                worktree: plan.target.worktree.clone(),
                target: plan.target.path.clone(),
                exclusive_bytes: exclusive,
                status: None,
                freed_bytes: None,
            });
        }
    }
    blocked.sort_by(|a, b| (&a.worktree, &a.target).cmp(&(&b.worktree, &b.target)));
    DiskReport {
        volume: volume.state.volume.clone(),
        reading,
        lines: volume.state.lines,
        checks_ms: observation.checks_ms,
        targets,
        shortfall_bytes: recovery.saturating_sub(lifted),
        blocked,
        incomplete: incomplete(&volume.state.volume, observation),
    }
}

/// How `plan` enters the report: listed by the bytes removing it frees (`Ok`), counted in
/// `blocked` with its first failed check, or as `aliased` when its regular files' bytes are
/// all aliased, whatever its directories' blocks (`Err(Some)`), or left out when it holds
/// nothing (`Err(None)`). `own_file_bytes` is [`plan::Built::own_file_bytes`]. The exclusive
/// bytes include the directories' blocks, for the order and the shortfall.
fn listed(plan: &Plan, own_file_bytes: Option<u64>) -> Result<u64, Option<ReportBlocked>> {
    let allocated = plan.allocated_bytes.unwrap_or(0);
    if !(plan.invariants_ok && plan.approval == Approval::None) {
        let check = plan
            .checks
            .iter()
            .find(|c| c.result != CheckResult::Pass)
            .map_or_else(|| "approval".to_owned(), |c| c.name.clone());
        return Err(Some(blocked_entry(plan, check, allocated)));
    }
    let aliased = plan.aliased_bytes.unwrap_or(0);
    let exclusive = allocated.saturating_sub(aliased);
    let files_all_aliased = aliased > 0 && own_file_bytes == Some(0);
    if exclusive == 0 || files_all_aliased {
        return Err((allocated > 0).then(|| blocked_entry(plan, "aliased".to_owned(), allocated)));
    }
    Ok(exclusive)
}

/// The worktrees on `volume` that `observation` read only in part: those with an error of
/// their own or of their repository, each with its message. A repository's error counts for
/// each of its worktrees, on the volume that worktree is on.
fn incomplete(volume: &str, observation: &Observation) -> Vec<ReportIncomplete> {
    let inventory = &observation.inventory;
    let mut keys: BTreeMap<&str, Option<String>> = BTreeMap::new();
    let mut incomplete = Vec::new();
    for error in &inventory.errors {
        let worktrees: Vec<&str> = if let Some(path) = error.scope.strip_prefix("worktree:") {
            vec![path]
        } else if let Some(main) = error.scope.strip_prefix("repo:") {
            let repo = inventory.repos.iter().find(|r| r.main_worktree == main);
            repo.map_or_else(
                || vec![main],
                |r| r.worktrees.iter().map(|w| w.path.as_str()).collect(),
            )
        } else {
            continue;
        };
        for path in worktrees {
            let key = keys
                .entry(path)
                .or_insert_with(|| disk::key_of(Path::new(path)));
            if key.as_deref() == Some(volume) {
                incomplete.push(ReportIncomplete {
                    worktree: path.to_owned(),
                    reason: error.message.clone(),
                });
            }
        }
    }
    incomplete.sort_by(|a, b| a.worktree.cmp(&b.worktree));
    incomplete
}

/// The `reclaim_dir` plans whose target is on the volume with key `volume`: the target
/// itself, so a link is on the volume of the directory holding it, whatever it points to.
fn plans_on<'a>(volume: &str, plans: &'a [Plan]) -> Vec<&'a Plan> {
    let mut keys: BTreeMap<&str, Option<String>> = BTreeMap::new();
    plans
        .iter()
        .filter(|p| p.kind == PlanKind::ReclaimDir)
        .filter(|p| {
            keys.entry(p.target.path.as_str())
                .or_insert_with(|| disk::entry_key_of(Path::new(&p.target.path)))
                .as_deref()
                == Some(volume)
        })
        .collect()
}

fn blocked_entry(plan: &Plan, check: String, bytes: u64) -> ReportBlocked {
    ReportBlocked {
        worktree: plan.target.worktree.clone(),
        target: plan.target.path.clone(),
        check,
        bytes,
    }
}

/// A `planned` record for each target of `report`, with the plan's checks and provenance as
/// step 5 evaluated them, and the bytes it was listed by; the first record that cannot be
/// appended is returned, the others still tried.
fn record_planned(
    config: &Config,
    state: &StateDir,
    observation: &Observation,
    report: &DiskReport,
) -> Result<(), String> {
    let mut failed = Ok(());
    let recorder = plan::recorder(config, state);
    for target in &report.targets {
        let Some(plan) = observation.plans.iter().find(|p| p.id == target.plan_id) else {
            continue;
        };
        let mut provenance = plan.provenance.clone();
        if !config.host.unit_may_act(UNIT) {
            provenance.config.push(ConfigKey {
                file: config.dir.join("host.toml").display().to_string(),
                key: format!("unit.{UNIT}.act"),
                value: false.into(),
            });
        }
        let mut record = recorder.record(RecordKind::Planned);
        record.plan_id = Some(plan.id.clone());
        let data = json!({
            "caller": format!("unit:{UNIT}"),
            "kind": plan.kind,
            "target": plan.target.path,
            "status": ActStatus::DryRun,
            "reasons": [EXECUTION_NOT_DESIGNED],
            "checks": plan.checks,
            "checks_ms": report.checks_ms,
            "volume": report.volume,
            "allocated_bytes": plan.allocated_bytes,
            "aliased_bytes": plan.aliased_bytes,
            "exclusive_bytes": target.exclusive_bytes,
            "provenance": provenance,
        });
        record.data = data.as_object().cloned().unwrap_or_default();
        if let (Err(err), Ok(())) = (recorder.append(&record), &failed) {
            failed = Err(format!("a planned record of {}: {err}", plan.id));
        }
    }
    failed
}

#[cfg(test)]
mod tests {
    use super::listed;
    use crate::contract::{Plan, ReportBlocked};

    fn plan(allocated: u64, aliased: u64) -> Plan {
        serde_json::from_value(serde_json::json!({
            "schema": "cardinal.plan.v1", "id": "fedcba9876543210", "created_ms": 5,
            "host_id": "00000000000000aa", "kind": "reclaim_dir",
            "target": {"path": "/w/node_modules", "repo": "/r/.git", "worktree": "/w",
                       "head": "abc", "branch": "feature", "git_dir": "/r/.git/worktrees/w",
                       "root_id": {"dev": 1, "ino": 2}, "root_created_ms": null},
            "allocated_bytes": allocated, "aliased_bytes": aliased,
            "checks": [], "invariants_ok": true, "approval": "none",
            "provenance": {"errors": [], "config": []}
        }))
        .unwrap()
    }

    fn aliased(bytes: u64) -> Result<u64, Option<ReportBlocked>> {
        Err(Some(ReportBlocked {
            worktree: "/w".into(),
            target: "/w/node_modules".into(),
            check: "aliased".into(),
            bytes,
        }))
    }

    /// A target whose regular files are all aliased frees none of them, whatever blocks its
    /// directories hold (two ext4 blocks here, none on APFS or tmpfs): it is blocked as
    /// `aliased`. With a file of its own it is listed by every byte not aliased, the
    /// directories' blocks included.
    #[test]
    fn aliased_files_are_blocked_whatever_the_directories_hold() {
        assert_eq!(
            listed(&plan(8192 + 4096, 4096), Some(0)),
            aliased(8192 + 4096)
        );
        assert_eq!(listed(&plan(4096, 4096), Some(0)), aliased(4096));
        assert_eq!(listed(&plan(8192 + 8192, 4096), Some(4096)), Ok(12288));
        assert_eq!(listed(&plan(8192, 0), Some(0)), Ok(8192));
        assert_eq!(listed(&plan(0, 0), Some(0)), Err(None));
    }
}
