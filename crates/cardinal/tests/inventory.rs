//! `cardinal inventory` (`docs/architecture.md` sections 8 and 11): host facts, budgets and
//! reservations, in-use state, ignored data, sizes, and outlived processes. Reservations
//! come from a fake until the runner work unit provides `admission::list_reservations`.

mod observe_common;
mod support;

use std::num::NonZeroU64;
use std::path::Path;
use std::time::Duration;

use cardinal::contract::{
    DerivedState, Error, ErrorCode, IgnoredClass, Reservation, ReservationView,
    ReservationWorktree, SessionProcess, SessionSnapshot, SessionSnapshotSchema, SnapshotAgent,
};
use cardinal::state::{LockMode, try_lock, worktree_key};
use observe_common::{FakeReservations, Scene, worktree};
use support::helper::{self, Mode};

#[test]
fn helper() {
    helper::run_if_requested();
}

fn reservation(
    job: &str,
    derived: DerivedState,
    worktree: Option<&Path>,
    cpu: u64,
) -> ReservationView {
    let reservation: Reservation = serde_json::from_value(serde_json::json!({
        "schema": "cardinal.reservation.v1",
        "job_id": job,
        "state": "running",
        "boot_s": 1, "created_ms": 2, "updated_ms": 3,
        "runner": {"pid": 1, "start_s": 1},
        "group": {"kind": "pgid", "pgid": 1, "leader_start_s": 1},
        "child": null, "stop_signal_ms": null, "priority": "normal",
        "resources": {"cpu": cpu},
        "admission": "within_budget",
        "class": null, "rule": null, "worktree": null, "session": null,
        "cwd": "/", "argv": ["cargo", "test"]
    }))
    .unwrap();
    ReservationView {
        reservation: Reservation {
            worktree: worktree.map(|path| ReservationWorktree {
                key: worktree_key(&dunce::canonicalize(path).unwrap()),
                path: path.to_string_lossy().into_owned(),
                protected: true,
            }),
            ..reservation
        },
        derived,
        members: vec![],
        verified: false,
    }
}

#[test]
fn the_binary_prints_one_inventory_document() {
    let scene = Scene::new();
    std::fs::write(scene.config_dir.join("host.toml"), "").unwrap();
    let output = scene
        .cardinal()
        .args(["inventory", "--repo"])
        .arg(&scene.repo)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(text.lines().count(), 1);
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(doc["schema"], "cardinal.inventory.v1");
    assert_eq!(doc["host"]["id"].as_str().unwrap().len(), 16);
    assert!(["macos", "linux", "windows"].contains(&doc["host"]["os"].as_str().unwrap()));
    assert!(doc["host"]["boot_s"].as_u64().unwrap() > 0);
    assert_eq!(doc["repos"][0]["worktrees"][0]["main"], true);
    assert_eq!(
        doc["repos"][0]["worktrees"][0]["allocated_bytes"],
        serde_json::Value::Null
    );
    let parsed: cardinal::contract::Inventory = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed.repos.len(), 1);
}

#[test]
fn budget_reserved_and_reservations() {
    let scene = Scene::new();
    std::fs::write(
        scene.config_dir.join("host.toml"),
        "[budget]\ncpu = 8\ngpu = 1\n",
    )
    .unwrap();
    let config = cardinal::config::Config::load(&scene.config_dir).unwrap();
    let fake = FakeReservations(Ok(vec![
        reservation("aaaaaaaaaaaaaaaa", DerivedState::Live, None, 2),
        reservation("bbbbbbbbbbbbbbbb", DerivedState::RecoveryNeeded, None, 3),
        reservation("cccccccccccccccc", DerivedState::Stale, None, 4),
    ]));
    let inv = cardinal::observe::Observer::new(&config)
        .with_env(scene.env())
        .with_reservations(&fake)
        .inventory(
            &scene.state(),
            &cardinal::observe::InventoryOptions::default(),
        )
        .unwrap();
    assert_eq!(inv.budget["cpu"], NonZeroU64::new(8).unwrap());
    assert_eq!(inv.budget["gpu"], NonZeroU64::new(1).unwrap());
    assert_eq!(inv.reserved["cpu"].get(), 5, "live plus recovery-needed");
    assert_eq!(inv.reservations.len(), 3);
    assert!(inv.repos.is_empty(), "nothing configured, nothing asked");
}

#[test]
fn reservations_lock_and_processes_make_a_worktree_in_use() {
    let scene = Scene::new();
    let by_job = scene.worktree("by-job", "by-job");
    let stale = scene.worktree("stale", "stale");
    let by_lock = scene.worktree("by-lock", "by-lock");
    let by_process = scene.worktree("by-process", "by-process");
    let free = scene.worktree("free", "free");
    let fake = FakeReservations(Ok(vec![
        reservation(
            "aaaaaaaaaaaaaaaa",
            DerivedState::RecoveryNeeded,
            Some(&by_job),
            1,
        ),
        reservation("bbbbbbbbbbbbbbbb", DerivedState::Stale, Some(&stale), 1),
    ]));
    let state_dir = scene.state();
    let lock = try_lock(
        &state_dir.worktree_lock(&worktree_key(&by_lock)),
        LockMode::Shared,
    )
    .unwrap()
    .unwrap();
    // Reading another process's working directory is tested where the OS backend reads it
    // reliably; elsewhere the worktree is simply not in use.
    let mut child = cfg!(unix).then(|| helper::spawn(&Mode::Sleep, Some(&by_process)));
    if let Some(child) = &child {
        let pid = child.id();
        helper::wait_for("the helper's cwd", Duration::from_secs(10), || {
            (cardinal::os::cwd(pid).as_deref() == Some(by_process.as_path())).then_some(())
        });
    }

    let inv = scene.inventory_with(&fake, false);
    let w = worktree(&inv, &by_job);
    assert!(w.facts.in_use.value);
    assert_eq!(w.facts.in_use.jobs, ["aaaaaaaaaaaaaaaa"]);
    assert!(w.reclaim.failed.contains(&"not_in_use".to_owned()));
    assert!(
        !worktree(&inv, &stale).facts.in_use.value,
        "a stale reservation counts for nothing"
    );
    assert!(worktree(&inv, &by_lock).facts.in_use.value);
    assert_eq!(
        worktree(&inv, &by_process).facts.in_use.value,
        child.is_some()
    );
    assert!(!worktree(&inv, &free).facts.in_use.value);
    drop(lock);
    if let Some(child) = &mut child {
        child.kill();
    }
}

#[test]
fn unreadable_reservations_leave_in_use_unknown() {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    let fake = FakeReservations(Err(Error::new(ErrorCode::StateUnavailable, "broken")));
    let inv = scene.inventory_with(&fake, false);
    assert!(
        inv.errors
            .iter()
            .any(|e| e.scope == "reservations" && e.message == "broken")
    );
    let w = worktree(&inv, &wt);
    assert!(w.reclaim.unknown.contains(&"not_in_use".to_owned()));
    assert!(!w.reclaim.candidate);
}

#[test]
fn inventory_is_read_only_for_the_state_dir() {
    let scene = Scene::new();
    scene.worktree("feat", "feat");
    let state = scene.state();
    let fake = FakeReservations::none();
    scene.inventory_with(&fake, true);
    assert_eq!(
        std::fs::read_dir(state.worktrees_dir()).unwrap().count(),
        0,
        "probing in-use locks creates no lock files"
    );
}

#[test]
fn ignored_entries_are_classified_and_sized() {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    std::fs::write(wt.join(".gitignore"), "node_modules/\ntarget/\nnotes.log\n").unwrap();
    scene.fx.git(&wt, &["add", ".gitignore"]);
    scene.fx.git(&wt, &["commit", "-q", "-m", "ignore"]);
    for (rel, bytes) in [
        ("node_modules/pkg/index.js", 5000),
        ("target/a/b/fuzz/corpus/seed", 10),
        ("notes.log", 10),
    ] {
        let path = wt.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![b'x'; bytes]).unwrap();
    }
    scene.profile(
        "data",
        "[data]\nregenerable = ['node_modules', 'target']\nprotected = ['fuzz/corpus']\n",
    );
    let class_of = |inv: &cardinal::contract::Inventory, rel: &str| {
        let w = worktree(inv, &wt);
        let path = wt.join(rel).to_string_lossy().into_owned();
        w.ignored
            .iter()
            .find(|e| e.path == path)
            .unwrap_or_else(|| panic!("{path} not in {:?}", w.ignored))
            .clone()
    };

    let inv = scene.inventory();
    assert_eq!(
        class_of(&inv, "node_modules").class,
        IgnoredClass::Regenerable
    );
    assert_eq!(class_of(&inv, "node_modules").allocated_bytes, None);
    assert_eq!(class_of(&inv, "target").class, IgnoredClass::Protected);
    assert_eq!(
        class_of(&inv, "notes.log").class,
        IgnoredClass::Unclassifiable
    );
    let w = worktree(&inv, &wt);
    assert_eq!(w.allocated_bytes, None);
    for check in ["no_protected", "no_unclassifiable"] {
        assert!(w.reclaim.failed.contains(&check.to_owned()), "{check}");
    }

    let inv = scene.inventory_with(&FakeReservations::none(), true);
    let sized = class_of(&inv, "node_modules").allocated_bytes.unwrap();
    assert!(sized >= 5000, "{sized}");
    assert_eq!(
        class_of(&inv, "target").allocated_bytes,
        None,
        "only regenerable entries are sized"
    );
    assert!(worktree(&inv, &wt).allocated_bytes.unwrap() >= sized);
}

#[test]
fn a_nested_repository_in_ignored_data_is_reported() {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    std::fs::write(wt.join(".gitignore"), "vendor/\n").unwrap();
    scene.fx.git(&wt, &["add", ".gitignore"]);
    scene.fx.git(&wt, &["commit", "-q", "-m", "ignore"]);
    std::fs::create_dir_all(wt.join("vendor/dep/.git")).unwrap();
    scene.profile("data", "[data]\nregenerable = ['vendor']\n");
    let inv = scene.inventory();
    let w = worktree(&inv, &wt);
    assert_eq!(w.ignored[0].class, IgnoredClass::NestedRepository);
    assert!(
        w.reclaim
            .failed
            .contains(&"no_nested_repository".to_owned())
    );
}

#[test]
fn repositories_are_deduplicated_by_common_dir() {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    let config = scene.config();
    let inv = cardinal::observe::Observer::new(&config)
        .with_env(scene.env())
        .with_reservations(&FakeReservations::none())
        .inventory(
            &scene.state(),
            &cardinal::observe::InventoryOptions {
                repos: vec![scene.repo.clone(), wt.clone()],
                sizes: false,
            },
        )
        .unwrap();
    assert_eq!(inv.repos.len(), 1);
    assert_eq!(inv.repos[0].worktrees.len(), 2);
}

#[test]
fn processes_that_outlived_their_session_are_listed() {
    let scene = Scene::new();
    let state = scene.state();
    let mut child = helper::spawn(&Mode::Sleep, None);
    let pid = child.id();
    let start_s = helper::wait_for("the helper", Duration::from_secs(10), || {
        cardinal::os::start_time_s(pid)
    });
    let me = cardinal::os::current_pid();
    let my_start = cardinal::os::start_time_s(me).unwrap();
    let snapshot = |session: &str, agent: SnapshotAgent| SessionSnapshot {
        schema: SessionSnapshotSchema,
        session_id: session.into(),
        reason: "other".into(),
        ended_ms: 1,
        agent,
        processes: vec![SessionProcess {
            pid,
            start_s,
            name: "helper".into(),
            command: "helper".into(),
        }],
    };
    let gone = SnapshotAgent {
        pid,
        start_s: start_s + 1,
        name: "claude".into(),
    };
    let alive = SnapshotAgent {
        pid: me,
        start_s: my_start,
        name: "claude".into(),
    };
    for doc in [snapshot("ended", gone), snapshot("running", alive)] {
        std::fs::write(
            state.session(&doc.session_id),
            serde_json::to_vec(&doc).unwrap(),
        )
        .unwrap();
    }
    std::fs::write(state.session("torn"), b"{").unwrap();

    let inv = scene.inventory();
    assert_eq!(inv.outlived.len(), 1, "{:?}", inv.outlived);
    assert_eq!(inv.outlived[0].session_id, "ended");
    assert_eq!(inv.outlived[0].processes[0].pid, pid);
    assert!(inv.errors.iter().any(|e| e.scope.starts_with("session:")));

    child.kill();
    let inv = scene.inventory();
    assert!(
        inv.outlived.is_empty(),
        "a process that ended is not listed"
    );
}
