//! Complete JSON documents of `docs/architecture.md` section 11.3 against the contract types:
//! every field present round-trips unchanged, and each malformed variant is rejected.

use cardinal::contract::{
    ActResult, ArbiterPass, ClassifyRequest, ClassifyResponse, DiskState, ErrorDoc, Explain, Inbox,
    InboxItem, Inventory, Plan, PlanList, Reconcile, ReconcileState, Record, Reservation,
    ReservationView, RunSpec, SessionSnapshot, Snapshot,
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
    let name = doc["schema"].as_str().unwrap().replace(".v1", ".v2");
    rejects::<T>(doc, "another version", |d| d["schema"] = json!(name));
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
    clean["facts"] = json!([{"name": "worktree.facts.dirty", "value": "no", "source": "git"}]);
    json!({
        "schema": "cardinal.plan.v1", "id": "fedcba9876543210", "created_ms": 5,
        "host_id": "00000000000000aa", "kind": "reclaim_worktree",
        "target": {"path": "/w", "repo": "/r/.git", "worktree": "/w", "head": "abc",
                   "branch": "feature", "git_dir": "/r/.git/worktrees/w",
                   "root_id": {"dev": 1, "ino": 2}, "root_created_ms": null},
        "allocated_bytes": 4096, "aliased_bytes": 1024,
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
        "arbiter": {"pass": "fedcba9876543210", "assigned_ms": 2, "outcome": "failed:exit 1",
                    "ended_ms": 3},
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
                     "allocated_bytes": 10, "aliased_bytes": 4},
                    {"path": "/w/fuzz/corpus", "class": "protected", "regenerable_match": false,
                     "allocated_bytes": null, "aliased_bytes": null}],
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
                     "exclusive_bytes": 120}],
        "shortfall_bytes": 30,
        "blocked": [{"worktree": "/w2", "target": "/w2/node_modules", "check": "not_in_use",
                     "bytes": 80},
                    {"worktree": "/w", "target": "/w/.pnpm", "check": "aliased", "bytes": 9}]
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
        "inbox": {"raised": ["disk_low:af63dc4c8601ec8c@5"], "cleared": []},
        "arbiter": "interval", "errors": [{"scope": "observe", "message": "m"}]
    });
    round_trips::<Reconcile>(&doc);
    rejects_wrong_schema_and_missing::<Reconcile>(&doc, "disk_reports");
    let busy = json!({
        "schema": "cardinal.reconcile.v1", "trigger": "timer", "skipped": "busy", "disk": [],
        "observed": false, "disk_reports": [], "inbox": {"raised": [], "cleared": []},
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

    let state = json!({"schema": "cardinal.reconcile-state.v1", "last_pass_ms": 7,
                       "last_observation_ms": null});
    round_trips::<ReconcileState>(&state);
    rejects_wrong_schema_and_missing::<ReconcileState>(&state, "last_pass_ms");
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
        "observation": {"observed_ms": 10, "repos": [repo()]},
        "reservations": {"observed_ms": 10, "items": [view()]},
        "inbox": [item],
        "records": [record()],
        "arbiter": arbiter_pass()
    });
    round_trips::<Snapshot>(&doc);
    rejects_wrong_schema_and_missing::<Snapshot>(&doc, "reservations");
    rejects::<Snapshot>(&doc, "an item without needs_owner", |d| {
        d["inbox"][0].as_object_mut().unwrap().remove("needs_owner");
    });
    let empty = json!({
        "schema": "cardinal.snapshot.v1", "generated_ms": 11, "host": host(), "disk": [],
        "observation": null, "reservations": {"observed_ms": 10, "items": []}, "inbox": [],
        "records": [], "arbiter": null
    });
    round_trips::<Snapshot>(&empty);
}

#[test]
fn plan_documents() {
    round_trips::<Plan>(&plan());
    rejects_wrong_schema_and_missing::<Plan>(&plan(), "target");
    // A plan stored before `git_dir` and `aliased_bytes` were recorded still loads, with both
    // null.
    let mut old = plan();
    old["target"].as_object_mut().unwrap().remove("git_dir");
    old.as_object_mut().unwrap().remove("aliased_bytes");
    let loaded: Plan = serde_json::from_value(old).unwrap();
    assert_eq!(loaded.target.git_dir, None);
    assert_eq!(loaded.aliased_bytes, None);
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
        "schema": "cardinal.act-result.v1", "plan_id": "fedcba9876543210", "caller": "unit:disk",
        "status": "already_executed", "reasons": ["identity_changed:head"],
        "checks": [check("not_in_use", "invariant", "unknown")], "freed_bytes": null,
        "skipped_changed": ["/w/new.txt"], "error": null, "finished_ms": 8
    });
    round_trips::<ActResult>(&doc);
    rejects_wrong_schema_and_missing::<ActResult>(&doc, "status");
    rejects::<ActResult>(&doc, "an unknown status", |d| d["status"] = json!("done"));
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
    "cardinal.act-result.v1",
    "cardinal.inbox-item.v1",
    "cardinal.inbox.v1",
    "cardinal.explain.v1",
    "cardinal.record.v1",
    "cardinal.session-snapshot.v1",
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
