//! A plan runs at most once (C14, `docs/architecture.md` sections 4.9 and 9.5): `act` claims
//! it before touching the target, also across processes, and a dry run claims nothing.

#![cfg_attr(
    not(unix),
    allow(
        dead_code,
        unused_imports,
        reason = "deletion is Unix-only in phase 0, and so are the tests that delete"
    )
)]

mod fakes;
mod support;

use std::time::Duration;

use cardinal::act::ActOptions;
use cardinal::contract::{ActStatus, ErrorCode, IgnoredClass, PlanKind};
use cardinal::testpoint::ACT_LOCKED;
use fakes::{Setup, arbiter, write};
use support::helper::ChildGuard;
use support::testpoints::TestPoints;

#[test]
fn helper() {
    fakes::run_act_if_requested();
}

const WAIT: Duration = Duration::from_secs(20);

/// The first `act` holds its claim while paused at `act.locked`; a second `act` of the same
/// plan returns `already_executed` without touching anything, and the first then executes.
#[cfg(unix)]
#[test]
fn concurrent_second_act_is_already_executed() {
    let s = Setup::new();
    let wt = s.worktree("twice", &[("target", IgnoredClass::Regenerable)]);
    write(&wt, "target/debug/app", "bin");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));

    let points = TestPoints::new();
    let out = points.dir().join("first.json");
    let mut command = s.act_command(&plan.id, &arbiter(), &out);
    points.apply(&mut command, &[ACT_LOCKED], None);
    let mut first = ChildGuard::new(command.spawn().unwrap());
    points.wait_reached(ACT_LOCKED, first.id(), WAIT);

    let second = s.act(&plan.id, &arbiter()).unwrap();
    assert_eq!(second.status, ActStatus::AlreadyExecuted);
    assert!(
        wt.join("target/debug/app").exists(),
        "the second act touched nothing"
    );

    points.release(ACT_LOCKED);
    assert!(first.child().wait().unwrap().success());
    let first = fakes::read_result(&out);
    assert_eq!(first.status, ActStatus::Executed, "{first:#?}");
    assert!(!wt.join("target").exists());
}

/// A claim without a result (an interrupted run) is never retried.
#[cfg(unix)]
#[test]
fn interrupted_plan_is_not_retried() {
    let s = Setup::new();
    let wt = s.worktree("interrupted", &[("target", IgnoredClass::Regenerable)]);
    write(&wt, "target/x", "x");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
    std::fs::write(s.state.plan_claim(&plan.id), br#"{"pid":1,"claimed_ms":1}"#).unwrap();
    let result = s.act(&plan.id, &arbiter()).unwrap();
    assert_eq!(result.status, ActStatus::AlreadyExecuted);
    assert!(wt.join("target/x").exists());
}

/// A dry run evaluates every check without claiming or deleting, so the plan can still be
/// executed afterwards.
#[cfg(unix)]
#[test]
fn dry_run_neither_claims_nor_deletes() {
    let s = Setup::new();
    let wt = s.worktree("dry", &[("target", IgnoredClass::Regenerable)]);
    write(&wt, "target/debug/app", "bin");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
    let dry = ActOptions {
        dry_run: true,
        ..arbiter()
    };
    let result = s.act(&plan.id, &dry).unwrap();
    assert_eq!(result.status, ActStatus::DryRun, "{result:#?}");
    assert!(
        result.freed_bytes.unwrap() > 0,
        "reports what it would free"
    );
    assert!(!s.state.plan_claim(&plan.id).exists());
    assert!(wt.join("target/debug/app").exists());
    let result = s.act(&plan.id, &arbiter()).unwrap();
    assert_eq!(result.status, ActStatus::Executed);
    let stored: cardinal::contract::ActResult =
        serde_json::from_slice(&std::fs::read(s.state.plan_result(&plan.id)).unwrap()).unwrap();
    assert_eq!(stored, result, "result.json holds the result");
}

#[test]
fn plan_ids_resolve_by_unique_prefix() {
    let s = Setup::new();
    let wt = s.worktree("prefix", &[("target", IgnoredClass::Regenerable)]);
    write(&wt, "target/x", "x");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
    let dry = ActOptions {
        dry_run: true,
        ..arbiter()
    };
    let result = s.act(&plan.id[..4], &dry).unwrap();
    assert_eq!(result.plan_id, plan.id);
    assert_eq!(
        s.act(&plan.id[..3], &dry).unwrap_err().code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        s.act("ffffffffffffffff", &dry).unwrap_err().code,
        ErrorCode::NotFound
    );
}

/// Through the binary: an unknown plan is `not_found` (exit 1); a refusal exits 3.
#[test]
fn cli_exit_codes() {
    let s = Setup::new();
    let assert = s.cardinal().args(["act", "abcd"]).assert().code(1);
    let doc: serde_json::Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(doc["error"]["code"], "not_found");

    // A plan whose worktree no longer exists expires; `act` exits 3 and prints the result.
    let id = "abcdef0123456789";
    let gone = s.fx.path("gone");
    std::fs::create_dir_all(s.state.plan_dir(id)).unwrap();
    let plan = serde_json::json!({
        "schema": "cardinal.plan.v1", "id": id, "created_ms": 1, "host_id": s.state.host_id(),
        "kind": "reclaim_dir",
        "target": { "path": gone.join("wt/target"), "repo": gone.join("main/.git"),
                    "worktree": gone.join("wt"), "head": null, "branch": null,
                    "root_id": null, "root_created_ms": null },
        "allocated_bytes": null, "checks": [], "invariants_ok": true, "approval": "none"
    });
    std::fs::write(s.state.plan_file(id), plan.to_string()).unwrap();
    fakes::assert_plans_inside(s.fx.root(), &s.state, "abcdef");
    let assert = s.cardinal().args(["act", "abcdef"]).assert().code(3);
    let doc: serde_json::Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(doc["schema"], "cardinal.act-result.v1");
    assert_eq!(doc["status"], "expired");
    assert_eq!(doc["caller"], "arbiter");
}
