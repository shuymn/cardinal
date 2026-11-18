//! The `reconcile` pass (`docs/architecture.md` sections 10.3 and 13.6): one at a time
//! (A19), the inbox refresh at step 3, which raises processes that outlived their session
//! and clears them once they are gone (C9), the handoff of the disk items after the
//! observation, and the arbiter trigger at step 7, which starts a pass for due items and
//! none for disk items. The readings fake keeps every volume above its lines unless a test
//! says otherwise.

mod disk_support;
mod observe_common;
mod plan_support;
mod support;

use std::process::Stdio;
use std::time::Duration;

use cardinal::contract::{
    ArbiterStart, DiskLevel, InboxItem, InboxKind, InboxStatus, ResolutionOutcome, SessionProcess,
    SessionSnapshot, SessionSnapshotSchema, Skipped, Snapshot, SnapshotAgent, Tri, WorktreeView,
};
use cardinal::inbox::{self, StatusFilter};
use cardinal::testpoint::{RECONCILE_OBSERVED, release_file};
use cardinal_test_support::Child;
use cardinal_test_support::process::wait_for;
use disk_support::{Disk, MIB, carried_report, own_errors, pass, refused_start};
use support::helper::{self, Mode};

#[test]
fn helper() {
    helper::run_if_requested();
}

const LINES: (u64, u64, u64) = (1, 2, 3);

/// A pass paused at `reconcile.observed` holds `reconcile.lock`: a second pass prints
/// `skipped: "busy"`, exits 0, and reads and writes nothing (no reading is taken, no disk
/// state is written). Released, the first pass completes, and the next one runs.
#[test]
fn one_pass_at_a_time() {
    let d = Disk::new(LINES);
    d.fixture_reads(&[Some(10 * MIB)]);
    let mut first = d.reconcile_command();
    d.points.apply(&mut first, &[RECONCILE_OBSERVED], None);
    let first = first.stdout(std::process::Stdio::piped()).spawn().unwrap();
    d.points
        .wait_reached(RECONCILE_OBSERVED, first.id(), Duration::from_secs(30));
    let states = std::fs::read(
        d.s.state.disk_dir().join(
            std::fs::read_dir(d.s.state.disk_dir())
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .file_name(),
        ),
    )
    .unwrap();
    let readings = std::fs::read(d.points.dir().join("disk-readings.json")).unwrap();

    let busy = pass(
        &d.reconcile_command()
            .output_within(plan_support::RUN_LIMIT)
            .unwrap(),
    );
    assert_eq!(busy.skipped, Some(Skipped::Busy));
    assert!(busy.disk.is_empty() && !busy.observed, "{busy:#?}");
    assert_eq!(d.states().len(), 1);
    assert_eq!(serde_json::to_vec(&d.state()).unwrap().len(), states.len());
    assert_eq!(
        std::fs::read(d.points.dir().join("disk-readings.json")).unwrap(),
        readings
    );

    d.points.release(RECONCILE_OBSERVED);
    let done = pass(&first.wait_with_output().unwrap());
    assert_eq!(done.skipped, None);
    assert!(done.observed);
    assert_eq!(own_errors(&done), Vec::<String>::new());
    let next = d.reconcile();
    assert_eq!(next.skipped, None);
    assert_eq!(d.state().readings.len(), 2);
}

/// A pass started paused at `reconcile.observed`, once it is there.
fn paused_pass(d: &Disk) -> Child {
    match std::fs::remove_file(release_file(d.points.dir(), RECONCILE_OBSERVED)) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => panic!("{err}"),
        _ => {}
    }
    let mut command = d.reconcile_command();
    d.points.apply(&mut command, &[RECONCILE_OBSERVED], None);
    let child = command.stdout(Stdio::piped()).spawn().unwrap();
    d.points
        .wait_reached(RECONCILE_OBSERVED, child.id(), Duration::from_secs(30));
    child
}

/// Section 13.6, the handoff: paused at `reconcile.observed`, a pass under pressure has
/// written no disk item; released, its `disk_low` and `disk_critical` items carry its disk
/// report, and so do they on the page the pass wrote; the trigger starts no arbiter pass for
/// them. The pass ends only the items it read before step 4: `disk_critical`, escalated by
/// the owner while the next pass observes, keeps the escalation although that pass left
/// `critical`; the pass after it ends the item, and `disk_low` stays until the recovery line.
#[test]
fn disk_items_follow_the_observation() {
    let d = Disk::new(LINES);
    let critical = MIB;
    d.fixture_reads(&[
        Some(critical / 2),
        Some(critical / 2),
        Some(critical + 1),
        Some(critical + 1),
        Some(critical + 1),
        Some(critical + 1),
        Some(3 * MIB + 1),
    ]);
    let first = paused_pass(&d);
    assert_eq!(d.disk_items(), Vec::new());
    d.points.release(RECONCILE_OBSERVED);
    let first = pass(&first.wait_with_output().unwrap());
    assert_eq!(own_errors(&first), Vec::<String>::new());
    assert_eq!(first.disk[0].state, DiskLevel::Critical);
    assert_eq!(
        first.arbiter,
        ArbiterStart::NotNeeded,
        "disk items are not due"
    );
    assert!(!d.s.state.arbiter_last().exists(), "no arbiter pass");
    let items = d.disk_items();
    let kinds: Vec<_> = items.iter().map(|i| (i.kind, i.status)).collect();
    assert_eq!(
        kinds,
        [
            (InboxKind::DiskLow, InboxStatus::Open),
            (InboxKind::DiskCritical, InboxStatus::Open),
        ]
    );
    for item in &items {
        assert_eq!(
            carried_report(item).as_ref(),
            Some(disk_support::report(&first))
        );
        assert!(first.inbox.raised.contains(&item.key), "{first:#?}");
    }
    // The page the pass wrote at step 8 read the items as complete as the store holds them.
    let page: Snapshot =
        serde_json::from_slice(&std::fs::read(d.s.state.ui_snapshot()).unwrap()).unwrap();
    let mut shown: Vec<&InboxItem> = page
        .inbox
        .iter()
        .map(|i| &i.item)
        .filter(|i| matches!(i.kind, InboxKind::DiskLow | InboxKind::DiskCritical))
        .collect();
    shown.sort_by_key(|i| i.kind == InboxKind::DiskCritical);
    assert_eq!(shown, items.iter().collect::<Vec<_>>());
    assert_eq!(page.arbiter, None, "the trigger started no arbiter pass");

    let second = paused_pass(&d);
    inbox::resolve(
        &d.s.config(),
        &d.s.state,
        &items[1].key,
        ResolutionOutcome::Escalated,
        None,
    )
    .unwrap();
    d.points.release(RECONCILE_OBSERVED);
    let second = pass(&second.wait_with_output().unwrap());
    assert_eq!(own_errors(&second), Vec::<String>::new());
    assert_eq!(second.disk[0].state, DiskLevel::Warning);
    let (open, escalated, resolved) = (
        Some(InboxStatus::Open),
        Some(InboxStatus::Escalated),
        Some(InboxStatus::Resolved),
    );
    assert_eq!(d.disk_statuses(), (open, escalated));
    assert_eq!(d.disk_items()[1].ended_ms, None);

    d.reconcile();
    assert_eq!(d.disk_statuses(), (open, resolved));
    assert_eq!(
        d.disk_items()[1].resolution.as_ref().unwrap().by,
        "cardinal"
    );
    let last = d.reconcile();
    assert_eq!(last.disk[0].state, DiskLevel::Ok);
    assert_eq!(d.disk_statuses(), (resolved, resolved));
}

/// A records file that a rotation keeping one file set aside in `tmp/` and never removed (a
/// crash between the two) is removed by a pass once it is older than the cleanup's hour; a
/// younger one stays.
#[test]
fn a_pass_removes_a_records_file_a_rotation_left_behind() {
    let d = Disk::new(LINES);
    d.fixture_reads(&[Some(10 * MIB)]);
    let (old, young) = (
        cardinal::record::set_aside(&d.s.state),
        cardinal::record::set_aside(&d.s.state),
    );
    for path in [&old, &young] {
        std::fs::write(path, b"{}\n").unwrap();
    }
    std::fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_hours(2))
        .unwrap();

    let pass = d.reconcile();

    assert_eq!(own_errors(&pass), Vec::<String>::new());
    assert!(!old.exists(), "{} is left", old.display());
    assert!(young.exists(), "{} was removed", young.display());
}

/// C9: a process listed in a session snapshot that is alive after the session's agent ended
/// is raised at the next pass, and cleared by cardinal at the first pass after it ended.
#[test]
fn refresh_raises_and_clears() {
    let d = Disk::new(LINES);
    d.fixture_reads(&[Some(10 * MIB)]);
    let scope = d.s.fx.scope();
    let mut agent = helper::spawn(scope, &Mode::Sleep, scope.root());
    let mut server = helper::spawn(scope, &Mode::Sleep, scope.root());
    let start = |pid: u32| {
        wait_for("the process start time", Duration::from_secs(10), || {
            cardinal::os::start_time_s(pid)
        })
    };
    let (agent_start, server_start) = (start(agent.id()), start(server.id()));
    let snapshot = SessionSnapshot {
        schema: SessionSnapshotSchema,
        session_id: "s1".into(),
        reason: "prompt_input_exit".into(),
        ended_ms: 1,
        agent: SnapshotAgent {
            pid: agent.id(),
            start_s: agent_start,
            name: "claude".into(),
        },
        processes: vec![SessionProcess {
            pid: server.id(),
            start_s: server_start,
            name: "dev-server".into(),
            command: "dev-server --port 3000".into(),
        }],
    };
    std::fs::write(
        d.s.state.session("s1"),
        serde_json::to_vec(&snapshot).unwrap(),
    )
    .unwrap();
    agent.kill();
    let config = d.s.config();
    let items = || {
        inbox::list(&config, &d.s.state, Some(StatusFilter::All), false)
            .unwrap()
            .items
            .into_iter()
            .filter(|i| i.key.starts_with("outlived:s1"))
            .collect::<Vec<_>>()
    };
    assert_eq!(items().len(), 0);

    let raised = d.reconcile();
    assert_eq!(own_errors(&raised), Vec::<String>::new());
    let open = items();
    assert_eq!(open.len(), 1, "{open:#?}");
    assert_eq!(open[0].status, InboxStatus::Open);

    server.kill();
    d.reconcile();
    let cleared = items();
    assert_eq!(cleared.len(), 1, "{cleared:#?}");
    assert_eq!(cleared[0].status, InboxStatus::Resolved);
    assert_eq!(cleared[0].resolution.as_ref().unwrap().by, "cardinal");
    assert!(!d.s.state.session("s1").exists());
}

/// Step 2 removes only the temporary files cardinal's atomic writes leave: a state dir that
/// overlaps other files (`CARDINAL_STATE_DIR` pointing into a worktree) keeps them, however
/// old. The control, a leftover temporary file as old, is removed.
#[test]
fn temp_cleanup_keeps_other_files() {
    let d = Disk::new(LINES);
    d.fixture_reads(&[Some(8 * MIB)]);
    let tmp = d.s.state.tmp_dir();
    let two_hours_ago = std::time::SystemTime::now() - Duration::from_hours(2);
    let names = [
        "notes.txt",
        "build.log.1.2",
        ".cardinal-tmp.",
        ".cardinal-tmp.state.json.12.3",
    ];
    for name in names {
        let file = std::fs::File::create(tmp.join(name)).unwrap();
        file.set_modified(two_hours_ago).unwrap();
    }

    let pass = d.reconcile();
    assert_eq!(own_errors(&pass), Vec::<String>::new());
    for name in &names[..3] {
        assert!(tmp.join(name).exists(), "{name} was removed");
    }
    assert!(!tmp.join(names[3]).exists(), "the leftover temp file stays");
}

/// The item `pass` raised whose key starts with `prefix`.
fn raised(d: &Disk, pass: &cardinal::contract::Reconcile, prefix: &str) -> InboxItem {
    let key = pass
        .inbox
        .raised
        .iter()
        .find(|key| key.starts_with(prefix))
        .unwrap_or_else(|| panic!("no {prefix} item raised: {pass:#?}"));
    inbox::list(&d.s.config(), &d.s.state, Some(StatusFilter::All), false)
        .unwrap()
        .items
        .into_iter()
        .find(|item| item.key == *key)
        .unwrap()
}

/// Section 13.6, the handoff to the arbiter: under pressure, a pass that raises a due worktree
/// item along with its disk items starts an arbiter pass at step 7, which is assigned that item
/// and no disk item; the next pass, with nothing due, starts none, and its page shows the pass
/// with the item. On Windows the test's job refuses the launcher's start: the pass reports it
/// and assigns nothing.
#[test]
fn a_due_worktree_item_starts_an_arbiter_pass() {
    let d = Disk::new(LINES);
    d.fixture_reads(&[Some(MIB + MIB / 2)]);
    d.s.worktree("done");
    let since_ms = cardinal::state::now_ms();
    let first = d.reconcile();
    assert_eq!(own_errors(&first), Vec::<String>::new());
    assert_eq!(first.disk[0].state, DiskLevel::Warning);
    let reclaimable = raised(&d, &first, "reclaimable:");
    let disk_low = raised(&d, &first, "disk_low:");
    if cfg!(windows) {
        assert!(refused_start(&first), "{first:#?}");
        assert_eq!(reclaimable.arbiter, None, "never assigned");
        return;
    }
    assert_eq!(first.arbiter, ArbiterStart::Started);
    let arbiter = d.arbiter_ended(since_ms);
    assert_eq!(arbiter.items, std::slice::from_ref(&reclaimable.key));
    let assigned = |item: &InboxItem| {
        inbox::list(&d.s.config(), &d.s.state, Some(StatusFilter::All), false)
            .unwrap()
            .items
            .into_iter()
            .find(|i| i.key == item.key)
            .unwrap()
            .arbiter
            .map(|a| a.pass)
    };
    assert_eq!(assigned(&reclaimable), Some(arbiter.id.clone()));
    assert_eq!(assigned(&disk_low), None, "never assigned");

    let next = d.reconcile();
    assert_eq!(own_errors(&next), Vec::<String>::new());
    assert_eq!(next.arbiter, ArbiterStart::NotNeeded);
    // The page the next pass wrote shows the arbiter pass the first one's trigger started, and
    // the item assigned to it; the launcher records the pass on its own, so the first pass's
    // page, written meanwhile, may not.
    let page: Snapshot =
        serde_json::from_slice(&std::fs::read(d.s.state.ui_snapshot()).unwrap()).unwrap();
    assert_eq!(page.arbiter.as_ref(), Some(&arbiter));
    let shown = page
        .inbox
        .iter()
        .find(|i| i.item.key == reclaimable.key)
        .expect("the reclaimable item on the page");
    assert_eq!(
        shown.item.arbiter.as_ref().map(|a| &a.pass),
        Some(&arbiter.id)
    );
}

/// A `closed_pr_dirty` item from a real pass reaches the arbiter with what its subject check
/// compares (`integrations/claude-code/skills/cardinal-arbiter/SKILL.md`): its pass is
/// assigned it, and `cardinal inventory --worktree` shows its worktree with the item's HEAD,
/// dirty, and with the closed pull request its key names. On Windows the test's job refuses the
/// launcher's start, and the worktree's view is checked alone.
#[test]
fn a_closed_pr_dirty_item_reaches_the_subject_check() {
    let d = Disk::new(LINES);
    d.fixture_reads(&[Some(10 * MIB)]);
    let wt = d.s.fx.worktree_add(&d.s.main, "abandoned", "abandoned");
    d.s.fx
        .commit_file(&wt, "src/lib.rs", "// abandoned\n", "work");
    d.s.publish(&wt, "abandoned");
    let head = d.s.head(&wt);
    d.s.set_closed(&format!(
        r#"[{{"number":7,"state":"CLOSED","headRefOid":"{head}","url":"https://github.com/acme/app/pull/7"}}]"#
    ));
    std::fs::write(wt.join("src/lib.rs"), "// changed\n").unwrap();

    let since_ms = cardinal::state::now_ms();
    let first = d.reconcile();
    assert_eq!(own_errors(&first), Vec::<String>::new());
    let item = raised(&d, &first, "closed_pr_dirty:");
    let worktree = item.subject.worktree.clone().expect("the item's worktree");
    assert!(
        item.key
            .starts_with(&format!("closed_pr_dirty:{worktree}#7@")),
        "{}",
        item.key
    );
    if cfg!(windows) {
        assert!(refused_start(&first), "{first:#?}");
    } else {
        assert_eq!(first.arbiter, ArbiterStart::Started);
        let arbiter = d.arbiter_ended(since_ms);
        assert!(arbiter.items.contains(&item.key), "{arbiter:#?}");
    }

    let output =
        d.s.cardinal()
            .args(["inventory", "--worktree", &worktree])
            .output_within(plan_support::RUN_LIMIT)
            .unwrap();
    assert!(output.status.success(), "{output:?}");
    let view: WorktreeView = serde_json::from_slice(&output.stdout).unwrap();
    let listed = &view.worktree;
    assert_eq!(listed.path, worktree);
    assert_eq!(
        Some(listed.head.clone()),
        item.subject.head,
        "the item's HEAD"
    );
    assert_eq!(listed.facts.dirty, Tri::Yes);
    assert_eq!(listed.facts.closed_pr.as_ref().map(|pr| pr.number), Some(7));
}
