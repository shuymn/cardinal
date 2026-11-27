//! `disk-reclaim` (`docs/architecture.md` section 10.4, C10, A31): for each volume under
//! pressure, from the plans of step 5 and the pass's last successful reading, the eligible
//! `reclaim_dir` targets in reclamation order (those with a positive bound of the freed bytes
//! first, oldest worktree first and the largest bound first, then those whose bound is 0 or
//! null), the entries held by a failed check or by aliased files, and the worktrees observed
//! only in part.
//!
//! Listing, it cuts the order where the bounds would lift the reading above the recovery
//! line, reports the shortfall when they would not, runs no `act`, observes nothing again, and
//! gives each listed target a `planned` record with the checks and bytes it was listed by.
//! Executing, on the state dir's volume when `[unit.disk-reclaim] act` is true and
//! `[act] execute` grants the unit `reclaim_dir` (section 9.7), it runs `act` as the unit on
//! the targets in that order, reads the volume again after every `act`, and stops once a
//! reading is above the recovery line, at a reading it cannot take, when the targets run out,
//! or once the pass's reclaim budget is spent; the volume's readings, not the bounds, decide.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::json;

use crate::act::{self, ActOptions, EXECUTION_NOT_DESIGNED, EXECUTION_NOT_GRANTED};
use crate::config::Config;
use crate::contract::{
    ActStatus, Approval, Caller, CheckResult, ConfigKey, DiskReport, Plan, PlanKind, RecordKind,
    ReportBlocked, ReportIncomplete, ReportReading, ReportTarget, ScopedError,
};
use crate::plan;
use crate::state::StateDir;

use super::disk::{self, Watched};
use super::{Budget, Observation};

/// The unit `disk-reclaim` runs as.
pub const UNIT: &str = "disk-reclaim";

/// Whether `disk-reclaim` executes on the state dir's volume: its `act` is true and
/// `[act] execute` grants it `reclaim_dir` (section 9.7). Never off Unix.
#[must_use]
pub fn executes(config: &Config) -> bool {
    cfg!(unix)
        && config.host.unit_may_act(UNIT)
        && config
            .host
            .grants(&Caller::Unit(UNIT.into()), PlanKind::ReclaimDir)
}

/// The reports of every volume under pressure with a successful reading in this pass; a
/// volume whose readings all failed is skipped (A16), and one without an observation is
/// reported in `errors`. On the state dir's volume, when [`executes`], the targets are
/// executed within `budget`, each volume reading kept in `disks`.
pub fn reports(
    config: &Config,
    state: &StateDir,
    disks: &mut [Watched],
    observation: Option<&Observation>,
    budget: &mut Budget,
    errors: &mut Vec<ScopedError>,
) -> Vec<DiskReport> {
    let mut reports = Vec::new();
    for volume in disks.iter_mut().filter(|d| d.under_pressure()) {
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
        let (order, blocked) = order(&volume.state.volume, observation);
        let recovery = volume.state.lines.recovery_bytes;
        let on_state = volume.path == state.root();
        // The execution queue (section 9.8): the order without the targets of worktrees not
        // under a covered root, the rest keeping their order, before any budget is spent.
        let (queue, uncovered): (Vec<&Plan>, Vec<&Plan>) = order
            .iter()
            .partition(|plan| covered(config, &plan.target.worktree));
        // The bounds of the queue as it ends: without a target a reload found uncovered.
        let reachable = |withdrawn: &[(ReportTarget, Config)]| -> u64 {
            if !(cfg!(unix) && on_state) {
                return 0;
            }
            queue
                .iter()
                .filter(|plan| !withdrawn.iter().any(|(t, _)| t.plan_id == plan.id))
                .map(|plan| plan.exclusive_bytes.unwrap_or(0))
                .sum()
        };
        let (targets, reading, shortfall_bytes, reachable_bytes) = if executes(config) && on_state {
            let (mut targets, withdrawn) = execute(config, state, volume, &queue, budget, errors);
            let reading = volume.reading.unwrap_or(reading);
            let shortfall = recovery.saturating_sub(reading.free_bytes);
            let reachable_bytes = reachable(&withdrawn);
            // Each uncovered target recorded under the configuration that left it out: the
            // startup one, or the reload that withdrew it.
            let listed: Vec<(ReportTarget, &Config)> = uncovered
                .iter()
                .map(|plan| (target_of(plan), config))
                .chain(
                    withdrawn
                        .iter()
                        .map(|(target, current)| (target.clone(), current)),
                )
                .collect();
            for (target, by) in &listed {
                let recorded = record_planned(
                    by,
                    state,
                    observation,
                    &volume.state.volume,
                    std::slice::from_ref(target),
                    Some(act::SESSIONS_UNCOVERED),
                );
                if let Err(message) = recorded {
                    errors.push(ScopedError {
                        scope: "disk_reclaim".into(),
                        message,
                    });
                }
            }
            targets.extend(listed.into_iter().map(|(target, _)| target));
            (targets, reading, shortfall, reachable_bytes)
        } else {
            let (targets, shortfall) = list(&order, reading, recovery);
            if let Err(message) = record_planned(
                config,
                state,
                observation,
                &volume.state.volume,
                &targets,
                None,
            ) {
                errors.push(ScopedError {
                    scope: "disk_reclaim".into(),
                    message,
                });
            }
            (targets, reading, shortfall, reachable(&[]))
        };
        reports.push(DiskReport {
            volume: volume.state.volume.clone(),
            reading,
            lines: volume.state.lines,
            checks_ms: observation.checks_ms,
            targets,
            shortfall_bytes,
            reachable_bytes: Some(reachable_bytes),
            blocked,
            incomplete: incomplete(&volume.state.volume, observation),
        });
    }
    reports
}

/// The eligible targets on `volume` in reclamation order (section 10.4), and the entries
/// blocked by a failed check or aliased files, by worktree and path. Targets with a positive
/// bound come first, worktrees by last activity, oldest first (unknown last), and within a
/// worktree the largest bound first; then the targets whose bound is 0 or null, worktrees
/// oldest first, and within a worktree the most bytes not aliased first.
fn order<'a>(volume: &str, observation: &'a Observation) -> (Vec<&'a Plan>, Vec<ReportBlocked>) {
    let mut blocked = Vec::new();
    let mut eligible: Vec<(&Plan, u64)> = Vec::new();
    for plan in plans_on(volume, &observation.plans) {
        let own_file_bytes = observation.own_file_bytes.get(&plan.id).copied();
        match listed(plan, own_file_bytes) {
            Ok(unaliased) => eligible.push((plan, unaliased)),
            Err(Some(entry)) => blocked.push(entry),
            Err(None) => {}
        }
    }
    let activity = |worktree: &str| {
        observation
            .inventory
            .repos
            .iter()
            .flat_map(|r| &r.worktrees)
            .find(|w| w.path == worktree)
            .and_then(|w| w.last_activity_ms)
    };
    eligible.sort_by_cached_key(|(plan, unaliased)| {
        let bound = plan.exclusive_bytes.filter(|b| *b > 0);
        let last = activity(&plan.target.worktree);
        (
            bound.is_none(),
            last.is_none(),
            last,
            plan.target.worktree.clone(),
            std::cmp::Reverse(bound.unwrap_or(*unaliased)),
            plan.target.path.clone(),
        )
    });
    blocked.sort_by(|a, b| (&a.worktree, &a.target).cmp(&(&b.worktree, &b.target)));
    (
        eligible.into_iter().map(|(plan, _)| plan).collect(),
        blocked,
    )
}

/// The listing: the targets of `order` until their bounds would lift `reading` above the
/// recovery line, counting nothing for a bound of 0 or null (section 10.4), with the
/// shortfall the bounds leave.
fn list(order: &[&Plan], reading: ReportReading, recovery: u64) -> (Vec<ReportTarget>, u64) {
    let mut lifted = reading.free_bytes;
    let mut targets = Vec::new();
    for plan in order {
        if lifted > recovery {
            break;
        }
        let target = target_of(plan);
        lifted = lifted.saturating_add(target.exclusive_bytes);
        targets.push(target);
    }
    (targets, recovery.saturating_sub(lifted))
}

/// Executing (section 10.4, A31): `act` as the unit on each target of `order` while the
/// volume's last reading is at or below the recovery line and `budget` is left, the volume
/// read again after every `act`, a refused or `partial` one included; a reading that fails
/// stops it. The configuration is loaded again before each `act`, which runs under it: one
/// that does not load, or no longer lets the unit execute, stops it, and a target whose
/// worktree it no longer covers (section 9.8) is skipped before any budget is spent on it,
/// the rest of the queue going on. Each target run is returned with its status and the bytes
/// `act` expects it freed, then each skipped one with the configuration that skipped it.
fn execute(
    config: &Config,
    state: &StateDir,
    volume: &mut Watched,
    order: &[&Plan],
    budget: &mut Budget,
    errors: &mut Vec<ScopedError>,
) -> (Vec<ReportTarget>, Vec<(ReportTarget, Config)>) {
    let options = ActOptions {
        dry_run: false,
        owner_approved: false,
        squash_merged: false,
        caller: Caller::Unit(UNIT.into()),
    };
    let recovery = volume.state.lines.recovery_bytes;
    let mut targets = Vec::new();
    let mut uncovered = Vec::new();
    for plan in order {
        let above = volume.reading.is_none_or(|r| r.free_bytes > recovery);
        if above || !budget.left() {
            break;
        }
        let current = match Config::load(&config.dir) {
            Ok(current) if executes(&current) => current,
            Ok(_) => {
                errors.push(ScopedError {
                    scope: "disk_reclaim".into(),
                    message: format!(
                        "{}: disk-reclaim may no longer execute, so execution stops",
                        volume.state.mount
                    ),
                });
                break;
            }
            Err(err) => {
                errors.push(ScopedError {
                    scope: "disk_reclaim".into(),
                    message: format!("the configuration does not load, so execution stops: {err}"),
                });
                break;
            }
        };
        if !covered(&current, &plan.target.worktree) {
            uncovered.push((target_of(plan), current));
            continue;
        }
        let mut target = target_of(plan);
        match budget.spend(|| act::execute(&current, state, &plan.id, &options)) {
            Ok(result) => {
                target.status = Some(result.status);
                target.freed_bytes = Some(result.freed_bytes);
            }
            Err(err) => errors.push(ScopedError {
                scope: "disk_reclaim".into(),
                message: format!("act {}: {}", plan.id, err.message),
            }),
        }
        targets.push(target);
        if !disk::read(config, state, volume, errors) {
            break;
        }
    }
    (targets, uncovered)
}

/// Whether the worktree at `worktree` lies under a `[sessions] covered` root; one that cannot
/// be resolved is not covered (section 9.8).
fn covered(config: &Config, worktree: &str) -> bool {
    config.host.sessions.covers(Path::new(worktree)) == Ok(true)
}

/// `plan` as a target of the report, with its bound, 0 where that is null.
fn target_of(plan: &Plan) -> ReportTarget {
    ReportTarget {
        plan_id: plan.id.clone(),
        worktree: plan.target.worktree.clone(),
        target: plan.target.path.clone(),
        exclusive_bytes: plan.exclusive_bytes.unwrap_or(0),
        exclusive_measure: Some(plan.exclusive_measure),
        status: None,
        freed_bytes: None,
    }
}

/// How `plan` enters the report: listed, ordered by its bytes not aliased (`Ok`), counted in
/// `blocked` with its first failed check, or as `aliased` when its regular files' bytes are
/// all aliased, whatever its directories' blocks (`Err(Some)`), or left out when it holds
/// nothing (`Err(None)`). `own_file_bytes` is [`plan::Built::own_file_bytes`]. The bytes not
/// aliased include the directories' blocks.
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
    let unaliased = allocated.saturating_sub(aliased);
    let files_all_aliased = aliased > 0 && own_file_bytes == Some(0);
    if unaliased == 0 || files_all_aliased {
        return Err((allocated > 0).then(|| blocked_entry(plan, "aliased".to_owned(), allocated)));
    }
    Ok(unaliased)
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

/// A `planned` record for each listed target, with the plan's checks and provenance as step
/// 5 evaluated them, the bytes it was listed by, and the reason it did not execute: with
/// `uncovered`, that reason, `sessions_uncovered`, naming `sessions.covered`; otherwise not
/// designed off Unix, not granted otherwise, naming `unit.disk-reclaim.act` while it is false
/// and `act.execute` while it does not grant the unit (C16); the first record that cannot be
/// appended is returned, the others still tried.
fn record_planned(
    config: &Config,
    state: &StateDir,
    observation: &Observation,
    volume: &str,
    targets: &[ReportTarget],
    uncovered: Option<&str>,
) -> Result<(), String> {
    let mut failed = Ok(());
    let recorder = plan::recorder(config, state);
    let reason = match uncovered {
        Some(reason) => reason,
        None if cfg!(unix) => EXECUTION_NOT_GRANTED,
        None => EXECUTION_NOT_DESIGNED,
    };
    let file = config.dir.join("host.toml").display().to_string();
    let mut keys = Vec::new();
    if uncovered.is_some() {
        keys.push(ConfigKey {
            file: file.clone(),
            key: "sessions.covered".to_owned(),
            value: config
                .host
                .sessions
                .covered
                .iter()
                .map(|root| root.display().to_string())
                .collect(),
        });
    } else if !config.host.unit_may_act(UNIT) {
        keys.push(ConfigKey {
            file: file.clone(),
            key: format!("unit.{UNIT}.act"),
            value: false.into(),
        });
    } else if !executes(config) && cfg!(unix) {
        let unit = Caller::Unit(UNIT.into()).to_string();
        let kinds = config
            .host
            .act
            .execute
            .get(&unit)
            .cloned()
            .unwrap_or_default();
        keys.push(ConfigKey {
            file,
            key: "act.execute".to_owned(),
            value: serde_json::to_value(kinds).unwrap_or_default(),
        });
    }
    for target in targets {
        let Some(plan) = observation.plans.iter().find(|p| p.id == target.plan_id) else {
            continue;
        };
        let mut provenance = plan.provenance.clone();
        provenance.config.extend(keys.iter().cloned());
        let mut record = recorder.record(RecordKind::Planned);
        record.plan_id = Some(plan.id.clone());
        let data = json!({
            "caller": format!("unit:{UNIT}"),
            "kind": plan.kind,
            "target": plan.target.path,
            "status": ActStatus::DryRun,
            "reasons": [reason],
            "checks": plan.checks,
            "checks_ms": observation.checks_ms,
            "volume": volume,
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
