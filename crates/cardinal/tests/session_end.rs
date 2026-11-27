//! `cardinal hook claude subagent-stop` and `session-end` (`docs/architecture.md` section
//! 6.6): both record the end and never stop processes (C9); `session-end` snapshots the agent
//! process and its descendants.
//!
//! The test binary re-runs itself as a stand-in agent (`support::helper::Mode::Agent`): it
//! starts a background process (the "dev server" an agent leaves running), runs the hook as
//! its child, directly or through `sh -c` as Claude Code does, and then stays alive until the
//! test kills it. On Unix one case puts the session's processes in groups shared with
//! processes outside the session. Its lease write comes first (`docs/architecture.md` section
//! 9.8); `tests/session_lease.rs` holds the lease tests.

mod hook_support;
mod support;

use std::path::Path;
use std::time::Duration;

use cardinal::contract::{Record, RecordKind, SessionSnapshot};
use cardinal::state::StateDir;
use cardinal_test_support::Child;
use cardinal_test_support::process::{PidGuard, wait_for, wait_for_file};
use hook_support::output_with_stdin;
use serde_json::{Value, json};
use support::cardinal::Dirs;
use support::helper::Mode;

#[test]
fn helper() {
    support::helper::run_if_requested();
}

/// An agent run: the agent process and its background process.
struct Run {
    agent: Child,
    _background: PidGuard,
    background: u32,
    status: String,
}

/// Starts the stand-in agent in `dirs`' scope, with `dirs`' state and config dirs for the
/// hook it runs.
fn run_agent(dirs: &Dirs, event: &str, input: &Value, via_shell: bool) -> Run {
    let work = agent_work(dirs, input);
    let mode = agent_mode(dirs, &work, event, via_shell, false);
    let agent = support::helper::spawn(dirs.scope(), &mode, dirs.scope().root());
    finish_start(dirs, &work, agent)
}

/// The agent's work dir in `dirs`' scope, with `input` as `input.json`.
fn agent_work(dirs: &Dirs, input: &Value) -> std::path::PathBuf {
    let work = dirs.scope().path("agent");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("input.json"), input.to_string()).unwrap();
    work
}

fn agent_mode(dirs: &Dirs, work: &Path, event: &str, via_shell: bool, stray: bool) -> Mode {
    Mode::Agent {
        dir: work.to_path_buf(),
        event: event.to_owned(),
        state: dirs.state().to_path_buf(),
        config: dirs.config().to_path_buf(),
        via_shell,
        stray,
    }
}

/// Waits until the started `agent` ran its hook.
fn finish_start(dirs: &Dirs, work: &Path, agent: Child) -> Run {
    wait_for_file(&work.join("hook.done"), Duration::from_secs(30));
    let background: u32 = std::fs::read_to_string(work.join("background.pid"))
        .unwrap()
        .parse()
        .unwrap();
    let status = std::fs::read_to_string(work.join("hook.done")).unwrap();
    Run {
        agent,
        _background: PidGuard::new(dirs.scope(), background),
        background,
        status,
    }
}

fn records(dirs: &Dirs) -> Vec<Record> {
    cardinal::record::read_all(&StateDir::open(dirs.state()).unwrap()).unwrap()
}

fn alive(pid: u32) -> bool {
    cardinal::os::start_time_s(pid).is_some()
}

fn session_input(cwd: &Path) -> Value {
    json!({
        "session_id": "sess-1",
        "transcript_path": "/tmp/t.jsonl",
        "cwd": cwd,
        "permission_mode": "default",
        "hook_event_name": "SessionEnd",
        "reason": "prompt_input_exit"
    })
}

fn snapshots_and_records(via_shell: bool) {
    let dirs = Dirs::new();
    let run = run_agent(
        &dirs,
        "session-end",
        &session_input(Path::new("/w")),
        via_shell,
    );
    assert_eq!(run.status, "0");
    assert!(alive(run.background), "the background process was stopped");
    assert!(alive(run.agent.id()), "the agent was stopped");

    let state = StateDir::open(dirs.state()).unwrap();
    let snapshot: SessionSnapshot =
        serde_json::from_slice(&std::fs::read(state.session("sess-1")).unwrap()).unwrap();
    assert_eq!(snapshot.session_id, "sess-1");
    assert_eq!(snapshot.reason, "prompt_input_exit");
    assert_eq!(snapshot.agent.pid, run.agent.id());
    assert_eq!(
        Some(snapshot.agent.start_s),
        cardinal::os::start_time_s(run.agent.id())
    );
    let listed: Vec<(u32, u64)> = snapshot
        .processes
        .iter()
        .map(|p| (p.pid, p.start_s))
        .collect();
    assert_eq!(
        listed,
        [(
            run.background,
            cardinal::os::start_time_s(run.background).unwrap()
        )],
        "only the background process; the hook and its shell are left out"
    );
    assert!(snapshot.processes[0].command.len() <= 256);

    let found = records(&dirs);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].kind, RecordKind::SessionEnded);
    assert_eq!(found[0].session_id.as_deref(), Some("sess-1"));
    assert_eq!(
        Value::Object(found[0].data.clone()),
        json!({"agent_id": null, "agent_type": null, "reason": "prompt_input_exit", "cwd": "/w"})
    );
}

/// C9: session end records, snapshots the agent's processes, and stops none of them.
#[test]
fn session_end_records_and_snapshots_without_stopping_processes() {
    snapshots_and_records(false);
}

/// C9: the shell Claude Code runs the hook command in is not taken for the agent.
#[cfg(unix)]
#[test]
fn session_end_skips_the_shell_between_agent_and_hook() {
    snapshots_and_records(true);
}

/// C9 with groups shared beyond the session: the agent runs in a group that a process outside
/// the session leads, as under a wrapper without job control, and its background process
/// shares its group with a process that is not the agent's descendant. The snapshot lists the
/// background process alone, and every member of both groups keeps running.
#[cfg(unix)]
#[test]
fn session_end_leaves_shared_groups_alone() {
    use cardinal::os::unix::pgid_of;
    use std::process::Stdio;

    let dirs = Dirs::new();
    let scope = dirs.scope();
    let leader = scope
        .command("cat", scope.root())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let work = agent_work(&dirs, &session_input(Path::new("/w")));
    let agent = support::helper::command(
        scope,
        &agent_mode(&dirs, &work, "session-end", false, true),
        scope.root(),
    )
    .join_group(&leader)
    .spawn()
    .unwrap();
    let run = finish_start(&dirs, &work, agent);
    let stray: u32 = std::fs::read_to_string(work.join("stray.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let _stray = PidGuard::new(scope, stray);
    assert_eq!(run.status, "0");
    assert_eq!(
        pgid_of(run.agent.id()),
        Some(leader.id()),
        "the agent's group"
    );
    assert_eq!(
        pgid_of(stray),
        Some(run.background),
        "the stray process's group"
    );
    assert_ne!(
        cardinal::os::parent_pid(stray),
        Some(run.agent.id()),
        "the stray process is not the agent's child"
    );

    let state = StateDir::open(dirs.state()).unwrap();
    let snapshot: SessionSnapshot =
        serde_json::from_slice(&std::fs::read(state.session("sess-1")).unwrap()).unwrap();
    assert_eq!(snapshot.agent.pid, run.agent.id());
    let listed: Vec<u32> = snapshot.processes.iter().map(|p| p.pid).collect();
    assert_eq!(listed, [run.background], "{snapshot:#?}");
    for (pid, what) in [
        (leader.id(), "the group leader outside the session"),
        (run.agent.id(), "the agent"),
        (run.background, "the background process"),
        (stray, "the stray process"),
    ] {
        assert!(alive(pid), "{what} was stopped");
    }
}

/// C9: subagent stop records and stops nothing; it takes no snapshot.
#[test]
fn subagent_stop_records_without_stopping_processes() {
    let dirs = Dirs::new();
    let input = json!({
        "session_id": "sess-2",
        "transcript_path": "/tmp/t.jsonl",
        "cwd": "/w",
        "permission_mode": "default",
        "hook_event_name": "SubagentStop",
        "agent_id": "agent-3",
        "agent_type": "Explore",
        "agent_transcript_path": "/tmp/a.jsonl",
        "stop_hook_active": false,
        "last_assistant_message": "done"
    });
    let run = run_agent(&dirs, "subagent-stop", &input, false);
    assert_eq!(run.status, "0");
    assert!(alive(run.background), "the background process was stopped");
    let found = records(&dirs);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].kind, RecordKind::SubagentStopped);
    assert_eq!(found[0].session_id.as_deref(), Some("sess-2"));
    assert_eq!(
        Value::Object(found[0].data.clone()),
        json!({"agent_id": "agent-3", "agent_type": "Explore", "reason": null, "cwd": "/w"})
    );
    let sessions = StateDir::open(dirs.state()).unwrap().sessions_dir();
    assert_eq!(std::fs::read_dir(sessions).unwrap().count(), 0);
}

/// Malformed input or an unusable state dir: exit 0, no output, and the process tree is
/// left alone.
#[test]
fn failures_exit_0() {
    let dirs = Dirs::new();
    for event in ["session-end", "subagent-stop"] {
        for stdin in [&b""[..], b"not json", b"[]"] {
            let output = output_with_stdin(dirs.cmd().args(["hook", "claude", event]), stdin);
            assert_eq!(output.status.code(), Some(0), "{event}");
            assert!(
                output.stdout.is_empty() && output.stderr.is_empty(),
                "{event}"
            );
        }
        let file = dirs.config().join("not-a-dir");
        std::fs::write(&file, b"").unwrap();
        let output = output_with_stdin(
            dirs.cmd()
                .env("CARDINAL_STATE_DIR", &file)
                .args(["hook", "claude", event]),
            session_input(Path::new("/w")).to_string().as_bytes(),
        );
        assert_eq!(output.status.code(), Some(0), "{event}");
        assert!(
            output.stdout.is_empty() && output.stderr.is_empty(),
            "{event}"
        );
    }
    assert!(!dirs.state().exists());
}

/// Without a session ID there is nothing to name a snapshot after; the end is still
/// recorded, also with an invalid configuration.
#[test]
fn session_end_without_session_id_or_config_still_records() {
    let dirs = Dirs::new();
    std::fs::write(dirs.config().join("host.toml"), "typo = 1\n").unwrap();
    let mut input = session_input(Path::new("/w"));
    input.as_object_mut().unwrap().remove("session_id");
    let output = output_with_stdin(
        dirs.cmd().args(["hook", "claude", "session-end"]),
        input.to_string().as_bytes(),
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    let found = wait_for("the record", Duration::from_secs(5), || {
        let found = records(&dirs);
        (!found.is_empty()).then_some(found)
    });
    assert_eq!(found[0].kind, RecordKind::SessionEnded);
    assert_eq!(found[0].session_id, None);
    let sessions = StateDir::open(dirs.state()).unwrap().sessions_dir();
    assert_eq!(std::fs::read_dir(sessions).unwrap().count(), 0);
}

/// Section 9.8: `session-end` writes its lease before its snapshot and before it takes
/// `records/.lock`. Paused before its lease, the hook has written neither. Paused at the
/// records lock in a second run, its lease and snapshot are written: that pause comes after
/// both, so it uses none of the budget they are written within.
#[cfg(unix)]
#[test]
fn session_end_writes_its_lease_first() {
    use cardinal::testpoint::{HOOK_LEASE_AGENT, RECORD_OPENED};
    use cardinal_test_support::fixture::Fixture;
    use std::io::Write as _;
    use std::process::Stdio;
    use support::testpoints::TestPoints;

    let dirs = Dirs::new();
    let fixture = Fixture::new();
    let repo = fixture.init("repo");
    let lease = StateDir::open(dirs.state())
        .unwrap()
        .lease(&cardinal::state::worktree_key(
            &dunce::canonicalize(&repo).unwrap(),
        ));
    let snapshot = StateDir::open(dirs.state()).unwrap().session("sess-1");
    let points = TestPoints::new(dirs.scope());
    let spawn = |point: &str| {
        let mut command = dirs.cmd();
        points.apply(&mut command, &[point], None);
        let mut hook = command
            .args(["hook", "claude", "session-end"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = hook.stdin.take().unwrap();
        stdin
            .write_all(session_input(&repo).to_string().as_bytes())
            .unwrap();
        drop(stdin);
        points.wait_reached(point, hook.id(), Duration::from_secs(30));
        hook
    };

    let mut hook = spawn(HOOK_LEASE_AGENT);
    assert!(!snapshot.exists(), "no snapshot before the lease");
    assert!(!lease.exists());
    assert!(records(&dirs).is_empty(), "no record before the lease");
    points.release(HOOK_LEASE_AGENT);
    assert_eq!(hook.wait().unwrap().code(), Some(0));
    for written in [&lease, &snapshot] {
        if written.exists() {
            std::fs::remove_file(written).unwrap();
        }
    }

    let mut hook = spawn(RECORD_OPENED);
    assert!(
        lease.exists(),
        "the lease is written before the records lock is taken"
    );
    assert!(snapshot.exists(), "and the snapshot");
    points.release(RECORD_OPENED);
    assert_eq!(hook.wait().unwrap().code(), Some(0));
    assert_eq!(
        records(&dirs).last().unwrap().kind,
        RecordKind::SessionEnded
    );
}

/// Section 9.8: `session-end` starts no snapshot without the least budget it needs. Paused
/// before the decision until less than that is left, the hook writes no snapshot, records its
/// end, and exits 0 within `SessionEnd`'s 1.5 s; released at once, it writes the snapshot
/// (control, run first so the binary's first start is not timed).
#[cfg(unix)]
#[test]
fn session_end_skips_a_snapshot_that_does_not_fit() {
    use cardinal::testpoint::HOOK_SNAPSHOT_ADMISSION;
    use std::io::Write as _;
    use std::process::Stdio;
    use std::time::Instant;
    use support::testpoints::TestPoints;

    /// `SessionEnd`'s budget from the hook's start (1.4 s) less half the least a snapshot
    /// starts with (100 ms).
    const HOLD: Duration = Duration::from_millis(1350);

    let dirs = Dirs::new();
    let snapshot = StateDir::open(dirs.state()).unwrap().session("sess-1");
    let points = TestPoints::new(dirs.scope());
    let run = |hold: Duration| {
        let mut command = dirs.cmd();
        points.apply(&mut command, &[HOOK_SNAPSHOT_ADMISSION], None);
        let mut hook = command
            .args(["hook", "claude", "session-end"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = hook.stdin.take().unwrap();
        stdin
            .write_all(session_input(Path::new("/w")).to_string().as_bytes())
            .unwrap();
        drop(stdin);
        points.wait_reached(HOOK_SNAPSHOT_ADMISSION, hook.id(), Duration::from_secs(30));
        // The hook started before it reached the point: its deadline is at most this far off.
        let reached = Instant::now();
        std::thread::sleep((reached + hold).saturating_duration_since(Instant::now()));
        points.release(HOOK_SNAPSHOT_ADMISSION);
        assert_eq!(hook.wait().unwrap().code(), Some(0));
        // Timed from the point, not from the spawn: the hook's own clock starts after the
        // process does, so a slow start counts against the test, not against the budget.
        let took = reached.elapsed();
        std::fs::remove_file(
            points
                .dir()
                .join(format!("{HOOK_SNAPSHOT_ADMISSION}.release")),
        )
        .unwrap();
        took
    };

    run(Duration::ZERO);
    assert!(snapshot.exists(), "a snapshot with the budget left");
    std::fs::remove_file(&snapshot).unwrap();

    let took = run(HOLD);
    assert!(!snapshot.exists(), "no snapshot started");
    // Released at `HOLD`; with no snapshot the hook exits right after.
    assert!(took < HOLD + Duration::from_millis(500), "took {took:?}");
    let ended: Vec<_> = records(&dirs)
        .into_iter()
        .filter(|r| r.kind == RecordKind::SessionEnded)
        .collect();
    assert_eq!(ended.len(), 2, "{ended:#?}");
}

/// Section 9.8: `session-end` keeps one budget from its start. With the lease lock and
/// `records/.lock` both held it exits 0 within `SessionEnd`'s 1.5 s, its records dropped;
/// with both free it records its end (control, run first so the binary's first start is not
/// timed).
#[cfg(unix)]
#[test]
fn session_end_keeps_its_budget() {
    use cardinal::state::{self, LockMode};
    use cardinal_test_support::fixture::Fixture;
    use std::time::Instant;

    let dirs = Dirs::new();
    let fixture = Fixture::new();
    let repo = fixture.init("repo");
    let run = || {
        let started = Instant::now();
        let output = output_with_stdin(
            dirs.cmd().args(["hook", "claude", "session-end"]),
            session_input(&repo).to_string().as_bytes(),
        );
        assert_eq!(output.status.code(), Some(0));
        started.elapsed()
    };
    // The control: the process's own start, work, and exit with nothing held. A loaded host
    // starts processes slower than the 100 ms section 9.8 sets aside for that, so the bound
    // below is the hook's own deadline on top of what this host takes, not a wall-clock 1.5 s.
    let control = run();
    assert_eq!(records(&dirs).len(), 1);

    let state = StateDir::open(dirs.state()).unwrap();
    let key = state::worktree_key(&dunce::canonicalize(&repo).unwrap());
    let held = Instant::now() + Duration::from_secs(5);
    let lease_lock = state::lock(&state.lease_lock(&key), LockMode::Exclusive, held).unwrap();
    let records_lock = state::lock(&state.records_lock(), LockMode::Exclusive, held).unwrap();
    let took = run();
    drop((lease_lock, records_lock));
    // 1.4 s: `session-end`'s deadline from its start (section 9.8), plus the control and a
    // margin for one slow process start on a loaded CI runner (the held run took 147 ms more
    // than the control there). Waits that stack, 250 ms for the lease and 1 s for each record,
    // took 2.27 s, still well past this bound.
    let bound = Duration::from_millis(1400) + control + Duration::from_millis(500);
    assert!(took < bound, "took {took:?}, bound {bound:?}");
    assert_eq!(records(&dirs).len(), 1, "dropped, never waited for");
}
