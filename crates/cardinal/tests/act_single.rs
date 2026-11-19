//! A plan is evaluated, never claimed (C14, `docs/architecture.md` section 9.5): `act`
//! requires a plan ID, reports on every call, and leaves the plan unclaimed, so repeating it
//! reports again.

mod observe_common;
mod plan_support;
mod support;

use cardinal::act::ActOptions;
use cardinal::contract::{ActStatus, ErrorCode, PlanKind};
use plan_support::{Setup, arbiter, write, write_synced};

/// With or without `--dry-run`, `act` evaluates every check, reports the bound of what it would
/// free, and neither claims, records a result for, nor deletes anything; a repeated call
/// reports again.
#[test]
fn act_neither_claims_nor_deletes() {
    let s = Setup::new();
    let wt = s.worktree("dry");
    write_synced(&wt, "target/debug/app", b"bin");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
    let dry = ActOptions {
        dry_run: true,
        ..arbiter()
    };
    for options in [dry, arbiter(), arbiter()] {
        let result = s.act(&plan.id, &options).unwrap();
        assert_eq!(result.status, ActStatus::DryRun, "{result:#?}");
        plan_support::assert_dry_run_frees(result.freed_bytes, &result);
        assert!(!s.state.execution_claim(&plan.id).exists());
        assert!(!s.state.execution_result(&plan.id).exists());
        assert!(wt.join("target/debug/app").exists());
    }
}

#[test]
fn plan_ids_resolve_by_unique_prefix() {
    let s = Setup::new();
    let wt = s.worktree("prefix");
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
    plan_support::assert_plans_inside(s.fx.root(), &s.state, "abcdef");
    let assert = s.cardinal().args(["act", "abcdef"]).assert().code(3);
    let doc: serde_json::Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(doc["schema"], "cardinal.act-result.v2");
    assert_eq!(doc["status"], "expired");
    assert_eq!(doc["caller"], "arbiter");
}
