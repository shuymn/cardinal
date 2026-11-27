//! Complete JSON documents of `docs/architecture.md` section 11.3 against the contract types:
//! every field present round-trips unchanged, and each malformed variant is rejected.

use cardinal::contract::{
    ActClaim, ActManifest, ActResult, ArbiterPass, ClassifyRequest, ClassifyResponse, DiskState,
    EntryPath, ErrorDoc, Explain, Inbox, InboxBrief, InboxItem, Inventory, ManifestEntry, Plan,
    PlanList, Reconcile, ReconcileState, Record, Reservation, ReservationView, RunSpec,
    SessionLease, SessionSnapshot, Snapshot, WorktreeView,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// Parses `doc` as `T` and checks that serializing it gives `doc` back exactly.
fn round_trips<T: DeserializeOwned + Serialize>(doc: &Value) {
    let parsed: T =
        serde_json::from_value(doc.clone()).unwrap_or_else(|err| panic!("{err}: {doc}"));
    assert_eq!(&serde_json::to_value(parsed).unwrap(), doc);
}

/// Applies `change` to a copy of `doc` and checks that `T` rejects the result.
fn rejects<T: DeserializeOwned>(doc: &Value, what: &str, change: impl FnOnce(&mut Value)) {
    let mut bad = doc.clone();
    change(&mut bad);
    assert!(
        serde_json::from_value::<T>(bad.clone()).is_err(),
        "{what} was accepted: {bad}"
    );
}

/// The common negative cases: another schema, another version, and a missing field.
fn rejects_wrong_schema_and_missing<T: DeserializeOwned>(doc: &Value, required: &str) {
    rejects::<T>(doc, "another schema", |d| {
        d["schema"] = json!("cardinal.other.v1");
    });
    let (base, version) = doc["schema"].as_str().unwrap().rsplit_once(".v").unwrap();
    let next = format!("{base}.v{}", version.parse::<u32>().unwrap() + 1);
    rejects::<T>(doc, "another version", |d| d["schema"] = json!(next));
    rejects::<T>(doc, "a missing field", |d| {
        d.as_object_mut().unwrap().remove(required);
    });
}

fn reservation() -> Value {
    json!({
        "schema": "cardinal.reservation.v1", "job_id": "0123456789abcdef", "state": "running",
        "boot_s": 1_700_000_000, "created_ms": 1, "updated_ms": 2,
        "runner": {"pid": 100, "start_s": 1_700_000_100},
        "group": {"kind": "pgid", "pgid": 100, "leader_start_s": 1_700_000_100},
        "child": {"pid": 101, "start_s": 1_700_000_101}, "stop_signal_ms": null,
        "priority": "normal", "resources": {"cpu": 4, "memory_mb": 9000},
        "admission": "within_budget", "class": "test", "rule": "generic/test",
        "worktree": {"key": "af63dc4c8601ec8c", "path": "/w", "protected": true},
        "session": {"id": "s", "agent_id": null, "tool_use_id": "toolu_1", "stage_index": 0},
        "cwd": "/w/sub", "argv": ["pnpm", "test"]
    })
}

fn view() -> Value {
    let mut view = reservation();
    let object = view.as_object_mut().unwrap();
    object.insert("derived".into(), json!("recovery_needed"));
    object.insert(
        "members".into(),
        json!([{"pid": 101, "start_s": 1_700_000_101, "name": "node"}]),
    );
    object.insert("verified".into(), json!(true));
    view
}

fn check(name: &str, role: &str, result: &str) -> Value {
    json!({"name": name, "role": role, "result": result, "detail": "d"})
}

fn plan() -> Value {
    let mut clean = check("clean", "invariant", "pass");
    clean["facts"] = json!([
        {"name": "worktree.facts.dirty", "value": "no", "source": "git"},
        {"name": "worktree.ignored", "value": [{"path": "a", "class": "regenerable"}],
         "source": "git", "omitted": 12}
    ]);
    json!({
        "schema": "cardinal.plan.v1", "id": "fedcba9876543210", "created_ms": 5,
        "host_id": "00000000000000aa", "kind": "reclaim_worktree",
        "target": {"path": "/w", "repo": "/r/.git", "worktree": "/w", "head": "abc",
                   "branch": "feature", "git_dir": "/r/.git/worktrees/w",
                   "root_id": {"dev": 1, "ino": 2}, "root_created_ms": null},
        "allocated_bytes": 4096, "aliased_bytes": 1024,
        "exclusive_bytes": 2048, "exclusive_measure": "private_size",
        "checks": [clean, check("merge_evidence", "condition", "fail")],
        "invariants_ok": true, "approval": "owner",
        "provenance": {
            "errors": [{"source": "gh", "message": "HTTP 502"}],
            "config": [{"file": "/c/repos/app.toml", "key": "data.protected", "value": ["fuzz/corpus"]}]
        }
    })
}

fn inbox_item() -> Value {
    json!({
        "schema": "cardinal.inbox-item.v1", "key": "recovery_needed:0123456789abcdef@1",
        "kind": "recovery_needed", "status": "escalated",
        "subject": {"job_id": "0123456789abcdef", "worktree": "/w"},
        "evidence": {"members": [101], "verified": false},
        "created_ms": 1, "updated_ms": 2, "seen": 3, "ended_ms": null,
        "arbiter": {"pass": "fedcba9876543210", "assigned_ms": 2,
                    "answer": {"outcome": "escalated", "note": "n", "at_ms": 2},
                    "outcome": "failed:exit 1", "ended_ms": 3},
        "resolution": {"outcome": "escalated", "by": "arbiter", "note": "n", "at_ms": 2}
    })
}

fn worktree() -> Value {
    json!({
        "path": "/w", "main": false, "head": "abc", "branch": "feature",
        "upstream": "origin/feature", "git_dir": "/r/.git/worktrees/w",
        "last_activity_ms": 1_700_000_000_000_u64, "managed_by": "git-lock",
        "facts": {"dirty": "unknown",
                  "dirty_unknown": {"reason": "filtered_paths", "paths": ["assets/big.bin"]},
                  "has_upstream": "yes", "unpushed": "unknown",
                  "open_pr": "no", "merge_evidence": "merged_pr",
                  "merged_pr": {"number": 12, "url": "https://example.invalid/pr/12"},
                  "content_in_base": "no", "content_diff": ["src/a.rs"],
                  "closed_pr": {"number": 9, "url": "https://example.invalid/pr/9"},
                  "in_use": {"value": true, "jobs": ["0123456789abcdef"]}},
        "ignored": [{"path": "/w/node_modules", "class": "regenerable", "regenerable_match": true,
                     "allocated_bytes": 10, "aliased_bytes": 4,
                     "exclusive_bytes": 3, "exclusive_measure": "links"},
                    {"path": "/w/target", "class": "regenerable", "regenerable_match": true,
                     "allocated_bytes": 10, "aliased_bytes": 0,
                     "exclusive_bytes": null, "exclusive_measure": "read_failed"},
                    {"path": "/w/fuzz/corpus", "class": "protected", "regenerable_match": false,
                     "allocated_bytes": null, "aliased_bytes": null,
                     "exclusive_bytes": null, "exclusive_measure": null}],
        "reclaim": {"candidate": false, "failed": ["not_managed"], "unknown": ["no_unpushed"]},
        "allocated_bytes": null
    })
}

fn repo() -> Value {
    json!({
        "common_dir": "/r/.git", "main_worktree": "/r", "profiles": ["generic"],
        "github": "owner/name", "base": {"ref": "origin/main", "oid": "abc"},
        "gh": "unknown", "gh_error": "timeout", "worktrees": [worktree()]
    })
}

fn host() -> Value {
    json!({"id": "00000000000000aa", "hostname": "h", "os": "macos", "arch": "aarch64",
           "boot_s": 1_700_000_000})
}

fn disk_state() -> Value {
    json!({
        "schema": "cardinal.disk-state.v1", "volume": "af63dc4c8601ec8c", "mount": "/",
        "measure": "important_usage", "state": "warning", "since_ms": 5,
        "predicted_only": true,
        "lines": {"warning_bytes": 300, "recovery_bytes": 400, "critical_bytes": 100},
        "readings": [{"at_ms": 4, "free_bytes": 250, "total_bytes": 1000},
                     {"at_ms": 5, "free_bytes": null, "total_bytes": null}]
    })
}

fn disk_report() -> Value {
    json!({
        "volume": "af63dc4c8601ec8c", "reading": {"at_ms": 5, "free_bytes": 250},
        "lines": {"warning_bytes": 300, "recovery_bytes": 400, "critical_bytes": 100},
        "checks_ms": 4,
        "targets": [{"plan_id": "fedcba9876543210", "worktree": "/w", "target": "/w/target",
                     "exclusive_bytes": 120, "exclusive_measure": "private_size"},
                    {"plan_id": "0123456789abcdef", "worktree": "/w", "target": "/w/dist",
                     "exclusive_bytes": 60, "status": "partial", "freed_bytes": 40},
                    {"plan_id": "0123456789abcdee", "worktree": "/w", "target": "/w/out",
                     "exclusive_bytes": 0, "exclusive_measure": null, "status": "rejected",
                     "freed_bytes": null}],
        "shortfall_bytes": 30, "reachable_bytes": 180,
        "blocked": [{"worktree": "/w2", "target": "/w2/node_modules", "check": "not_in_use",
                     "bytes": 80},
                    {"worktree": "/w", "target": "/w/.pnpm", "check": "aliased", "bytes": 9}],
        "incomplete": [{"worktree": "/w3",
                        "reason": "git status is incomplete: warning: could not open directory 'private/': Permission denied"}]
    })
}

fn arbiter_pass() -> Value {
    json!({
        "schema": "cardinal.arbiter-pass.v1", "id": "fedcba9876543210", "started_ms": 1,
        "ended_ms": 2, "outcome": "timed_out", "items": ["reclaimable:/w@abc@1"],
        "log": "/s/arbiter/fedcba9876543210.log", "failures_in_a_row": 1
    })
}

fn record() -> Value {
    json!({
        "schema": "cardinal.record.v1", "ts_ms": 7, "host_id": "00000000000000aa",
        "kind": "job_finished", "job_id": "0123456789abcdef", "plan_id": "fedcba9876543210",
        "inbox_key": "k", "session_id": "s",
        "data": {"exit": 0, "signal": null, "waited_ms": 12, "admission": "within_budget",
                 "measurements": [{"kind": "wall_time", "ms": 900}]}
    })
}

fn classify_request() -> Value {
    json!({
        "schema": "cardinal.classify-request.v1", "command": "cd x && pnpm test",
        "stage": "pnpm test", "argv": ["pnpm", "test"], "cwd": "/w",
        "repo": {"common_dir": "/w/.git", "main_worktree": "/w", "remote": null},
        "tool": {"name": "Bash", "timeout_ms": 120_000, "background": false}
    })
}

#[test]
fn error_document() {
    let doc = json!({"schema": "cardinal.error.v1",
                     "error": {"code": "ambiguous_id", "message": "m"}});
    round_trips::<ErrorDoc>(&doc);
    rejects_wrong_schema_and_missing::<ErrorDoc>(&doc, "error");
    rejects::<ErrorDoc>(&doc, "an unknown code", |d| {
        d["error"]["code"] = json!("oops");
    });
    for code in [
        "state_unavailable",
        "config_invalid",
        "not_found",
        "ambiguous_id",
        "invalid_argument",
        "not_assigned",
        "not_implemented",
        "internal",
    ] {
        round_trips::<ErrorDoc>(&json!({"schema": "cardinal.error.v1",
                                        "error": {"code": code, "message": "m"}}));
    }
}

#[test]
fn run_spec_document() {
    let doc = json!({
        "schema": "cardinal.run-spec.v1", "class": "test", "rule": "generic/test",
        "resources": {"cpu": 4}, "timeout_ms": 120_000, "background": true, "stage_index": 2,
        "stage": "cargo test", "session_id": "s", "agent_id": "a", "tool_use_id": "t"
    });
    round_trips::<RunSpec>(&doc);
    rejects_wrong_schema_and_missing::<RunSpec>(&doc, "stage");
    rejects::<RunSpec>(&doc, "a zero resource", |d| {
        d["resources"]["cpu"] = json!(0);
    });
    rejects::<RunSpec>(&doc, "a negative resource", |d| {
        d["resources"]["cpu"] = json!(-1);
    });
    rejects::<RunSpec>(&doc, "a fractional resource", |d| {
        d["resources"]["cpu"] = json!(1.5);
    });
}

#[test]
fn reservation_documents() {
    round_trips::<Reservation>(&reservation());
    rejects_wrong_schema_and_missing::<Reservation>(&reservation(), "group");
    rejects::<Reservation>(&reservation(), "a zero resource", |d| {
        d["resources"]["memory_mb"] = json!(0);
    });
    rejects::<Reservation>(&reservation(), "an unknown state", |d| {
        d["state"] = json!("done");
    });
    rejects::<Reservation>(&reservation(), "an unknown group kind", |d| {
        d["group"]["kind"] = json!("job_object");
    });
    rejects::<Reservation>(&reservation(), "another priority", |d| {
        d["priority"] = json!("high");
    });

    round_trips::<ReservationView>(&view());
    rejects::<ReservationView>(&view(), "an unknown derived state", |d| {
        d["derived"] = json!("dead");
    });
    rejects::<ReservationView>(&view(), "a view without its reservation", |d| {
        d.as_object_mut().unwrap().remove("job_id");
    });
}

#[test]
fn inventory_document() {
    let doc = json!({
        "schema": "cardinal.inventory.v1", "observed_ms": 9,
        "host": host(),
        "budget": {"cpu": 8}, "reserved": {"cpu": 4},
        "reservations": [view()],
        "repos": [repo()],
        "outlived": [{"session_id": "s", "ended_ms": 3,
                      "processes": [{"pid": 5, "start_s": 6, "name": "vite", "command": "vite dev"}]}],
        "disk": [disk_state()],
        "arbiter": {"running": true, "last": arbiter_pass()},
        "errors": [{"scope": "repo:/r", "message": "m"}]
    });
    round_trips::<Inventory>(&doc);
    rejects_wrong_schema_and_missing::<Inventory>(&doc, "host");
    rejects::<Inventory>(&doc, "a zero budget", |d| d["budget"]["cpu"] = json!(0));
    rejects::<Inventory>(&doc, "another OS", |d| d["host"]["os"] = json!("freebsd"));
    rejects::<Inventory>(&doc, "a tri-state outside yes/no/unknown", |d| {
        d["repos"][0]["worktrees"][0]["facts"]["dirty"] = json!(true);
    });
    rejects::<Inventory>(&doc, "an unknown ignored class", |d| {
        d["repos"][0]["worktrees"][0]["ignored"][0]["class"] = json!("cache");
    });
    rejects::<Inventory>(&doc, "another dirty_unknown reason", |d| {
        d["repos"][0]["worktrees"][0]["facts"]["dirty_unknown"]["reason"] = json!("lfs");
    });
    // Nullable fields read as null when absent; the others are required.
    for field in ["content_in_base", "content_diff"] {
        rejects::<Inventory>(&doc, "facts without a phase 1 fact", |d| {
            d["repos"][0]["worktrees"][0]["facts"]
                .as_object_mut()
                .unwrap()
                .remove(field);
        });
    }
    // An observation made before the freed-bytes bound loads with it null.
    let mut old = doc.clone();
    let entry = old["repos"][0]["worktrees"][0]["ignored"][0]
        .as_object_mut()
        .unwrap();
    entry.remove("exclusive_bytes");
    entry.remove("exclusive_measure");
    let loaded: Inventory = serde_json::from_value(old).unwrap();
    let entry = &loaded.repos[0].worktrees[0].ignored[0];
    assert_eq!(
        (entry.exclusive_bytes, entry.exclusive_measure),
        (None, None)
    );
    rejects::<Inventory>(&doc, "an ignored entry without regenerable_match", |d| {
        d["repos"][0]["worktrees"][0]["ignored"][0]
            .as_object_mut()
            .unwrap()
            .remove("regenerable_match");
    });
    rejects::<Inventory>(&doc, "a disk state of another schema", |d| {
        d["disk"][0]["schema"] = json!("cardinal.disk-state.v2");
    });
    rejects::<Inventory>(&doc, "an inventory without the arbiter", |d| {
        d.as_object_mut().unwrap().remove("arbiter");
    });
}

#[test]
fn disk_state_document() {
    round_trips::<DiskState>(&disk_state());
    rejects_wrong_schema_and_missing::<DiskState>(&disk_state(), "lines");
    // A state written before `predicted_only` keeps the recovery line.
    let mut before = disk_state();
    before.as_object_mut().unwrap().remove("predicted_only");
    let before: DiskState = serde_json::from_value(before).unwrap();
    assert!(!before.predicted_only);
    rejects::<DiskState>(&disk_state(), "an unknown measure", |d| {
        d["measure"] = json!("statfs");
    });
    rejects::<DiskState>(&disk_state(), "an unknown state", |d| {
        d["state"] = json!("full");
    });
}

#[test]
fn reconcile_documents() {
    let doc = json!({
        "schema": "cardinal.reconcile.v1", "trigger": "job-end", "skipped": null,
        "disk": [disk_state()], "observed": true, "disk_reports": [disk_report()],
        "recovered": [{"plan_id": "0123456789abcdef", "status": "executed"},
                      {"plan_id": "0123456789abcdee", "status": "rejected"}],
        "inbox": {"raised": ["disk_low:af63dc4c8601ec8c@5"], "cleared": []},
        "arbiter": "interval", "errors": [{"scope": "observe", "message": "m"}]
    });
    round_trips::<Reconcile>(&doc);
    rejects_wrong_schema_and_missing::<Reconcile>(&doc, "disk_reports");
    rejects_wrong_schema_and_missing::<Reconcile>(&doc, "recovered");
    rejects::<Reconcile>(&doc, "a recovered execution with an unknown status", |d| {
        d["recovered"][0]["status"] = json!("done");
    });
    rejects::<Reconcile>(&doc, "a disk report target with an unknown status", |d| {
        d["disk_reports"][0]["targets"][1]["status"] = json!("done");
    });
    let busy = json!({
        "schema": "cardinal.reconcile.v1", "trigger": "timer", "skipped": "busy", "disk": [],
        "observed": false, "disk_reports": [], "recovered": [],
        "inbox": {"raised": [], "cleared": []},
        "arbiter": "not_needed", "errors": []
    });
    round_trips::<Reconcile>(&busy);
    for (field, bad) in [
        ("trigger", json!("job_end")),
        ("skipped", json!("locked")),
        ("arbiter", json!("waiting")),
    ] {
        rejects::<Reconcile>(&doc, "an unknown enum value", |d| d[field] = bad);
    }
    rejects::<Reconcile>(&doc, "a disk report without its shortfall", |d| {
        d["disk_reports"][0]
            .as_object_mut()
            .unwrap()
            .remove("shortfall_bytes");
    });
    // `incomplete` is additive: a report made before it reads as having none.
    let mut before = doc.clone();
    before["disk_reports"][0]
        .as_object_mut()
        .unwrap()
        .remove("incomplete");
    let parsed: Reconcile = serde_json::from_value(before).unwrap();
    assert_eq!(parsed.disk_reports[0].incomplete, Vec::new());
    // `reachable_bytes` is additive too: a report made before it reads as null.
    let mut before = doc.clone();
    before["disk_reports"][0]
        .as_object_mut()
        .unwrap()
        .remove("reachable_bytes");
    let parsed: Reconcile = serde_json::from_value(before).unwrap();
    assert_eq!(parsed.disk_reports[0].reachable_bytes, None);
    rejects::<Reconcile>(&doc, "negative reachable bytes", |d| {
        d["disk_reports"][0]["reachable_bytes"] = json!(-1);
    });
    rejects::<Reconcile>(&doc, "an incomplete worktree without its reason", |d| {
        d["disk_reports"][0]["incomplete"][0]
            .as_object_mut()
            .unwrap()
            .remove("reason");
    });

    let state = json!({"schema": "cardinal.reconcile-state.v1", "last_pass_ms": 7,
                       "last_observation_ms": null});
    round_trips::<ReconcileState>(&state);
    rejects_wrong_schema_and_missing::<ReconcileState>(&state, "last_pass_ms");
    // The runs of `not_in_use` read as `unknown` (section 9.8), absent when there are none.
    let streaks = json!({"schema": "cardinal.reconcile-state.v1", "last_pass_ms": 7,
                         "last_observation_ms": 6,
                         "in_use_unknown": [{"worktree": "/w", "streak": 3,
                                             "input": "session lease: /s/leases/k.json: corrupt"}]});
    round_trips::<ReconcileState>(&streaks);
    let parsed: ReconcileState = serde_json::from_value(state.clone()).unwrap();
    assert_eq!(parsed.in_use_unknown, Vec::new());
    rejects::<ReconcileState>(&streaks, "a run without its input", |d| {
        d["in_use_unknown"][0]
            .as_object_mut()
            .unwrap()
            .remove("input");
    });
}

#[test]
fn arbiter_pass_document() {
    round_trips::<ArbiterPass>(&arbiter_pass());
    let running = json!({
        "schema": "cardinal.arbiter-pass.v1", "id": "fedcba9876543210", "started_ms": 1,
        "ended_ms": null, "outcome": null, "items": [], "log": "/l", "failures_in_a_row": 0
    });
    round_trips::<ArbiterPass>(&running);
    rejects_wrong_schema_and_missing::<ArbiterPass>(&arbiter_pass(), "items");
    for bad in ["failed", "failed:", "done"] {
        rejects::<ArbiterPass>(&arbiter_pass(), "an unknown outcome", |d| {
            d["outcome"] = json!(bad);
        });
    }
}

#[test]
fn snapshot_document() {
    let mut item = inbox_item();
    item["needs_owner"] = json!(true);
    let doc = json!({
        "schema": "cardinal.snapshot.v1", "generated_ms": 11, "host": host(),
        "disk": [disk_state()],
        "observation": {"observed_ms": 10, "repos": [repo()],
                        "errors": [{"scope": "repo:/r", "message": "git failed"}]},
        "reservations": {"observed_ms": 10, "items": [view()]},
        "inbox": [item],
        "records": [record()],
        "arbiter": arbiter_pass(),
        "errors": [{"scope": "inbox", "message": "1 item could not be read"}]
    });
    round_trips::<Snapshot>(&doc);
    rejects_wrong_schema_and_missing::<Snapshot>(&doc, "reservations");
    rejects::<Snapshot>(&doc, "an item without needs_owner", |d| {
        d["inbox"][0].as_object_mut().unwrap().remove("needs_owner");
    });
    // A snapshot written before `errors` and the observation's `errors` existed still loads.
    let mut old = doc.clone();
    old.as_object_mut().unwrap().remove("errors");
    old["observation"].as_object_mut().unwrap().remove("errors");
    let loaded: Snapshot = serde_json::from_value(old).unwrap();
    assert_eq!(loaded.errors, []);
    assert_eq!(loaded.observation.unwrap().errors, []);
    let empty = json!({
        "schema": "cardinal.snapshot.v1", "generated_ms": 11, "host": host(), "disk": [],
        "observation": null, "reservations": {"observed_ms": 10, "items": []}, "inbox": [],
        "records": [], "arbiter": null, "errors": []
    });
    round_trips::<Snapshot>(&empty);
}

#[test]
fn plan_documents() {
    round_trips::<Plan>(&plan());
    rejects_wrong_schema_and_missing::<Plan>(&plan(), "target");
    // A plan stored before `git_dir`, `aliased_bytes`, and the freed-bytes bound were recorded
    // still loads, with each null.
    let mut old = plan();
    old["target"].as_object_mut().unwrap().remove("git_dir");
    for field in ["aliased_bytes", "exclusive_bytes", "exclusive_measure"] {
        old.as_object_mut().unwrap().remove(field);
    }
    let loaded: Plan = serde_json::from_value(old).unwrap();
    assert_eq!(loaded.target.git_dir, None);
    assert_eq!(loaded.aliased_bytes, None);
    assert_eq!(
        (loaded.exclusive_bytes, loaded.exclusive_measure),
        (None, None)
    );
    rejects::<Plan>(&plan(), "an unknown measure", |d| {
        d["exclusive_measure"] = json!("blocks");
    });
    rejects::<Plan>(&plan(), "an unknown kind", |d| {
        d["kind"] = json!("reclaim_cache");
    });
    rejects::<Plan>(&plan(), "an unknown check role", |d| {
        d["checks"][0]["role"] = json!("advice");
    });
    rejects::<Plan>(&plan(), "an unknown approval", |d| {
        d["approval"] = json!("arbiter");
    });

    let list = json!({"schema": "cardinal.plan-list.v1", "plans": [plan()],
                      "errors": [{"scope": "s", "message": "m"}]});
    round_trips::<PlanList>(&list);
    rejects_wrong_schema_and_missing::<PlanList>(&list, "plans");
}

#[test]
fn act_result_document() {
    let doc = json!({
        "schema": "cardinal.act-result.v2", "plan_id": "fedcba9876543210", "caller": "unit:disk",
        "status": "already_executed", "reasons": ["identity_changed:head"],
        "checks": [check("not_in_use", "invariant", "unknown")], "freed_bytes": null,
        "skipped_changed": ["/w/new.txt"], "trash": null, "recovered": false, "error": null,
        "finished_ms": 8
    });
    round_trips::<ActResult>(&doc);
    rejects_wrong_schema_and_missing::<ActResult>(&doc, "status");
    rejects::<ActResult>(&doc, "an unknown status", |d| d["status"] = json!("done"));
    // Version 1, whose dry run's `freed_bytes` meant allocated bytes.
    rejects::<ActResult>(&doc, "version 1", |d| {
        d["schema"] = json!("cardinal.act-result.v1");
    });
    // A partial execution's result as recovery writes it to `executions/<plan>/result.json`.
    let partial = json!({
        "schema": "cardinal.act-result.v2", "plan_id": "fedcba9876543210",
        "caller": "unit:disk-reclaim", "status": "partial", "reasons": ["in_use_while_deleting"],
        "checks": [], "freed_bytes": 4096, "skipped_changed": ["debug/app", "debug/app.d"],
        "trash": "/state/trash/fedcba9876543210", "recovered": true, "error": null,
        "finished_ms": 9
    });
    round_trips::<ActResult>(&partial);
    // A result recorded before `trash` and `recovered` existed still loads.
    let mut old = doc.clone();
    old.as_object_mut().unwrap().remove("trash");
    old.as_object_mut().unwrap().remove("recovered");
    let loaded: ActResult = serde_json::from_value(old).unwrap();
    assert_eq!((loaded.trash, loaded.recovered), (None, false));
}

#[test]
fn act_claim_document() {
    let doc = json!({
        "schema": "cardinal.act-claim.v1", "plan_id": "fedcba9876543210", "caller": "owner",
        "stage": "claimed", "created_ms": 5, "updated_ms": 6,
        "worktree": {"path": "/w", "root": {"dev": 1, "ino": 2}},
        "repo": {"common_dir": "/w/.git", "remote": "https://github.com/o/r.git"},
        "chain": [{"name": "packages", "dev": 1, "ino": 3}, {"name": "app", "dev": 1, "ino": 4}],
        "target": {"name": "node_modules", "dev": 1, "ino": 5, "type": "dir"},
        "manifest_entries": 120
    });
    round_trips::<ActClaim>(&doc);
    let mut committed = doc.clone();
    committed["stage"] = json!("committed");
    committed["chain"] = json!([]);
    committed["repo"]["remote"] = json!(null);
    round_trips::<ActClaim>(&committed);
    rejects_wrong_schema_and_missing::<ActClaim>(&doc, "chain");
    rejects::<ActClaim>(&doc, "a claim without its repository", |d| {
        d.as_object_mut().unwrap().remove("repo");
    });
    rejects::<ActClaim>(&doc, "an unknown stage", |d| d["stage"] = json!("moved"));
    rejects::<ActClaim>(&doc, "an unknown target type", |d| {
        d["target"]["type"] = json!("fifo");
    });
    rejects::<ActClaim>(&doc, "a worktree root without its inode", |d| {
        d["worktree"]["root"].as_object_mut().unwrap().remove("ino");
    });
}

#[test]
fn act_manifest_document() {
    let header = json!({"schema": "cardinal.act-manifest.v1", "plan_id": "fedcba9876543210",
                        "entries": 3});
    round_trips::<ActManifest>(&header);
    rejects_wrong_schema_and_missing::<ActManifest>(&header, "entries");

    let entry = |path: Value| {
        let mut entry = json!({"i": 1, "dev": 1, "ino": 7, "type": "file", "size": 10,
                               "mtime_ns": 1_700_000_000_123_456_789_i64, "nlink": 1});
        entry
            .as_object_mut()
            .unwrap()
            .extend(path.as_object().unwrap().clone());
        entry
    };
    // The target itself, a UTF-8 name, and a name that is not UTF-8 (`f`, 0xff).
    for (path, expected) in [
        (json!({"path": ""}), EntryPath::Utf8(String::new())),
        (
            json!({"path": "debug/app"}),
            EntryPath::Utf8("debug/app".to_owned()),
        ),
        (
            json!({"path_b64": "Zv8="}),
            EntryPath::Bytes(vec![b'f', 0xff]),
        ),
    ] {
        let doc = entry(path);
        round_trips::<ManifestEntry>(&doc);
        let parsed: ManifestEntry = serde_json::from_value(doc).unwrap();
        assert_eq!(parsed.path, expected);
    }
    for (path, what) in [
        (json!({}), "an entry without a path"),
        (
            json!({"path": "a", "path_b64": "YQ=="}),
            "an entry with both paths",
        ),
        (
            json!({"path_b64": "not base64!"}),
            "a path_b64 that is not base64",
        ),
    ] {
        let doc = entry(path);
        assert!(
            serde_json::from_value::<ManifestEntry>(doc.clone()).is_err(),
            "{what} was accepted: {doc}"
        );
    }
    let doc = entry(json!({"path": "debug"}));
    rejects::<ManifestEntry>(&doc, "an unknown type", |d| d["type"] = json!("socket"));
    rejects::<ManifestEntry>(&doc, "a missing mtime", |d| {
        d.as_object_mut().unwrap().remove("mtime_ns");
    });
}

#[test]
fn inbox_documents() {
    round_trips::<InboxItem>(&inbox_item());
    rejects_wrong_schema_and_missing::<InboxItem>(&inbox_item(), "evidence");
    // Every kind of section 10.1, and a worktree subject with a null branch (detached HEAD).
    for kind in [
        "recovery_needed",
        "outlived_processes",
        "reclaimable",
        "merge_undecided",
        "closed_pr_dirty",
        "managed_merged",
        "unclassifiable_data",
        "disk_low",
        "disk_critical",
        "arbiter_failing",
        "reclaim_stranded",
        "in_use_unknown",
    ] {
        let mut doc = inbox_item();
        doc["kind"] = json!(kind);
        doc["subject"] = json!({"worktree": "/w", "head": "abc", "branch": null});
        doc["arbiter"] = json!(null);
        doc["ended_ms"] = json!(9);
        round_trips::<InboxItem>(&doc);
    }
    rejects::<InboxItem>(&inbox_item(), "an unknown kind", |d| {
        d["kind"] = json!("disk_full");
    });
    rejects::<InboxItem>(&inbox_item(), "an unknown pass outcome", |d| {
        d["arbiter"]["outcome"] = json!("lost");
    });
    for outcome in ["resolved", "escalated", "dismissed", "deferred"] {
        let mut doc = inbox_item();
        doc["arbiter"]["answer"]["outcome"] = json!(outcome);
        round_trips::<InboxItem>(&doc);
    }
    rejects::<InboxItem>(&inbox_item(), "an unknown answer", |d| {
        d["arbiter"]["answer"]["outcome"] = json!("ignored");
    });
    // An item assigned before passes answered still loads, with no answer.
    let mut unanswered = inbox_item();
    unanswered["arbiter"]
        .as_object_mut()
        .unwrap()
        .remove("answer");
    let loaded: InboxItem = serde_json::from_value(unanswered).unwrap();
    assert_eq!(loaded.arbiter.unwrap().answer, None);
    // An item stored before occurrences and passes still loads, with both null.
    let mut old = inbox_item();
    old.as_object_mut().unwrap().remove("ended_ms");
    old.as_object_mut().unwrap().remove("arbiter");
    let loaded: InboxItem = serde_json::from_value(old).unwrap();
    assert_eq!((loaded.ended_ms, loaded.arbiter), (None, None));
    for not_an_object in [json!([1]), json!("text"), json!(null), json!(3)] {
        rejects::<InboxItem>(&inbox_item(), "evidence that is not an object", |d| {
            d["evidence"] = not_an_object;
        });
    }
    rejects::<InboxItem>(&inbox_item(), "an unknown status", |d| {
        d["status"] = json!("closed");
    });

    let inbox = json!({"schema": "cardinal.inbox.v1", "items": [inbox_item()]});
    round_trips::<Inbox>(&inbox);
    rejects_wrong_schema_and_missing::<Inbox>(&inbox, "items");

    let item = inbox_item();
    let brief = json!({"schema": "cardinal.inbox-brief.v1", "items": [{
        "key": item["key"], "kind": item["kind"], "status": item["status"],
        "subject": item["subject"], "evidence": {"members": [101]},
        "omitted": {"/evidence/members": 7},
        "truncated": {"/resolution/note": 300}, "arbiter": item["arbiter"],
        "resolution": item["resolution"]
    }]});
    round_trips::<InboxBrief>(&brief);
    rejects_wrong_schema_and_missing::<InboxBrief>(&brief, "items");
    for field in ["omitted", "truncated"] {
        rejects::<InboxBrief>(&brief, &format!("an item without {field}"), |d| {
            d["items"][0].as_object_mut().unwrap().remove(field);
        });
    }
}

#[test]
fn worktree_document() {
    let mut repo = repo();
    repo["worktrees"] = json!([]);
    let doc = json!({
        "schema": "cardinal.worktree.v1", "observed_ms": 10, "repo": repo,
        "worktree": worktree(), "ignored_entries": 40,
        "unclassifiable_set": "0123456789abcdef",
        "errors": [{"scope": "worktree:/w", "message": "status failed"}]
    });
    round_trips::<WorktreeView>(&doc);
    rejects_wrong_schema_and_missing::<WorktreeView>(&doc, "ignored_entries");
    let mut none = doc.clone();
    none["unclassifiable_set"] = json!(null);
    round_trips::<WorktreeView>(&none);
}

#[test]
fn explain_document() {
    let doc = json!({
        "schema": "cardinal.explain.v1", "subject": {"kind": "job", "id": "0123456789abcdef"},
        "summary": "admitted within budget after 12 s",
        "sources": [
            {"kind": "config", "file": "host.toml", "key": "budget.cpu", "value": 8},
            {"kind": "rule", "rule": "generic/test", "pattern": "pnpm test",
             "normalized_argv": ["pnpm", "test"]},
            {"kind": "observation", "command": ["git", "status"], "exit": 0, "result": "clean"},
            {"kind": "record", "record": record()}
        ],
        "rewritten": null, "outcome": "runner_died"
    });
    round_trips::<Explain>(&doc);
    rejects_wrong_schema_and_missing::<Explain>(&doc, "sources");
    rejects::<Explain>(&doc, "a record source that is not an object", |d| {
        d["sources"][3]["record"] = json!("line");
    });
    rejects::<Explain>(&doc, "an unknown source kind", |d| {
        d["sources"][0]["kind"] = json!("guess");
    });
}

#[test]
fn record_document() {
    round_trips::<Record>(&record());
    rejects_wrong_schema_and_missing::<Record>(&record(), "kind");
    for not_an_object in [json!([1]), json!("text"), json!(null)] {
        rejects::<Record>(&record(), "data that is not an object", |d| {
            d["data"] = not_an_object;
        });
    }
    rejects::<Record>(&record(), "an unknown kind", |d| {
        d["kind"] = json!("job_lost");
    });
    for kind in [
        "disk_state",
        "reconcile",
        "lease_corrupt",
        "lease_corrupt_removed",
        "arbiter_pass",
        "planned",
        "inbox_raised",
    ] {
        let mut doc = record();
        doc["kind"] = json!(kind);
        round_trips::<Record>(&doc);
    }
}

/// The schemas tested above: every `cardinal.<name>.v<N>` that section 11.3 and the
/// classifier seam of section 7.6 name.
const TESTED: &[&str] = &[
    "cardinal.error.v1",
    "cardinal.run-spec.v1",
    "cardinal.reservation.v1",
    "cardinal.inventory.v1",
    "cardinal.plan.v1",
    "cardinal.plan-list.v1",
    "cardinal.act-result.v2",
    "cardinal.act-claim.v1",
    "cardinal.act-manifest.v1",
    "cardinal.inbox-item.v1",
    "cardinal.inbox.v1",
    "cardinal.inbox-brief.v1",
    "cardinal.worktree.v1",
    "cardinal.explain.v1",
    "cardinal.record.v1",
    "cardinal.session-snapshot.v1",
    "cardinal.session-lease.v1",
    "cardinal.disk-state.v1",
    "cardinal.reconcile.v1",
    "cardinal.reconcile-state.v1",
    "cardinal.arbiter-pass.v1",
    "cardinal.snapshot.v1",
    "cardinal.classify-request.v1",
    "cardinal.classify-response.v1",
];

#[test]
fn every_documented_schema_is_tested() {
    let doc = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/architecture.md"),
    )
    .unwrap();
    let mut documented = std::collections::BTreeSet::new();
    for (start, _) in doc.match_indices("cardinal.") {
        let name: String = doc[start..]
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || ".-".contains(*c))
            .collect();
        let name = name.trim_end_matches('.');
        let versioned = name
            .rsplit_once(".v")
            .is_some_and(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if versioned && name.matches('.').count() == 2 {
            documented.insert(name.to_owned());
        }
    }
    let tested: std::collections::BTreeSet<String> =
        TESTED.iter().map(|s| (*s).to_owned()).collect();
    // `cardinal.job.v1` is a phase 2 placeholder (section 11.4).
    documented.remove("cardinal.job.v1");
    assert_eq!(documented, tested);
}

#[test]
fn session_snapshot_document() {
    let doc = json!({
        "schema": "cardinal.session-snapshot.v1", "session_id": "s", "reason": "clear",
        "ended_ms": 4, "agent": {"pid": 1, "start_s": 2, "name": "claude"},
        "processes": [{"pid": 3, "start_s": 4, "name": "node", "command": "node server.js"}]
    });
    round_trips::<SessionSnapshot>(&doc);
    rejects_wrong_schema_and_missing::<SessionSnapshot>(&doc, "agent");
}

fn lease() -> Value {
    json!({
        "schema": "cardinal.session-lease.v1", "worktree": "/w",
        "sessions": [
            {"session_id": "s1",
             "agent": {"pid": 10, "start_s": 2, "start_us": 2_000_100, "boot_id": "b"},
             "transcript_path": "/t/s1.jsonl", "last_event": "session_start",
             "last_event_ms": 5, "ended_ms": null},
            {"session_id": "s2", "agent": null, "transcript_path": null,
             "last_event": "subagent_stop", "last_event_ms": 6, "ended_ms": 6}
        ]
    })
}

/// The session lease's states (section 9.8): plain entries, an `uncertain` entry standing for a
/// corrupt file's, and a file marked `overflow`; both markers exist only as `true`.
#[test]
fn session_lease_documents() {
    round_trips::<SessionLease>(&lease());
    rejects_wrong_schema_and_missing::<SessionLease>(&lease(), "sessions");

    let mut uncertain = lease();
    uncertain["sessions"][1]["uncertain"] = json!(true);
    uncertain["sessions"][1]["last_event"] = json!("session_end");
    round_trips::<SessionLease>(&uncertain);
    let parsed: SessionLease = serde_json::from_value(uncertain.clone()).unwrap();
    assert!(parsed.sessions[1].uncertain && !parsed.sessions[0].uncertain);

    let mut overflow = lease();
    overflow["overflow"] = json!(true);
    round_trips::<SessionLease>(&overflow);
    let parsed: SessionLease = serde_json::from_value(overflow.clone()).unwrap();
    assert!(parsed.overflow);
    assert!(
        !serde_json::from_value::<SessionLease>(lease())
            .unwrap()
            .overflow
    );

    rejects::<SessionLease>(&overflow, "overflow false", |d| {
        d["overflow"] = json!(false);
    });
    rejects::<SessionLease>(&uncertain, "uncertain false", |d| {
        d["sessions"][1]["uncertain"] = json!(false);
    });
    rejects::<SessionLease>(&lease(), "an unknown event", |d| {
        d["sessions"][0]["last_event"] = json!("pre_tool_use");
    });
    rejects::<SessionLease>(&lease(), "an agent without its boot ID", |d| {
        d["sessions"][0]["agent"]
            .as_object_mut()
            .unwrap()
            .remove("boot_id");
    });
    for (at, key) in [
        ("/sessions/1", "agent"),
        ("/sessions/1", "transcript_path"),
        ("/sessions/0", "ended_ms"),
        ("/sessions/0/agent", "start_us"),
    ] {
        rejects::<SessionLease>(&lease(), &format!("{at} without {key}"), |d| {
            d.pointer_mut(at)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(key);
        });
    }
    let mut unread = lease();
    unread["sessions"][0]["agent"]["start_us"] = Value::Null;
    round_trips::<SessionLease>(&unread);
    rejects::<SessionLease>(&lease(), "an entry without its last event time", |d| {
        d["sessions"][0]
            .as_object_mut()
            .unwrap()
            .remove("last_event_ms");
    });
}

#[test]
fn classify_documents() {
    round_trips::<ClassifyRequest>(&classify_request());
    rejects_wrong_schema_and_missing::<ClassifyRequest>(&classify_request(), "tool");
    for name in ["PowerShell", "bash", ""] {
        rejects::<ClassifyRequest>(&classify_request(), "a tool other than Bash", |d| {
            d["tool"]["name"] = json!(name);
        });
    }

    let wrap = json!({"schema": "cardinal.classify-response.v1", "decision": "wrap",
                      "class": "test", "rule": "generic/test", "resources": {"cpu": 2},
                      "reason": "matched"});
    round_trips::<ClassifyResponse>(&wrap);
    let pass = json!({"schema": "cardinal.classify-response.v1", "decision": "pass",
                      "reason": "no rule"});
    round_trips::<ClassifyResponse>(&pass);
    rejects_wrong_schema_and_missing::<ClassifyResponse>(&wrap, "reason");
    rejects::<ClassifyResponse>(&wrap, "a zero resource", |d| {
        d["resources"]["cpu"] = json!(0);
    });
    rejects::<ClassifyResponse>(&wrap, "an unknown decision", |d| {
        d["decision"] = json!("defer");
    });
}
