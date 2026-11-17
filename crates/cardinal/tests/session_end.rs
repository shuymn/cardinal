//! `cardinal hook claude subagent-stop` and `session-end` (`docs/architecture.md` section
//! 6.6): both record the end and never stop processes (C9); `session-end` snapshots the agent
//! process and its descendants.
//!
//! The test binary re-runs itself as a stand-in agent (`support::helper::Mode::Agent`): it
//! starts a background process (the "dev server" an agent leaves running), runs the hook as
//! its child, directly or through `sh -c` as Claude Code does, and then stays alive until the
//! test kills it.

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
    let work = dirs.scope().path("agent");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("input.json"), input.to_string()).unwrap();
    let mode = Mode::Agent {
        dir: work.clone(),
        event: event.to_owned(),
        state: dirs.state().to_path_buf(),
        config: dirs.config().to_path_buf(),
        via_shell,
    };
    let agent = support::helper::spawn(dirs.scope(), &mode, dirs.scope().root());
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
