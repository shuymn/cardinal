//! The inbox (`docs/architecture.md` sections 4.8 and 10.1): no duplicate keys, resolved and
//! dismissed items never reopened (C19), outlived processes raised from session snapshots
//! (C9), and `recovery_needed` items raised and cleared by the refresh.
//!
//! The recovery pass is a fake until the integration gate runs these against the real
//! `admission` module; sessions and processes are real.

mod fakes;
mod support;

use std::time::Duration;

use cardinal::admission::RecoveryReport;
use cardinal::config::Config;
use cardinal::contract::{
    AdmissionResult, DerivedState, Error, ErrorCode, GroupIdentity, GroupMember, InboxKind,
    InboxStatus, InboxSubject, Priority, ProcessIdentity, RecordKind, Reservation,
    ReservationSchema, ReservationState, ReservationView, ResolutionOutcome, SessionProcess,
    SessionSnapshot, SessionSnapshotSchema, SnapshotAgent,
};
use cardinal::inbox::{self, NewItem, StatusFilter};
use cardinal::state::StateDir;
use support::cardinal::Dirs;
use support::helper::{self, Mode};

#[test]
fn helper() {
    helper::run_if_requested();
}

struct Env {
    fx: support::fixture::Fixture,
    dirs: Dirs,
    config: Config,
    state: StateDir,
}

fn env() -> Env {
    let dirs = Dirs::new();
    let config = Config::load(dirs.config()).unwrap();
    let state = StateDir::open(dirs.state()).unwrap();
    Env {
        fx: support::fixture::Fixture::new(),
        dirs,
        config,
        state,
    }
}

impl Env {
    /// The `cardinal` binary on this state and config, confined to the fixture.
    fn cardinal(&self) -> assert_cmd::Command {
        fakes::cardinal(&self.fx, &self.dirs)
    }

    /// A sleeping helper process, confined to the fixture.
    fn sleeper(&self) -> helper::ChildGuard {
        fakes::spawn_helper(&self.fx, &Mode::Sleep, self.fx.root())
    }
}

fn item(key: &str, evidence: &serde_json::Value) -> NewItem {
    NewItem {
        key: key.into(),
        kind: InboxKind::RecoveryNeeded,
        subject: InboxSubject {
            job_id: Some("j".into()),
            ..InboxSubject::default()
        },
        evidence: evidence.as_object().unwrap().clone(),
    }
}

fn item_files(state: &StateDir) -> usize {
    std::fs::read_dir(state.inbox_dir())
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "json")
        })
        .count()
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "it has the recovery pass signature"
)]
fn no_recovery(_: &StateDir, _: &cardinal::config::HostConfig) -> Result<RecoveryReport, Error> {
    Ok(RecoveryReport::default())
}

fn list(e: &Env, status: Option<StatusFilter>) -> Vec<cardinal::contract::InboxItem> {
    inbox::list_with(&e.config, &e.state, status, false, &no_recovery)
        .unwrap()
        .items
}

fn refresh(e: &Env, recovery: &inbox::RecoveryPass<'_>) -> Vec<cardinal::contract::InboxItem> {
    inbox::list_with(&e.config, &e.state, Some(StatusFilter::All), true, recovery)
        .unwrap()
        .items
}

/// C19: raising a key twice keeps one item, counts it, and replaces its evidence.
#[test]
fn raising_twice_updates_one_item() {
    let e = env();
    let first = inbox::raise(&e.state, &item("k", &serde_json::json!({"n": 1}))).unwrap();
    assert_eq!(first.status, InboxStatus::Open);
    assert_eq!(first.seen, 1);
    let second = inbox::raise(&e.state, &item("k", &serde_json::json!({"n": 2}))).unwrap();
    assert_eq!(second.seen, 2);
    assert_eq!(second.evidence["n"], 2);
    assert_eq!(second.created_ms, first.created_ms);
    assert_eq!(item_files(&e.state), 1);
    assert_eq!(list(&e, None).len(), 1);
}

/// C19: resolved and dismissed items stay closed when their key is raised again.
#[test]
fn closed_items_are_not_reopened() {
    let e = env();
    for (key, outcome, status) in [
        ("a", ResolutionOutcome::Resolved, InboxStatus::Resolved),
        ("b", ResolutionOutcome::Dismissed, InboxStatus::Dismissed),
    ] {
        inbox::raise(&e.state, &item(key, &serde_json::json!({"n": 1}))).unwrap();
        inbox::resolve(&e.config, &e.state, key, outcome, Some("done")).unwrap();
        let again = inbox::raise(&e.state, &item(key, &serde_json::json!({"n": 2}))).unwrap();
        assert_eq!(again.status, status, "{key}");
        assert_eq!(again.seen, 1, "{key}");
        assert_eq!(again.evidence["n"], 1, "{key}: evidence unchanged");
    }
    assert!(list(&e, None).is_empty(), "nothing pending");
    assert_eq!(list(&e, Some(StatusFilter::All)).len(), 2);
    assert_eq!(item_files(&e.state), 2);
}

/// An escalated item is still pending: raising it updates it and keeps it escalated.
#[test]
fn escalated_item_is_updated() {
    let e = env();
    inbox::raise(&e.state, &item("k", &serde_json::json!({"n": 1}))).unwrap();
    inbox::resolve(
        &e.config,
        &e.state,
        "k",
        ResolutionOutcome::Escalated,
        Some("owner decides"),
    )
    .unwrap();
    let again = inbox::raise(&e.state, &item("k", &serde_json::json!({"n": 2}))).unwrap();
    assert_eq!(again.status, InboxStatus::Escalated);
    assert_eq!(again.seen, 2);
    assert_eq!(
        list(&e, Some(StatusFilter::Only(InboxStatus::Escalated))).len(),
        1
    );
    let resolved =
        inbox::resolve(&e.config, &e.state, "k", ResolutionOutcome::Resolved, None).unwrap();
    assert_eq!(resolved.status, InboxStatus::Resolved);
    assert_eq!(resolved.resolution.unwrap().by, "cli");
}

/// Another key whose hash takes the slot is never overwritten; the key probes on.
#[test]
fn colliding_hash_probes_the_next_slot() {
    let e = env();
    let slot = e.state.inbox_item("k", 0);
    let mut other = inbox::raise(&e.state, &item("other", &serde_json::json!({}))).unwrap();
    other.key = "not-k".into();
    std::fs::write(&slot, serde_json::to_vec(&other).unwrap()).unwrap();
    inbox::raise(&e.state, &item("k", &serde_json::json!({}))).unwrap();
    let raised = inbox::raise(&e.state, &item("k", &serde_json::json!({}))).unwrap();
    assert_eq!(raised.seen, 2);
    let at_slot: cardinal::contract::InboxItem =
        serde_json::from_slice(&std::fs::read(&slot).unwrap()).unwrap();
    assert_eq!(at_slot.key, "not-k", "the colliding item is untouched");
    assert!(e.state.inbox_item("k", 1).exists());
}

#[test]
fn resolving_needs_a_pending_item() {
    let e = env();
    let missing = inbox::resolve(
        &e.config,
        &e.state,
        "nope",
        ResolutionOutcome::Resolved,
        None,
    );
    assert_eq!(missing.unwrap_err().code, ErrorCode::NotFound);
    inbox::raise(&e.state, &item("k", &serde_json::json!({}))).unwrap();
    inbox::resolve(&e.config, &e.state, "k", ResolutionOutcome::Dismissed, None).unwrap();
    let closed = inbox::resolve(&e.config, &e.state, "k", ResolutionOutcome::Resolved, None);
    assert_eq!(closed.unwrap_err().code, ErrorCode::InvalidArgument);
}

fn alive_start(pid: u32) -> u64 {
    helper::wait_for("the process start time", Duration::from_secs(10), || {
        cardinal::os::start_time_s(pid)
    })
}

fn session_snapshot(state: &StateDir, id: &str, agent: (u32, u64), processes: &[(u32, u64)]) {
    let snapshot = SessionSnapshot {
        schema: SessionSnapshotSchema,
        session_id: id.into(),
        reason: "prompt_input_exit".into(),
        ended_ms: 1,
        agent: SnapshotAgent {
            pid: agent.0,
            start_s: agent.1,
            name: "claude".into(),
        },
        processes: processes
            .iter()
            .map(|&(pid, start_s)| SessionProcess {
                pid,
                start_s,
                name: "dev-server".into(),
                command: "dev-server --port 3000".into(),
            })
            .collect(),
    };
    std::fs::write(state.session(id), serde_json::to_vec(&snapshot).unwrap()).unwrap();
}

/// C9: a process listed in a session snapshot that is still alive after the session's agent
/// process ended is raised; once none is alive, the item is resolved by cardinal and the
/// snapshot deleted.
#[test]
fn outlived_processes_raised() {
    let e = env();
    let mut agent = e.sleeper();
    let agent_id = (agent.id(), alive_start(agent.id()));
    let mut server = e.sleeper();
    let server_id = (server.id(), alive_start(server.id()));
    let mut gone = e.sleeper();
    let gone_id = (gone.id(), alive_start(gone.id()));
    gone.kill();
    session_snapshot(&e.state, "s1", agent_id, &[server_id, gone_id]);

    // The agent still runs (`clear`, `resume`): nothing has outlived it.
    assert_eq!(refresh(&e, &no_recovery).len(), 0);

    agent.kill();
    let items = refresh(&e, &no_recovery);
    assert_eq!(items.len(), 1, "{items:#?}");
    let raised = &items[0];
    assert_eq!(raised.key, "outlived:s1");
    assert_eq!(raised.kind, InboxKind::OutlivedProcesses);
    assert_eq!(raised.status, InboxStatus::Open);
    assert_eq!(raised.subject.session_id.as_deref(), Some("s1"));
    let pids: Vec<u64> = raised.evidence["processes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["pid"].as_u64().unwrap())
        .collect();
    assert_eq!(
        pids,
        [u64::from(server_id.0)],
        "only live processes are evidence"
    );
    assert_eq!(refresh(&e, &no_recovery)[0].seen, 2, "no duplicate");

    server.kill();
    let items = refresh(&e, &no_recovery);
    assert_eq!(items[0].status, InboxStatus::Resolved);
    assert_eq!(items[0].resolution.as_ref().unwrap().by, "cardinal");
    assert!(!e.state.session("s1").exists(), "the snapshot is deleted");
    let records = cardinal::record::read_all(&e.state).unwrap();
    assert!(records.iter().any(
        |r| r.kind == RecordKind::InboxRaised && r.inbox_key.as_deref() == Some("outlived:s1")
    ));
}

fn reservation_view(job: &str, derived: DerivedState) -> ReservationView {
    ReservationView {
        reservation: Reservation {
            schema: ReservationSchema,
            job_id: job.into(),
            state: ReservationState::Running,
            boot_s: 1,
            created_ms: 1,
            updated_ms: 1,
            runner: ProcessIdentity {
                pid: 10,
                start_s: 1,
            },
            group: GroupIdentity::Pgid {
                pgid: 10,
                leader_start_s: 1,
            },
            child: Some(ProcessIdentity {
                pid: 11,
                start_s: 1,
            }),
            stop_signal_ms: None,
            priority: Priority::Normal,
            resources: std::collections::BTreeMap::new(),
            admission: AdmissionResult::WithinBudget,
            class: None,
            rule: None,
            worktree: Some(cardinal::contract::ReservationWorktree {
                key: "k".into(),
                path: "/w".into(),
                protected: true,
            }),
            session: None,
            cwd: "/w".into(),
            argv: vec!["cargo".into(), "test".into()],
        },
        derived,
        members: vec![GroupMember {
            pid: 12,
            start_s: 1,
            name: "sleep".into(),
        }],
        verified: false,
    }
}

/// `recovery_needed` items are raised for recovery-needed reservations only, and resolved by
/// cardinal once the reservation is gone.
#[test]
fn recovery_needed_raised_and_cleared() {
    let e = env();
    let with_reservations = |_: &StateDir, _: &cardinal::config::HostConfig| {
        Ok(RecoveryReport {
            reservations: vec![
                reservation_view("aaaaaaaaaaaaaaaa", DerivedState::RecoveryNeeded),
                reservation_view("bbbbbbbbbbbbbbbb", DerivedState::Live),
            ],
            ..RecoveryReport::default()
        })
    };
    let items = refresh(&e, &with_reservations);
    assert_eq!(items.len(), 1, "{items:#?}");
    assert_eq!(items[0].key, "recovery_needed:aaaaaaaaaaaaaaaa");
    assert_eq!(items[0].subject.worktree.as_deref(), Some("/w"));
    assert_eq!(items[0].evidence["verified"], false);
    assert_eq!(items[0].evidence["members"][0]["pid"], 12);

    let items = refresh(&e, &no_recovery);
    assert_eq!(items[0].status, InboxStatus::Resolved);
    assert_eq!(
        items[0].resolution.as_ref().unwrap().note,
        "the reservation is gone"
    );
}

#[test]
fn refresh_failure_is_reported() {
    let e = env();
    let failing =
        |_: &StateDir, _: &cardinal::config::HostConfig| -> Result<RecoveryReport, Error> {
            Err(Error::new(
                ErrorCode::StateUnavailable,
                "admission lock timed out",
            ))
        };
    let err = inbox::list_with(&e.config, &e.state, None, true, &failing).unwrap_err();
    assert_eq!(err.code, ErrorCode::StateUnavailable);
    assert!(inbox::list_with(&e.config, &e.state, None, false, &failing).is_ok());
}

fn json(stdout: &[u8]) -> serde_json::Value {
    serde_json::from_slice(stdout).unwrap()
}

/// Through the binary: listing without refresh and resolving.
#[test]
fn cli_lists_and_resolves() {
    let e = env();
    let assert = e
        .cardinal()
        .args(["inbox", "--no-refresh"])
        .assert()
        .success();
    let doc = json(&assert.get_output().stdout);
    assert_eq!(doc["schema"], "cardinal.inbox.v1");
    assert_eq!(doc["items"], serde_json::json!([]));

    let assert = e
        .cardinal()
        .args(["inbox", "resolve", "k", "--outcome", "resolved"])
        .assert()
        .code(1);
    assert_eq!(
        json(&assert.get_output().stdout)["error"]["code"],
        "not_found"
    );

    inbox::raise(&e.state, &item("k", &serde_json::json!({}))).unwrap();
    let assert = e
        .cardinal()
        .args([
            "inbox",
            "resolve",
            "k",
            "--outcome",
            "dismissed",
            "--note",
            "known",
        ])
        .assert()
        .success();
    let doc = json(&assert.get_output().stdout);
    assert_eq!(doc["schema"], "cardinal.inbox-item.v1");
    assert_eq!(doc["status"], "dismissed");
    assert_eq!(doc["resolution"]["note"], "known");
    let assert = e
        .cardinal()
        .args(["inbox", "--no-refresh", "--status", "all"])
        .assert()
        .success();
    assert_eq!(json(&assert.get_output().stdout)["items"][0]["key"], "k");
    let records = cardinal::record::read_all(&e.state).unwrap();
    assert!(records.iter().any(|r| r.kind == RecordKind::InboxResolved));
}
