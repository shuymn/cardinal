//! The inbox (`docs/architecture.md` sections 4.8 and 10.1): no duplicate keys, resolved and
//! dismissed items never reopened (C19), occurrences that end and recur as new items, outlived
//! processes raised from session snapshots (C9), and `recovery_needed` items raised and
//! cleared by the refresh.
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

/// The config and state dirs a helper process refreshes, and the jobs its fake recovery pass
/// reports, if it runs one ([`recovery_needed`]).
#[derive(serde::Serialize, serde::Deserialize)]
struct RefreshRequest {
    config: std::path::PathBuf,
    state: std::path::PathBuf,
    recovery_needed: Option<Vec<String>>,
}

/// Runs the refresh a helper process was started for, then exits; returns at once otherwise.
fn run_refresh_if_requested() {
    let Some(request) = helper::custom_request(REFRESH_MODE) else {
        return;
    };
    let request: RefreshRequest = serde_json::from_str(&request).expect("a refresh request");
    let config = Config::load(&request.config).expect("load the config");
    let state = StateDir::open(&request.state).expect("open the state dir");
    let refreshed = match &request.recovery_needed {
        Some(jobs) => {
            let jobs: Vec<&str> = jobs.iter().map(String::as_str).collect();
            inbox::list_with(&config, &state, None, true, &recovery_needed(&jobs))
        }
        None => inbox::list(&config, &state, None, true),
    };
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

    /// A helper process that refreshes this state with the real recovery pass.
    fn refresh_command(&self) -> Command {
        self.refresh_command_reporting(None)
    }

    /// A helper process that refreshes this state, with the fake recovery pass of `jobs` if
    /// given.
    fn refresh_command_reporting(&self, jobs: Option<&[&str]>) -> Command {
        let request = RefreshRequest {
            config: self.dirs.config().to_path_buf(),
            state: self.state.root().to_path_buf(),
            recovery_needed: jobs.map(|jobs| jobs.iter().map(|&j| j.to_owned()).collect()),
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
    assert_eq!(inbox::occurrence_of(&raised.key).0, "outlived:s1");
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
    assert!(
        records
            .iter()
            .any(|r| r.kind == RecordKind::InboxRaised
                && r.inbox_key.as_deref() == Some(&*raised.key))
    );
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
    assert_eq!(
        inbox::occurrence_of(raised["key"].as_str().unwrap()).0,
        format!("recovery_needed:{}", crashed.job)
    );
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
    let reservations =
        cardinal::admission::list_reservations(&e.state, &cardinal::config::HostConfig::defaults())
            .unwrap();
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

/// Section 10.1: refreshes are serialized from their observation to their last write.
/// Refresh A (the real recovery pass) observes no reservations and pauses; refresh B, from a
/// newer (faked) observation, waits for it. A ends the item it saw, and B, applied after it,
/// raises that condition again as a new occurrence and raises another item. The next refresh
/// that observes no reservations ends both.
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
    let mut second = contend(&points, e.refresh_command_reporting(Some(&[old, new])));
    points.release(cardinal::testpoint::INBOX_OBSERVED);
    assert!(first.wait().unwrap().success());
    assert!(second.wait().unwrap().success());

    let items = list(&e, Some(StatusFilter::All));
    let statuses = |job: &str| -> Vec<InboxStatus> {
        let key = format!("recovery_needed:{job}");
        items
            .iter()
            .filter(|i| inbox::occurrence_of(&i.key).0 == key)
            .map(|i| i.status)
            .collect()
    };
    assert_eq!(
        statuses(old),
        [InboxStatus::Resolved, InboxStatus::Open],
        "{items:#?}"
    );
    assert_eq!(statuses(new), [InboxStatus::Open], "{items:#?}");

    let items = refresh(&e);
    assert!(
        items.iter().all(|i| i.status == InboxStatus::Resolved),
        "{items:#?}"
    );
}

/// Starts the refresh `command` while another refresh holds `inbox/.refresh.lock`, and returns
/// once it has found the lock held, on its way to wait for it.
fn contend(points: &support::testpoints::TestPoints, mut command: Command) -> Child {
    points.apply(
        &mut command,
        &[cardinal::testpoint::INBOX_REFRESH_WAITING],
        None,
    );
    let contender = command.spawn().unwrap();
    points.wait_reached(
        cardinal::testpoint::INBOX_REFRESH_WAITING,
        contender.id(),
        Duration::from_secs(20),
    );
    points.release(cardinal::testpoint::INBOX_REFRESH_WAITING);
    contender
}

/// Section 10.1: an observation that a newer refresh superseded raises nothing. Refresh A
/// observes a session's process alive and pauses; the process ends; refresh B waits for A,
/// which updates the item, and then ends it. No second occurrence is raised.
#[test]
fn a_superseded_refresh_raises_no_occurrence() {
    let e = env();
    let mut agent = e.sleeper();
    let agent_id = (agent.id(), alive_start(agent.id()));
    agent.kill();
    let mut server = e.sleeper();
    let server_id = (server.id(), alive_start(server.id()));
    session_snapshot(&e.state, "s1", agent_id, &[server_id]);
    assert_eq!(refresh(&e).len(), 1);

    let points = support::testpoints::TestPoints::new(e.dirs.scope());
    let mut command = e.refresh_command();
    points.apply(&mut command, &[cardinal::testpoint::INBOX_OBSERVED], None);
    let mut first = command.spawn().unwrap();
    points.wait_reached(
        cardinal::testpoint::INBOX_OBSERVED,
        first.id(),
        Duration::from_secs(20),
    );
    server.kill();
    let mut second = contend(&points, e.refresh_command());
    points.release(cardinal::testpoint::INBOX_OBSERVED);
    assert!(first.wait().unwrap().success());
    assert!(second.wait().unwrap().success());

    let items = list(&e, Some(StatusFilter::All));
    assert_eq!(items.len(), 1, "one occurrence: {items:#?}");
    assert_eq!(items[0].seen, 2, "A's observation updated it: {items:#?}");
    assert_eq!(items[0].status, InboxStatus::Resolved);
    assert!(items[0].ended_ms.is_some());
}

/// A refresh ends `outlived_processes` only once the session snapshot is absent: a snapshot
/// file that does not parse, or (Unix) cannot be read, ends nothing. The control, the file
/// removed, ends it.
#[test]
fn an_unreadable_session_snapshot_ends_nothing() {
    let e = env();
    let mut agent = e.sleeper();
    let agent_id = (agent.id(), alive_start(agent.id()));
    agent.kill();
    let mut server = e.sleeper();
    let server_id = (server.id(), alive_start(server.id()));
    session_snapshot(&e.state, "s1", agent_id, &[server_id]);
    assert_eq!(refresh(&e).len(), 1);
    let pending = |items: &[cardinal::contract::InboxItem]| {
        assert_eq!(items.len(), 1, "{items:#?}");
        assert_eq!(
            (items[0].status, items[0].ended_ms),
            (InboxStatus::Open, None)
        );
    };

    std::fs::write(e.state.session("s1"), b"{ not json").unwrap();
    pending(&refresh(&e));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        session_snapshot(&e.state, "s1", agent_id, &[server_id]);
        let path = e.state.session("s1");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let items = refresh(&e);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        pending(&items);
    }

    std::fs::remove_file(e.state.session("s1")).unwrap();
    let items = refresh(&e);
    assert_eq!(items[0].status, InboxStatus::Resolved, "{items:#?}");
    assert_eq!(
        items[0].resolution.as_ref().unwrap().note,
        "the session snapshot is gone"
    );
    server.kill();
}

/// A refresh ends `recovery_needed` only once the reservation file is absent: a reservation
/// that the recovery pass skips because it does not parse ends nothing. The control, the file
/// removed once the group has ended, ends it.
#[cfg(unix)]
#[test]
fn an_unparseable_reservation_ends_nothing() {
    let e = env_with("[budget]\ncpu = 1\n");
    let fx = Fixture::new();
    let repo = fx.init("repo");
    fx.commit_file(&repo, "README.md", "hello\n", "initial");
    let jobs = support::jobs::Jobs::new(fx.scope(), e.dirs.state(), e.dirs.config());
    let mut crashed = jobs.crashed(&repo, 1);
    let items = refresh(&e);
    assert_eq!(items.len(), 1, "{items:#?}");
    assert_eq!(items[0].kind, InboxKind::RecoveryNeeded);

    std::fs::write(e.state.reservation(&crashed.job), b"{ not json").unwrap();
    let items = refresh(&e);
    assert_eq!(items.len(), 1, "{items:#?}");
    assert_eq!(
        (items[0].status, items[0].ended_ms),
        (InboxStatus::Open, None)
    );

    crashed.end_group();
    std::fs::remove_file(e.state.reservation(&crashed.job)).unwrap();
    let items = refresh(&e);
    assert_eq!(items[0].status, InboxStatus::Resolved, "{items:#?}");
    assert_eq!(
        items[0].resolution.as_ref().unwrap().note,
        "the reservation is gone"
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

/// An occurrence of the condition `condition` found at `since_ms`.
fn occurrence(
    e: &Env,
    condition: &str,
    since_ms: u64,
    evidence: &serde_json::Value,
) -> (cardinal::contract::InboxItem, inbox::Raised) {
    inbox::raise_occurrence(&e.state, &item(condition, evidence), since_ms).unwrap()
}

/// The latest occurrence of `condition` as a raiser reads it before observing.
fn latest(e: &Env, condition: &str) -> Option<cardinal::contract::InboxItem> {
    inbox::latest(&e.state).unwrap().remove(condition)
}

/// C19, section 10.1: while an occurrence lasts, raising its condition updates the open item
/// and leaves a resolved one alone; once a raiser ends it, the condition found again raises a
/// new item, whatever the old one's status, and the old one stays as it was.
#[test]
fn occurrence_ends_and_recurs() {
    let e = env();
    let (first, raised) = occurrence(&e, "k", 100, &serde_json::json!({"n": 1}));
    assert_eq!(raised, inbox::Raised::New);
    assert_eq!(first.key, "k@100");
    assert_eq!(
        (first.status, first.seen, first.ended_ms),
        (InboxStatus::Open, 1, None)
    );

    let (again, raised) = occurrence(&e, "k", 200, &serde_json::json!({"n": 2}));
    assert_eq!(raised, inbox::Raised::Updated);
    assert_eq!((again.key.as_str(), again.seen), ("k@100", 2));
    assert_eq!(again.evidence["n"], 2);

    inbox::resolve(
        &e.config,
        &e.state,
        "k@100",
        ResolutionOutcome::Resolved,
        Some("done"),
    )
    .unwrap();
    let (kept, raised) = occurrence(&e, "k", 300, &serde_json::json!({"n": 3}));
    assert_eq!(raised, inbox::Raised::Unchanged, "{kept:#?}");
    assert_eq!((kept.status, kept.seen), (InboxStatus::Resolved, 2));
    assert_eq!(kept.evidence["n"], 2, "a resolved item is left alone");

    let before = latest(&e, "k");
    let ended = inbox::end_occurrence(&e.state, "k", before.as_ref(), 400, "gone")
        .unwrap()
        .expect("the occurrence ends");
    assert_eq!(ended.ended_ms, Some(400));
    assert_eq!(ended.status, InboxStatus::Resolved);
    assert_eq!(
        ended.resolution.as_ref().unwrap().by,
        "cli",
        "the resolution is kept"
    );
    assert_eq!(
        inbox::end_occurrence(&e.state, "k", Some(&ended), 450, "gone").unwrap(),
        None,
        "an ended occurrence ends once"
    );
    let first_ended = ended;

    let (recurred, raised) = occurrence(&e, "k", 500, &serde_json::json!({"n": 4}));
    assert_eq!(raised, inbox::Raised::New);
    assert_eq!(recurred.key, "k@500");
    assert_eq!((recurred.status, recurred.seen), (InboxStatus::Open, 1));

    let before = latest(&e, "k");
    let ended = inbox::end_occurrence(&e.state, "k", before.as_ref(), 600, "cleared")
        .unwrap()
        .expect("the open occurrence ends");
    assert_eq!(ended.key, "k@500");
    assert_eq!(ended.status, InboxStatus::Resolved);
    let resolution = ended.resolution.as_ref().unwrap();
    assert_eq!(
        (resolution.by.as_str(), resolution.note.as_str()),
        ("cardinal", "cleared")
    );

    let items = list(&e, Some(StatusFilter::All));
    assert_eq!(items.len(), 2, "{items:#?}");
    assert_eq!(items[0], first_ended, "the first occurrence is as it was");
    assert_eq!(item_files(&e.state), 2);
}

/// An open or escalated occurrence that ends is resolved by cardinal; an escalated one too.
#[test]
fn ending_resolves_an_escalated_item() {
    let e = env();
    occurrence(&e, "k", 100, &serde_json::json!({}));
    inbox::resolve(
        &e.config,
        &e.state,
        "k@100",
        ResolutionOutcome::Escalated,
        None,
    )
    .unwrap();
    let before = latest(&e, "k");
    let ended = inbox::end_occurrence(&e.state, "k", before.as_ref(), 200, "cleared")
        .unwrap()
        .unwrap();
    assert_eq!(ended.status, InboxStatus::Resolved);
    assert_eq!(ended.resolution.unwrap().by, "cardinal");
}

/// Section 10.1: a raiser ends only an occurrence it read before it started observing, and
/// only while it is unchanged; one it did not read, or one updated since, is left alone.
#[test]
fn ending_needs_the_item_as_read_before_observing() {
    let e = env();
    let (raised, _) = occurrence(&e, "k", 100, &serde_json::json!({"n": 1}));

    assert_eq!(
        inbox::end_occurrence(&e.state, "k", None, 200, "cleared").unwrap(),
        None,
        "not read before observing"
    );
    occurrence(&e, "k", 150, &serde_json::json!({"n": 2}));
    assert_eq!(
        inbox::end_occurrence(&e.state, "k", Some(&raised), 200, "cleared").unwrap(),
        None,
        "updated since it was read"
    );
    let items = list(&e, None);
    assert_eq!(items.len(), 1);
    assert_eq!(
        (items[0].status, items[0].ended_ms, items[0].seen),
        (InboxStatus::Open, None, 2)
    );

    let before = latest(&e, "k");
    assert!(
        inbox::end_occurrence(&e.state, "k", before.as_ref(), 200, "cleared")
            .unwrap()
            .is_some(),
        "the control, read and unchanged, ends"
    );
}

/// A condition found again with a time no later than its ended occurrence's still raises an
/// item of its own (a clock set back), under a key after the ended one's.
#[test]
fn a_recurrence_never_takes_an_earlier_key() {
    let e = env();
    occurrence(&e, "k", 500, &serde_json::json!({}));
    let before = latest(&e, "k");
    inbox::end_occurrence(&e.state, "k", before.as_ref(), 600, "cleared").unwrap();
    let (recurred, raised) = occurrence(&e, "k", 400, &serde_json::json!({}));
    assert_eq!(raised, inbox::Raised::New);
    assert_eq!(recurred.key, "k@501");
    assert_eq!(list(&e, Some(StatusFilter::All)).len(), 2);
}

/// An item stored before occurrences, whose key has no `@<since_ms>`, is the latest occurrence
/// of the condition its key names: raising the condition updates it.
#[test]
fn an_item_stored_before_occurrences_continues() {
    let e = env();
    inbox::raise(&e.state, &item("outlived:s1", &serde_json::json!({"n": 1}))).unwrap();
    let (updated, raised) = occurrence(&e, "outlived:s1", 100, &serde_json::json!({"n": 2}));
    assert_eq!(raised, inbox::Raised::Updated);
    assert_eq!((updated.key.as_str(), updated.seen), ("outlived:s1", 2));
    assert_eq!(item_files(&e.state), 1);
}

/// Every writer of an item holds `inbox/.lock` (section 13.5), and a raise looks up the
/// condition's latest occurrence inside it: an occurrence written while the raise waits for the
/// lock is updated, not duplicated.
#[test]
fn a_raise_reads_the_latest_occurrence_under_the_lock() {
    let e = env();
    let held =
        cardinal::state::try_lock(&e.state.inbox_lock(), cardinal::state::LockMode::Exclusive)
            .unwrap()
            .expect("the inbox lock is free");
    let state = e.state.clone();
    let raising = std::thread::spawn(move || {
        inbox::raise_occurrence(&state, &item("k", &serde_json::json!({"n": 2})), 200).unwrap()
    });
    std::thread::sleep(Duration::from_millis(200));
    assert!(!raising.is_finished(), "the raise waits for the lock");
    // Another raiser's occurrence, written while it holds the lock.
    let mut other = item_document("k@100");
    other.evidence = serde_json::json!({"n": 1}).as_object().unwrap().clone();
    std::fs::write(
        e.state.inbox_item("k@100", 0),
        serde_json::to_vec(&other).unwrap(),
    )
    .unwrap();
    drop(held);
    let (stored, raised) = raising.join().unwrap();
    assert_eq!(raised, inbox::Raised::Updated);
    assert_eq!((stored.key.as_str(), stored.seen), ("k@100", 2));
    assert_eq!(item_files(&e.state), 1, "no duplicate occurrence");
}

/// An open item document under `key`.
fn item_document(key: &str) -> cardinal::contract::InboxItem {
    cardinal::contract::InboxItem {
        schema: cardinal::contract::InboxItemSchema,
        key: key.into(),
        kind: InboxKind::RecoveryNeeded,
        status: InboxStatus::Open,
        subject: InboxSubject::default(),
        evidence: serde_json::Map::new(),
        created_ms: 1,
        updated_ms: 1,
        seen: 1,
        ended_ms: None,
        arbiter: None,
        resolution: None,
    }
}

/// C9, section 10.1: a session whose outlived processes ended and that ends again leaving
/// processes behind raises a new item; the first stays resolved.
#[test]
fn outlived_processes_recur_as_a_new_item() {
    let e = env();
    let mut agent = e.sleeper();
    let agent_id = (agent.id(), alive_start(agent.id()));
    agent.kill();
    let mut server = e.sleeper();
    let server_id = (server.id(), alive_start(server.id()));
    session_snapshot(&e.state, "s1", agent_id, &[server_id]);
    let first = refresh(&e);
    assert_eq!(first.len(), 1, "{first:#?}");
    server.kill();
    let ended = refresh(&e);
    assert_eq!(ended[0].status, InboxStatus::Resolved);
    assert!(ended[0].ended_ms.is_some());

    let mut again = e.sleeper();
    let again_id = (again.id(), alive_start(again.id()));
    session_snapshot(&e.state, "s1", agent_id, &[again_id]);
    let items = refresh(&e);
    assert_eq!(items.len(), 2, "{items:#?}");
    let open: Vec<_> = items
        .iter()
        .filter(|i| i.status == InboxStatus::Open)
        .collect();
    assert_eq!(open.len(), 1, "{items:#?}");
    assert_ne!(open[0].key, first[0].key);
    assert_eq!(inbox::occurrence_of(&open[0].key).0, "outlived:s1");
    assert!(
        items.contains(&ended[0]),
        "the first occurrence is as it was: {items:#?}"
    );
    again.kill();
}

/// An occurrence's raise reads every item under the lock: an item file that does not parse,
/// or (Unix) cannot be read, fails it with `state_unavailable` and writes nothing, since that
/// file may hold the condition's occurrence. The control, the same occurrence readable, is
/// updated rather than raised a second time.
#[test]
fn an_unreadable_item_fails_occurrence_writes() {
    let e = env();
    let slot = e.state.inbox_item("k@100", 0);
    let raise = || inbox::raise_occurrence(&e.state, &item("k", &serde_json::json!({})), 200);
    std::fs::write(&slot, b"{ not json").unwrap();
    let err = raise().unwrap_err();
    assert_eq!(err.code, ErrorCode::StateUnavailable, "{err:?}");
    assert_eq!(item_files(&e.state), 1, "nothing written");

    std::fs::write(&slot, serde_json::to_vec(&item_document("k@100")).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&slot, std::fs::Permissions::from_mode(0o000)).unwrap();
        let raised = raise();
        std::fs::set_permissions(&slot, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(raised.unwrap_err().code, ErrorCode::StateUnavailable);
        assert_eq!(item_files(&e.state), 1, "nothing written");
    }
    let (stored, raised) = raise().unwrap();
    assert_eq!(
        (stored.key.as_str(), raised),
        ("k@100", inbox::Raised::Updated)
    );
    assert_eq!(item_files(&e.state), 1);
}

/// An item file that is listed but gone when read, here a dangling `*.json` link, may still
/// stand for an occurrence: occurrence writes fail with `state_unavailable` until it is
/// removed. The control, the link removed, raises the item.
#[cfg(unix)]
#[test]
fn a_dangling_item_link_fails_occurrence_writes() {
    let e = env();
    let link = e.state.inbox_dir().join("dangling.json");
    std::os::unix::fs::symlink(e.state.inbox_dir().join("missing"), &link).unwrap();
    let raise = || inbox::raise_occurrence(&e.state, &item("k", &serde_json::json!({})), 200);
    let err = raise().unwrap_err();
    assert_eq!(err.code, ErrorCode::StateUnavailable, "{err:?}");
    assert_eq!(item_files(&e.state), 1, "nothing written");

    std::fs::remove_file(&link).unwrap();
    let (stored, raised) = raise().unwrap();
    assert_eq!((stored.key.as_str(), raised), ("k@200", inbox::Raised::New));
    assert_eq!(item_files(&e.state), 1);
}

/// An `outlived_processes` item ends only once its session snapshot is deleted: a snapshot the
/// refresh cannot delete keeps the item open, and the refresh that deletes it ends it.
#[cfg(unix)]
#[test]
fn a_snapshot_that_cannot_be_deleted_keeps_its_item() {
    use std::os::unix::fs::PermissionsExt;
    let e = env();
    let mut agent = e.sleeper();
    let agent_id = (agent.id(), alive_start(agent.id()));
    agent.kill();
    let mut server = e.sleeper();
    let server_id = (server.id(), alive_start(server.id()));
    session_snapshot(&e.state, "s1", agent_id, &[server_id]);
    assert_eq!(refresh(&e).len(), 1);
    server.kill();

    let sessions = e.state.sessions_dir();
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o555)).unwrap();
    let items = refresh(&e);
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        (items[0].status, items[0].ended_ms),
        (InboxStatus::Open, None),
        "{items:#?}"
    );
    assert!(
        e.state.session("s1").exists(),
        "the snapshot is still there"
    );

    let items = refresh(&e);
    assert_eq!(items[0].status, InboxStatus::Resolved, "{items:#?}");
    assert!(!e.state.session("s1").exists());
}
