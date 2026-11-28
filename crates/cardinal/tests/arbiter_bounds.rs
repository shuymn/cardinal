//! The bound on what a pass the timer started reads (`docs/architecture.md` sections 10.5 and
//! 14.3): on a host with 200 worktrees and a pass's worth of items of each kind, every command
//! the skill runs in such a pass prints at most [`PASS_OUTPUT`] bytes when it exits 0, and
//! `act`, which exits 3 when it refuses, at most [`REFUSAL_OUTPUT`], while the whole inventory
//! and the full assigned items, which a pass read before, are each larger than Claude Code shows
//! inline.
//!
//! Claude Code (2.1.322 and 2.1.325 here) shows a successful Bash command's output inline up to
//! `bashOutputMaxChars`, 30,000 characters by default, and saves a longer one to a file the
//! arbiter's tools cannot read, showing a 2,000-character preview; a failing command's output
//! is cut to a head-and-tail excerpt of about 10,000 characters (tools reference, "Output
//! limits"). A character takes at least one byte of the UTF-8 output, so bytes bound characters.
//!
//! The items are raised with the evidence each raiser writes, every list at its longest and
//! every free-form string long; the launcher runs `cardinal inbox --assigned --brief` itself as
//! the pass's session, so the pass log holds exactly what a session reads.

mod observe_common;
mod plan_support;
mod support;

use std::path::Path;
use std::time::Duration;

use cardinal::config::MAX_ITEMS;
use cardinal::contract::{
    ArbiterPass, BriefItem, Check, InboxBrief, InboxKind, InboxSubject, Plan, PlanKind, RecordKind,
    WorktreeView,
};
use cardinal::inbox::{self, NewItem};
use cardinal::observe::unclassifiable_set;
use cardinal::plan::FACT_ENTRIES;
use cardinal::state::StateDir;
use plan_support::Setup;
use serde_json::{Value, json};
use support::cardinal::Dirs;

/// Claude Code's default inline limit for a successful command, in characters.
const INLINE: usize = 30_000;

/// Claude Code's excerpt of a failing command, in characters.
const FAILING_INLINE: usize = 10_000;

/// The most a timer pass's command that exits 0 prints: half of [`INLINE`].
const PASS_OUTPUT: usize = INLINE / 2;

/// The most `act` prints, which exits 3 when it refuses: 60% of [`FAILING_INLINE`].
const REFUSAL_OUTPUT: usize = FAILING_INLINE * 6 / 10;

/// How long a `cardinal` command may take.
const WAIT: Duration = Duration::from_secs(120);

/// A worktree path as long as an app's worktree root makes them.
fn long_path(i: usize) -> String {
    format!(
        "/Users/someone/.t3/worktrees/a-typescript-monorepo/t3-{i:08x}-0123456789abcdef/packages/feature-{i:04}"
    )
}

/// The evidence of each assignable kind as its raiser writes it, every list at its longest.
fn evidence(kind: InboxKind, path: &str) -> Value {
    let facts = json!({
        "dirty": "yes", "dirty_unknown": null, "has_upstream": "yes", "unpushed": "no",
        "open_pr": "no", "merge_evidence": "merged_pr",
        "merged_pr": {"number": 4242, "url": "https://github.com/acme/app/pull/4242"},
        "content_in_base": "unknown",
        "content_diff": (0..20).map(|i| format!("packages/feature/src/module-{i}/index.ts")).collect::<Vec<_>>(),
        "closed_pr": {"number": 4241, "url": "https://github.com/acme/app/pull/4241"},
        "in_use": {"value": false, "jobs": []}
    });
    let checks: Vec<Value> = [
        "observed",
        "not_in_use",
        "not_managed",
        "clean",
        "no_unpushed",
        "no_open_pr",
        "has_upstream",
        "plain_target",
        "no_protected",
        "no_unclassifiable",
        "no_nested_repository",
        "merge_evidence",
    ]
    .iter()
    .map(|name| {
        json!({"name": name, "role": "invariant", "result": "pass", "detail": "",
               "facts": [{"name": "worktree.ignored", "source": "git", "omitted": 400,
                          "value": (0..10).map(|i| json!({"path": format!("packages/p-{i}/node_modules"), "class": "regenerable"})).collect::<Vec<_>>()}]})
    })
    .collect();
    match kind {
        InboxKind::Reclaimable => json!({
            "observed_ms": 1, "plan_id": "0123456789abcdef", "repo": "/r/.git",
            "checks_ms": 1, "checks": checks
        }),
        InboxKind::MergeUndecided => json!({
            "observed_ms": 1, "plan_id": "0123456789abcdef", "repo": "/r/.git",
            "checks_ms": 1, "checks": checks, "merge_evidence": "none",
            "content_in_base": "yes", "content_diff": facts["content_diff"],
            "closed_pr": facts["closed_pr"]
        }),
        InboxKind::ClosedPrDirty => json!({
            "observed_ms": 1, "repo": "/r/.git", "closed_pr": facts["closed_pr"], "facts": facts
        }),
        InboxKind::ManagedMerged => json!({
            "observed_ms": 1, "repo": "/r/.git", "managed_by": "t3code", "facts": facts
        }),
        InboxKind::UnclassifiableData => json!({
            "observed_ms": 1, "repo": "/r/.git", "entries": 900,
            "paths": (0..50).map(|i| json!({"path": format!("{path}/generated/cache-{i:03}"), "allocated_bytes": 4096})).collect::<Vec<_>>(),
            "errors": (0..10).map(|i| json!({"scope": format!("worktree:{path}"), "message": format!("{i}: {}", "e".repeat(200))})).collect::<Vec<_>>()
        }),
        InboxKind::RecoveryNeeded => json!({
            "schema": "cardinal.reservation.v1", "job_id": "0123456789abcdef", "state": "running",
            "boot_s": 1, "created_ms": 1, "updated_ms": 1,
            "runner": {"pid": 100, "start_s": 1},
            "group": {"kind": "pgid", "pgid": 100, "leader_start_s": 1},
            "child": {"pid": 101, "start_s": 1}, "stop_signal_ms": null, "priority": "normal",
            "resources": {"cpu": 4}, "admission": "within_budget", "class": "test",
            "rule": "generic/test",
            "worktree": {"key": "af63dc4c8601ec8c", "path": path, "protected": true},
            "session": {"id": "s", "agent_id": null, "tool_use_id": "toolu_1", "stage_index": 0},
            "cwd": path, "argv": (0..64).map(|i| format!("--flag-{i}={}", "v".repeat(240))).collect::<Vec<_>>(),
            "derived": "recovery_needed", "verified": false,
            "members": (0..300).map(|pid| json!({"pid": 1000 + pid, "start_s": 1, "name": "node-worker-process-name"})).collect::<Vec<_>>()
        }),
        InboxKind::OutlivedProcesses => json!({
            "session_id": "0f0e0d0c-0b0a-0908-0706-050403020100", "reason": "other",
            "ended_ms": 1, "agent": {"pid": 99, "start_s": 1, "name": "claude"},
            "processes": (0..300).map(|pid| json!({"pid": 2000 + pid, "start_s": 1, "name": "node", "command": format!("node {} --watch", "x".repeat(4000))})).collect::<Vec<_>>()
        }),
        _ => unreachable!("{kind:?} is not assigned"),
    }
}

/// The key a raiser gives an item of `kind` about `path`.
fn key(kind: InboxKind, path: &str, i: usize) -> String {
    let condition = match kind {
        InboxKind::Reclaimable => format!("reclaimable:{path}@{i:040x}"),
        InboxKind::MergeUndecided => format!("merge_undecided:{path}@{i:040x}"),
        InboxKind::ClosedPrDirty => format!("closed_pr_dirty:{path}#{i}"),
        InboxKind::ManagedMerged => format!("managed_merged:{path}@{i:040x}"),
        InboxKind::UnclassifiableData => format!("unclassifiable:{path}:{i:016x}"),
        InboxKind::RecoveryNeeded => format!("recovery_needed:{i:016x}"),
        InboxKind::OutlivedProcesses => format!("outlived:session-{i:08}"),
        _ => unreachable!(),
    };
    format!("{condition}@1700000000000")
}

/// Every kind a pass is assigned.
const KINDS: [InboxKind; 7] = [
    InboxKind::Reclaimable,
    InboxKind::MergeUndecided,
    InboxKind::ClosedPrDirty,
    InboxKind::ManagedMerged,
    InboxKind::UnclassifiableData,
    InboxKind::RecoveryNeeded,
    InboxKind::OutlivedProcesses,
];

/// How many items the host holds: `max_items` of each kind, as many as a pass may take.
/// Every command's output is bounded per pass, so more items of a kind would repeat the same
/// passes, each reading the whole inbox again.
const ITEMS: usize = KINDS.len() * MAX_ITEMS;

/// Raises [`ITEMS`] items kind by kind, so a pass may take `max_items` of one kind's largest
/// evidence; returns their keys, oldest first.
fn raise_items(state: &StateDir) -> Vec<String> {
    let mut keys = Vec::new();
    for i in 0..ITEMS {
        let kind = KINDS[i / (ITEMS / KINDS.len())];
        let path = long_path(i);
        let item = NewItem {
            key: key(kind, &path, i),
            kind,
            subject: InboxSubject {
                worktree: Some(path.clone()),
                head: Some(Some(format!("{i:040x}"))),
                branch: Some(Some(format!("feature/a-descriptive-branch-name-{i:04}"))),
                ..InboxSubject::default()
            },
            evidence: evidence(kind, &path).as_object().cloned().unwrap(),
        };
        inbox::raise(state, &item).unwrap();
        keys.push(item.key);
    }
    keys
}

/// Gives every item in the inbox the same `created_ms`, as when one pass raises them all
/// within a millisecond, and returns their keys in the order they are due: `created_ms`, ties
/// going by key (`docs/architecture.md` section 10.5), so across kinds not the order they
/// were raised in.
fn raise_together(state: &StateDir) -> Vec<String> {
    let mut items = inbox::read_all(state).unwrap();
    let created_ms = items.iter().map(|(_, item)| item.created_ms).min().unwrap();
    for (path, item) in &mut items {
        item.created_ms = created_ms;
        let bytes = serde_json::to_vec(item).unwrap();
        state
            .write_atomic(path, &bytes, cardinal::state::Durability::Durable)
            .unwrap();
    }
    let mut due: Vec<_> = items
        .into_iter()
        .map(|(_, item)| (item.created_ms, item.key))
        .collect();
    due.sort();
    due.into_iter().map(|(_, key)| key).collect()
}

/// A host whose `[arbiter] command` is `cardinal inbox --assigned --brief`, so each pass's log
/// holds exactly what a session reads, with `max_items` items at most.
fn listing_host(max_items: usize) -> Dirs {
    let dirs = Dirs::new();
    let cardinal = cardinal_test_support::bin_exe("cardinal");
    let host = format!(
        "[arbiter]\ncommand = {}\ntimeout_s = 60\nmin_interval_s = 0\nretry_s = 3600\nmax_items = {max_items}\n",
        serde_json::to_string(&[
            cardinal.to_str().expect("a UTF-8 path"),
            "inbox",
            "--assigned",
            "--brief"
        ])
        .unwrap()
    );
    std::fs::write(dirs.config().join("host.toml"), host).unwrap();
    dirs
}

/// Runs passes until every one of `keys`, due in that order, was listed, checking that each
/// pass takes the next of them, at most `max_items`, and lists them within [`PASS_OUTPUT`];
/// returns how many items each pass took and the largest listing in bytes.
fn list_in_passes(dirs: &Dirs, keys: &[String], max_items: usize) -> (Vec<usize>, usize) {
    let mut listed = Vec::new();
    let mut taken = Vec::new();
    let mut largest = 0;
    while listed.len() < keys.len() {
        let output = dirs
            .cmd()
            .args(["arbiter", "run"])
            .output_within(WAIT)
            .unwrap();
        let pass: ArbiterPass = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|err| panic!("{err}: {}", String::from_utf8_lossy(&output.stdout)));
        let rest = &keys[listed.len()..];
        assert!(
            !pass.items.is_empty()
                && pass.items.len() <= max_items
                && rest.starts_with(&pass.items),
            "the next due items, at most max_items: {:?}",
            pass.items
        );
        let log = std::fs::read_to_string(&pass.log).unwrap();
        let printed = log.lines().next().unwrap();
        largest = largest.max(printed.len());
        assert!(
            printed.len() <= PASS_OUTPUT,
            "inbox --assigned --brief printed {} bytes",
            printed.len()
        );
        let brief: InboxBrief = serde_json::from_str(printed).unwrap();
        let shown: Vec<_> = brief.items.iter().map(|item| item.key.clone()).collect();
        assert_eq!(shown, pass.items);
        taken.push(shown.len());
        listed.extend(shown);
    }
    assert_eq!(listed, keys);
    (taken, largest)
}

/// A19: `max_items` due items of each kind with the largest evidence its raiser writes, all
/// raised in one millisecond, go to passes of at most `max_items`, in the order they became
/// due (ties by key), the rest waiting for later passes, and `cardinal inbox --assigned
/// --brief` lists each pass's items within [`PASS_OUTPUT`], as does `inbox resolve --brief`
/// for each item, while `cardinal inbox --assigned` would print more than Claude Code shows.
#[test]
fn assigned_items_stay_within_the_bound() {
    let dirs = listing_host(MAX_ITEMS);
    let state = StateDir::open(dirs.state()).unwrap();
    let keys = raise_items(&state);
    let due = raise_together(&state);
    assert_ne!(due, keys, "the tie reorders items across kinds");
    let (taken, largest) = list_in_passes(&dirs, &due, MAX_ITEMS);
    eprintln!("largest inbox --assigned --brief: {largest} bytes; items per pass: {taken:?}");

    // What a pass read before: the same items in full, for the pass whose items are largest.
    let items: Vec<_> = inbox::read_all(&state)
        .unwrap()
        .into_iter()
        .map(|(_, item)| item)
        .collect();
    let mut start = 0;
    let mut full = 0;
    for n in &taken {
        let pass = &due[start..start + n];
        start += n;
        let listing = serde_json::to_string(&cardinal::contract::Inbox {
            schema: cardinal::contract::InboxSchema,
            items: items
                .iter()
                .filter(|item| pass.contains(&item.key))
                .cloned()
                .collect(),
        })
        .unwrap();
        full = full.max(listing.len());
    }
    assert!(full > INLINE, "{full} bytes in full");

    // An answer prints its item as briefly.
    for key in keys.iter().step_by(ITEMS / KINDS.len()) {
        let output = dirs
            .cmd()
            .args([
                "inbox",
                "resolve",
                key,
                "--outcome",
                "escalated",
                "--note",
                &"n".repeat(2000),
                "--brief",
            ])
            .output_within(WAIT)
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{key}");
        assert!(
            output.stdout.len() <= PASS_OUTPUT,
            "resolve --brief printed {} bytes for {key}",
            output.stdout.len()
        );
        // The note is a preview, marked with its full length.
        let answered: InboxBrief = serde_json::from_slice(&output.stdout).unwrap();
        let item = &answered.items[0];
        let note = &item.resolution.as_ref().unwrap().note;
        assert_eq!(note.chars().count(), inbox::TEXT_CHARS);
        assert_eq!(item.truncated["/resolution/note"], 2000);
    }
}

/// Raises `outlived_processes` item `i` with three processes whose commands are
/// `command_bytes` long; returns its key and its brief listing.
fn raise_outlived(state: &StateDir, i: usize, command_bytes: usize) -> (String, BriefItem) {
    let item = NewItem {
        key: format!("outlived:0f0e0d0c-0b0a-0908-0706-{i:012}@1700000000000"),
        kind: InboxKind::OutlivedProcesses,
        subject: InboxSubject {
            session_id: Some(format!("0f0e0d0c-0b0a-0908-0706-{i:012}")),
            ..InboxSubject::default()
        },
        evidence: json!({
            "session_id": format!("0f0e0d0c-0b0a-0908-0706-{i:012}"), "reason": "other",
            "ended_ms": 1_700_000_000_000_u64,
            "agent": {"pid": 99_999, "start_s": 1_700_000_000, "name": "claude"},
            "processes": (0..3).map(|p| json!({
                "pid": 10_000 + 3 * i + p, "start_s": 1_700_000_000, "name": "node",
                "command": format!("node {} --watch", "x".repeat(command_bytes)),
            })).collect::<Vec<_>>(),
        })
        .as_object()
        .cloned()
        .unwrap(),
    };
    inbox::raise(state, &item).unwrap();
    let raised = inbox::read_all(state)
        .unwrap()
        .into_iter()
        .find(|(_, raised)| raised.key == item.key)
        .unwrap()
        .1;
    (item.key, inbox::brief(&raised))
}

/// The bytes of one brief listing of `items`.
fn listing_bytes(items: Vec<BriefItem>) -> usize {
    serde_json::to_string(&InboxBrief {
        schema: cardinal::contract::InboxBriefSchema,
        items,
    })
    .unwrap()
    .len()
}

/// A19: with `max_items` 40, 40 `outlived_processes` items of three processes each, whose
/// brief listing together would exceed [`PASS_OUTPUT`], go to passes that take as many as the
/// listing holds, in order, the rest staying due for the next pass.
#[test]
fn a_pass_takes_as_many_items_as_its_listing_holds() {
    const MANY: usize = 40;
    let dirs = listing_host(MANY);
    let state = StateDir::open(dirs.state()).unwrap();
    let (keys, whole): (Vec<_>, Vec<_>) = (0..MANY).map(|i| raise_outlived(&state, i, 250)).unzip();
    let all = listing_bytes(whole);
    assert!(all > PASS_OUTPUT, "{all} bytes in one listing");
    let (taken, largest) = list_in_passes(&dirs, &keys, MANY);
    assert!(taken[0] < MANY, "{taken:?}");
    eprintln!(
        "40 outlived items: {all} bytes in one listing; items per pass: {taken:?}; largest {largest}"
    );
}

/// A19: with `max_items` 2, five compact items, whose brief listing together fits within
/// [`PASS_OUTPUT`], go to passes of two, two, and one: `max_items`, not the listing, bounds
/// these passes.
#[test]
fn a_pass_takes_at_most_max_items() {
    const CAP: usize = 2;
    let dirs = listing_host(CAP);
    let state = StateDir::open(dirs.state()).unwrap();
    let (keys, whole): (Vec<_>, Vec<_>) = (0..5).map(|i| raise_outlived(&state, i, 10)).unzip();
    let all = listing_bytes(whole);
    assert!(all <= PASS_OUTPUT, "{all} bytes in one listing");
    let (taken, _) = list_in_passes(&dirs, &keys, CAP);
    assert_eq!(taken, [2, 2, 1]);
}

/// A plan, the record of an `act`, and `cardinal explain plan` keep every entry of a list fact;
/// only the commands a timer pass runs, `cardinal plan reclaim-worktree` and `cardinal act`,
/// print it cut to [`FACT_ENTRIES`] entries, with the rest counted in `omitted`.
#[test]
fn stored_facts_stay_whole() {
    const ENTRIES: usize = 12;
    let setup = Setup::new();
    let wt = setup.worktree("wt");
    std::fs::create_dir_all(setup.main.join(".git/info")).unwrap();
    std::fs::write(setup.main.join(".git/info/exclude"), "cache-*/\n").unwrap();
    for i in 0..ENTRIES {
        let dir = wt.join(format!("cache-{i:02}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("blob"), "data").unwrap();
    }
    let ignored = |checks: &[Check]| -> (Value, Option<u64>) {
        let fact = checks
            .iter()
            .find(|check| check.name == "no_unclassifiable")
            .and_then(|check| check.facts.iter().find(|f| f.name == "worktree.ignored"))
            .expect("the ignored entries the check read");
        (fact.value.clone(), fact.omitted)
    };
    let cut = (FACT_ENTRIES, Some((ENTRIES - FACT_ENTRIES) as u64));

    let printed = setup.plan(PlanKind::ReclaimWorktree, &wt);
    let (value, omitted) = ignored(&printed.checks);
    assert_eq!((value.as_array().unwrap().len(), omitted), cut);
    let stored = cardinal::plan::load(&setup.state, &printed.id).unwrap();
    let (whole, omitted) = ignored(&stored.checks);
    assert_eq!((whole.as_array().unwrap().len(), omitted), (ENTRIES, None));
    assert_eq!(
        whole.as_array().unwrap()[..FACT_ENTRIES],
        value.as_array().unwrap()[..]
    );

    let explain = setup
        .cardinal()
        .args(["explain", "plan", &printed.id])
        .output_within(WAIT)
        .unwrap();
    assert_eq!(explain.status.code(), Some(0));
    let explain: Value = serde_json::from_slice(&explain.stdout).unwrap();
    let observed = explain["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|source| {
            source["result"]
                .as_str()?
                .strip_prefix("no_unclassifiable: worktree.ignored = ")
        })
        .expect("the fact in explain");
    assert_eq!(serde_json::from_str::<Value>(observed).unwrap(), whole);

    let result = setup.act(&printed.id, &plan_support::arbiter()).unwrap();
    let (value, omitted) = ignored(&result.checks);
    assert_eq!((value.as_array().unwrap().len(), omitted), cut);
    let recorded = cardinal::record::read_all(&setup.state)
        .unwrap()
        .into_iter()
        .rev()
        .find(|record| record.kind == RecordKind::Act)
        .expect("the act record");
    let checks: Vec<Check> = serde_json::from_value(recorded.data["checks"].clone()).unwrap();
    assert_eq!(ignored(&checks), (whole, None));
}

/// The worktree commands of a timer pass on a repository with 200 linked worktrees and one
/// with 240 ignored entries: `cardinal inventory --worktree PATH` and `cardinal plan
/// reclaim-worktree PATH` print at most [`PASS_OUTPUT`], and `cardinal act PLAN --caller
/// arbiter`, refused, at most [`REFUSAL_OUTPUT`], with or without `--squash-merged`; the view
/// names the same unclassifiable set as the full observation, which a pass read before and
/// which is larger than Claude Code shows.
#[test]
fn worktree_commands_stay_within_the_bound() {
    let setup = Setup::new();
    for i in 0..200 {
        setup.fx.worktree_add(
            &setup.main,
            &format!("t3-{i:08x}-0123456789abcdef-a-feature-worktree"),
            &format!("feature/a-descriptive-branch-name-{i:04}"),
        );
    }
    let big = setup.worktree("t3-ffffffff-0123456789abcdef-the-worktree-with-much-data");
    // Ignored data no profile classifies.
    std::fs::create_dir_all(setup.main.join(".git/info")).unwrap();
    std::fs::write(setup.main.join(".git/info/exclude"), "cache-*/\n").unwrap();
    for i in 0..120 {
        for dir in [
            format!("pkg-{i:03}/node_modules/x"),
            format!("cache-{i:03}"),
        ] {
            let dir = big.join(dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("blob"), "data").unwrap();
        }
    }

    let run = |args: &[&Path], codes: &[i32]| -> Vec<u8> {
        let output = setup
            .cardinal()
            .args(args)
            .output_within(WAIT)
            .expect("run cardinal");
        let code = output.status.code().unwrap_or(-1);
        assert!(
            codes.contains(&code),
            "{args:?}: exit {code}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    let p = Path::new;

    let printed = run(&[p("inventory"), p("--worktree"), &big], &[0]);
    assert!(printed.len() <= PASS_OUTPUT, "{} bytes", printed.len());
    let view: WorktreeView = serde_json::from_slice(&printed).unwrap();
    assert_eq!(view.ignored_entries, 240);
    assert_eq!(view.worktree.ignored.len(), 10);
    let set = view.unclassifiable_set.clone().expect("a set");
    eprintln!("inventory --worktree: {} bytes", printed.len());

    let printed = run(&[p("plan"), p("reclaim-worktree"), &big], &[0]);
    assert!(printed.len() <= PASS_OUTPUT, "{} bytes", printed.len());
    let plan: Plan = serde_json::from_slice(&printed).unwrap();
    assert_eq!(plan.kind, PlanKind::ReclaimWorktree);
    let ignored = plan
        .checks
        .iter()
        .flat_map(|check| &check.facts)
        .find(|fact| fact.name == "worktree.ignored")
        .expect("a fact of the ignored entries");
    assert_eq!(ignored.value.as_array().unwrap().len(), 10);
    assert_eq!(ignored.omitted, Some(230));
    eprintln!("plan reclaim-worktree: {} bytes", printed.len());

    for squash in [false, true] {
        let mut args = vec![p("act"), p(&plan.id), p("--caller"), p("arbiter")];
        if squash {
            args.push(p("--squash-merged"));
        }
        let printed = run(&args, &[3]);
        assert!(printed.len() <= REFUSAL_OUTPUT, "{} bytes", printed.len());
        eprintln!("act (squash {squash}): {} bytes", printed.len());
    }

    // What a pass read before: the whole inventory, whose worktree has the set the view names.
    let inventory = setup.inventory();
    let observed = inventory.repos[0]
        .worktrees
        .iter()
        .find(|worktree| Path::new(&worktree.path) == big)
        .unwrap();
    assert_eq!(unclassifiable_set(observed).map(|(set, _)| set), Some(set));
    let whole = serde_json::to_string(&inventory).unwrap();
    assert!(whole.len() > INLINE, "{} bytes", whole.len());
}
