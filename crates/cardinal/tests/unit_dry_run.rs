//! Unit gating (C16, `docs/architecture.md` section 9.6): while a unit's `act` is false (the
//! default for every unit), `act` called as that unit leaves the target unchanged, claims
//! nothing, and records the planned operation.

mod observe_common;
mod plan_support;
mod support;

use cardinal::act::ActOptions;
use cardinal::contract::{ActStatus, Caller, PlanKind, RecordKind};
use plan_support::{Setup, write};

fn unit(name: &str) -> ActOptions {
    ActOptions {
        dry_run: false,
        owner_approved: false,
        caller: Caller::Unit(name.into()),
    }
}

fn records_of(s: &Setup, plan: &str, kind: RecordKind) -> Vec<cardinal::contract::Record> {
    cardinal::record::read_all(&s.state)
        .unwrap()
        .into_iter()
        .filter(|r| r.kind == kind && r.plan_id.as_deref() == Some(plan))
        .collect()
}

fn assert_only_planned(host_toml: Option<&str>) {
    let s = Setup::with_host(host_toml.unwrap_or_default());
    let wt = s.worktree("unit");
    write(&wt, "target/debug/app", "bin");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
    let before = plan_support::listing(&wt);
    for _ in 0..2 {
        let result = s.act(&plan.id, &unit("disk-reclaim")).unwrap();
        assert_eq!(result.status, ActStatus::DryRun, "{result:#?}");
        assert_eq!(result.caller, "unit:disk-reclaim");
        assert!(result.freed_bytes.unwrap() > 0);
    }
    assert_eq!(
        plan_support::listing(&wt),
        before,
        "the target is unchanged"
    );
    assert!(!s.state.plan_claim(&plan.id).exists(), "nothing claimed");
    let planned = records_of(&s, &plan.id, RecordKind::Planned);
    assert_eq!(planned.len(), 2, "each run records the planned operation");
    assert_eq!(planned[0].data["caller"], "unit:disk-reclaim");
    assert_eq!(planned[0].data["status"], "dry_run");
    assert_eq!(planned[0].data["kind"], "reclaim_dir");
    assert!(
        planned[0].data["checks"]
            .as_array()
            .is_some_and(|c| !c.is_empty())
    );
    assert_eq!(records_of(&s, &plan.id, RecordKind::Act).len(), 0);
}

#[test]
fn unit_without_a_table_only_reports() {
    assert_only_planned(None);
}

#[test]
fn unit_with_act_false_only_reports() {
    assert_only_planned(Some("[unit.disk-reclaim]\nact = false\n"));
}

#[test]
fn enabling_one_unit_leaves_the_others_gated() {
    assert_only_planned(Some("[unit.other]\nact = true\n"));
}

/// The control: a unit whose `act` is true is recorded like any caller, as an `act` record.
/// Phase 0 still only reports, so the target stays.
#[test]
fn unit_with_act_true_is_recorded_as_act() {
    let s = Setup::with_host("[unit.disk-reclaim]\nact = true\n");
    let wt = s.worktree("enabled");
    write(&wt, "target/debug/app", "bin");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
    let result = s.act(&plan.id, &unit("disk-reclaim")).unwrap();
    assert_eq!(result.status, ActStatus::DryRun, "{result:#?}");
    assert!(wt.join("target/debug/app").exists());
    assert!(!s.state.plan_claim(&plan.id).exists());
    assert_eq!(records_of(&s, &plan.id, RecordKind::Act).len(), 1);
    assert_eq!(records_of(&s, &plan.id, RecordKind::Planned).len(), 0);
}

/// A gated unit is still refused what every caller is refused: the dry run reports the
/// refusal it would get.
#[test]
fn gated_unit_reports_refusals() {
    let s = Setup::new();
    let wt = s.worktree("protected");
    write(&wt, "target/proptest-regressions/x.txt", "keep");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
    let result = s.act(&plan.id, &unit("disk-reclaim")).unwrap();
    assert_eq!(result.status, ActStatus::Rejected);
    // Observation classifies `target`, which holds protected data, as protected.
    assert_eq!(result.reasons, ["regenerable", "no_protected"]);
    assert!(!s.state.plan_claim(&plan.id).exists());
    assert_eq!(records_of(&s, &plan.id, RecordKind::Planned).len(), 1);
}
