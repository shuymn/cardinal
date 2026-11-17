//! The inbox (`docs/architecture.md` sections 4.8 and 10.1): no duplicate keys, resolved and
//! dismissed items never reopened (C19), outlived processes raised from session snapshots
//! (C9), and `recovery_needed` items raised and cleared by the refresh.
//!
//! A refresh runs the recovery pass of `admission` over the reservations of the real runner
//! (`support::jobs`); sessions and processes are real. One test keeps a fake recovery pass:
//! [`concurrent_refresh_keeps_newer_items`] needs a reservation that a newer observation
//! reports while an older, paused refresh observed none, and a real reservation, once
//! released, never comes back. Its paused refresh runs in a helper process (this test binary
//! re-run in the custom helper mode `refresh`).

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
#[cfg(unix)]
use cardinal_test_support::fixture::Fixture;
use cardinal_test_support::process::wait_for;
use cardinal_test_support::{Child, Command};
use support::cardinal::Dirs;
use support::helper::{self, Mode};

#[test]
fn helper() {
    run_refresh_if_requested();
    helper::run_if_requested();
}

/// The custom helper mode that refreshes.
const REFRESH_MODE: &str = "refresh";

/// The config and state dirs a helper process refreshes.
#[derive(serde::Serialize, serde::Deserialize)]
struct RefreshRequest {
    config: std::path::PathBuf,
    state: std::path::PathBuf,
}

/// Runs the refresh a helper process was started for, then exits; returns at once otherwise.
fn run_refresh_if_requested() {
    let Some(request) = helper::custom_request(REFRESH_MODE) else {
        return;
    };
    let request: RefreshRequest = serde_json::from_str(&request).expect("a refresh request");
    let config = Config::load(&request.config).expect("load the config");
    let state = StateDir::open(&request.state).expect("open the state dir");
    let refreshed = inbox::list(&config, &state, None, true);
    std::process::exit(i32::from(refreshed.is_err()));
}

struct Env {
    dirs: Dirs,
    config: Config,
    state: StateDir,
}

fn env() -> Env {
    env_with("")
}

/// An environment whose `host.toml` is `host`.
fn env_with(host: &str) -> Env {
    let dirs = Dirs::new();
    std::fs::write(dirs.config().join("host.toml"), host).unwrap();
    let config = Config::load(dirs.config()).unwrap();
    let state = StateDir::open(dirs.state()).unwrap();
    Env {
        dirs,
        config,
        state,
    }
}

impl Env {
    /// The `cardinal` binary on this state and config.
    fn cardinal(&self) -> Command {
        self.dirs.cmd()
    }

    /// A helper process that refreshes this state.
    fn refresh_command(&self) -> Command {
        let request = RefreshRequest {
            config: self.dirs.config().to_path_buf(),
            state: self.state.root().to_path_buf(),
        };
        let mode = Mode::Custom {
            name: REFRESH_MODE,
            arg: serde_json::to_string(&request).unwrap(),
        };
        let scope = self.dirs.scope();
        helper::command(scope, &mode, scope.root())
    }

    /// A sleeping helper process.
    fn sleeper(&self) -> Child {
        let scope = self.dirs.scope();
        helper::spawn(scope, &Mode::Sleep, scope.root())
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

fn list(e: &Env, status: Option<StatusFilter>) -> Vec<cardinal::contract::InboxItem> {
    inbox::list(&e.config, &e.state, status, false)
        .unwrap()
        .items
}

/// Every item after a refresh that runs the real recovery pass.
fn refresh(e: &Env) -> Vec<cardinal::contract::InboxItem> {
    inbox::list(&e.config, &e.state, Some(StatusFilter::All), true)
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
    wait_for("the process start time", Duration::from_secs(10), || {
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
    assert_eq!(refresh(&e).len(), 0);

    agent.kill();
    let items = refresh(&e);
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
    assert_eq!(refresh(&e)[0].seen, 2, "no duplicate");

    server.kill();
    let items = refresh(&e);
    assert_eq!(items[0].status, InboxStatus::Resolved);
    assert_eq!(items[0].resolution.as_ref().unwrap().by, "cardinal");
    assert!(!e.state.session("s1").exists(), "the snapshot is deleted");
    let records = cardinal::record::read_all(&e.state).unwrap();
    assert!(records.iter().any(
        |r| r.kind == RecordKind::InboxRaised && r.inbox_key.as_deref() == Some("outlived:s1")
    ));
}

/// A refreshing `cardinal inbox` raises a `recovery_needed` item for the reservation of a
/// runner killed by `SIGKILL` while its group lives, naming the job's worktree and with the
/// group's members as evidence, and none for a live job. Once the stage's shell, the child
/// the reservation records, is gone, the group is no longer verified and the item's evidence
/// says so with the members left. Once the group is empty, the next refresh releases the
/// reservation and cardinal resolves the item. Both jobs run in a fixture repository, within
/// a budget that admits them at once.
#[cfg(unix)]
#[test]
fn recovery_needed_raised_and_cleared() {
    let e = env_with("[budget]\ncpu = 2\n");
    let fx = Fixture::new();
    let repo = fx.init("repo");
    fx.commit_file(&repo, "README.md", "hello\n", "initial");
    let jobs = support::jobs::Jobs::new(fx.scope(), e.dirs.state(), e.dirs.config());
    let live = jobs.running(&repo, 1);
    let mut crashed = jobs.crashed(&repo, 1);
    let inbox = || -> Vec<serde_json::Value> {
        let output = e
            .cardinal()
            .args(["inbox", "--status", "all"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        json(&output.stdout)["items"].as_array().unwrap().clone()
    };
    let member_pids = |item: &serde_json::Value| -> Vec<u32> {
        let mut pids: Vec<u32> = item["evidence"]["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| u32::try_from(m["pid"].as_u64().unwrap()).unwrap())
            .collect();
        pids.sort_unstable();
        pids
    };
    let [a, b] = crashed.stage.cats;
    let mut cats = vec![a, b];
    cats.sort_unstable();
    let mut stage = vec![crashed.stage.shell, a, b];
    stage.sort_unstable();

    let items = inbox();
    assert_eq!(items.len(), 1, "{items:#?}");
    let raised = &items[0];
    assert_eq!(raised["key"], format!("recovery_needed:{}", crashed.job));
    assert_eq!(raised["status"], "open");
    assert_eq!(raised["subject"]["job_id"], crashed.job.as_str());
    assert_eq!(raised["subject"]["worktree"], repo.to_str().unwrap());
    assert_eq!(raised["evidence"]["verified"], true, "{raised:#}");
    assert_eq!(
        member_pids(raised),
        stage,
        "the shell and both cats: {raised:#}"
    );
    assert_eq!(inbox()[0]["seen"], 2, "no duplicate");

    crashed.end_shell(fx.scope());
    let items = inbox();
    assert_eq!(items.len(), 1, "{items:#?}");
    let raised = &items[0];
    assert_eq!(raised["status"], "open");
    assert_eq!(raised["seen"], 3);
    assert_eq!(raised["evidence"]["verified"], false, "{raised:#}");
    assert_eq!(member_pids(raised), cats, "both cats: {raised:#}");

    crashed.end_group();
    let items = inbox();
    assert_eq!(items.len(), 1, "{items:#?}");
    assert_eq!(items[0]["status"], "resolved");
    assert_eq!(items[0]["resolution"]["by"], "cardinal");
    assert_eq!(items[0]["resolution"]["note"], "the reservation is gone");
    let reservations = cardinal::admission::list_reservations(&e.state).unwrap();
    let jobs: Vec<&str> = reservations
        .iter()
        .map(|v| v.reservation.job_id.as_str())
        .collect();
    assert_eq!(
        jobs,
        [live.job.as_str()],
        "only the live job's reservation remains"
    );
}

/// A refresh whose recovery pass cannot take the admission lock in `lock_timeout_ms` fails;
/// listing without a refresh does not take it.
#[test]
fn refresh_failure_is_reported() {
    let e = env_with("[admission]\nlock_timeout_ms = 200\n");
    let _held = cardinal::state::try_lock(
        &e.state.admission_lock(),
        cardinal::state::LockMode::Exclusive,
    )
    .unwrap()
    .expect("the admission lock is free");
    let err = inbox::list(&e.config, &e.state, None, true).unwrap_err();
    assert_eq!(err.code, ErrorCode::StateUnavailable, "{err:?}");
    assert!(inbox::list(&e.config, &e.state, None, false).is_ok());
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

/// A refresh clears only the items it saw before it observed, and only while they are
/// unchanged. Refresh A (the real recovery pass) observes no reservations and pauses; refresh
/// B, from a newer (faked) observation, updates the item A saw and raises a new one; A then
/// closes neither. The next refresh that observes no reservations clears both.
#[test]
fn concurrent_refresh_keeps_newer_items() {
    let e = env();
    let (old, new) = ("aaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbb");
    refresh_reporting(&e, &[old]);
    let points = support::testpoints::TestPoints::new(e.dirs.scope());
    let mut command = e.refresh_command();
    points.apply(&mut command, &[cardinal::testpoint::INBOX_OBSERVED], None);
    let mut first = command.spawn().unwrap();
    points.wait_reached(
        cardinal::testpoint::INBOX_OBSERVED,
        first.id(),
        Duration::from_secs(20),
    );
    refresh_reporting(&e, &[old, new]);
    points.release(cardinal::testpoint::INBOX_OBSERVED);
    assert!(first.wait().unwrap().success());

    let items = list(&e, Some(StatusFilter::All));
    let status = |job: &str| {
        let key = format!("recovery_needed:{job}");
        items.iter().find(|i| i.key == key).map(|i| i.status)
    };
    assert_eq!(status(old), Some(InboxStatus::Open), "{items:#?}");
    assert_eq!(status(new), Some(InboxStatus::Open), "{items:#?}");

    let items = refresh(&e);
    assert!(
        items.iter().all(|i| i.status == InboxStatus::Resolved),
        "{items:#?}"
    );
}

/// A fake recovery pass reporting `jobs` as `recovery_needed` reservations, for
/// [`concurrent_refresh_keeps_newer_items`] only (see the module documentation).
fn recovery_needed(
    jobs: &[&str],
) -> impl Fn(&StateDir, &cardinal::config::HostConfig) -> Result<RecoveryReport, Error> {
    let jobs: Vec<String> = jobs.iter().map(|&job| job.to_owned()).collect();
    move |_, _| {
        Ok(RecoveryReport {
            reservations: jobs
                .iter()
                .map(|job| reservation_view(job, DerivedState::RecoveryNeeded))
                .collect(),
            ..RecoveryReport::default()
        })
    }
}

/// A refresh with the fake recovery pass [`recovery_needed`] of `jobs`.
fn refresh_reporting(e: &Env, jobs: &[&str]) {
    inbox::list_with(&e.config, &e.state, None, true, &recovery_needed(jobs)).unwrap();
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
