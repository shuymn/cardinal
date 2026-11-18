//! `cardinal inventory` (`docs/architecture.md` sections 8 and 11): host facts, budgets and
//! reservations, in-use state, ignored data, sizes, and outlived processes. Reservations and
//! the in-use locks they come with are made by jobs of the real runner (`support::jobs`).

mod observe_common;
mod support;

use std::path::Path;
use std::time::Duration;

use cardinal::contract::{
    IgnoredClass, SessionProcess, SessionSnapshot, SessionSnapshotSchema, SnapshotAgent,
};
use cardinal::state::{LockMode, try_lock, worktree_key};
use cardinal_test_support::Child;
use cardinal_test_support::process::wait_for;
use observe_common::{Scene, worktree};
use support::helper::{self, Mode};

/// A sleeping helper in `cwd`, inside the fixture.
fn spawn_helper(scene: &Scene, cwd: &Path) -> Child {
    helper::spawn(scene.fx.scope(), &Mode::Sleep, cwd)
}

#[test]
fn helper() {
    helper::run_if_requested();
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

/// The configured budget, and the reservations of a live job, a job whose runner was killed
/// while its group runs, and a job whose runner was killed before it started: only the first
/// two count as reserved. The stale job comes last, since the admission pass of any later
/// runner would delete its reservation. The budget admits all three jobs at once.
#[cfg(unix)]
#[test]
fn budget_reserved_and_reservations() {
    use std::num::NonZeroU64;

    use cardinal::contract::DerivedState;

    let mut scene = Scene::new();
    scene.observe_config("[budget]\ncpu = 16\ngpu = 1");
    scene.config();
    let jobs = support::jobs::Jobs::new(scene.fx.scope(), &scene.state_dir, &scene.config_dir);
    let live = jobs.running(&scene.repo, 2);
    let crashed = jobs.crashed(&scene.repo, 3);
    let stale = jobs.stale(&scene.repo, 4);

    let inv = scene.inventory();
    assert_eq!(inv.budget["cpu"], NonZeroU64::new(16).unwrap());
    assert_eq!(inv.budget["gpu"], NonZeroU64::new(1).unwrap());
    assert_eq!(inv.reserved["cpu"].get(), 5, "live plus recovery-needed");
    let mut found: Vec<(&str, DerivedState)> = inv
        .reservations
        .iter()
        .map(|v| (v.reservation.job_id.as_str(), v.derived))
        .collect();
    found.sort_unstable_by_key(|(job, _)| *job);
    let mut expected = vec![
        (live.job.as_str(), DerivedState::Live),
        (crashed.job.as_str(), DerivedState::RecoveryNeeded),
        (stale.as_str(), DerivedState::Stale),
    ];
    expected.sort_unstable_by_key(|(job, _)| *job);
    assert_eq!(found, expected, "{:#?}", inv.reservations);
    assert_eq!(inv.repos.len(), 1);
}

/// A worktree is in use while a live job runs in it (its reservation and the in-use lock its
/// runner holds) and while the reservation of a job whose runner was killed counts; the
/// reservation of a job that never started counts for nothing. The budget admits the three
/// one-CPU jobs at once, whatever the host's CPU count.
#[cfg(unix)]
#[test]
fn jobs_make_a_worktree_in_use() {
    let mut scene = Scene::new();
    let by_live = scene.worktree("by-live", "by-live");
    let by_crashed = scene.worktree("by-crashed", "by-crashed");
    let stale = scene.worktree("stale", "stale");
    let free = scene.worktree("free", "free");
    scene.observe_config("[budget]\ncpu = 3");
    scene.config();
    let jobs = support::jobs::Jobs::new(scene.fx.scope(), &scene.state_dir, &scene.config_dir);
    let live = jobs.running(&by_live, 1);
    let crashed = jobs.crashed(&by_crashed, 1);
    let stale_job = jobs.stale(&stale, 1);

    let inv = scene.inventory();
    for (wt, job) in [(&by_live, &live.job), (&by_crashed, &crashed.job)] {
        let w = worktree(&inv, wt);
        assert!(w.facts.in_use.value, "{w:#?}");
        assert_eq!(w.facts.in_use.jobs, [job.as_str()], "{w:#?}");
        assert!(w.reclaim.failed.contains(&"not_in_use".to_owned()));
    }
    let w = worktree(&inv, &stale);
    assert!(
        !w.facts.in_use.value && w.facts.in_use.jobs.is_empty(),
        "a stale reservation ({stale_job}) counts for nothing: {w:#?}"
    );
    assert!(!worktree(&inv, &free).facts.in_use.value);
}

/// A worktree is in use while someone holds its in-use lock or a process works inside it.
#[test]
fn locks_and_processes_make_a_worktree_in_use() {
    let scene = Scene::new();
    let by_lock = scene.worktree("by-lock", "by-lock");
    let by_process = scene.worktree("by-process", "by-process");
    let free = scene.worktree("free", "free");
    let state_dir = scene.state();
    let lock = try_lock(
        &state_dir.worktree_lock(&worktree_key(&by_lock)),
        LockMode::Shared,
    )
    .unwrap()
    .unwrap();
    // Reading another process's working directory is tested where the OS backend reads it
    // reliably; elsewhere the worktree is simply not in use.
    let mut child = cfg!(unix).then(|| spawn_helper(&scene, &by_process));
    if let Some(child) = &child {
        let pid = child.id();
        wait_for("the helper's cwd", Duration::from_secs(10), || {
            (cardinal::os::cwd(pid).as_deref() == Some(by_process.as_path())).then_some(())
        });
    }

    let inv = scene.inventory();
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

/// Reservations that cannot be read are an error of the inventory, and leave every
/// worktree's in-use state unknown.
#[test]
fn unreadable_reservations_leave_in_use_unknown() {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    let config = scene.config();
    // Opened first: opening the state dir creates `reservations/` and fails on a file there.
    let state = scene.state();
    let reservations = state.reservations_dir();
    std::fs::remove_dir(&reservations).unwrap();
    std::fs::write(&reservations, "not a directory").unwrap();
    let inv = cardinal::observe::Observer::new(&config)
        .with_env(scene.env())
        .inventory(
            &state,
            &cardinal::observe::InventoryOptions {
                repos: vec![scene.repo.clone()],
                sizes: false,
            },
        )
        .unwrap();
    assert!(
        inv.errors.iter().any(|e| e.scope == "reservations"),
        "{:#?}",
        inv.errors
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
    scene.inventory_with(true);
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
    scene.fx.add(&wt, ".gitignore");
    scene.fx.commit(&wt, "ignore");
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

    let inv = scene.inventory_with(true);
    let sized = class_of(&inv, "node_modules").allocated_bytes.unwrap();
    assert!(sized >= 5000, "{sized}");
    // Entries matching a regenerable pattern are sized whatever their class, so the disk
    // report can count a protected `target/` as blocked (sections 8.3 and 10.4).
    assert!(class_of(&inv, "target").allocated_bytes.unwrap() > 0);
    assert_eq!(
        class_of(&inv, "notes.log").allocated_bytes,
        None,
        "only entries matching a regenerable pattern are sized"
    );
    assert!(worktree(&inv, &wt).allocated_bytes.unwrap() >= sized);
}

#[test]
fn a_nested_repository_in_ignored_data_is_reported() {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    std::fs::write(wt.join(".gitignore"), "vendor/\n").unwrap();
    scene.fx.add(&wt, ".gitignore");
    scene.fx.commit(&wt, "ignore");
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
    let mut child = spawn_helper(&scene, scene.fx.root());
    let pid = child.id();
    let start_s = wait_for("the helper", Duration::from_secs(10), || {
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
