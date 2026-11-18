//! The read-only web UI (`docs/architecture.md` section 10.6, unit U of section 14.2): the
//! page rendered from a fixed snapshot embeds it unchanged, shows every value as text, and is
//! self-contained, loading and sending nothing; `cardinal ui` writes it with no git, gh, or
//! network call; the snapshot holds what the state directory holds (the open and escalated
//! items with the owner's first, the newest 50 `act`, `planned`, and `inbox_*` records, and
//! each part from its file); and the page shows what it cannot vouch for: a disk report or a
//! time it cannot read, a stale part, and a worktree whose use is unknown.
//!
//! Every test names one check and pins what it assumes in its own `host.toml` or page
//! options: the record files kept, the full-observation interval the page's stale marks come
//! from, and the admission lock's deadline for the reservations it reads.

mod disk_support;
mod observe_common;
mod plan_support;
mod support;

use std::path::Path;
use std::time::Duration;

use cardinal::config::Config;
use cardinal::contract::{
    InboxItem, InboxKind, InboxStatus, Record, RecordKind, Snapshot, SnapshotItem,
};
use cardinal::record::Recorder;
use cardinal::snapshot::{self, PageOptions};
use cardinal::state::StateDir;
use cardinal::testpoint::{SNAPSHOT_RECORDS_OPENED, SNAPSHOT_RESERVATIONS_READ};
use serde_json::json;
use support::cardinal::Dirs;
use support::testpoints::TestPoints;

/// The fixed snapshot's generation time: 2026-11-18T00:00:00Z.
const GENERATED_MS: u64 = 1_794_960_000_000;
const MINUTE_MS: u64 = 60_000;

/// What every test pins: three record files, a full observation due every hour (so the page
/// marks parts older than two hours stale), and a two-second admission lock deadline.
const HOST_TOML: &str = "\
[records]
keep = 3
[reconcile]
observe_every_s = 3600
[admission]
lock_timeout_ms = 2000
";

struct Env {
    _dirs: Dirs,
    config: Config,
    state: StateDir,
}

fn env_with(host: &str) -> Env {
    let dirs = Dirs::new();
    std::fs::write(dirs.config().join("host.toml"), host).unwrap();
    let config = Config::load(dirs.config()).unwrap();
    let state = StateDir::open(dirs.state()).unwrap();
    Env {
        _dirs: dirs,
        config,
        state,
    }
}

/// A disk report as `disk-reclaim` writes it (section 11.3).
fn disk_report() -> serde_json::Value {
    json!({
        "volume": "9f3c1e2d4b5a6978",
        "reading": { "at_ms": GENERATED_MS - 10_000, "free_bytes": 12_884_901_888_u64 },
        "lines": { "warning_bytes": 21_474_836_480_u64, "recovery_bytes": 32_212_254_720_u64,
                   "critical_bytes": 5_368_709_120_u64 },
        "checks_ms": GENERATED_MS - 20_000,
        "targets": [
            { "plan_id": "a1b2c3d4e5f60718", "worktree": "/src/app/wt/old",
              "target": "/src/app/wt/old/node_modules", "exclusive_bytes": 3_221_225_472_u64 },
            { "plan_id": "a1b2c3d4e5f60719", "worktree": "/src/app/wt/old",
              "target": "/src/app/wt/old/.turbo", "exclusive_bytes": 536_870_912 }
        ],
        "shortfall_bytes": 15_569_256_448_u64,
        "blocked": [
            { "worktree": "/src/app/wt/busy", "target": "/src/app/wt/busy/node_modules",
              "check": "not_in_use", "bytes": 2_147_483_648_u64 }
        ]
    })
}

fn worktree(path: &str, branch: &str, main: bool) -> serde_json::Value {
    json!({
        "path": path, "main": main, "head": "0123456789abcdef0123456789abcdef01234567",
        "branch": branch, "upstream": null, "git_dir": format!("{path}/.git"),
        "last_activity_ms": GENERATED_MS - 26 * 60 * MINUTE_MS,
        "managed_by": if main { serde_json::Value::Null } else { json!("t3code") },
        "facts": {
            "dirty": "no", "dirty_unknown": null, "has_upstream": "no", "unpushed": "unknown",
            "open_pr": "no", "merge_evidence": "none", "merged_pr": null,
            "content_in_base": "yes", "content_diff": [],
            "closed_pr": { "number": 42, "url": "https://github.com/owner/app/pull/42" },
            "in_use": { "value": false, "jobs": [] }
        },
        "ignored": [
            { "path": format!("{path}/node_modules"), "class": "regenerable",
              "regenerable_match": true, "allocated_bytes": 3_221_225_472_u64,
              "aliased_bytes": 1_073_741_824 }
        ],
        "reclaim": { "candidate": false, "failed": ["merge_evidence"], "unknown": ["unpushed"] },
        "allocated_bytes": 4_294_967_296_u64
    })
}

fn inbox_item(key: &str, kind: &str, status: &str, created_ms: u64) -> serde_json::Value {
    json!({
        "schema": "cardinal.inbox-item.v1", "key": key, "kind": kind, "status": status,
        "subject": {}, "evidence": {}, "created_ms": created_ms, "updated_ms": created_ms + 1,
        "seen": 2, "ended_ms": null, "arbiter": null, "resolution": null
    })
}

fn record(kind: &str, ts_ms: u64) -> serde_json::Value {
    json!({ "schema": "cardinal.record.v1", "ts_ms": ts_ms, "host_id": "00000000000000aa",
            "kind": kind, "data": { "n": ts_ms } })
}

/// A branch name that would close the snapshot's script element if embedded unescaped.
const HOSTILE_BRANCH: &str = "feat/</script><script>document.title='x'</script>";

/// A watched volume's disk state, its last reading `free`.
fn volume(volume: &str, mount: &str, state: &str, free: u64) -> serde_json::Value {
    json!({
        "schema": "cardinal.disk-state.v1", "volume": volume, "mount": mount,
        "measure": "important_usage", "state": state, "since_ms": GENERATED_MS - 30 * MINUTE_MS,
        "lines": { "warning_bytes": 21_474_836_480_u64, "recovery_bytes": 32_212_254_720_u64,
                   "critical_bytes": 5_368_709_120_u64 },
        "readings": [
            { "at_ms": GENERATED_MS - 5 * MINUTE_MS, "free_bytes": free + 1_073_741_824,
              "total_bytes": 994_662_584_320_u64 },
            { "at_ms": GENERATED_MS - 10_000, "free_bytes": free, "total_bytes": 994_662_584_320_u64 }
        ]
    })
}

/// The fixed snapshot's items, as the snapshot lists them: those for the owner first (an
/// escalated item whose note would open a comment, a stranded execution, and a disk item with
/// its disk report), then an item for the arbiter, each group by creation.
fn fixed_inbox() -> Vec<serde_json::Value> {
    let mut disk_low = inbox_item(
        "disk_low:9f3c1e2d4b5a6978@1794959990000",
        "disk_low",
        "open",
        GENERATED_MS - 30 * MINUTE_MS,
    );
    disk_low["evidence"] = json!({
        "volume": "9f3c1e2d4b5a6978", "mount": "/",
        "disk": volume("9f3c1e2d4b5a6978", "/", "warning", 12_884_901_888),
        "report": disk_report(),
    });
    let mut escalated = inbox_item(
        "merge_undecided:/src/app/wt/it's@0123456789ab@1794950000000",
        "merge_undecided",
        "escalated",
        GENERATED_MS - 3 * 60 * MINUTE_MS,
    );
    escalated["subject"] = json!({ "worktree": "/src/app/wt/it's", "head": "0123456789abcdef",
                                   "branch": HOSTILE_BRANCH });
    escalated["evidence"] = json!({ "plan_id": "b1b2c3d4e5f60718", "content_in_base": "yes",
                                    "content_diff": [] });
    escalated["arbiter"] = json!({ "pass": "c1c2c3d4e5f60718", "assigned_ms": GENERATED_MS - 60 * MINUTE_MS,
                                   "outcome": "completed", "ended_ms": GENERATED_MS - 50 * MINUTE_MS });
    escalated["resolution"] = json!({ "outcome": "escalated", "by": "cli",
        "note": "<!-- Was HEAD squash-merged? --> & \"</script>\"", "at_ms": GENERATED_MS - 50 * MINUTE_MS });
    let mut stranded = inbox_item(
        "reclaim_stranded:d1d2d3d4e5f60718@1794955000000",
        "reclaim_stranded",
        "open",
        GENERATED_MS - 2 * 60 * MINUTE_MS,
    );
    stranded["evidence"] = json!({ "original_path": "/src/app/wt/old/target",
        "trash": "/state/trash/d1d2d3d4e5f60718", "entries": 3, "bytes": 4096,
        "reason": "trash_occupied" });
    let mut reclaimable = inbox_item(
        "reclaimable:/src/app/wt/done@0123456789ab@1794957000000",
        "reclaimable",
        "open",
        GENERATED_MS - 40 * MINUTE_MS,
    );
    reclaimable["subject"] =
        json!({ "worktree": "/src/app/wt/done", "head": "0123456789ab", "branch": "done" });
    reclaimable["evidence"] = json!({ "plan_id": "e1e2e3d4e5f60718" });

    let mut inbox = Vec::new();
    for (item, needs_owner) in [
        (escalated, true),
        (stranded, true),
        (disk_low, true),
        (reclaimable, false),
    ] {
        let mut item = item;
        item["needs_owner"] = json!(needs_owner);
        inbox.push(item);
    }
    inbox
}

/// A snapshot with every part filled, and strings that would close the snapshot's script
/// element or open a comment if they were embedded unescaped: a branch name, an arbiter's
/// note, and a key with a quote.
fn fixed_snapshot() -> Snapshot {
    let inbox = fixed_inbox();
    serde_json::from_value(json!({
        "schema": "cardinal.snapshot.v1",
        "generated_ms": GENERATED_MS,
        "host": { "id": "00000000000000aa", "hostname": "devbox", "os": "macos",
                  "arch": "aarch64", "boot_s": 1_794_000_000 },
        "disk": [
            volume("9f3c1e2d4b5a6978", "/", "warning", 12_884_901_888),
            volume("0a1b2c3d4e5f6071", "/Volumes/Scratch", "ok", 107_374_182_400),
        ],
        "observation": {
            "observed_ms": GENERATED_MS - 3 * 60 * MINUTE_MS,
            "repos": [{
                "common_dir": "/src/app/.git", "main_worktree": "/src/app",
                "profiles": ["app.toml"], "github": "owner/app",
                "base": { "ref": "origin/main", "oid": "fedcba9876543210fedcba9876543210fedcba98" },
                "gh": "ok", "gh_error": null,
                "worktrees": [worktree("/src/app", "main", true),
                              worktree("/src/app/wt/it's", HOSTILE_BRANCH, false)]
            }],
            "errors": [{ "scope": "repo:/src/other", "message": "not a git repository" }]
        },
        "reservations": {
            "observed_ms": GENERATED_MS - 1_000,
            "items": [{
                "schema": "cardinal.reservation.v1", "job_id": "f1f2f3f4f5f60718",
                "state": "running", "boot_s": 1_794_000_000,
                "created_ms": GENERATED_MS - 2 * MINUTE_MS, "updated_ms": GENERATED_MS - 2 * MINUTE_MS,
                "runner": { "pid": 4242, "start_s": 1_794_959_880 },
                "group": { "kind": "pgid", "pgid": 4242, "leader_start_s": 1_794_959_880 },
                "child": { "pid": 4243, "start_s": 1_794_959_881 }, "stop_signal_ms": null,
                "priority": "normal", "resources": { "cpu": 4 }, "admission": "within_budget",
                "class": "test", "rule": "test",
                "worktree": { "key": "0011223344556677", "path": "/src/app/wt/it's", "protected": true },
                "session": null, "cwd": "/src/app/wt/it's", "argv": ["pnpm", "test", "--", "it's"],
                "derived": "live", "members": [{ "pid": 4243, "start_s": 1_794_959_881, "name": "node" }],
                "verified": true
            }]
        },
        "inbox": inbox,
        "records": [
            record("inbox_raised", GENERATED_MS - 30 * MINUTE_MS),
            record("planned", GENERATED_MS - 20 * MINUTE_MS),
            record("act", GENERATED_MS - 10 * MINUTE_MS),
        ],
        "arbiter": {
            "schema": "cardinal.arbiter-pass.v1", "id": "c1c2c3d4e5f60718",
            "started_ms": GENERATED_MS - 60 * MINUTE_MS, "ended_ms": GENERATED_MS - 50 * MINUTE_MS,
            "outcome": "completed", "items": ["merge_undecided:/src/app/wt/it's@0123456789ab@1794950000000"],
            "log": "/state/arbiter/c1c2c3d4e5f60718.log", "failures_in_a_row": 0
        },
        "errors": []
    }))
    .expect("the fixed snapshot is a cardinal.snapshot.v1")
}

const OPTIONS: PageOptions = PageOptions {
    stale_after_ms: 2 * 60 * MINUTE_MS,
};

/// The headings of the page's sections, in order.
const SECTIONS: [(&str, &str); 7] = [
    ("owner", "For the owner"),
    ("disk", "Disk"),
    ("inbox", "Open items for the arbiter"),
    ("worktrees", "Worktrees"),
    ("reservations", "Reservations"),
    ("arbiter", "Arbiter"),
    ("records", "Recent records"),
];

fn render(snapshot: &Snapshot) -> String {
    snapshot::render(snapshot, OPTIONS).unwrap()
}

/// The content of the snapshot's JSON data block: from its start tag to the first
/// `</script` after it, which is its end tag unless the data could close the element early.
fn data_block(page: &str) -> &str {
    let open = r#"<script type="application/json" id="cardinal-snapshot">"#;
    let start = page.find(open).expect("the data block") + open.len();
    let len = page[start..]
        .find("</script")
        .expect("the data block's end");
    &page[start..start + len]
}

/// The content of the element that starts with `open`, up to its first `close`.
fn element<'a>(html: &'a str, open: &str, close: &str) -> &'a str {
    let start = html
        .find(open)
        .unwrap_or_else(|| panic!("no {open} in the page"));
    let len = html[start..].find(close).expect("the element's end");
    &html[start..start + len]
}

/// The section with `id`, as HTML.
fn section<'a>(page: &'a str, id: &str) -> &'a str {
    element(page, &format!(r#"<section id="{id}""#), "</section>")
}

/// The text a reader sees in `html`: its tags removed and its entities decoded.
fn text(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// Every section's heading is on the page, in order.
fn assert_every_section(page: &str) {
    let mut from = 0;
    for (id, heading) in SECTIONS {
        let at = page[from..]
            .find(&format!(r#"<h2 id="{id}-heading">{heading}</h2>"#))
            .unwrap_or_else(|| panic!("the {id} section is missing or out of order"));
        from += at;
    }
}

/// The page embeds the snapshot it was rendered from, unchanged, although strings in it
/// would close the data block or open a comment if they were embedded as they are.
#[test]
fn the_page_embeds_its_snapshot_unchanged() {
    let snapshot = fixed_snapshot();
    let page = render(&snapshot);

    let embedded: Snapshot = serde_json::from_str(data_block(&page))
        .expect("the embedded snapshot parses up to its end tag");
    assert_eq!(embedded, snapshot);
    assert!(
        !page.contains("<!--"),
        "no string opens a comment in the page"
    );
}

/// Every value from the snapshot is shown as text: a branch name and an arbiter's note that
/// hold markup appear as written and add no element to the page.
#[test]
fn values_from_the_snapshot_are_text() {
    let page = render(&fixed_snapshot());

    assert_eq!(
        page.matches("<script").count(),
        2,
        "the data block and the page's own script"
    );
    assert!(!page.contains("<script>document.title"));
    let owner = text(section(&page, "owner"));
    assert!(owner.contains(r#"Question: <!-- Was HEAD squash-merged? --> & "</script>""#));
    assert!(owner.contains(&format!("branch {HOSTILE_BRANCH}")));
}

/// The page is one self-contained file that loads and sends nothing: a content security
/// policy that allows only its own inline style and script and no inline attributes, no
/// element or style that loads anything, no control, and a script that neither sends nor
/// writes markup.
#[test]
fn the_page_loads_and_sends_nothing() {
    let page = render(&fixed_snapshot());
    let markup = page.replacen(data_block(&page), "", 1);
    let style = element(&page, "<style>", "</style>");
    let script = element(&page, "<script>\n", "</script>");

    assert!(markup.contains(
        r#"<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'unsafe-inline'; script-src-attr 'none'; style-src 'unsafe-inline'; style-src-attr 'none'; base-uri 'none'; form-action 'none'">"#
    ));
    for banned in [
        "src=",
        "href=",
        "<link",
        "<img",
        "<iframe",
        "<object",
        "<embed",
        "<form",
        "<button",
        "<input",
        "<select",
        "<textarea",
        "<base",
        "style=",
    ] {
        assert!(!markup.contains(banned), "the page contains {banned:?}");
    }
    assert!(
        !markup.split(" on").skip(1).any(|rest| {
            let name = rest.trim_start_matches(|c: char| c.is_ascii_lowercase());
            name.len() < rest.len() && name.starts_with('=')
        }),
        "the page has an event handler attribute"
    );
    for banned in ["url(", "@import", "http"] {
        assert!(!style.contains(banned), "the style contains {banned:?}");
    }
    for banned in [
        "fetch",
        "XMLHttpRequest",
        "sendBeacon",
        "WebSocket",
        "EventSource",
        "import",
        "http",
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
        "eval",
        "Function(",
        "Storage",
    ] {
        assert!(!script.contains(banned), "the script contains {banned:?}");
    }
}

/// `cardinal ui` writes the page from the state dir and prints its path, running no git and
/// no gh and making no connection: the configured git and gh, and `git`, `gh`, `claude`, and
/// `curl` first in `PATH`, are recording fakes, and the proxy variables name a recording
/// listener (Linux) or a closed port (macOS, whose sandbox denies the test every TCP socket).
#[cfg(unix)]
#[test]
fn cardinal_ui_runs_no_git_gh_or_network_call() {
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt as _;

    let dirs = Dirs::new();
    let scope = dirs.scope();
    let fakes = scope.path("fakes");
    let calls = scope.path("calls");
    let repo = scope.path("repo");
    std::fs::create_dir_all(&fakes).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    for name in ["git", "gh", "claude", "curl"] {
        let fake = fakes.join(name);
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\necho {name} \"$@\" >> '{}'\nexit 1\n",
                calls.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // A repository to observe, and git and gh that would record it, so a page built by
    // observing again would show up in the calls.
    std::fs::write(
        dirs.config().join("host.toml"),
        format!(
            "{HOST_TOML}[observe]\ngit = {:?}\ngh = {:?}\nrepos = [{:?}]\n",
            fakes.join("git").display().to_string(),
            fakes.join("gh").display().to_string(),
            repo.display().to_string(),
        ),
    )
    .unwrap();
    Config::load(dirs.config()).expect("a valid host.toml");
    let state = StateDir::open(dirs.state()).unwrap();
    seed(&state, &fixed_snapshot());

    let listener = (!cfg!(target_os = "macos")).then(|| {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        listener
    });
    let proxy = format!(
        "http://{}",
        listener.as_ref().map_or_else(
            || "127.0.0.1:9".to_owned(),
            |l| l.local_addr().unwrap().to_string()
        )
    );
    let mut command = dirs.cmd();
    command.arg("ui").env(
        "PATH",
        format!(
            "{}:{}",
            fakes.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    for var in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env(var, &proxy);
    }
    let out = command.output_within(Duration::from_secs(60)).unwrap();

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("{}\n", state.ui_page().display())
    );
    let recorded = std::fs::read_to_string(&calls).unwrap_or_default();
    assert_eq!(recorded, "", "no fake was invoked");
    if let Some(listener) = &listener {
        match listener.accept() {
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("a connection arrived: {other:?}"),
        }
    }
    let page = std::fs::read_to_string(state.ui_page()).unwrap();
    let written: Snapshot =
        serde_json::from_slice(&std::fs::read(state.ui_snapshot()).unwrap()).unwrap();
    let embedded: Snapshot = serde_json::from_str(data_block(&page)).unwrap();
    assert_eq!(embedded, written, "the page embeds ui/snapshot.json");
    assert_eq!(
        embedded.observation,
        fixed_snapshot().observation,
        "the observation is the one the state dir holds"
    );
    // The page's stale marks follow `observe_every_s`.
    assert!(page.contains(r#"<body data-stale-after-ms="7200000">"#));
}

/// Each part of the snapshot is the document its file holds: the disk states by mount
/// (a file that does not parse skipped), the last full observation, and the last arbiter
/// pass; the reservations are read with their derived state.
#[test]
fn each_part_comes_from_its_file() {
    let env = env_with(HOST_TOML);
    let fixed = fixed_snapshot();
    seed(&env.state, &fixed);
    std::fs::write(env.state.disk_dir().join("0000000000000000.json"), b"{").unwrap();
    // A reservation from another boot whose runner holds no live lock: `stale`.
    let mut reservation = serde_json::to_value(&fixed.reservations.items[0]).unwrap();
    let view = reservation.as_object_mut().unwrap();
    for derived in ["derived", "members", "verified"] {
        view.remove(derived);
    }
    view["boot_s"] = json!(1);
    std::fs::write(
        env.state.reservation("f1f2f3f4f5f60718"),
        serde_json::to_vec(&reservation).unwrap(),
    )
    .unwrap();

    let before = cardinal::state::now_ms();
    let built = snapshot::build(&env.config, &env.state).unwrap();
    let after = cardinal::state::now_ms();

    let mut disk = fixed.disk.clone();
    disk.sort_by(|a, b| a.mount.cmp(&b.mount));
    assert_eq!(built.disk, disk);
    assert_eq!(built.observation, fixed.observation);
    assert_eq!(built.arbiter, fixed.arbiter);
    assert_eq!(built.reservations.items.len(), 1);
    assert_eq!(
        built.reservations.items[0].derived,
        cardinal::contract::DerivedState::Stale
    );
    assert!((before..=after).contains(&built.reservations.observed_ms));
    assert!((before..=after).contains(&built.generated_ms));
    assert_eq!(built.host.id, env.state.host_id());
}

/// A state dir that holds nothing yet gives an empty snapshot, not an error.
#[test]
fn an_empty_state_dir_gives_an_empty_snapshot() {
    let env = env_with(HOST_TOML);

    let built = snapshot::build(&env.config, &env.state).unwrap();

    assert_eq!(built.disk, Vec::new());
    assert_eq!(built.observation, None);
    assert_eq!(built.reservations.items, Vec::new());
    assert_eq!(built.inbox, Vec::new());
    assert_eq!(built.records, Vec::new());
    assert_eq!(built.arbiter, None);
}

/// The inbox part holds the open and escalated items, never resolved or dismissed ones,
/// marks those only the owner can answer (escalated items and the owner's kinds), and lists
/// them first, each group by creation.
#[test]
fn pending_items_the_owners_first() {
    let env = env_with(HOST_TOML);
    let items = [
        ("reclaimable:/w/a@1", "reclaimable", "open", 1),
        ("disk_critical:v@5", "disk_critical", "open", 5),
        ("merge_undecided:/w/b@2", "merge_undecided", "escalated", 2),
        ("closed_pr_dirty:/w/c#3@3", "closed_pr_dirty", "resolved", 3),
        ("arbiter_failing:p@4", "arbiter_failing", "dismissed", 4),
        (
            "unclassifiable:/w/d:abc@6",
            "unclassifiable_data",
            "open",
            0,
        ),
        ("arbiter_failing:p@7", "arbiter_failing", "open", 7),
    ];
    for (key, kind, status, created_ms) in items {
        write_item(
            &env.state,
            &serde_json::from_value(inbox_item(key, kind, status, created_ms)).unwrap(),
        );
    }

    let built = snapshot::build(&env.config, &env.state).unwrap();

    let listed: Vec<(&str, InboxKind, InboxStatus, bool)> = built
        .inbox
        .iter()
        .map(|SnapshotItem { item, needs_owner }| {
            (item.key.as_str(), item.kind, item.status, *needs_owner)
        })
        .collect();
    assert_eq!(
        listed,
        [
            (
                "merge_undecided:/w/b@2",
                InboxKind::MergeUndecided,
                InboxStatus::Escalated,
                true
            ),
            (
                "disk_critical:v@5",
                InboxKind::DiskCritical,
                InboxStatus::Open,
                true
            ),
            (
                "arbiter_failing:p@7",
                InboxKind::ArbiterFailing,
                InboxStatus::Open,
                true
            ),
            (
                "unclassifiable:/w/d:abc@6",
                InboxKind::UnclassifiableData,
                InboxStatus::Open,
                false
            ),
            (
                "reclaimable:/w/a@1",
                InboxKind::Reclaimable,
                InboxStatus::Open,
                false
            ),
        ]
    );
}

/// The records part holds the newest 50 `act`, `planned`, and `inbox_*` records, oldest
/// first, from the current file and then the rotated ones, newest first; an older file is
/// not read once enough are found (here `records.2.jsonl` is a directory, which would fail
/// the read).
#[test]
fn the_newest_50_shown_records() {
    let env = env_with(HOST_TOML);
    let kinds = ["act", "planned", "inbox_raised", "inbox_resolved"];
    let file = |records: &[serde_json::Value]| {
        records
            .iter()
            .map(|r| r.to_string() + "\n")
            .collect::<String>()
    };
    // Each file: 30 shown records, each after a record the page does not show, and a torn
    // line.
    let lines = |base: u64| {
        let mut lines = Vec::new();
        for i in 0..30 {
            lines.push(record("job_finished", base + 2 * i));
            lines.push(record(
                kinds[usize::try_from(i).unwrap() % 4],
                base + 2 * i + 1,
            ));
        }
        lines
    };
    let older = lines(1_000);
    let newer = lines(2_000);
    std::fs::write(env.state.rotated_records(1), file(&older)).unwrap();
    std::fs::write(env.state.records_file(), file(&newer) + "{\"torn\n").unwrap();
    std::fs::create_dir(env.state.rotated_records(2)).unwrap();

    let built = snapshot::build(&env.config, &env.state).unwrap();

    let expected: Vec<Record> = older
        .iter()
        .chain(&newer)
        .map(|r| serde_json::from_value::<Record>(r.clone()).unwrap())
        .filter(|r| r.kind != RecordKind::JobFinished)
        .skip(10)
        .collect();
    assert_eq!(expected.len(), snapshot::RECORDS_SHOWN);
    assert_eq!(built.records, expected);
}

/// The record files are found by listing `records/`, so the files a larger `keep` left are
/// read too, newest first: here `keep` is 1 and `records.1.jsonl` and `records.2.jsonl` hold
/// the older records.
#[test]
fn record_files_are_found_whatever_keep_is() {
    let env = env_with("[records]\nkeep = 1\n");
    let line = |kind: &str, ts: u64| record(kind, ts).to_string() + "\n";
    std::fs::write(env.state.rotated_records(2), line("act", 1)).unwrap();
    std::fs::write(env.state.rotated_records(1), line("planned", 2)).unwrap();
    std::fs::write(env.state.records_file(), line("inbox_raised", 3)).unwrap();

    let built = snapshot::build(&env.config, &env.state).unwrap();

    let times: Vec<u64> = built.records.iter().map(|r| r.ts_ms).collect();
    assert_eq!(times, [1, 2, 3]);
}

/// No rotation renames a record file while the snapshot opens them, so no record is read
/// twice: it opens them under `records/.lock`, held shared, and while a rotation holds the
/// lock exclusively it reads none, and the records show as not read rather than as none.
#[test]
fn the_record_files_are_opened_under_the_records_lock() {
    let env = env_with(HOST_TOML);
    std::fs::write(
        env.state.records_file(),
        record("act", 1).to_string() + "\n",
    )
    .unwrap();
    let _rotation = cardinal::state::try_lock(
        &env.state.records_lock(),
        cardinal::state::LockMode::Exclusive,
    )
    .unwrap()
    .expect("the records lock is free");

    let built = snapshot::build(&env.config, &env.state).unwrap();

    assert_eq!(built.records, Vec::new());
    let scopes: Vec<&str> = built.errors.iter().map(|e| e.scope.as_str()).collect();
    assert_eq!(scopes, ["records"]);
    let records = text(section(&render(&built), "records"));
    assert!(records.contains("No record could be read."), "{records}");
}

/// The snapshot reads the record files after it releases `records/.lock`: paused there, a
/// rotation takes the lock at once and renames every file, and the page still shows each
/// record it opened once, and none appended since.
#[test]
fn a_rotation_while_the_records_are_read_waits_for_nothing() {
    let dirs = Dirs::new();
    std::fs::write(dirs.config().join("host.toml"), HOST_TOML).unwrap();
    let state = StateDir::open(dirs.state()).unwrap();
    let lines = |times: [u64; 2]| {
        times
            .map(|ts| record("act", ts).to_string() + "\n")
            .concat()
    };
    std::fs::write(state.rotated_records(2), lines([1, 2])).unwrap();
    std::fs::write(state.rotated_records(1), lines([3, 4])).unwrap();
    std::fs::write(state.records_file(), lines([5, 6])).unwrap();
    let points = TestPoints::new(dirs.scope());
    let mut command = dirs.cmd();
    command.arg("ui");
    points.apply(&mut command, &[SNAPSHOT_RECORDS_OPENED], None);
    let mut paused = command.spawn().unwrap();
    points.wait_reached(
        SNAPSHOT_RECORDS_OPENED,
        paused.id(),
        Duration::from_secs(30),
    );

    let rotation =
        cardinal::state::try_lock(&state.records_lock(), cardinal::state::LockMode::Exclusive)
            .unwrap();
    assert!(rotation.is_some(), "the snapshot holds the records lock");
    drop(rotation);
    // Past the limit with three files kept: records.1 replaces records.2, and records.jsonl
    // becomes records.1.
    let recorder = Recorder::new(&state, 1, 3);
    let mut late = recorder.record(RecordKind::Act);
    late.ts_ms = 7;
    recorder.append(&late).unwrap();
    assert!(!state.records_file().exists(), "the append did not rotate");
    points.release(SNAPSHOT_RECORDS_OPENED);
    let status = paused.wait().unwrap();

    assert!(status.success(), "{status}");
    let written: Snapshot =
        serde_json::from_slice(&std::fs::read(state.ui_snapshot()).unwrap()).unwrap();
    let times: Vec<u64> = written.records.iter().map(|r| r.ts_ms).collect();
    assert_eq!(times, [1, 2, 3, 4, 5, 6]);
    assert_eq!(written.errors, []);
}

/// With one record file kept, a rotation while the snapshot has the file open moves it out of
/// the way before removing it, so the next append creates `records.jsonl` at once (Windows
/// keeps a removed file's name until its last handle closes), and the page shows the records
/// the snapshot opened.
#[test]
fn with_one_file_kept_an_append_lands_while_the_records_are_read() {
    let dirs = Dirs::new();
    std::fs::write(dirs.config().join("host.toml"), HOST_TOML).unwrap();
    let state = StateDir::open(dirs.state()).unwrap();
    let line = |ts: u64| record("act", ts).to_string() + "\n";
    std::fs::write(state.records_file(), line(1) + &line(2)).unwrap();
    let points = TestPoints::new(dirs.scope());
    let mut command = dirs.cmd();
    command.arg("ui");
    points.apply(&mut command, &[SNAPSHOT_RECORDS_OPENED], None);
    let mut paused = command.spawn().unwrap();
    points.wait_reached(
        SNAPSHOT_RECORDS_OPENED,
        paused.id(),
        Duration::from_secs(30),
    );

    let rotating = Recorder::new(&state, 1, 1);
    let mut late = rotating.record(RecordKind::Act);
    late.ts_ms = 7;
    rotating.append(&late).unwrap();
    assert!(!state.records_file().exists(), "the append did not rotate");
    late.ts_ms = 8;
    Recorder::new(&state, 1 << 20, 1).append(&late).unwrap();
    points.release(SNAPSHOT_RECORDS_OPENED);
    let status = paused.wait().unwrap();

    assert!(status.success(), "{status}");
    let current = std::fs::read_to_string(state.records_file()).unwrap();
    let current: Vec<Record> = current
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(current.iter().map(|r| r.ts_ms).collect::<Vec<_>>(), [8]);
    let written: Snapshot =
        serde_json::from_slice(&std::fs::read(state.ui_snapshot()).unwrap()).unwrap();
    let times: Vec<u64> = written.records.iter().map(|r| r.ts_ms).collect();
    assert_eq!(times, [1, 2]);
}

/// Out of file descriptors, the snapshot loses the oldest record files, never the current one:
/// it opens them newest first, and each it could not open is an error of the part.
#[cfg(unix)]
#[test]
fn out_of_descriptors_the_oldest_record_files_are_lost() {
    let dirs = Dirs::new();
    std::fs::write(dirs.config().join("host.toml"), HOST_TOML).unwrap();
    let state = StateDir::open(dirs.state()).unwrap();
    for n in 1..=64_u32 {
        let ts = 100 - u64::from(n);
        std::fs::write(
            state.rotated_records(n),
            record("act", ts).to_string() + "\n",
        )
        .unwrap();
    }
    std::fs::write(state.records_file(), record("act", 1000).to_string() + "\n").unwrap();
    let mut command = dirs
        .scope()
        .shell(r#"ulimit -n 32 && exec "$0" ui"#, dirs.scope().root());
    command
        .arg(env!("CARGO_BIN_EXE_cardinal"))
        .env("CARDINAL_STATE_DIR", dirs.state())
        .env("CARDINAL_CONFIG_DIR", dirs.config());

    let out = command.output().unwrap();

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let written: Snapshot =
        serde_json::from_slice(&std::fs::read(state.ui_snapshot()).unwrap()).unwrap();
    assert_eq!(written.records.last().map(|r| r.ts_ms), Some(1000));
    let unopened = written
        .errors
        .iter()
        .filter(|e| e.scope == "records" && e.message.contains("records.64.jsonl"))
        .count();
    assert_eq!(unopened, 1, "{:?}", written.errors);
}

/// A record file that could not be opened is an error of the part even when newer files
/// already hold the 50 records shown, so the page never shows the records as read in full.
#[cfg(unix)]
#[test]
fn an_unopened_record_file_past_the_records_shown_is_reported() {
    let env = env_with(HOST_TOML);
    let lines: String = (1..=50)
        .map(|ts| record("act", ts).to_string() + "\n")
        .collect();
    std::fs::write(env.state.records_file(), lines).unwrap();
    std::os::unix::fs::symlink(
        env.state.root().join("nothing"),
        env.state.rotated_records(1),
    )
    .unwrap();

    let built = snapshot::build(&env.config, &env.state).unwrap();

    assert_eq!(built.records.len(), 50);
    let scopes: Vec<&str> = built.errors.iter().map(|e| e.scope.as_str()).collect();
    assert_eq!(scopes, ["records"], "{:?}", built.errors);
}

/// A disk item whose evidence holds a report that does not read as a disk report shows the
/// evidence as it is stored, with a note, and the rest of the page as usual.
#[test]
fn an_unreadable_disk_report_shows_the_evidence_as_stored() {
    let mut snapshot = fixed_snapshot();
    let item = &mut snapshot.inbox[2].item;
    assert_eq!(item.kind, InboxKind::DiskLow);
    item.evidence.get_mut("report").unwrap()["reading"] = serde_json::Value::Null;

    let page = render(&snapshot);

    let article = element(
        section(&page, "owner"),
        r#"<article class="item owner" aria-labelledby="item-3">"#,
        "</article>",
    );
    assert!(text(article).contains("The evidence holds a disk report that could not be read"));
    assert!(text(article).contains(r#""reading": null"#));
    assert!(!article.contains("Disk report"));
    assert_every_section(&page);
}

/// A disk report that names worktrees its pass could not see in full lists them under "Could
/// not see" with why, and with no target among what the pass saw it never reads as nothing
/// reclaimable; an item whose pass made no report says so.
#[test]
fn a_disk_report_shows_what_its_pass_could_not_see() {
    let mut snapshot = fixed_snapshot();
    let report = &mut snapshot.inbox[2].item.evidence["report"];
    report["targets"] = json!([]);
    report["incomplete"] = json!([{ "worktree": "/src/app/wt/hidden",
                                    "reason": "could not open directory 'private/'" }]);
    let open = r#"<article class="item owner" aria-labelledby="item-3">"#;

    let page = render(&snapshot);

    let article = element(section(&page, "owner"), open, "</article>");
    let shown = text(article);
    assert!(
        shown.contains("could not see 1 of the volume's worktrees in full"),
        "{shown}"
    );
    assert!(shown.contains("No target listed among what the pass could see."));
    assert!(!shown.contains("No target listed."), "{shown}");
    let unseen = text(element(article, ">Could not see</caption>", "</table>"));
    assert!(unseen.contains("/src/app/wt/hidden"), "{unseen}");
    assert!(
        unseen.contains("could not open directory 'private/'"),
        "{unseen}"
    );

    snapshot.inbox[2].item.evidence["report"] = serde_json::Value::Null;
    let page = render(&snapshot);
    let shown = text(element(section(&page, "owner"), open, "</article>"));
    assert!(
        shown.contains("No disk report came with this item"),
        "{shown}"
    );
}

/// On a real pass under pressure, step 8 writes the page with the `disk_low` item the pass
/// raised: the disk report from the item's evidence, with its final reading, its target, and
/// the worktree observation read only in part (a directory git status cannot open) under
/// "Could not see", never as nothing reclaimable.
#[cfg(unix)]
#[test]
fn a_real_pass_writes_the_disk_report_and_what_it_could_not_see() {
    use std::os::unix::fs::PermissionsExt;

    use disk_support::{Disk, MIB, carried_report, fill, own_errors};

    /// Gives the directory its permissions back, so the setup can be removed.
    struct Readable(std::path::PathBuf);
    impl Drop for Readable {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    let d = Disk::new((1, 2, 3));
    let hidden = d.s.worktree("hidden");
    let seen = d.s.worktree("seen");
    for wt in [&hidden, &seen] {
        fill(wt, "target/out.bin", 128 * 1024);
    }
    let private = hidden.join("private");
    std::fs::create_dir(&private).unwrap();
    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o000)).unwrap();
    let _readable = Readable(private.clone());
    if std::fs::read_dir(&private).is_ok() {
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            b"skipped a_real_pass_writes_the_disk_report_and_what_it_could_not_see: \
              the directory stays readable (root)\n",
        );
        return;
    }
    d.fixture_reads(&[Some(3 * MIB / 2)]);

    let pass = d.reconcile();

    assert_eq!(own_errors(&pass), Vec::<String>::new());
    let report = disk_support::report(&pass);
    assert_eq!(
        (report.targets.len(), report.incomplete.len()),
        (1, 1),
        "{report:#?}"
    );
    let written: Snapshot =
        serde_json::from_slice(&std::fs::read(d.s.state.ui_snapshot()).unwrap()).unwrap();
    let item = written
        .inbox
        .iter()
        .find(|i| i.item.kind == InboxKind::DiskLow)
        .expect("the disk_low item");
    assert_eq!(carried_report(&item.item).as_ref(), Some(report));
    let page = std::fs::read_to_string(d.s.state.ui_page()).unwrap();
    let owner = text(section(&page, "owner"));
    assert!(owner.contains("1.5 MiB free at"), "{owner}");
    assert!(owner.contains(&report.targets[0].target), "{owner}");
    let unseen = text(element(
        section(&page, "owner"),
        ">Could not see</caption>",
        "</table>",
    ));
    assert!(unseen.contains(&report.incomplete[0].worktree), "{unseen}");
    assert!(
        unseen.contains("could not open directory 'private/'"),
        "{unseen}"
    );
    assert!(!owner.contains("No target listed"), "{owner}");
}

/// A time past what a date can show (here past JavaScript's last date) is shown as unknown,
/// with nothing for the script to rewrite, and the rest of the page as usual.
#[test]
fn an_impossible_time_shows_as_unknown() {
    let mut snapshot = fixed_snapshot();
    snapshot.inbox[3].item.created_ms = 8_640_000_000_000_001;

    let page = render(&snapshot);

    assert!(
        section(&page, "inbox")
            .contains(r#"<dt>Raised</dt><dd><span class="badge unknown">unknown time</span></dd>"#)
    );
    assert!(!page.contains("8640000000000001\""));
    assert_every_section(&page);
}

/// Each part is marked stale by its own observation time, older than the threshold when
/// the page was written: the observation, a volume's last reading, an item's last sighting,
/// a disk report's checks, and the arbiter's last pass; records are events, so an old one is
/// not stale, and the part that lists them shows when they were read.
#[test]
fn stale_marks_follow_each_parts_observation_time() {
    let mut snapshot = fixed_snapshot();
    let hours = |h: u64| GENERATED_MS - h * 60 * MINUTE_MS;
    snapshot.disk[1].readings[1].at_ms = hours(3);
    if let Some(arbiter) = &mut snapshot.arbiter {
        arbiter.ended_ms = Some(hours(3));
    }
    snapshot.inbox[2].item.evidence["report"]["checks_ms"] = json!(hours(3));
    snapshot.records[0].ts_ms = hours(5);
    let stale = r#"<span class="badge stale">stale</span>"#;

    let page = render(&snapshot);

    let owner = section(&page, "owner");
    let articles: Vec<&str> = owner.split("<article").skip(1).collect();
    let marks: Vec<usize> = articles.iter().map(|a| a.matches(stale).count()).collect();
    // The escalated item was last seen three hours ago; the disk item's report was checked
    // three hours ago; the stranded execution was seen just under two hours ago.
    assert_eq!(marks, [1, 0, 1]);
    assert_eq!(section(&page, "disk").matches(stale).count(), 1);
    assert_eq!(section(&page, "worktrees").matches(stale).count(), 1);
    assert_eq!(section(&page, "reservations").matches(stale).count(), 0);
    assert_eq!(section(&page, "arbiter").matches(stale).count(), 1);
    let records = section(&page, "records");
    assert_eq!(records.matches(stale).count(), 0);
    assert!(text(records).contains("Read when this page was generated"));
    assert_eq!(
        element(&page, "<header>", "</header>")
            .matches(stale)
            .count(),
        0
    );
}

/// The stale marks follow `[reconcile] observe_every_s`: with a full observation due every 15
/// minutes, an observation 45 minutes old is stale on the page `write_page` writes, which
/// carries the 30-minute threshold for the view-time marks, and a reading 10 minutes old is
/// not.
#[test]
fn the_stale_threshold_follows_observe_every_s() {
    let env = env_with("[reconcile]\nobserve_every_s = 900\n");
    let now = cardinal::state::now_ms();
    let mut snapshot = fixed_snapshot();
    snapshot.observation.as_mut().unwrap().observed_ms = now - 45 * MINUTE_MS;
    for disk in &mut snapshot.disk {
        for reading in &mut disk.readings {
            reading.at_ms = now - 10 * MINUTE_MS;
        }
    }
    seed(&env.state, &snapshot);

    let page =
        std::fs::read_to_string(snapshot::write_page(&env.config, &env.state).unwrap()).unwrap();

    assert!(page.contains(r#"<body data-stale-after-ms="1800000">"#));
    let stale = r#"<span class="badge stale">stale</span>"#;
    assert_eq!(section(&page, "worktrees").matches(stale).count(), 1);
    assert_eq!(section(&page, "disk").matches(stale).count(), 0);
}

/// A worktree whose use could not be read (its `not_in_use` check is unknown) shows its use
/// as unknown, never as "no".
#[test]
fn a_use_that_could_not_be_read_shows_unknown() {
    let mut snapshot = fixed_snapshot();
    let repo = &mut snapshot.observation.as_mut().unwrap().repos[0];
    repo.worktrees[1].reclaim.unknown = vec!["not_in_use".into()];

    let page = render(&snapshot);

    let cells: Vec<String> = section(&page, "worktrees")
        .split(r#"<td class="in-use">"#)
        .skip(1)
        .map(|rest| text(&rest[..rest.find("</td>").unwrap()]))
        .collect();
    assert_eq!(cells, ["no", "unknown"]);
}

/// A worktree whose index time could not be read shows its last activity as unknown, never
/// as "never".
#[test]
fn an_unknown_last_activity_shows_unknown() {
    let mut snapshot = fixed_snapshot();
    let repo = &mut snapshot.observation.as_mut().unwrap().repos[0];
    repo.worktrees[1].last_activity_ms = None;

    let page = render(&snapshot);

    let worktrees = section(&page, "worktrees");
    let unknown = r#"<td><span class="badge unknown">unknown</span></td>"#;
    assert_eq!(worktrees.matches(unknown).count(), 1);
    assert!(!text(worktrees).contains("never"));
}

/// An item for the arbiter shows its evidence as an owner's item does.
#[test]
fn items_for_the_arbiter_show_their_evidence() {
    let page = render(&fixed_snapshot());

    let inbox = section(&page, "inbox");
    let evidence = element(inbox, "<details><summary>Evidence</summary>", "</details>");
    assert!(text(evidence).contains(r#""plan_id": "e1e2e3d4e5f60718""#));
}

/// Every table keeps its column headers at any width, in a scroller the keyboard can reach:
/// no style hides or restacks a table's parts.
#[test]
fn tables_keep_their_column_headers() {
    let page = render(&fixed_snapshot());
    let style = element(&page, "<style>", "</style>");

    let tables: Vec<&str> = page.split("<table>").skip(1).collect();
    assert!(tables.len() >= 6, "{} tables", tables.len());
    for table in &tables {
        assert!(table.starts_with("<caption") && table.contains("<thead><tr><th scope=\"col\">"));
    }
    assert_eq!(
        page.matches(r#"<div class="table-wrap" tabindex="0" role="region""#)
            .count(),
        tables.len()
    );
    for banned in ["@container", "display: none", "display: block"] {
        assert!(!style.contains(banned), "the style contains {banned:?}");
    }
    let scroller = element(style, ".table-wrap {", "}");
    assert!(scroller.contains("overflow: auto"), "{scroller}");
}

/// `cardinal ui` writes the page when no part can be read, and the page says so for each
/// part instead of showing it as empty: an inbox item, a reservation, a disk state, the
/// observation, the arbiter pass, and a record line that do not parse.
#[test]
fn cardinal_ui_marks_the_parts_it_cannot_read() {
    let dirs = Dirs::new();
    std::fs::write(dirs.config().join("host.toml"), HOST_TOML).unwrap();
    let state = StateDir::open(dirs.state()).unwrap();
    for path in [
        state.inbox_dir().join("0000000000000000.json"),
        state.reservation("0123456789abcdef"),
        state.disk_dir().join("0000000000000000.json"),
        state.reconcile_observation(),
        state.arbiter_last(),
    ] {
        std::fs::write(path, b"{").unwrap();
    }
    std::fs::write(state.records_file(), b"not a record\n").unwrap();

    let out = dirs.cmd().arg("ui").output().unwrap();

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let written: Snapshot =
        serde_json::from_slice(&std::fs::read(state.ui_snapshot()).unwrap()).unwrap();
    let mut scopes: Vec<&str> = written.errors.iter().map(|e| e.scope.as_str()).collect();
    scopes.sort_unstable();
    assert_eq!(
        scopes,
        [
            "arbiter",
            "disk",
            "inbox",
            "observation",
            "records",
            "reservations"
        ]
    );
    let page = std::fs::read_to_string(state.ui_page()).unwrap();
    assert_every_section(&page);
    let header = text(element(&page, "<header>", "</header>"));
    assert!(
        header.contains("Some parts could not be read in full"),
        "{header}"
    );
    assert!(header.contains("At least 0 for the owner"), "{header}");
    for (id, says) in [
        ("owner", "No item that could be read waits for the owner."),
        ("disk", "No disk state could be read."),
        (
            "inbox",
            "No item that could be read is open for the arbiter.",
        ),
        ("worktrees", "The last full observation could not be read."),
        ("reservations", "No reservation could be read."),
        ("arbiter", "The last arbiter pass could not be read."),
        ("records", "No record could be read."),
    ] {
        let section = text(section(&page, id));
        assert!(section.contains("Not read in full: "), "{id}: {section}");
        assert!(section.contains(says), "{id}: {section}");
    }
    for healthy in [
        "Nothing waits for the owner.",
        "No watched volume has been read yet.",
        "No open item for the arbiter.",
        "No full observation yet.",
        "No reservation.",
        "No arbiter pass yet.",
        "No record yet.",
    ] {
        assert!(!text(&page).contains(healthy), "the page says {healthy:?}");
    }
}

/// A file a listing found but that does not read, here a link to nothing in `inbox/`,
/// `disk/`, `reservations/`, and as a rotated record file, is an error for its part, never an
/// absent file.
#[cfg(unix)]
#[test]
fn listed_entries_that_do_not_read_are_errors() {
    let env = env_with(HOST_TOML);
    let nothing = env.state.root().join("nothing");
    for listed in [
        env.state.inbox_dir().join("0000000000000000.json"),
        env.state.disk_dir().join("0000000000000000.json"),
        env.state.reservation("0000000000000000"),
        env.state.rotated_records(1),
    ] {
        std::os::unix::fs::symlink(&nothing, listed).unwrap();
    }

    let built = snapshot::build(&env.config, &env.state).unwrap();

    let mut scopes: Vec<&str> = built.errors.iter().map(|e| e.scope.as_str()).collect();
    scopes.sort_unstable();
    assert_eq!(
        scopes,
        ["disk", "inbox", "records", "reservations"],
        "{:?}",
        built.errors
    );
    let page = render(&built);
    assert!(text(section(&page, "owner")).contains("No item that could be read waits"));
    assert!(text(section(&page, "disk")).contains("No disk state could be read."));
    assert!(text(section(&page, "reservations")).contains("No reservation could be read."));
    assert!(text(section(&page, "records")).contains("No record could be read."));
}

/// The reservations and the files among them that do not read come from one listing: a
/// directory named like a reservation leaves the other reservations shown, with the directory
/// as an error, and a file that did not parse when it was listed stays an error, its job not
/// shown, when a writer replaces it with a reservation before the states are derived, taking
/// the admission lock as a runner does, which the snapshot no longer holds.
#[test]
fn the_reservations_come_from_one_listing() {
    let dirs = Dirs::new();
    std::fs::write(dirs.config().join("host.toml"), HOST_TOML).unwrap();
    let state = StateDir::open(dirs.state()).unwrap();
    let mut reservation = serde_json::to_value(&fixed_snapshot().reservations.items[0]).unwrap();
    for derived in ["derived", "members", "verified"] {
        reservation.as_object_mut().unwrap().remove(derived);
    }
    let write = |job: &str| {
        let mut doc = reservation.clone();
        doc["job_id"] = json!(job);
        let _admission = cardinal::admission::lock(&state, Duration::from_secs(1))
            .expect("the admission lock is free");
        state
            .write_atomic(
                &state.reservation(job),
                &serde_json::to_vec(&doc).unwrap(),
                cardinal::state::Durability::Volatile,
            )
            .unwrap();
    };
    write("f1f2f3f4f5f60718");
    std::fs::create_dir(state.reservation("0000000000000001")).unwrap();
    std::fs::write(state.reservation("0000000000000002"), b"{").unwrap();
    let points = TestPoints::new(dirs.scope());
    let mut command = dirs.cmd();
    command.arg("ui");
    points.apply(&mut command, &[SNAPSHOT_RESERVATIONS_READ], None);
    let mut paused = command.spawn().unwrap();
    points.wait_reached(
        SNAPSHOT_RESERVATIONS_READ,
        paused.id(),
        Duration::from_secs(30),
    );

    write("0000000000000002");
    points.release(SNAPSHOT_RESERVATIONS_READ);
    let status = paused.wait().unwrap();

    assert!(status.success(), "{status}");
    let written: Snapshot =
        serde_json::from_slice(&std::fs::read(state.ui_snapshot()).unwrap()).unwrap();
    let jobs: Vec<&str> = written
        .reservations
        .items
        .iter()
        .map(|view| view.reservation.job_id.as_str())
        .collect();
    assert_eq!(jobs, ["f1f2f3f4f5f60718"]);
    let mut unread: Vec<&str> = written
        .errors
        .iter()
        .map(|e| {
            assert_eq!(e.scope, "reservations", "{:?}", written.errors);
            let file = e.message.split(": ").next().unwrap();
            Path::new(file).file_stem().unwrap().to_str().unwrap()
        })
        .collect();
    unread.sort_unstable();
    assert_eq!(unread, ["0000000000000001", "0000000000000002"]);
    let page = std::fs::read_to_string(state.ui_page()).unwrap();
    let shown = text(section(&page, "reservations"));
    assert!(shown.contains("f1f2f3f4f5f60718") && shown.contains("Not read in full"));
}

/// While another process holds the admission, inbox, or records lock past its deadline, the
/// part behind it shows as not read, and the rest of the page is written.
#[test]
fn busy_locks_mark_their_parts_not_read() {
    let env = env_with("[admission]\nlock_timeout_ms = 100\n");
    let hold = |path: &Path| {
        cardinal::state::try_lock(path, cardinal::state::LockMode::Exclusive)
            .unwrap()
            .expect("the lock is free")
    };
    let _locks = [
        hold(&env.state.admission_lock()),
        hold(&env.state.inbox_lock()),
        hold(&env.state.records_lock()),
    ];

    let built = snapshot::build(&env.config, &env.state).unwrap();

    let mut scopes: Vec<&str> = built.errors.iter().map(|e| e.scope.as_str()).collect();
    scopes.dedup();
    assert_eq!(scopes, ["reservations", "inbox", "records"]);
    // The reservations' reading failed: they show as not read, never as none.
    assert!(
        built.errors.iter().any(|e| e.scope == "reservations"
            && e.message.starts_with("the reservations could not be read")),
        "{:?}",
        built.errors
    );
    let page = render(&built);
    assert!(text(section(&page, "reservations")).contains("No reservation could be read."));
    assert!(text(section(&page, "owner")).contains("No item that could be read waits"));
    assert!(text(section(&page, "records")).contains("No record could be read."));
    assert!(text(section(&page, "disk")).contains("No watched volume has been read yet."));
}

/// The observation's own errors are kept with it and shown with its worktrees.
#[test]
fn the_observations_own_errors_are_shown() {
    let page = render(&fixed_snapshot());

    let worktrees = text(section(&page, "worktrees"));
    assert!(
        worktrees.contains(
            "The observation could not observe everything:repo:/src/other: not a git repository"
        ),
        "{worktrees}"
    );
}

fn write_item(state: &StateDir, item: &InboxItem) {
    std::fs::write(
        state.inbox_item(&item.key, 0),
        serde_json::to_vec(item).unwrap(),
    )
    .unwrap();
}

/// Writes the files a pass would have left for `snapshot`: its disk states, its observation
/// as `reconcile/observation.json`, its items with a resolved one beside them, its records
/// after one the page does not show, and its arbiter pass.
fn seed(state: &StateDir, snapshot: &Snapshot) {
    for disk in &snapshot.disk {
        write_json(&state.disk_state(&disk.volume), disk);
    }
    if let Some(observation) = &snapshot.observation {
        write_json(
            &state.reconcile_observation(),
            &json!({
                "schema": "cardinal.inventory.v1", "observed_ms": observation.observed_ms,
                "host": snapshot.host, "budget": { "cpu": 8 }, "reserved": {},
                "reservations": [], "repos": observation.repos, "outlived": [], "disk": [],
                "arbiter": { "running": false, "last": null }, "errors": observation.errors
            }),
        );
    }
    for item in &snapshot.inbox {
        write_item(state, &item.item);
    }
    write_item(
        state,
        &serde_json::from_value(inbox_item(
            "reclaimable:/w/x@1",
            "reclaimable",
            "resolved",
            0,
        ))
        .unwrap(),
    );
    let mut records = format!("{}\n", record("job_started", 1));
    for record in &snapshot.records {
        records.push_str(&serde_json::to_string(record).unwrap());
        records.push('\n');
    }
    std::fs::write(state.records_file(), records).unwrap();
    if let Some(arbiter) = &snapshot.arbiter {
        write_json(&state.arbiter_last(), arbiter);
    }
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) {
    std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}
