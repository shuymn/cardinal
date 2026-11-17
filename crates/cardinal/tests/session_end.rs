//! `cardinal hook claude subagent-stop` and `session-end` (`docs/architecture.md` section
//! 6.6): both record the end and never stop processes (C9); `session-end` snapshots the agent
//! process and its descendants.
//!
//! The test binary re-runs itself as a stand-in agent: it starts a background process (the
//! "dev server" an agent leaves running), runs the hook as its child, directly or through
//! `sh -c` as Claude Code does, and then stays alive until the test kills it.

mod hook_support;
mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use cardinal::contract::{Record, RecordKind, SessionSnapshot};
use cardinal::state::StateDir;
use hook_support::Sandbox;
use serde_json::{Value, json};
use support::helper::{ChildGuard, Mode, PidGuard, wait_for, wait_for_file};

const AGENT: &str = "CARDINAL_TEST_AGENT";

#[test]
fn helper() {
    support::helper::run_if_requested();
}

/// The stand-in agent, active only when the test binary is started with `AGENT` set to a
/// work dir holding `event` and `input.json`.
#[test]
fn agent() {
    let Some(dir) = std::env::var_os(AGENT) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let background = support::helper::spawn(&Mode::Sleep, None);
    std::fs::write(dir.join("background.pid"), background.id().to_string()).unwrap();
    let event = std::fs::read_to_string(dir.join("event")).unwrap();
    let input = dir.join("input.json");
    let cardinal = env!("CARGO_BIN_EXE_cardinal");
    let status = if dir.join("via-shell").exists() {
        Command::new("sh")
            .arg("-c")
            // `; true` keeps the shell alive as the hook's parent.
            .arg(format!(
                "'{cardinal}' hook claude {event} < '{}'; true",
                input.display()
            ))
            .stdout(Stdio::null())
            .status()
    } else {
        Command::new(cardinal)
            .args(["hook", "claude", &event])
            .stdin(std::fs::File::open(&input).unwrap())
            .stdout(Stdio::null())
            .status()
    }
    .unwrap();
    std::fs::write(
        dir.join("hook.done"),
        status.code().unwrap_or(-1).to_string(),
    )
    .unwrap();
    std::thread::sleep(Duration::from_secs(60));
    drop(background);
    std::process::exit(0);
}

/// An agent run: the agent process and its background process.
struct Run {
    agent: ChildGuard,
    _background: PidGuard,
    background: u32,
    status: String,
}

/// Starts the stand-in agent in `dirs`' work dir with its constructed environment, which the
/// agent's background process, shell, and hook inherit.
fn run_agent(dirs: &Sandbox, event: &str, input: &Value, via_shell: bool) -> Run {
    let work = dirs.fixture().root().join("agent");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("event"), event).unwrap();
    std::fs::write(work.join("input.json"), input.to_string()).unwrap();
    if via_shell {
        std::fs::write(work.join("via-shell"), b"").unwrap();
    }
    let agent = dirs
        .command(std::env::current_exe().unwrap())
        .args(["agent", "--exact", "--nocapture", "--test-threads=1"])
        .env(AGENT, &work)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let agent = ChildGuard::new(agent);
    wait_for_file(&work.join("hook.done"), Duration::from_secs(30));
    let background: u32 = std::fs::read_to_string(work.join("background.pid"))
        .unwrap()
        .parse()
        .unwrap();
    let status = std::fs::read_to_string(work.join("hook.done")).unwrap();
    Run {
        agent,
        _background: PidGuard(background),
        background,
        status,
    }
}

fn records(dirs: &Sandbox) -> Vec<Record> {
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
    let dirs = Sandbox::new();
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
    let dirs = Sandbox::new();
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
    let dirs = Sandbox::new();
    for event in ["session-end", "subagent-stop"] {
        for stdin in [&b""[..], b"not json", b"[]"] {
            let output = dirs
                .cmd()
                .args(["hook", "claude", event])
                .write_stdin(stdin)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(0), "{event}");
            assert!(
                output.stdout.is_empty() && output.stderr.is_empty(),
                "{event}"
            );
        }
        let file = dirs.config().join("not-a-dir");
        std::fs::write(&file, b"").unwrap();
        let output = dirs
            .cmd()
            .env("CARDINAL_STATE_DIR", &file)
            .args(["hook", "claude", event])
            .write_stdin(session_input(Path::new("/w")).to_string())
            .output()
            .unwrap();
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
    let dirs = Sandbox::new();
    std::fs::write(dirs.config().join("host.toml"), "typo = 1\n").unwrap();
    let mut input = session_input(Path::new("/w"));
    input.as_object_mut().unwrap().remove("session_id");
    dirs.cmd()
        .args(["hook", "claude", "session-end"])
        .write_stdin(input.to_string())
        .assert()
        .success()
        .stdout("");
    let found = wait_for("the record", Duration::from_secs(5), || {
        let found = records(&dirs);
        (!found.is_empty()).then_some(found)
    });
    assert_eq!(found[0].kind, RecordKind::SessionEnded);
    assert_eq!(found[0].session_id, None);
    let sessions = StateDir::open(dirs.state()).unwrap().sessions_dir();
    assert_eq!(std::fs::read_dir(sessions).unwrap().count(), 0);
}
