//! The `reconcile` pass (`docs/architecture.md` sections 10.3 and 13.6): one at a time
//! (A19), and the inbox refresh at step 3, which raises processes that outlived their session
//! and clears them once they are gone (C9). The readings fake keeps every volume above its
//! lines unless a test says otherwise.

mod disk_support;
mod observe_common;
mod plan_support;
mod support;

use std::time::Duration;

use cardinal::contract::{
    InboxStatus, SessionProcess, SessionSnapshot, SessionSnapshotSchema, Skipped, SnapshotAgent,
};
use cardinal::inbox::{self, StatusFilter};
use cardinal::testpoint::RECONCILE_OBSERVED;
use cardinal_test_support::process::wait_for;
use disk_support::{Disk, MIB, own_errors, pass};
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

    let busy = pass(&d.reconcile_command().output().unwrap());
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
