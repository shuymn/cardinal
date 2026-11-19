//! Items raised from a pass's observation and disk states (`docs/architecture.md` sections
//! 10.1 and 10.4; C19, A22, A23): `reclaimable`, `merge_undecided`, `closed_pr_dirty`,
//! `managed_merged`, and `unclassifiable_data` from step 5's observation and plans,
//! `disk_low` and `disk_critical` from the disk states with their disk reports, and
//! `reclaim_stranded` from step 2's trash listing.
//!
//! The raisers run against fixed observations, plans, disk states, and disk reports built from
//! the contract types (section 11.3), as step 5 and step 6 of a pass hand them over. Each test
//! starts from a fixture on which only the condition it names varies, and pairs it with a
//! control that raises or ends nothing.

mod support;

use cardinal::config::Config;
use cardinal::contract::{
    DiskReport, DiskState, InboxItem, InboxKind, InboxStatus, Inventory, Plan, RecordKind,
    ReportReading, ReportTarget, ResolutionOutcome,
};
use cardinal::inbox::{self, StatusFilter};
use cardinal::reconcile::Observation;
use cardinal::reconcile::raise;
use cardinal::state::StateDir;
use std::path::Path;

use cardinal_test_support::fixture::Fixture;
use serde_json::{Value, json};
use support::cardinal::Dirs;

struct Env {
    dirs: Dirs,
    config: Config,
    state: StateDir,
}

fn env() -> Env {
    let dirs = Dirs::new();
    std::fs::write(dirs.config().join("host.toml"), "").unwrap();
    let config = Config::load(dirs.config()).unwrap();
    let state = StateDir::open(dirs.state()).unwrap();
    Env {
        dirs,
        config,
        state,
    }
}

impl Env {
    /// A directory standing for a worktree that exists.
    fn worktree_dir(&self, name: &str) -> String {
        let path = self.dirs.scope().path(name);
        std::fs::create_dir_all(&path).unwrap();
        path.to_str().unwrap().to_owned()
    }

    /// A path standing for a worktree that does not exist.
    fn missing_dir(&self, name: &str) -> String {
        self.dirs.scope().path(name).to_str().unwrap().to_owned()
    }

    /// The items as a pass reads them before step 4.
    fn before(&self) -> raise::Before {
        raise::snapshot(&self.state).unwrap()
    }

    /// Step 5's raiser on `observation`, with the items read just before it.
    fn worktree_pass(&self, observation: &Observation) -> cardinal::contract::InboxChanges {
        self.worktree_pass_after(&self.before(), observation)
    }

    fn worktree_pass_after(
        &self,
        before: &raise::Before,
        observation: &Observation,
    ) -> cardinal::contract::InboxChanges {
        raise::worktree_items(&self.config, &self.state, before, observation).unwrap()
    }

    /// Step 6's raiser on `disks` and `reports`, with the items read just before it.
    fn disk_pass(
        &self,
        disks: &[DiskState],
        reports: &[DiskReport],
    ) -> cardinal::contract::InboxChanges {
        self.disk_pass_after(&self.before(), disks, reports)
    }

    fn disk_pass_after(
        &self,
        before: &raise::Before,
        disks: &[DiskState],
        reports: &[DiskReport],
    ) -> cardinal::contract::InboxChanges {
        raise::disk_items(&self.config, &self.state, before, disks, reports, &[]).unwrap()
    }

    fn items(&self) -> Vec<InboxItem> {
        inbox::list(&self.config, &self.state, Some(StatusFilter::All), false)
            .unwrap()
            .items
    }

    /// The items whose condition is `condition`, oldest first.
    fn occurrences(&self, condition: &str) -> Vec<InboxItem> {
        oldest_first(
            self.items()
                .into_iter()
                .filter(|item| inbox::occurrence_of(&item.key).0 == condition),
        )
    }

    fn resolve(&self, key: &str, outcome: ResolutionOutcome) {
        inbox::resolve(&self.config, &self.state, key, outcome, Some("owner")).unwrap();
    }
}

const H1: &str = "1111111111111111111111111111111111111111";
const H2: &str = "2222222222222222222222222222222222222222";

/// A linked worktree at `path` on `H1`, clean, pushed, with an upstream, not in use, not
/// managed, without merge evidence (`content_in_base` `no`), and with no ignored data.
fn worktree(path: &str) -> Value {
    json!({
        "path": path, "main": false, "head": H1, "branch": "feat", "upstream": "origin/feat",
        "git_dir": null, "last_activity_ms": 1, "managed_by": null,
        "facts": {
            "dirty": "no", "dirty_unknown": null, "has_upstream": "yes", "unpushed": "no",
            "open_pr": "no", "merge_evidence": "none", "merged_pr": null,
            "content_in_base": "no", "content_diff": ["src/lib.rs"], "closed_pr": null,
            "in_use": {"value": false, "jobs": []}
        },
        "ignored": [],
        "reclaim": {"candidate": false, "failed": ["merge_evidence"], "unknown": []},
        "allocated_bytes": null
    })
}

/// An observation at `observed_ms` of one repository holding `worktrees`, with `errors`.
fn inventory(observed_ms: u64, worktrees: &[Value], errors: &[Value]) -> Inventory {
    serde_json::from_value(json!({
        "schema": "cardinal.inventory.v1", "observed_ms": observed_ms,
        "host": {"id": "h", "hostname": "h", "os": "linux", "arch": "x86_64", "boot_s": 1},
        "budget": {}, "reserved": {}, "reservations": [],
        "repos": [{
            "common_dir": "/repo/.git", "main_worktree": "/repo", "profiles": [],
            "github": "o/r", "base": {"ref": "origin/main", "oid": H2}, "gh": "ok",
            "gh_error": null, "worktrees": worktrees
        }],
        "outlived": [], "disk": [], "arbiter": {"running": false, "last": null},
        "errors": errors
    }))
    .unwrap()
}

/// The whole-worktree checks of section 9.2.
const INVARIANTS: [&str; 11] = [
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
];

/// The `reclaim_worktree` plan `id` of the worktree at `path` on `head`, every check passing
/// but those in `failing`.
fn plan(id: &str, path: &str, head: &str, failing: &[&str]) -> Plan {
    let check = |name: &str, role: &str| {
        let result = if failing.contains(&name) {
            "fail"
        } else {
            "pass"
        };
        json!({"name": name, "role": role, "result": result, "detail": "",
               "facts": [{"name": format!("{name}.fact"), "value": result, "source": "git"}]})
    };
    let mut checks: Vec<Value> = INVARIANTS.iter().map(|n| check(n, "invariant")).collect();
    checks.push(check("merge_evidence", "condition"));
    let invariants_ok = !failing.iter().any(|f| INVARIANTS.contains(f));
    let approval = if failing.contains(&"merge_evidence") {
        "owner"
    } else {
        "none"
    };
    serde_json::from_value(json!({
        "schema": "cardinal.plan.v1", "id": id, "created_ms": 1, "host_id": "h",
        "kind": "reclaim_worktree",
        "target": {"path": path, "repo": "/repo/.git", "worktree": path, "head": head,
                   "branch": "feat", "git_dir": null, "root_id": null, "root_created_ms": null},
        "allocated_bytes": null, "aliased_bytes": null, "checks": checks,
        "invariants_ok": invariants_ok, "approval": approval,
        "provenance": {"errors": [], "config": []}
    }))
    .unwrap()
}

fn observation(inventory: Inventory, plans: Vec<Plan>) -> Observation {
    let checks_ms = inventory.observed_ms + 1;
    Observation {
        inventory,
        plans,
        checks_ms,
        own_file_bytes: std::collections::BTreeMap::default(),
    }
}

/// The worktree `wt` with its HEAD and merge evidence set: a merge evidence lets every check
/// of its plan pass.
fn merged(mut wt: Value, head: &str) -> Value {
    wt["head"] = json!(head);
    wt["facts"]["merge_evidence"] = json!("base_ancestor");
    wt["facts"]["content_in_base"] = json!("unknown");
    wt["facts"]["content_diff"] = json!([]);
    wt["reclaim"] = json!({"candidate": true, "failed": [], "unknown": []});
    wt
}

fn assert_resolved_by_cardinal(item: &InboxItem, note: &str) {
    assert!(item.ended_ms.is_some(), "{item:#?}");
    assert_eq!(item.status, InboxStatus::Resolved, "{item:#?}");
    let resolution = item.resolution.as_ref().unwrap();
    assert_eq!(resolution.by, "cardinal", "{item:#?}");
    assert!(resolution.note.starts_with(note), "{item:#?}");
}

fn assert_open(item: &InboxItem) {
    assert_eq!(
        (item.status, item.ended_ms),
        (InboxStatus::Open, None),
        "{item:#?}"
    );
}

/// Section 10.1: a whole-worktree plan that passes every check raises `reclaimable` with the
/// plan and its checks as evidence, and the worktree, HEAD, and branch as its subject; the
/// next pass updates it; a new HEAD ends it and raises the new HEAD's own item; the worktree
/// gone ends that one. A plan that stops passing on the same HEAD keeps it. The pass reports
/// the keys it raised anew and ended, and records them.
#[test]
fn reclaimable_raised_and_ended() {
    let e = env();
    let path = e.worktree_dir("wt");
    let condition = format!("reclaimable:{path}@{H1}");

    let changes = e.worktree_pass(&observation(
        inventory(100, &[merged(worktree(&path), H1)], &[]),
        vec![plan("p1", &path, H1, &[])],
    ));
    let items = e.occurrences(&condition);
    assert_eq!(items.len(), 1, "{:#?}", e.items());
    let item = &items[0];
    assert_open(item);
    assert_eq!(item.key, format!("{condition}@100"));
    assert_eq!(item.kind, InboxKind::Reclaimable);
    assert_eq!(item.subject.worktree.as_deref(), Some(path.as_str()));
    assert_eq!(item.subject.head, Some(Some(H1.to_owned())));
    assert_eq!(item.subject.branch, Some(Some("feat".to_owned())));
    assert_eq!(item.evidence["plan_id"], "p1");
    assert_eq!(item.evidence["observed_ms"], 100);
    assert_eq!(item.evidence["checks"].as_array().unwrap().len(), 12);
    assert_eq!(item.evidence["checks"][0]["facts"][0]["source"], "git");
    assert_eq!(changes.raised, std::slice::from_ref(&item.key));
    assert_eq!(changes.cleared, Vec::<String>::new());

    // The same HEAD whose plan now fails a check keeps its item, updated by nothing.
    let changes = e.worktree_pass(&observation(
        inventory(200, &[merged(worktree(&path), H1)], &[]),
        vec![plan("p2", &path, H1, &["not_in_use"])],
    ));
    assert!(
        changes.raised.is_empty() && changes.cleared.is_empty(),
        "{changes:?}"
    );
    let changes = e.worktree_pass(&observation(
        inventory(300, &[merged(worktree(&path), H1)], &[]),
        vec![plan("p3", &path, H1, &[])],
    ));
    assert!(changes.raised.is_empty(), "{changes:?}");
    let items = e.occurrences(&condition);
    assert_eq!((items.len(), items[0].seen), (1, 2), "{items:#?}");
    assert_eq!(items[0].evidence["plan_id"], "p3");

    let changes = e.worktree_pass(&observation(
        inventory(400, &[merged(worktree(&path), H2)], &[]),
        vec![plan("p4", &path, H2, &[])],
    ));
    let first = &e.occurrences(&condition)[0];
    assert_resolved_by_cardinal(first, "HEAD changed");
    assert_eq!(first.ended_ms, Some(400));
    let second = e.occurrences(&format!("reclaimable:{path}@{H2}"));
    assert_eq!(second.len(), 1, "{:#?}", e.items());
    assert_open(&second[0]);
    assert_eq!(changes.raised, [second[0].key.clone()]);
    assert_eq!(changes.cleared, std::slice::from_ref(&first.key));

    std::fs::remove_dir(&path).unwrap();
    let changes = e.worktree_pass(&observation(inventory(500, &[], &[]), vec![]));
    let second = &e.occurrences(&format!("reclaimable:{path}@{H2}"))[0];
    assert_resolved_by_cardinal(second, "the worktree is gone");
    assert_eq!(changes.cleared, std::slice::from_ref(&second.key));

    let records = cardinal::record::read_all(&e.state).unwrap();
    let keys = |kind: RecordKind| -> Vec<String> {
        records
            .iter()
            .filter(|r| r.kind == kind)
            .filter_map(|r| r.inbox_key.clone())
            .collect()
    };
    assert_eq!(
        keys(RecordKind::InboxRaised),
        [first.key.clone(), second.key.clone()]
    );
    assert_eq!(
        keys(RecordKind::InboxResolved),
        [first.key.clone(), second.key.clone()]
    );
}

/// Section 10.1: a whole-worktree plan whose invariants pass and whose `merge_evidence`
/// fails raises `merge_undecided` with `content_in_base`, `content_diff`, `closed_pr`, and
/// the plan; it ends once merge evidence appears, when the plan raises `reclaimable`. The
/// control, the same plan failing an invariant too, raises nothing.
#[test]
fn merge_undecided_raised_and_ended_when_merge_evidence_appears() {
    let e = env();
    let path = e.worktree_dir("wt");
    let other = e.worktree_dir("other");
    let mut wt = worktree(&path);
    wt["facts"]["content_in_base"] = json!("yes");
    wt["facts"]["content_diff"] = json!([]);
    e.worktree_pass(&observation(
        inventory(100, &[wt, worktree(&other)], &[]),
        vec![
            plan("p1", &path, H1, &["merge_evidence"]),
            plan("p2", &other, H1, &["merge_evidence", "clean"]),
        ],
    ));
    let items = e.items();
    assert_eq!(
        items.len(),
        1,
        "only the plan whose invariants pass: {items:#?}"
    );
    let item = &items[0];
    assert_open(item);
    assert_eq!(item.kind, InboxKind::MergeUndecided);
    assert_eq!(item.key, format!("merge_undecided:{path}@{H1}@100"));
    assert_eq!(item.evidence["plan_id"], "p1");
    assert_eq!(item.evidence["content_in_base"], "yes");
    assert_eq!(item.evidence["content_diff"], json!([]));
    assert_eq!(item.evidence["closed_pr"], Value::Null);

    // Merge evidence still unknown: nothing ends.
    let mut unknown = worktree(&path);
    unknown["facts"]["merge_evidence"] = json!("unknown");
    e.worktree_pass(&observation(inventory(150, &[unknown], &[]), vec![]));
    assert_open(&e.items()[0]);

    e.worktree_pass(&observation(
        inventory(200, &[merged(worktree(&path), H1)], &[]),
        vec![plan("p3", &path, H1, &[])],
    ));
    let undecided = &e.occurrences(&format!("merge_undecided:{path}@{H1}"))[0];
    assert_resolved_by_cardinal(undecided, "merge evidence appeared: base_ancestor");
    let reclaimable = e.occurrences(&format!("reclaimable:{path}@{H1}"));
    assert_eq!(reclaimable.len(), 1, "{:#?}", e.items());
}

/// Section 10.1: a dirty worktree whose branch's pull request was closed without merging
/// raises `closed_pr_dirty`, keyed by the pull request; it ends once the worktree is clean,
/// not while `dirty` is unknown. The controls, dirty without a closed pull request and clean
/// with one, raise nothing.
#[test]
fn closed_pr_dirty_raised_and_ended_when_clean() {
    let e = env();
    let path = e.worktree_dir("wt");
    let dirty_open = e.worktree_dir("dirty-open");
    let clean_closed = e.worktree_dir("clean-closed");
    let closed_pr = json!({"number": 7, "url": "https://github.com/o/r/pull/7"});
    let with = |path: &str, dirty: &str, pr: &Value| {
        let mut wt = worktree(path);
        wt["facts"]["dirty"] = json!(dirty);
        wt["facts"]["closed_pr"] = pr.clone();
        wt
    };
    e.worktree_pass(&observation(
        inventory(
            100,
            &[
                with(&path, "yes", &closed_pr),
                with(&dirty_open, "yes", &Value::Null),
                with(&clean_closed, "no", &closed_pr),
            ],
            &[],
        ),
        vec![],
    ));
    let items = e.items();
    assert_eq!(items.len(), 1, "{items:#?}");
    let item = &items[0];
    assert_open(item);
    assert_eq!(item.kind, InboxKind::ClosedPrDirty);
    assert_eq!(item.key, format!("closed_pr_dirty:{path}#7@100"));
    assert_eq!(item.evidence["closed_pr"]["number"], 7);
    assert_eq!(item.evidence["facts"]["dirty"], "yes");

    e.worktree_pass(&observation(
        inventory(200, &[with(&path, "unknown", &closed_pr)], &[]),
        vec![],
    ));
    assert_open(&e.items()[0]);

    e.worktree_pass(&observation(
        inventory(300, &[with(&path, "no", &closed_pr)], &[]),
        vec![],
    ));
    assert_resolved_by_cardinal(&e.items()[0], "the worktree is clean");
}

/// Section 10.1: a managed worktree with merge evidence, clean and not in use, raises
/// `managed_merged`; a new HEAD ends it. The controls, the same worktree in use, dirty,
/// without merge evidence, or with its in-use state unknown, and a main worktree under a
/// managed root, raise nothing.
#[test]
fn managed_merged_raised_and_ended() {
    let e = env();
    let managed = |name: &str| {
        let path = e.worktree_dir(name);
        let mut wt = merged(worktree(&path), H1);
        wt["managed_by"] = json!("t3code");
        (path, wt)
    };
    let (path, wt) = managed("wt");
    let (_, mut in_use) = managed("in-use");
    in_use["facts"]["in_use"] = json!({"value": true, "jobs": ["j"]});
    let (_, mut dirty) = managed("dirty");
    dirty["facts"]["dirty"] = json!("yes");
    let (_, mut unmerged) = managed("unmerged");
    unmerged["facts"]["merge_evidence"] = json!("none");
    let (_, mut use_unknown) = managed("use-unknown");
    use_unknown["reclaim"]["unknown"] = json!(["not_in_use"]);
    let (_, mut main) = managed("main");
    main["main"] = json!(true);
    e.worktree_pass(&observation(
        inventory(100, &[wt, in_use, dirty, unmerged, use_unknown, main], &[]),
        vec![],
    ));
    let items = e.items();
    assert_eq!(items.len(), 1, "{items:#?}");
    let item = &items[0];
    assert_open(item);
    assert_eq!(item.kind, InboxKind::ManagedMerged);
    assert_eq!(item.key, format!("managed_merged:{path}@{H1}@100"));
    assert_eq!(item.evidence["managed_by"], "t3code");
    assert_eq!(item.evidence["facts"]["merge_evidence"], "base_ancestor");

    let (_, mut moved) = managed("wt");
    moved["head"] = json!(H2);
    moved["facts"]["dirty"] = json!("yes");
    e.worktree_pass(&observation(inventory(200, &[moved], &[]), vec![]));
    assert_resolved_by_cardinal(&e.items()[0], "HEAD changed");
}

/// Section 10.5: the brief view of each worktree item, as its raiser writes it, keeps the
/// evidence fields the skill's decision for its kind reads, and an `unclassifiable_data` item
/// names its key's set, which is `inventory --worktree`'s `unclassifiable_set` for the
/// worktree.
#[test]
fn brief_views_keep_what_the_decision_reads() {
    let e = env();
    let reclaimable = e.worktree_dir("reclaimable");
    let undecided = e.worktree_dir("undecided");
    let dirty = e.worktree_dir("dirty");
    let managed = e.worktree_dir("managed");
    let data = e.worktree_dir("data");
    let closed_pr = json!({"number": 7, "url": "https://github.com/o/r/pull/7"});
    let mut undecided_wt = worktree(&undecided);
    undecided_wt["facts"]["content_in_base"] = json!("yes");
    undecided_wt["facts"]["closed_pr"] = closed_pr.clone();
    let mut dirty_wt = worktree(&dirty);
    dirty_wt["facts"]["dirty"] = json!("yes");
    dirty_wt["facts"]["closed_pr"] = closed_pr.clone();
    let mut managed_wt = merged(worktree(&managed), H1);
    managed_wt["managed_by"] = json!("t3code");
    let entries: Vec<(String, &str, Option<u64>)> = (0..60)
        .map(|i| (format!("notes/{i:02}.log"), "unclassifiable", Some(10)))
        .collect();
    let entries: Vec<(&str, &str, Option<u64>)> = entries
        .iter()
        .map(|(p, c, b)| (p.as_str(), *c, *b))
        .collect();
    let data_wt = with_ignored(&data, &entries);
    let observed = inventory(
        100,
        &[
            merged(worktree(&reclaimable), H1),
            undecided_wt,
            dirty_wt,
            managed_wt,
            data_wt,
        ],
        &[],
    );
    let set = cardinal::observe::unclassifiable_set(&observed.repos[0].worktrees[4])
        .map(|(set, _)| set)
        .unwrap();
    e.worktree_pass(&observation(
        observed,
        vec![
            plan("p1", &reclaimable, H1, &[]),
            plan("p2", &undecided, H1, &["merge_evidence"]),
        ],
    ));
    let items = e.items();
    assert_eq!(items.len(), 5, "{items:#?}");
    for item in &items {
        let brief = inbox::brief(item);
        let mut kept: Vec<String> = brief.evidence.keys().cloned().collect();
        kept.sort();
        let expected: &[&str] = match item.kind {
            InboxKind::Reclaimable => &["observed_ms", "plan_id"],
            InboxKind::MergeUndecided => &[
                "closed_pr",
                "content_diff",
                "content_in_base",
                "merge_evidence",
                "observed_ms",
                "plan_id",
            ],
            InboxKind::ClosedPrDirty => &["closed_pr", "facts", "observed_ms"],
            InboxKind::ManagedMerged => &["facts", "managed_by", "observed_ms"],
            InboxKind::UnclassifiableData => &["entries", "observed_ms", "paths", "set"],
            other => panic!("{other:?}"),
        };
        assert_eq!(kept, expected, "{:?}", item.kind);
        match item.kind {
            InboxKind::ClosedPrDirty => {
                assert_eq!(brief.evidence["facts"], json!({"dirty": "yes"}));
            }
            InboxKind::ManagedMerged => assert_eq!(
                brief.evidence["facts"],
                json!({"merge_evidence": "base_ancestor", "merged_pr": null, "dirty": "no",
                       "in_use": {"value": false}})
            ),
            InboxKind::UnclassifiableData => {
                assert_eq!(brief.evidence["set"], json!(set));
                assert_eq!(brief.evidence["entries"], 60);
                assert_eq!(
                    brief.omitted["/evidence/paths"],
                    50 - inbox::LIST_ENTRIES as u64
                );
            }
            InboxKind::MergeUndecided => {
                assert_eq!(brief.evidence["content_in_base"], "yes");
                assert_eq!(brief.evidence["closed_pr"]["number"], 7);
            }
            _ => {}
        }
    }
}

/// The worktree `path` holding the ignored `entries`, each `(path, class, allocated_bytes)`.
fn with_ignored(path: &str, entries: &[(&str, &str, Option<u64>)]) -> Value {
    let mut wt = worktree(path);
    wt["ignored"] = entries
        .iter()
        .map(|(p, class, bytes)| {
            json!({"path": p, "class": class, "regenerable_match": *class == "regenerable",
                   "allocated_bytes": bytes, "aliased_bytes": null})
        })
        .collect();
    wt
}

/// The `unclassifiable_data` items of the worktree at `path`, oldest first.
fn unclassifiable(e: &Env, path: &str) -> Vec<InboxItem> {
    oldest_first(e.items().into_iter().filter(|item| {
        item.kind == InboxKind::UnclassifiableData && item.subject.worktree.as_deref() == Some(path)
    }))
}

/// `items` in the order of their occurrence times, the observations that raised them. Not
/// `inbox::list`'s order: that is by `created_ms`, the wall clock when the item was stored,
/// then by key, so the items of two passes within one millisecond come in key order, which
/// for `unclassifiable_data` is the order of the set hashes.
fn oldest_first(items: impl Iterator<Item = InboxItem>) -> Vec<InboxItem> {
    let mut items: Vec<InboxItem> = items.collect();
    items.sort_by_key(|item| inbox::occurrence_of(&item.key).1);
    items
}

/// A22: unclassifiable ignored data raises one item per worktree and set of entries: the
/// same set listed again, in another order, updates it; its evidence lists up to 50 paths in
/// order with their sizes when known, the number of entries, and the worktree's observation
/// errors. Regenerable, protected, and nested-repository entries raise nothing.
#[test]
fn unclassifiable_data_once_per_set() {
    let e = env();
    let path = e.worktree_dir("wt");
    let mut entries: Vec<(String, &str, Option<u64>)> = (0..60)
        .map(|i| (format!("notes/{i:02}.log"), "unclassifiable", Some(10)))
        .collect();
    entries[0].2 = None;
    let listed = |order: &[(String, &str, Option<u64>)]| {
        let mut all: Vec<(&str, &str, Option<u64>)> =
            order.iter().map(|(p, c, b)| (p.as_str(), *c, *b)).collect();
        all.push(("target", "regenerable", Some(100)));
        all.push(("fuzz", "protected", None));
        all.push(("vendor", "nested_repository", None));
        with_ignored(&path, &all)
    };
    let error = json!({"scope": format!("worktree:{path}"), "message": "walk failed"});
    let other_error = json!({"scope": "worktree:/elsewhere", "message": "not this one"});
    e.worktree_pass(&observation(
        inventory(100, &[listed(&entries)], &[error.clone(), other_error]),
        vec![],
    ));
    let items = unclassifiable(&e, &path);
    assert_eq!(items.len(), 1, "{:#?}", e.items());
    assert_eq!(
        e.items().len(),
        1,
        "only unclassifiable entries raise an item"
    );
    let item = &items[0];
    assert_open(item);
    let (condition, since) = inbox::occurrence_of(&item.key);
    assert_eq!(since, Some(100));
    let set = condition
        .strip_prefix(&format!("unclassifiable:{path}:"))
        .unwrap();
    assert_eq!(set.len(), 16, "{condition}");
    assert_eq!(item.evidence["entries"], 60);
    let paths = item.evidence["paths"].as_array().unwrap();
    assert_eq!(paths.len(), 50);
    assert_eq!(
        paths[0],
        json!({"path": "notes/00.log", "allocated_bytes": null})
    );
    assert_eq!(
        paths[49],
        json!({"path": "notes/49.log", "allocated_bytes": 10})
    );
    assert_eq!(item.evidence["errors"], json!([error]));

    entries.reverse();
    e.worktree_pass(&observation(
        inventory(200, &[listed(&entries)], &[]),
        vec![],
    ));
    let items = unclassifiable(&e, &path);
    assert_eq!(items.len(), 1, "{items:#?}");
    assert_eq!(
        (items[0].seen, items[0].key.as_str()),
        (2, item.key.as_str())
    );
}

/// A22: the item ends when no unclassifiable entry is left, and when the worktree is gone;
/// a worktree whose ignored entries were not listed (`no_unclassifiable` unknown) ends
/// nothing.
#[test]
fn unclassifiable_data_cleared() {
    let e = env();
    let path = e.worktree_dir("wt");
    let gone = e.worktree_dir("gone");
    let notes = [("notes.log", "unclassifiable", Some(1))];
    e.worktree_pass(&observation(
        inventory(
            100,
            &[with_ignored(&path, &notes), with_ignored(&gone, &notes)],
            &[],
        ),
        vec![],
    ));
    assert_eq!(e.items().len(), 2, "{:#?}", e.items());

    let mut unread = worktree(&path);
    unread["reclaim"]["unknown"] = json!(["observed", "no_unclassifiable"]);
    e.worktree_pass(&observation(
        inventory(200, &[unread, with_ignored(&gone, &notes)], &[]),
        vec![],
    ));
    assert_open(&unclassifiable(&e, &path)[0]);

    std::fs::remove_dir(&gone).unwrap();
    e.worktree_pass(&observation(
        inventory(
            300,
            &[with_ignored(&path, &[("target", "regenerable", None)])],
            &[],
        ),
        vec![],
    ));
    assert_resolved_by_cardinal(
        &unclassifiable(&e, &path)[0],
        "no unclassifiable entries are left",
    );
    assert_resolved_by_cardinal(&unclassifiable(&e, &gone)[0], "the worktree is gone");
}

/// A22: a different set of unclassifiable entries raises its own item and supersedes the
/// one before it.
#[test]
fn unclassifiable_data_superseded() {
    let e = env();
    let path = e.worktree_dir("wt");
    e.worktree_pass(&observation(
        inventory(
            100,
            &[with_ignored(&path, &[("a.log", "unclassifiable", None)])],
            &[],
        ),
        vec![],
    ));
    e.worktree_pass(&observation(
        inventory(
            200,
            &[with_ignored(
                &path,
                &[
                    ("a.log", "unclassifiable", None),
                    ("b.log", "unclassifiable", None),
                ],
            )],
            &[],
        ),
        vec![],
    ));
    let items = unclassifiable(&e, &path);
    assert_eq!(items.len(), 2, "{items:#?}");
    let new_condition = inbox::occurrence_of(&items[1].key).0;
    assert_resolved_by_cardinal(&items[0], &format!("superseded by {new_condition}"));
    assert_eq!(items[0].ended_ms, Some(200));
    assert_open(&items[1]);
    assert_eq!(items[1].evidence["entries"], 2);
}

/// A22: sets A, B, A raise three items; the first, dismissed by the owner, keeps its
/// dismissal when B ends it and when A returns.
#[test]
fn unclassifiable_data_set_returns() {
    let e = env();
    let path = e.worktree_dir("wt");
    let set_a = [("a.log", "unclassifiable", None)];
    let set_b = [("b.log", "unclassifiable", None)];
    e.worktree_pass(&observation(
        inventory(100, &[with_ignored(&path, &set_a)], &[]),
        vec![],
    ));
    let first = unclassifiable(&e, &path).remove(0);
    e.resolve(&first.key, ResolutionOutcome::Dismissed);
    e.worktree_pass(&observation(
        inventory(200, &[with_ignored(&path, &set_b)], &[]),
        vec![],
    ));
    e.worktree_pass(&observation(
        inventory(300, &[with_ignored(&path, &set_a)], &[]),
        vec![],
    ));
    let items = unclassifiable(&e, &path);
    assert_eq!(items.len(), 3, "{items:#?}");
    assert_eq!(items[0].key, first.key);
    assert_eq!(items[0].status, InboxStatus::Dismissed);
    assert_eq!(items[0].resolution.as_ref().unwrap().by, "cli");
    assert_eq!(items[0].ended_ms, Some(200));
    assert_resolved_by_cardinal(&items[1], "superseded by");
    assert_open(&items[2]);
    assert_eq!(
        inbox::occurrence_of(&items[2].key).0,
        inbox::occurrence_of(&first.key).0,
        "set A again"
    );
}

/// The observation of one repository, `repo`, whose common dir is `common_dir`, listing
/// `worktrees`.
fn inventory_of(observed_ms: u64, common_dir: &str, worktrees: &[Value]) -> Inventory {
    let mut inventory = inventory(observed_ms, worktrees, &[]);
    common_dir.clone_into(&mut inventory.repos[0].common_dir);
    inventory
}

/// A worktree the observation does not list is gone only when its path no longer exists or is
/// no longer a worktree of the item's repository. A listing that misses a live worktree
/// without failing, here with the repository's `worktrees/` directory unreadable or only
/// searchable (Unix) as `git worktree list` then lists only the main worktree, ends nothing,
/// also for a `.git` pointer of the form `<common dir>/worktrees/<id>/../<id>` and a
/// `commondir` whose path starts with a space, both read as Git reads them; the controls,
/// a worktree removed and a path whose `.git` entry was removed or leads to another
/// repository, end their items.
#[test]
fn a_worktree_missing_from_the_listing_is_gone_only_once_unlinked() {
    let e = env();
    let fx = Fixture::new();
    let repo = fx.init("repo");
    fx.commit_file(&repo, "README.md", "hello\n", "initial");
    let other = fx.init("other");
    fx.commit_file(&other, "README.md", "hello\n", "initial");
    let common = repo.join(".git");
    let common_dir = common.to_str().unwrap().to_owned();
    let names = ["kept", "removed", "unlinked", "moved"];
    let paths: Vec<_> = names
        .iter()
        .map(|name| fx.worktree_add(&repo, name, name))
        .collect();
    let path = |i: usize| paths[i].to_str().unwrap().to_owned();
    let notes = [("notes.log", "unclassifiable", None)];
    let listed: Vec<Value> = (0..names.len())
        .map(|i| with_ignored(&path(i), &notes))
        .collect();
    e.worktree_pass(&observation(
        inventory_of(100, &common_dir, &listed),
        vec![],
    ));
    assert_eq!(e.items().len(), 4, "{:#?}", e.items());
    // The kept worktree's pointer reaches its git dir through `..`.
    let pointer = std::fs::read_to_string(paths[0].join(".git")).unwrap();
    let git_dir = pointer.strip_prefix("gitdir:").unwrap().trim();
    let id = Path::new(git_dir).file_name().unwrap().to_str().unwrap();
    std::fs::write(
        paths[0].join(".git"),
        format!("gitdir: {git_dir}/../{id}\n"),
    )
    .unwrap();
    // Its `commondir` leads through a directory whose name starts with a space.
    let admin = Path::new(git_dir);
    std::fs::create_dir(admin.join(" up")).unwrap();
    std::fs::write(admin.join("commondir"), " up/../../..\n").unwrap();

    #[cfg(unix)]
    for unlistable in [0o000, 0o311] {
        use std::os::unix::fs::PermissionsExt;
        let worktrees = common.join("worktrees");
        let mode = std::fs::metadata(&worktrees).unwrap().permissions().mode();
        std::fs::set_permissions(&worktrees, std::fs::Permissions::from_mode(unlistable)).unwrap();
        let changes = e.worktree_pass(&observation(inventory_of(150, &common_dir, &[]), vec![]));
        std::fs::set_permissions(&worktrees, std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(changes.cleared.is_empty(), "{unlistable:o}: {changes:?}");
    }
    let changes = e.worktree_pass(&observation(inventory_of(200, &common_dir, &[]), vec![]));
    assert!(changes.cleared.is_empty(), "{changes:?}");

    fx.worktree_remove(&repo, &paths[1]);
    std::fs::remove_file(paths[2].join(".git")).unwrap();
    let elsewhere = fx.worktree_add(&other, "elsewhere", "elsewhere");
    std::fs::copy(elsewhere.join(".git"), paths[3].join(".git")).unwrap();
    e.worktree_pass(&observation(inventory_of(300, &common_dir, &[]), vec![]));
    let item = |i: usize| unclassifiable(&e, &path(i)).remove(0);
    assert_open(&item(0));
    assert_resolved_by_cardinal(&item(1), "the worktree is gone");
    assert_resolved_by_cardinal(
        &item(2),
        "the path is no longer a worktree of its repository",
    );
    assert_resolved_by_cardinal(
        &item(3),
        "the path is no longer a worktree of its repository",
    );
}

/// Section 10.1: the raisers end only what the pass read before it observed. An item
/// escalated while the pass observed, after its snapshot, keeps the escalation although the
/// observation found its clearing condition; the next pass ends it.
#[test]
fn an_item_changed_while_the_pass_observed_is_left_alone() {
    let e = env();
    let path = e.worktree_dir("wt");
    e.worktree_pass(&observation(
        inventory(100, &[merged(worktree(&path), H1)], &[]),
        vec![plan("p1", &path, H1, &[])],
    ));
    e.disk_pass(
        &[disk("warning", 100, Some(500 * MIB))],
        &[report(100, 500 * MIB)],
    );
    let before = e.before();
    for item in e.items() {
        e.resolve(&item.key, ResolutionOutcome::Escalated);
    }
    let changes = e.worktree_pass_after(
        &before,
        &observation(inventory(200, &[merged(worktree(&path), H2)], &[]), vec![]),
    );
    assert!(changes.cleared.is_empty(), "{changes:?}");
    let changes = e.disk_pass_after(&before, &[disk("ok", 200, Some(1000 * MIB))], &[]);
    assert!(changes.cleared.is_empty(), "{changes:?}");
    let items = e.items();
    assert_eq!(items.len(), 2, "{items:#?}");
    for item in &items {
        assert_eq!(
            (item.status, item.ended_ms),
            (InboxStatus::Escalated, None),
            "{item:#?}"
        );
    }

    e.worktree_pass(&observation(
        inventory(300, &[merged(worktree(&path), H2)], &[]),
        vec![],
    ));
    e.disk_pass(&[disk("ok", 300, Some(1000 * MIB))], &[]);
    for item in &e.items() {
        assert_resolved_by_cardinal(item, "");
    }
}

/// A worktree item ends only on what the observation read: a HEAD it could not read ends
/// no HEAD-keyed item, while the control, a HEAD read as another one, does.
#[test]
fn an_unread_head_ends_nothing() {
    let e = env();
    let path = e.worktree_dir("wt");
    e.worktree_pass(&observation(
        inventory(100, &[merged(worktree(&path), H1)], &[]),
        vec![plan("p1", &path, H1, &[])],
    ));
    let mut unread = merged(worktree(&path), H1);
    unread["head"] = Value::Null;
    e.worktree_pass(&observation(inventory(200, &[unread], &[]), vec![]));
    assert_open(&e.items()[0]);
    e.worktree_pass(&observation(
        inventory(300, &[merged(worktree(&path), H2)], &[]),
        vec![],
    ));
    assert_resolved_by_cardinal(&e.items()[0], "HEAD changed");
}

/// Items the worktree raiser does not own, about a worktree it no longer lists, are left
/// alone, as are worktree items another raiser resolved and ended.
#[test]
fn other_kinds_are_left_alone() {
    let e = env();
    let gone = e.missing_dir("gone");
    let job = inbox::NewItem {
        key: "recovery_needed:j".into(),
        kind: InboxKind::RecoveryNeeded,
        subject: cardinal::contract::InboxSubject {
            job_id: Some("j".into()),
            worktree: Some(gone),
            ..Default::default()
        },
        evidence: serde_json::Map::new(),
    };
    inbox::raise_occurrence(&e.state, &job, 1).unwrap();
    let changes = e.worktree_pass(&observation(inventory(100, &[], &[]), vec![]));
    assert!(changes.cleared.is_empty(), "{changes:?}");
    assert_open(&e.items()[0]);
}

const MIB: u64 = 1024 * 1024;

/// The disk state of the volume `v1`, in `level` since `since_ms`, with one reading of
/// `free_bytes` at `since_ms`.
fn disk(level: &str, since_ms: u64, free_bytes: Option<u64>) -> DiskState {
    serde_json::from_value(json!({
        "schema": "cardinal.disk-state.v1", "volume": "v1", "mount": "/mnt/v1",
        "measure": "statvfs", "state": level, "since_ms": since_ms,
        "lines": {"warning_bytes": 600 * MIB, "recovery_bytes": 900 * MIB,
                  "critical_bytes": 200 * MIB},
        "readings": [{"at_ms": since_ms, "free_bytes": free_bytes,
                      "total_bytes": 2048 * MIB}]
    }))
    .unwrap()
}

/// `disk-reclaim`'s report on `v1` from a reading of `free_bytes` at `at_ms`, listing one
/// target and one blocked entry.
fn report(at_ms: u64, free_bytes: u64) -> DiskReport {
    DiskReport {
        volume: "v1".into(),
        reading: ReportReading { at_ms, free_bytes },
        lines: disk("ok", at_ms, None).lines,
        checks_ms: at_ms - 1,
        targets: vec![ReportTarget {
            plan_id: format!("p{at_ms}"),
            worktree: "/w".into(),
            target: "/w/target".into(),
            exclusive_bytes: 300 * MIB,
            exclusive_measure: None,
            status: None,
            freed_bytes: None,
        }],
        shortfall_bytes: 0,
        blocked: vec![cardinal::contract::ReportBlocked {
            worktree: "/u".into(),
            target: "/u/target".into(),
            check: "not_in_use".into(),
            bytes: 300 * MIB,
        }],
        incomplete: vec![],
    }
}

/// A23, section 10.4: leaving `ok` raises `disk_low` and entering `critical` raises
/// `disk_critical`, each with the volume's disk report and owned by the owner; every pass
/// under pressure updates the report; leaving `critical` ends `disk_critical` while
/// `disk_low` stays, entering it again raises a new `disk_critical`, and back to `ok` ends
/// `disk_low`. A volume in `ok` raises nothing.
#[test]
fn disk_items_follow_the_volume_state() {
    let e = env();
    let changes = e.disk_pass(&[disk("ok", 50, Some(1000 * MIB))], &[]);
    assert!(changes.raised.is_empty() && e.items().is_empty());

    let changes = e.disk_pass(
        &[disk("warning", 100, Some(500 * MIB))],
        &[report(100, 500 * MIB)],
    );
    let low = e.occurrences("disk_low:v1");
    assert_eq!(low.len(), 1, "{:#?}", e.items());
    assert_open(&low[0]);
    assert_eq!(low[0].key, "disk_low:v1@100");
    assert!(low[0].kind.is_owners());
    assert_eq!(low[0].evidence["volume"], "v1");
    assert_eq!(low[0].evidence["mount"], "/mnt/v1");
    assert_eq!(low[0].evidence["disk"]["state"], "warning");
    let carried: DiskReport = serde_json::from_value(low[0].evidence["report"].clone()).unwrap();
    assert_eq!(carried, report(100, 500 * MIB));
    assert_eq!(changes.raised, [low[0].key.clone()]);

    e.disk_pass(
        &[disk("critical", 200, Some(100 * MIB))],
        &[report(200, 100 * MIB)],
    );
    let low = e.occurrences("disk_low:v1");
    assert_eq!((low.len(), low[0].seen), (1, 2), "{low:#?}");
    assert_eq!(low[0].evidence["report"]["reading"]["at_ms"], 200);
    let critical = e.occurrences("disk_critical:v1");
    assert_eq!(critical.len(), 1, "{:#?}", e.items());
    assert_eq!(critical[0].key, "disk_critical:v1@200");
    assert_eq!(
        critical[0].evidence["report"]["reading"]["free_bytes"],
        100 * MIB
    );

    let changes = e.disk_pass(
        &[disk("warning", 300, Some(300 * MIB))],
        &[report(300, 300 * MIB)],
    );
    let critical = e.occurrences("disk_critical:v1");
    assert_resolved_by_cardinal(&critical[0], "the volume left critical");
    assert_eq!(critical[0].ended_ms, Some(300));
    assert_eq!(changes.cleared, [critical[0].key.clone()]);
    assert_open(&e.occurrences("disk_low:v1")[0]);

    e.disk_pass(
        &[disk("critical", 400, Some(150 * MIB))],
        &[report(400, 150 * MIB)],
    );
    let critical = e.occurrences("disk_critical:v1");
    assert_eq!(critical.len(), 2, "{critical:#?}");
    assert_open(&critical[1]);

    e.disk_pass(&[disk("ok", 500, Some(1000 * MIB))], &[]);
    let low = e.occurrences("disk_low:v1");
    assert_resolved_by_cardinal(&low[0], "the volume is back to ok");
    assert_eq!(low[0].ended_ms, Some(500));
    assert_resolved_by_cardinal(
        &e.occurrences("disk_critical:v1")[1],
        "the volume left critical",
    );
}

/// A16, A23: a volume under pressure without a report this pass, its reading or
/// `disk-reclaim` failed, keeps its open items as they were; a volume whose state is
/// `unknown`, or that is not watched this pass, changes nothing. Without an open item, the
/// pressure is raised with no report.
#[test]
fn disk_items_without_a_report_stay_as_they_were() {
    let e = env();
    e.disk_pass(
        &[disk("critical", 100, Some(100 * MIB))],
        &[report(100, 100 * MIB)],
    );
    let before = e.items();
    assert_eq!(before.len(), 2, "{before:#?}");

    let changes = e.disk_pass(&[disk("critical", 100, None)], &[]);
    assert!(
        changes.raised.is_empty() && changes.cleared.is_empty(),
        "{changes:?}"
    );
    assert_eq!(e.items(), before);
    e.disk_pass(&[disk("unknown", 200, None)], &[]);
    e.disk_pass(&[], &[]);
    assert_eq!(e.items(), before);

    let fresh = env();
    fresh.disk_pass(&[disk("warning", 300, Some(500 * MIB))], &[]);
    let items = fresh.items();
    assert_eq!(items.len(), 1, "{items:#?}");
    assert_eq!(items[0].key, "disk_low:v1@300");
    assert_eq!(items[0].evidence["report"], Value::Null);
    assert_eq!(items[0].evidence["disk"]["state"], "warning");
}

/// A16: a raise without a report never replaces the evidence of an occurrence that has not
/// ended, also when the pass could not read the items before observing (`Before::default`):
/// the store decides under its lock, and the item is left as it was.
#[test]
fn a_reportless_raise_keeps_the_stored_report() {
    let e = env();
    e.disk_pass(
        &[disk("warning", 100, Some(500 * MIB))],
        &[report(100, 500 * MIB)],
    );
    let stored = e.items();
    let changes = e.disk_pass_after(
        &raise::Before::default(),
        &[disk("warning", 100, Some(500 * MIB))],
        &[],
    );
    assert!(changes.raised.is_empty(), "{changes:?}");
    assert_eq!(e.items(), stored);
}

/// C19: a disk item the owner resolved stays resolved while the pressure lasts, and the next
/// episode raises a new one.
#[test]
fn a_resolved_disk_item_returns_only_with_a_new_episode() {
    let e = env();
    e.disk_pass(
        &[disk("warning", 100, Some(500 * MIB))],
        &[report(100, 500 * MIB)],
    );
    e.resolve("disk_low:v1@100", ResolutionOutcome::Resolved);
    e.disk_pass(
        &[disk("warning", 100, Some(450 * MIB))],
        &[report(200, 450 * MIB)],
    );
    let low = e.occurrences("disk_low:v1");
    assert_eq!((low.len(), low[0].seen), (1, 1), "{low:#?}");
    assert_eq!(low[0].status, InboxStatus::Resolved);

    e.disk_pass(&[disk("ok", 300, Some(1000 * MIB))], &[]);
    e.disk_pass(
        &[disk("warning", 400, Some(500 * MIB))],
        &[report(400, 500 * MIB)],
    );
    let low = e.occurrences("disk_low:v1");
    assert_eq!(low.len(), 2, "{low:#?}");
    assert_eq!(low[0].ended_ms, Some(300));
    assert_eq!(low[0].status, InboxStatus::Resolved);
    assert_open(&low[1]);
}

/// `<state>/trash/` as the pass lists it: each entry by name, with whether an execution
/// directory names it, `None` when that could not be read.
fn trash(e: &Env, entries: &[(&str, Option<bool>)]) -> raise::Trash {
    raise::Trash {
        entries: entries
            .iter()
            .map(|(name, named)| raise::TrashEntry {
                name: (*name).to_owned(),
                path: e.state.trash_entry(name),
                named: *named,
                entries_left: Some(2),
                bytes_left: Some(4096),
            })
            .collect(),
    }
}

/// Sections 9.7 and 10.1: `reclaim_stranded`, raised by an execution or its recovery for the
/// data it left in the trash, ends once its trash entry reads as missing, whoever raised it;
/// while the entry is there it stays open, even when the listing leaves it out. A trash entry
/// no execution names raises its own item, naming the entry, which ends the same way; one
/// whose naming could not be read raises nothing. A listing read before the item was raised
/// ends nothing, and neither does a missing entry whose execution's claim cannot be read.
#[test]
fn reclaim_stranded_raised_and_ended() {
    let e = env();
    let plan = "a1b2c3d4e5f60718";
    let stranded = cardinal::inbox::NewItem {
        key: format!("reclaim_stranded:{plan}"),
        kind: InboxKind::ReclaimStranded,
        subject: cardinal::contract::InboxSubject::default(),
        evidence: json!({ "trash": e.state.trash_entry(plan).display().to_string(),
                          "reason": "entries kept" })
        .as_object()
        .unwrap()
        .clone(),
    };
    let condition = format!("reclaim_stranded:{plan}");
    let stray = "0123456789abcdef";
    let stray_condition = format!("reclaim_stranded:{stray}");
    let before_raise = e.before();
    inbox::raise_occurrence(&e.state, &stranded, 1_000).unwrap();
    let pass = |before: &raise::Before, listing: &raise::Trash| {
        raise::stranded_items(&e.config, &e.state, before, listing, &|_| false).unwrap()
    };
    std::fs::create_dir_all(e.state.trash_entry(plan)).unwrap();

    // The control: the entry is still there, so the item stays open.
    let changes = pass(&e.before(), &trash(&e, &[(plan, Some(true))]));
    assert_eq!(changes.cleared, Vec::<String>::new());
    assert_open(&e.occurrences(&condition)[0]);

    // A listing that leaves out an entry still there ends nothing.
    pass(&e.before(), &trash(&e, &[]));
    assert_open(&e.occurrences(&condition)[0]);

    // An entry gone ends nothing while its execution's claim cannot be read.
    std::fs::remove_dir(e.state.trash_entry(plan)).unwrap();
    let unreadable =
        raise::stranded_items(&e.config, &e.state, &e.before(), &trash(&e, &[]), &|p| {
            p == plan
        })
        .unwrap();
    assert_eq!(unreadable.cleared, Vec::<String>::new());
    assert_open(&e.occurrences(&condition)[0]);

    // A listing read before the item was raised ends nothing.
    pass(&before_raise, &trash(&e, &[]));
    assert_open(&e.occurrences(&condition)[0]);

    // The plan's entry gone ends the plan's item; a stray entry whose naming could not be read
    // raises nothing.
    let changes = pass(&e.before(), &trash(&e, &[(stray, None)]));
    let ended = e.occurrences(&condition);
    assert_eq!(changes.cleared, [ended[0].key.clone()]);
    assert_resolved_by_cardinal(&ended[0], "the trash entry is gone");
    assert_eq!(changes.raised, Vec::<String>::new());
    assert_eq!(e.occurrences(&stray_condition).len(), 0);

    // Read as named by no execution, the stray entry raises its own item.
    std::fs::create_dir_all(e.state.trash_entry(stray)).unwrap();
    let changes = pass(&e.before(), &trash(&e, &[(stray, Some(false))]));
    let raised = e.occurrences(&stray_condition);
    assert_eq!(raised.len(), 1, "{raised:#?}");
    assert_eq!(changes.raised, [raised[0].key.clone()]);
    assert_open(&raised[0]);
    assert_eq!(
        raised[0].evidence["trash"],
        e.state.trash_entry(stray).display().to_string()
    );
    assert_eq!(raised[0].evidence["target"], Value::Null);
    assert_eq!(raised[0].evidence["entries_left"], 2);

    // Listed again, it stays one occurrence; gone, it ends.
    pass(&e.before(), &trash(&e, &[(stray, Some(false))]));
    assert_eq!(e.occurrences(&stray_condition).len(), 1);
    assert_open(&e.occurrences(&stray_condition)[0]);
    std::fs::remove_dir(e.state.trash_entry(stray)).unwrap();
    pass(&e.before(), &trash(&e, &[]));
    assert_resolved_by_cardinal(
        &e.occurrences(&stray_condition)[0],
        "the trash entry is gone",
    );
}
