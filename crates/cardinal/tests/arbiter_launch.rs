//! The arbiter launcher (`docs/architecture.md` sections 10.5 and 13.6): passes start only
//! for due items, assign them durably before the session starts, and write their outcome
//! into those items only; a session lists and answers only its own pass's items while it
//! runs, and answers each once; a launcher that dies strands no item; a pass none of whose
//! items is left to assign runs no session; a `last.json` that cannot be read starts nothing;
//! a session is killed at `timeout_s`; a session that answers none of its open items fails its
//! pass; three failures in a row raise `arbiter_failing`, once per run of failures, also when
//! the launcher died before raising it, and a recorded completed pass ends it.
//!
//! Every pass runs through the binary (`cardinal arbiter run`), which re-checks the trigger
//! from the items it lists. `[arbiter] command` names a fake session: this test binary re-run
//! in the helper mode `session` (`support::helper`), which records its argv, environment,
//! working directory, and inherited descriptors, runs the `cardinal` commands its script
//! lists with the environment it was given, and waits for or exits on the test's files.
//! Items are raised through `cardinal::inbox`, as every raiser does. The trigger that
//! `cardinal reconcile` runs at step 7 is tested in `reconcile::arbiter`'s unit tests: the pass
//! that reaches it is another unit's.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cardinal::config::Config;
use cardinal::contract::{
    AnswerOutcome, ArbiterPass, InboxItem, InboxKind, InboxStatus, InboxSubject, PassOutcome,
    RecordKind, ResolutionOutcome,
};
use cardinal::inbox::{self, NewItem};
use cardinal::state::{self, LockMode, StateDir};
use cardinal::testpoint::{
    ARBITER_ANSWERING, ARBITER_ITEMS_WRITING, ARBITER_OUTCOMES, ARBITER_RECORDED,
};
use cardinal_test_support::process::wait_for;
use cardinal_test_support::{Child, Scope};
use serde::{Deserialize, Serialize};
use support::cardinal::Dirs;
use support::helper::{self, Mode};
use support::testpoints::TestPoints;

/// How long a launcher, a session, or a `cardinal` command may take.
const WAIT: Duration = Duration::from_secs(30);

/// The fake session's helper mode.
const SESSION: &str = "session";

#[test]
fn helper() {
    run_session_if_requested();
    helper::run_if_requested();
}

/// What the fake session does, in order.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Step {
    /// Waits until `<dir>/<name>` exists.
    Wait(String),
    /// Runs `cardinal` with these arguments.
    Run(Vec<String>),
    /// Waits until `<dir>/<name>` exists, then goes on.
    Hold(String),
    /// Lists the pass's items with `cardinal inbox --assigned` and defers each open one, as
    /// a session that checked them and left them open does.
    DeferAll,
    /// Exits with this status.
    Exit(i32),
}

/// The fake session's request: where it records, and its steps.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Script {
    dir: PathBuf,
    steps: Vec<Step>,
}

/// What the fake session recorded about itself, in `<dir>/<pass>.json`.
#[derive(Debug, Serialize, Deserialize)]
struct Recorded {
    pid: u32,
    start_s: Option<u64>,
    argv: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: PathBuf,
    /// Each descriptor above 2 it found open and the file it names (Unix).
    fds: Vec<(i32, Option<String>)>,
}

/// A `cardinal` command the fake session ran, in `<dir>/<pass>.run<i>.json`.
#[derive(Debug, Serialize, Deserialize)]
struct Ran {
    code: Option<i32>,
    stdout: String,
}

/// Runs the fake session a helper process was started for, then exits; returns at once
/// otherwise.
fn run_session_if_requested() {
    let Some(request) = helper::custom_request(SESSION) else {
        return;
    };
    let script: Script = serde_json::from_str(&request).expect("a session script");
    let pass = std::env::var("CARDINAL_ARBITER_PASS").unwrap_or_else(|_| "none".into());
    let recorded = Recorded {
        pid: std::process::id(),
        start_s: cardinal::os::start_time_s(std::process::id()),
        argv: std::env::args().collect(),
        env: std::env::vars_os()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect(),
        cwd: std::env::current_dir().expect("the working directory"),
        fds: open_fds(),
    };
    publish(&script.dir, &format!("{pass}.json"), &recorded);
    for (i, step) in script.steps.iter().enumerate() {
        match step {
            Step::Wait(name) | Step::Hold(name) => {
                let file = script.dir.join(name);
                let start = std::time::Instant::now();
                while !file.exists() && start.elapsed() < Duration::from_secs(60) {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            Step::Run(args) => {
                let ran = session_cardinal(args);
                publish(&script.dir, &format!("{pass}.run{i}.json"), &ran);
            }
            Step::DeferAll => {
                let listed = session_cardinal(&["inbox".into(), "--assigned".into()]);
                let listed: serde_json::Value =
                    serde_json::from_str(&listed.stdout).expect("the assigned items");
                for item in listed["items"].as_array().into_iter().flatten() {
                    if item["status"] == "open" {
                        let key = item["key"].as_str().expect("a key").to_owned();
                        session_cardinal(
                            &[
                                "inbox",
                                "resolve",
                                &key,
                                "--outcome",
                                "deferred",
                                "--note",
                                "checked",
                                "--assigned",
                            ]
                            .map(String::from),
                        );
                    }
                }
            }
            Step::Exit(code) => std::process::exit(*code),
        }
    }
    std::process::exit(0);
}

/// Runs `cardinal ARGS` as the fake session's Bash tool does: with the session's own cardinal
/// environment.
fn session_cardinal(args: &[String]) -> Ran {
    let scope = Scope::new();
    let mut command = scope.command(env!("CARGO_BIN_EXE_cardinal"), scope.root());
    command.args(args);
    for name in [
        "CARDINAL_STATE_DIR",
        "CARDINAL_CONFIG_DIR",
        "CARDINAL_ARBITER_PASS",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let output = command.output_within(WAIT).expect("run cardinal");
    Ran {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
    }
}

/// Writes `value` to `dir/name` atomically.
fn publish(dir: &Path, name: &str, value: &impl Serialize) {
    let tmp = dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec(value).unwrap()).expect("write a record");
    std::fs::rename(&tmp, dir.join(name)).expect("publish a record");
}

/// The descriptors above 2 this process holds, each with the file it names when the OS says.
#[cfg(unix)]
fn open_fds() -> Vec<(i32, Option<String>)> {
    let dir = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let fds: Vec<i32> = std::fs::read_dir(dir)
        .expect("list the descriptors")
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .filter(|&fd| fd > 2)
        .collect();
    fds.into_iter().map(|fd| (fd, fd_path(fd))).collect()
}

#[cfg(not(unix))]
fn open_fds() -> Vec<(i32, Option<String>)> {
    Vec::new()
}

#[cfg(target_os = "linux")]
fn fd_path(fd: i32) -> Option<String> {
    let link = std::fs::read_link(format!("/proc/self/fd/{fd}")).ok()?;
    Some(link.display().to_string())
}

#[cfg(target_os = "macos")]
fn fd_path(fd: i32) -> Option<String> {
    let mut buf = vec![0_u8; usize::try_from(libc::PATH_MAX).ok()?];
    // SAFETY: `F_GETPATH` writes at most `PATH_MAX` bytes into `buf`, which holds that many.
    let got = unsafe { libc::fcntl(fd, libc::F_GETPATH, buf.as_mut_ptr()) };
    if got == -1 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0)?;
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}

/// A state and config dir, and the directory the fake session records into.
struct Env {
    dirs: Dirs,
    state: StateDir,
    sessions: PathBuf,
}

/// The `[arbiter]` values a test pins.
#[derive(Clone, Copy)]
#[expect(
    clippy::struct_field_names,
    reason = "named as the [arbiter] keys they pin"
)]
struct Pinned {
    timeout_s: u64,
    min_interval_s: u64,
    retry_s: u64,
}

impl Env {
    fn new(pinned: Pinned, steps: &[Step]) -> Self {
        let dirs = Dirs::new();
        let sessions = dirs.scope().path("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let state = StateDir::open(dirs.state()).unwrap();
        let env = Self {
            dirs,
            state,
            sessions,
        };
        env.configure(pinned, steps);
        env
    }

    /// Writes `host.toml` with `[arbiter]` naming the fake session with `steps`.
    fn configure(&self, pinned: Pinned, steps: &[Step]) {
        let script = Script {
            dir: self.sessions.clone(),
            steps: steps.to_vec(),
        };
        let mode = Mode::Custom {
            name: SESSION,
            arg: serde_json::to_string(&script).unwrap(),
        };
        let mut command = vec![
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        ];
        command.extend(helper::args(&mode));
        let host = format!(
            "[arbiter]\ncommand = {}\ntimeout_s = {}\nmin_interval_s = {}\nretry_s = {}\n",
            serde_json::to_string(&command).unwrap(),
            pinned.timeout_s,
            pinned.min_interval_s,
            pinned.retry_s,
        );
        std::fs::write(self.dirs.config().join("host.toml"), host).unwrap();
    }

    fn config(&self) -> Config {
        Config::load(self.dirs.config()).unwrap()
    }

    fn raise(&self, key: &str, kind: InboxKind) {
        let item = NewItem {
            key: key.into(),
            kind,
            subject: InboxSubject::default(),
            evidence: serde_json::Map::new(),
        };
        inbox::raise(&self.state, &item).unwrap();
    }

    fn item(&self, key: &str) -> InboxItem {
        inbox::read_all(&self.state)
            .unwrap()
            .into_iter()
            .map(|(_, item)| item)
            .find(|item| item.key == key)
            .unwrap_or_else(|| panic!("no item {key}"))
    }

    fn items(&self) -> Vec<InboxItem> {
        inbox::read_all(&self.state)
            .unwrap()
            .into_iter()
            .map(|(_, item)| item)
            .collect()
    }

    fn last(&self) -> Option<ArbiterPass> {
        let bytes = std::fs::read(self.state.arbiter_last()).ok()?;
        Some(serde_json::from_slice(&bytes).unwrap())
    }

    /// `cardinal arbiter run` to its end: its exit code and stdout as JSON.
    fn arbiter_run(&self) -> (Option<i32>, serde_json::Value) {
        self.cardinal(&["arbiter", "run"], None)
    }

    /// `cardinal ARGS`, with `CARDINAL_ARBITER_PASS` set to `pass` when given.
    fn cardinal(&self, args: &[&str], pass: Option<&str>) -> (Option<i32>, serde_json::Value) {
        let mut command = self.dirs.cmd();
        command.args(args);
        if let Some(pass) = pass {
            command.env("CARDINAL_ARBITER_PASS", pass);
        }
        let output = command.output_within(WAIT).expect("run cardinal");
        let stdout = serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
            panic!(
                "{args:?}: {err}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.code(), stdout)
    }

    /// `cardinal arbiter run` in the background, paused at `pause` when given.
    fn spawn_launcher(&self, points: &TestPoints, pause: &[&str]) -> Child {
        self.spawn_launcher_with(points, pause, None)
    }

    /// [`Env::spawn_launcher`], also failing at `fail` when given.
    fn spawn_launcher_with(
        &self,
        points: &TestPoints,
        pause: &[&str],
        fail: Option<&str>,
    ) -> Child {
        let mut command = self.dirs.cmd();
        command
            .args(["arbiter", "run"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        points.apply(&mut command, pause, fail);
        command.spawn().expect("start the launcher")
    }

    /// Waits for a launcher [`Env::spawn_launcher`] started and asserts that it succeeded,
    /// failing with what it printed and `last.json` otherwise.
    fn launcher_succeeded(&self, launcher: Child) {
        let output = launcher.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "the launcher exited {:?}\nstdout: {}\nstderr: {}\nlast.json: {:?}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            self.last()
        );
    }

    /// `cardinal inbox resolve KEY --outcome resolved --assigned` for `pass` in the background,
    /// its stdout piped; it marks `arbiter.answering` as it starts to wait for `inbox/.lock`.
    fn spawn_answer(&self, points: &TestPoints, pass: &str, key: &str) -> Child {
        let mut command = self.dirs.cmd();
        command
            .args([
                "inbox",
                "resolve",
                key,
                "--outcome",
                "resolved",
                "--assigned",
            ])
            .env("CARDINAL_ARBITER_PASS", pass)
            .stdout(std::process::Stdio::piped());
        // Marked, not held.
        points.release(ARBITER_ANSWERING);
        points.apply(&mut command, &[ARBITER_ANSWERING], None);
        command.spawn().expect("start the answer")
    }

    /// What the fake session of `pass` recorded, once it has.
    fn session(&self, pass: &str) -> Recorded {
        let file = self.sessions.join(format!("{pass}.json"));
        wait_for(&format!("the session of {pass}"), WAIT, || {
            let bytes = std::fs::read(&file).ok()?;
            Some(serde_json::from_slice(&bytes).unwrap())
        })
    }

    fn session_ran(&self, pass: &str) -> bool {
        self.sessions.join(format!("{pass}.json")).exists()
    }

    /// The `cardinal` command the fake session of `pass` ran as step `step`.
    fn ran(&self, pass: &str, step: usize) -> Ran {
        let bytes = std::fs::read(self.sessions.join(format!("{pass}.run{step}.json")))
            .unwrap_or_else(|err| panic!("step {step} of {pass}: {err}"));
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Creates `<sessions>/<name>`, which a waiting or holding fake session reads.
    fn signal(&self, name: &str) {
        std::fs::write(self.sessions.join(name), b"").unwrap();
    }

    fn records(&self, kind: RecordKind) -> Vec<cardinal::contract::Record> {
        cardinal::record::read_all(&self.state)
            .unwrap()
            .into_iter()
            .filter(|record| record.kind == kind)
            .collect()
    }
}

fn pass_of(doc: &serde_json::Value) -> ArbiterPass {
    serde_json::from_value(doc.clone()).unwrap_or_else(|err| panic!("{err}: {doc}"))
}

/// The error code of a `cardinal.error.v1` document.
fn error_code(doc: &serde_json::Value) -> &str {
    doc["error"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("{doc}"))
}

/// Sleeps until the wall clock reads at least `ms`.
fn sleep_until(ms: u64) {
    let now = state::now_ms();
    if ms > now {
        std::thread::sleep(Duration::from_millis(ms - now));
    }
}

/// `timeout_s` of the tests that kill a launcher: long enough for the steps that must
/// happen while its pass still counts as running.
const KILLED_TIMEOUT_S: u64 = 3;
const KILLED_TIMEOUT_MS: u64 = KILLED_TIMEOUT_S * 1000;

/// The owner's kinds, never due for a pass.
const OWNERS: [(&str, InboxKind); 4] = [
    ("disk_low:v@1", InboxKind::DiskLow),
    ("disk_critical:v@1", InboxKind::DiskCritical),
    ("arbiter_failing:p@1", InboxKind::ArbiterFailing),
    ("reclaim_stranded:p@1", InboxKind::ReclaimStranded),
];

/// A19: a pass takes exactly the due items: open ones never assigned, not closed or
/// escalated ones, the owner's kinds, or one whose pass ended less than `retry_s` ago; and a
/// pass starts at most once per `min_interval_s`.
#[test]
fn due_items_only() {
    let pinned = Pinned {
        timeout_s: 60,
        min_interval_s: 3600,
        retry_s: 3600,
    };
    let e = Env::new(pinned, &[Step::DeferAll]);
    e.raise("reclaimable:/a@h@1", InboxKind::Reclaimable);
    // A later millisecond: items created in the same one are ordered by key, which would
    // put this one first.
    let first = e.item("reclaimable:/a@h@1").created_ms;
    while state::now_ms() <= first {
        std::thread::yield_now();
    }
    e.raise("merge_undecided:/b@h@1", InboxKind::MergeUndecided);
    for (key, kind) in OWNERS {
        e.raise(key, kind);
    }
    e.raise("closed_pr_dirty:/c#1@1", InboxKind::ClosedPrDirty);
    inbox::resolve(
        &e.config(),
        &e.state,
        "closed_pr_dirty:/c#1@1",
        ResolutionOutcome::Resolved,
        None,
    )
    .unwrap();
    e.raise("managed_merged:/d@h@1", InboxKind::ManagedMerged);
    inbox::resolve(
        &e.config(),
        &e.state,
        "managed_merged:/d@h@1",
        ResolutionOutcome::Escalated,
        None,
    )
    .unwrap();
    // Assigned to a pass that ended just now.
    let recent = "unclassifiable:/e:1@1";
    e.raise(recent, InboxKind::UnclassifiableData);
    let now = state::now_ms();
    inbox::assign(&e.state, "p0", &[recent.to_owned()], now).unwrap();
    let (written, published, _) = inbox::record_outcome(
        &e.state,
        "p0",
        &[recent.to_owned()],
        &PassOutcome::Completed,
        now,
        |_| Ok(()),
    );
    written.unwrap();
    published.unwrap();

    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(0), "{doc}");
    let pass = pass_of(&doc);
    assert_eq!(
        pass.items,
        ["reclaimable:/a@h@1", "merge_undecided:/b@h@1"],
        "the due items, oldest first"
    );
    assert_eq!(pass.outcome, Some(PassOutcome::Completed));
    assert_eq!(e.last(), Some(pass.clone()));
    for item in e.items() {
        let assigned = item.arbiter.as_ref().map(|a| a.pass.as_str());
        if pass.items.contains(&item.key) {
            assert_eq!(assigned, Some(pass.id.as_str()), "{}", item.key);
        } else if item.key == recent {
            assert_eq!(assigned, Some("p0"));
        } else {
            assert_eq!(assigned, None, "{}", item.key);
        }
    }

    // Due, but the last pass started less than `min_interval_s` ago.
    e.raise("reclaimable:/f@h@1", InboxKind::Reclaimable);
    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(1), "{doc}");
    assert_eq!(error_code(&doc), "not_found");
    assert!(
        doc["error"]["message"]
            .as_str()
            .unwrap()
            .contains("min_interval_s")
    );
    assert_eq!(e.last(), Some(pass), "no pass started");
}

/// A23: disk items, `arbiter_failing`, and `reclaim_stranded` are the owner's and start no
/// pass.
#[test]
fn disk_items_start_no_pass() {
    let pinned = Pinned {
        timeout_s: 60,
        min_interval_s: 0,
        retry_s: 0,
    };
    let e = Env::new(pinned, &[Step::DeferAll]);
    for (key, kind) in OWNERS {
        e.raise(key, kind);
    }
    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(1), "{doc}");
    assert_eq!(error_code(&doc), "not_found");
    assert_eq!(e.last(), None, "no pass recorded");
    assert_eq!(
        std::fs::read_dir(&e.sessions).unwrap().count(),
        0,
        "no session ran"
    );
    for item in e.items() {
        assert_eq!(item.arbiter, None, "{}", item.key);
    }
}

/// A19: the session runs in `arbiter/` with the launcher's environment and its pass in
/// `CARDINAL_ARBITER_PASS`, holding no lock; with `--assigned` it lists and answers only the
/// items assigned to its pass, also when another item becomes due while it runs, and
/// nothing once the pass has ended; the launcher writes the outcome into the pass's items
/// only. One launcher runs at a time, and an item that became due during a pass waits for
/// `min_interval_s`.
#[test]
fn assigned_items_only() {
    let pinned = Pinned {
        timeout_s: 60,
        min_interval_s: 3600,
        retry_s: 3600,
    };
    let steps = [
        Step::Wait("go".into()),
        Step::Run(vec!["inbox".into(), "--assigned".into()]),
        Step::Run(
            [
                "inbox",
                "resolve",
                "reclaimable:/a@h@1",
                "--outcome",
                "resolved",
                "--note",
                "done",
                "--assigned",
            ]
            .map(String::from)
            .to_vec(),
        ),
        Step::Run(
            [
                "inbox",
                "resolve",
                "reclaimable:/c@h@1",
                "--outcome",
                "resolved",
                "--assigned",
            ]
            .map(String::from)
            .to_vec(),
        ),
    ];
    let e = Env::new(pinned, &steps);
    e.raise("reclaimable:/a@h@1", InboxKind::Reclaimable);
    e.raise("reclaimable:/b@h@1", InboxKind::Reclaimable);
    let points = TestPoints::new(e.dirs.scope());
    let launcher = e.spawn_launcher(&points, &[]);
    let pass = wait_for("the pass in last.json", WAIT, || e.last());
    let recorded = e.session(&pass.id);

    // Due while the pass runs: it waits for a pass of its own.
    e.raise("reclaimable:/c@h@1", InboxKind::Reclaimable);
    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(0), "{doc}");
    assert_eq!(pass_of(&doc).id, pass.id, "the running pass is reported");
    assert_eq!(pass_of(&doc).outcome, None);
    e.signal("go");
    e.launcher_succeeded(launcher);

    assert_session_environment(&e, &pass, &recorded);

    let listed: serde_json::Value = serde_json::from_str(&e.ran(&pass.id, 1).stdout).unwrap();
    let listed: Vec<&str> = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["key"].as_str().unwrap())
        .collect();
    assert_eq!(listed, ["reclaimable:/a@h@1", "reclaimable:/b@h@1"]);
    let resolved = e.ran(&pass.id, 2);
    assert_eq!(resolved.code, Some(0), "{}", resolved.stdout);
    let a = e.item("reclaimable:/a@h@1");
    assert_eq!(a.status, InboxStatus::Resolved);
    assert_eq!(a.resolution.unwrap().note, "done");
    let refused = e.ran(&pass.id, 3);
    assert_eq!(refused.code, Some(1));
    let refused: serde_json::Value = serde_json::from_str(&refused.stdout).unwrap();
    assert_eq!(error_code(&refused), "not_assigned");

    let c = e.item("reclaimable:/c@h@1");
    assert_eq!(
        (c.status, c.arbiter),
        (InboxStatus::Open, None),
        "unchanged"
    );
    for key in ["reclaimable:/a@h@1", "reclaimable:/b@h@1"] {
        let assigned = e.item(key).arbiter.unwrap();
        assert_eq!(assigned.pass, pass.id);
        assert_eq!(assigned.outcome, Some(PassOutcome::Completed));
    }

    // The pass has ended: nothing is listed or answered for it.
    let b_before = e.item("reclaimable:/b@h@1");
    let (code, doc) = e.cardinal(&["inbox", "--assigned"], Some(&pass.id));
    assert_eq!(code, Some(0), "{doc}");
    assert_eq!(doc["items"], serde_json::json!([]));
    let resolve = [
        "inbox",
        "resolve",
        "reclaimable:/b@h@1",
        "--outcome",
        "resolved",
        "--assigned",
    ];
    let (code, doc) = e.cardinal(&resolve, Some(&pass.id));
    assert_eq!((code, error_code(&doc)), (Some(1), "not_assigned"));
    assert_eq!(e.item("reclaimable:/b@h@1"), b_before, "left unchanged");
    // No pass named: not a session of a pass.
    let (code, doc) = e.cardinal(&["inbox", "--assigned"], None);
    assert_eq!((code, error_code(&doc)), (Some(1), "not_assigned"));

    // The item raised during the pass waits for the interval.
    let (code, doc) = e.arbiter_run();
    assert_eq!((code, error_code(&doc)), (Some(1), "not_found"));
    assert_eq!(e.item("reclaimable:/c@h@1").arbiter, None);
}

/// The session runs `[arbiter] command` in `arbiter/`, with the launcher's environment and
/// its pass in `CARDINAL_ARBITER_PASS`, and inherits no descriptor of the state dir, such as
/// the launcher's `arbiter/.lock`.
fn assert_session_environment(e: &Env, pass: &ArbiterPass, recorded: &Recorded) {
    let state_dir = e.dirs.state();
    assert_eq!(recorded.env["CARDINAL_ARBITER_PASS"], pass.id);
    assert_eq!(
        Path::new(&recorded.env["CARDINAL_STATE_DIR"]),
        state_dir,
        "the launcher's environment"
    );
    assert_eq!(
        recorded.cwd.canonicalize().unwrap(),
        e.state.arbiter_dir().canonicalize().unwrap()
    );
    let configured = &e.config().host.arbiter.command;
    assert_eq!(&recorded.argv[1..], &configured[1..]);
    let state_root = state_dir.canonicalize().unwrap();
    for (fd, path) in &recorded.fds {
        let inside = path
            .as_deref()
            .is_some_and(|path| Path::new(path).starts_with(&state_root));
        assert!(!inside, "the session inherited descriptor {fd} on {path:?}");
    }
}

/// A19: an item a completed pass left open, and the items of a pass whose session exited
/// non-zero, are due again `retry_s` after the pass ended, and not before; an item the
/// session resolved never is.
#[test]
fn retried_after_the_cooldown() {
    let pinned = Pinned {
        timeout_s: 60,
        min_interval_s: 0,
        retry_s: 3,
    };
    let resolve = Step::Run(
        [
            "inbox",
            "resolve",
            "reclaimable:/r@h@1",
            "--outcome",
            "resolved",
            "--assigned",
        ]
        .map(String::from)
        .to_vec(),
    );
    let e = Env::new(pinned, &[resolve]);
    e.raise("reclaimable:/l@h@1", InboxKind::Reclaimable);
    e.raise("reclaimable:/r@h@1", InboxKind::Reclaimable);

    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(0), "{doc}");
    let first = pass_of(&doc);
    assert_eq!(first.items, ["reclaimable:/l@h@1", "reclaimable:/r@h@1"]);
    assert_eq!(first.outcome, Some(PassOutcome::Completed));
    let ended = first.ended_ms.unwrap();
    assert_eq!(e.item("reclaimable:/l@h@1").status, InboxStatus::Open);

    e.configure(pinned, &[Step::Exit(1)]);
    assert!(
        state::now_ms() < ended + 3000,
        "the check below runs within retry_s"
    );
    let (code, doc) = e.arbiter_run();
    assert_eq!((code, error_code(&doc)), (Some(1), "not_found"));
    sleep_until(ended + 3000);
    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(0), "{doc}");
    let second = pass_of(&doc);
    assert_eq!(
        second.items,
        ["reclaimable:/l@h@1"],
        "the resolved item never"
    );
    assert_eq!(second.outcome, Some(PassOutcome::Failed("exit 1".into())));

    let ended = second.ended_ms.unwrap();
    assert!(
        state::now_ms() < ended + 3000,
        "the check below runs within retry_s"
    );
    let (code, doc) = e.arbiter_run();
    assert_eq!((code, error_code(&doc)), (Some(1), "not_found"));
    sleep_until(ended + 3000);
    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(0), "{doc}");
    assert_eq!(pass_of(&doc).items, ["reclaimable:/l@h@1"]);
}

/// A19: a launcher killed after it recorded the pass in `last.json` and before it assigned
/// anything leaves the listed items unassigned: the pass counts as running until `timeout_s`
/// after its start, and after that the next launcher records it `abandoned` and takes the
/// items at once, `retry_s` not applying.
#[test]
fn killed_before_assigning() {
    let pinned = Pinned {
        timeout_s: KILLED_TIMEOUT_S,
        min_interval_s: 0,
        retry_s: 3600,
    };
    let e = Env::new(pinned, &[Step::DeferAll]);
    e.raise("reclaimable:/a@h@1", InboxKind::Reclaimable);
    let points = TestPoints::new(e.dirs.scope());
    let mut launcher = e.spawn_launcher(&points, &[ARBITER_RECORDED]);
    points.wait_reached(ARBITER_RECORDED, launcher.id(), WAIT);
    let first = e.last().unwrap();
    assert_eq!(first.items, ["reclaimable:/a@h@1"]);
    launcher.kill();

    assert_eq!(e.item("reclaimable:/a@h@1").arbiter, None, "never assigned");
    assert!(
        state::now_ms() < first.started_ms + KILLED_TIMEOUT_MS,
        "within timeout_s"
    );
    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(0), "{doc}");
    assert_eq!(pass_of(&doc), first, "still running");
    assert!(!e.session_ran(&first.id));

    sleep_until(first.started_ms + KILLED_TIMEOUT_MS);
    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(0), "{doc}");
    let second = pass_of(&doc);
    assert_eq!(second.items, ["reclaimable:/a@h@1"], "due at once");
    assert_eq!(second.outcome, Some(PassOutcome::Completed));
    assert_eq!(
        second.failures_in_a_row, 0,
        "the abandoned pass counted, then the completed one reset it"
    );
    let abandoned: Vec<ArbiterPass> = e
        .records(RecordKind::ArbiterPass)
        .into_iter()
        .map(|record| serde_json::from_value(record.data.into()).unwrap())
        .filter(|recorded: &ArbiterPass| recorded.id == first.id)
        .collect();
    assert_eq!(abandoned.len(), 1);
    assert_eq!(abandoned[0].outcome, Some(PassOutcome::Abandoned));
    assert_eq!(
        abandoned[0].ended_ms,
        Some(first.started_ms + KILLED_TIMEOUT_MS)
    );
    assert_eq!(abandoned[0].failures_in_a_row, 1);
}

/// A19: a launcher killed after it wrote the outcome into the pass's items and before it
/// recorded the end leaves those outcomes in place when the pass is recorded `abandoned`.
#[test]
fn outcomes_kept_when_abandoned() {
    let pinned = Pinned {
        timeout_s: KILLED_TIMEOUT_S,
        min_interval_s: 0,
        retry_s: 3600,
    };
    let e = Env::new(pinned, &[Step::DeferAll]);
    e.raise("reclaimable:/a@h@1", InboxKind::Reclaimable);
    e.raise("reclaimable:/b@h@1", InboxKind::Reclaimable);
    let points = TestPoints::new(e.dirs.scope());
    let mut launcher = e.spawn_launcher(&points, &[ARBITER_OUTCOMES]);
    points.wait_reached(ARBITER_OUTCOMES, launcher.id(), WAIT);
    launcher.kill();
    let first = e.last().unwrap();
    assert_eq!(first.ended_ms, None);
    let before: Vec<_> = ["reclaimable:/a@h@1", "reclaimable:/b@h@1"]
        .map(|key| e.item(key).arbiter.unwrap())
        .to_vec();
    for assigned in &before {
        assert_eq!(assigned.outcome, Some(PassOutcome::Completed));
    }

    sleep_until(first.started_ms + KILLED_TIMEOUT_MS);
    let (code, doc) = e.arbiter_run();
    assert_eq!(
        (code, error_code(&doc)),
        (Some(1), "not_found"),
        "nothing due"
    );
    let recorded = e.last().unwrap();
    assert_eq!(recorded.id, first.id);
    assert_eq!(recorded.outcome, Some(PassOutcome::Abandoned));
    assert_eq!(
        recorded.ended_ms,
        Some(first.started_ms + KILLED_TIMEOUT_MS)
    );
    let after: Vec<_> = ["reclaimable:/a@h@1", "reclaimable:/b@h@1"]
        .map(|key| e.item(key).arbiter.unwrap())
        .to_vec();
    assert_eq!(after, before, "the outcomes are kept");
}

/// A20: a session that does not exit is killed at `timeout_s`, and the pass is `timed_out`.
#[test]
fn timeout_kills_the_session() {
    let pinned = Pinned {
        timeout_s: 1,
        min_interval_s: 0,
        retry_s: 3600,
    };
    let e = Env::new(pinned, &[Step::Hold("never".into())]);
    e.raise("reclaimable:/a@h@1", InboxKind::Reclaimable);
    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(0), "{doc}");
    let pass = pass_of(&doc);
    assert_eq!(pass.outcome, Some(PassOutcome::TimedOut));
    assert_eq!(pass.failures_in_a_row, 1);
    let ended = pass.ended_ms.unwrap();
    assert!(
        (pass.started_ms + 1000..pass.started_ms + 10_000).contains(&ended),
        "killed at timeout_s, not before and not long after"
    );
    let recorded = e.session(&pass.id);
    let start_s = recorded.start_s.expect("the session's start time");
    assert!(
        !cardinal::os::is_alive(recorded.pid, start_s),
        "the session is gone"
    );
    let assigned = e.item("reclaimable:/a@h@1").arbiter.unwrap();
    assert_eq!(assigned.outcome, Some(PassOutcome::TimedOut));
    assert_eq!(assigned.ended_ms, Some(ended));
}

/// A20: a launcher killed by SIGKILL kills nothing (on Windows the session's Job Object
/// ends with its launcher, so this is Unix's case); its pass counts as running until
/// `timeout_s` after its start, and its session answers nothing after that; then the next
/// launcher records it `abandoned` in `last.json`
/// and in its items, all with that one end time, and signals nothing; the items are due
/// `retry_s` after it.
#[cfg(unix)]
#[test]
fn launcher_killed() {
    let pinned = Pinned {
        timeout_s: KILLED_TIMEOUT_S,
        min_interval_s: 0,
        retry_s: 2,
    };
    let e = Env::new(pinned, &[Step::Hold("release".into())]);
    let _leftovers = cardinal_test_support::process::LeftoverGuard::new(e.dirs.scope());
    e.raise("reclaimable:/a@h@1", InboxKind::Reclaimable);
    e.raise("reclaimable:/b@h@1", InboxKind::Reclaimable);
    let points = TestPoints::new(e.dirs.scope());
    let mut launcher = e.spawn_launcher(&points, &[]);
    let first = wait_for("the pass in last.json", WAIT, || e.last());
    let session = e.session(&first.id);
    let session_start = session.start_s.expect("the session's start time");
    launcher.kill();

    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(0), "{doc}");
    assert_eq!(pass_of(&doc), first, "running until timeout_s");
    let (code, doc) = e.cardinal(&["inbox", "--assigned"], Some(&first.id));
    assert_eq!(code, Some(0), "{doc}");
    assert_eq!(doc["items"].as_array().map(Vec::len), Some(2), "{doc}");

    let end = first.started_ms + KILLED_TIMEOUT_MS;
    sleep_until(end);
    // Past `timeout_s` the pass has ended, though no launcher recorded it yet: its session
    // answers nothing.
    let (code, doc) = e.cardinal(&["inbox", "--assigned"], Some(&first.id));
    assert_eq!(code, Some(0), "{doc}");
    assert_eq!(doc["items"], serde_json::json!([]));
    let resolve = [
        "inbox",
        "resolve",
        "reclaimable:/a@h@1",
        "--outcome",
        "resolved",
        "--assigned",
    ];
    let (code, doc) = e.cardinal(&resolve, Some(&first.id));
    assert_eq!((code, error_code(&doc)), (Some(1), "not_assigned"));
    assert_eq!(e.item("reclaimable:/a@h@1").status, InboxStatus::Open);
    e.configure(pinned, &[Step::DeferAll]);
    let (code, doc) = e.arbiter_run();
    assert_eq!(
        (code, error_code(&doc)),
        (Some(1), "not_found"),
        "due only after retry_s"
    );
    let recorded = e.last().unwrap();
    assert_eq!(recorded.outcome, Some(PassOutcome::Abandoned));
    assert_eq!(recorded.ended_ms, Some(end));
    for key in ["reclaimable:/a@h@1", "reclaimable:/b@h@1"] {
        let assigned = e.item(key).arbiter.unwrap();
        assert_eq!(assigned.outcome, Some(PassOutcome::Abandoned), "{key}");
        assert_eq!(assigned.ended_ms, Some(end), "{key}");
    }
    assert!(
        cardinal::os::is_alive(session.pid, session_start),
        "nothing signals the session"
    );

    sleep_until(end + 2000);
    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(0), "{doc}");
    let second = pass_of(&doc);
    assert_eq!(second.items, ["reclaimable:/a@h@1", "reclaimable:/b@h@1"]);
    e.signal("release");
}

/// A20: the third failed pass in a row raises `arbiter_failing`, keyed by the first failed pass
/// of the run, which is never assigned to a pass, and a completed pass ends it.
#[test]
fn failing_raised() {
    let pinned = Pinned {
        timeout_s: 60,
        min_interval_s: 0,
        retry_s: 0,
    };
    let e = Env::new(pinned, &[Step::Exit(2)]);
    e.raise("reclaimable:/a@h@1", InboxKind::Reclaimable);
    let mut failed = Vec::new();
    for round in 1..=3 {
        let failing: Vec<_> = e
            .items()
            .into_iter()
            .filter(|item| item.kind == InboxKind::ArbiterFailing)
            .collect();
        assert!(failing.is_empty(), "raised after {} failures", round - 1);
        let (code, doc) = e.arbiter_run();
        assert_eq!(code, Some(0), "{doc}");
        let pass = pass_of(&doc);
        assert_eq!(pass.items, ["reclaimable:/a@h@1"]);
        assert_eq!(pass.outcome, Some(PassOutcome::Failed("exit 2".into())));
        assert_eq!(pass.failures_in_a_row, round);
        failed.push(pass);
    }
    let key = failing_key(&failed[0]);
    let item = e.item(&key);
    assert_eq!(item.status, InboxStatus::Open);
    let evidence: Vec<ArbiterPass> =
        serde_json::from_value(item.evidence["passes"].clone()).unwrap();
    assert_eq!(evidence, failed, "the failed passes");

    e.configure(pinned, &[Step::DeferAll]);
    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(0), "{doc}");
    let completed = pass_of(&doc);
    assert_eq!(
        completed.items,
        ["reclaimable:/a@h@1"],
        "never arbiter_failing"
    );
    assert_eq!(completed.failures_in_a_row, 0);
    let item = e.item(&key);
    assert_eq!(item.status, InboxStatus::Resolved);
    assert_eq!(item.resolution.unwrap().by, "cardinal");
    assert_eq!(item.ended_ms, completed.ended_ms);
}

/// A20: a session that exits 0 having answered none of its items, as one that could not read
/// them does, ends the pass `failed:no_answer`, and the third such pass in a row raises
/// `arbiter_failing`; a pass that answers its items, deferring them included, completes and
/// ends it.
#[test]
fn a_pass_that_answers_nothing_fails() {
    let pinned = Pinned {
        timeout_s: 60,
        min_interval_s: 0,
        retry_s: 0,
    };
    let listing = Step::Run(vec!["inbox".into(), "--assigned".into(), "--brief".into()]);
    let e = Env::new(pinned, &[listing]);
    e.raise("reclaimable:/a@h@1", InboxKind::Reclaimable);
    e.raise("closed_pr_dirty:/b#1@1", InboxKind::ClosedPrDirty);
    let mut failed = Vec::new();
    for round in 1..=3 {
        assert_eq!(failing_keys(&e), BTreeSet::new(), "round {round}");
        let (code, doc) = e.arbiter_run();
        assert_eq!(code, Some(0), "{doc}");
        let pass = pass_of(&doc);
        let listed: serde_json::Value = serde_json::from_str(&e.ran(&pass.id, 0).stdout).unwrap();
        assert_eq!(
            listed["items"].as_array().unwrap().len(),
            2,
            "it read its items"
        );
        assert_eq!(
            pass.outcome,
            Some(PassOutcome::Failed(inbox::NO_ANSWER.into()))
        );
        assert_eq!(pass.failures_in_a_row, round);
        for key in ["reclaimable:/a@h@1", "closed_pr_dirty:/b#1@1"] {
            let assigned = e.item(key).arbiter.unwrap();
            assert_eq!(assigned.outcome, pass.outcome, "{key}");
            assert_eq!(assigned.answer, None, "{key}");
        }
        let log = std::fs::read_to_string(&pass.log).unwrap();
        assert!(log.contains("answered none"), "{log}");
        failed.push(pass);
    }
    let key = failing_key(&failed[0]);
    assert_eq!(failing_keys(&e), BTreeSet::from([key.clone()]));

    e.configure(pinned, &[Step::DeferAll]);
    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(0), "{doc}");
    let completed = pass_of(&doc);
    assert_eq!(completed.outcome, Some(PassOutcome::Completed));
    assert_eq!(completed.failures_in_a_row, 0);
    assert_eq!(e.item(&key).status, InboxStatus::Resolved);
}

/// An answer of any kind makes a pass that exits 0 complete, items it leaves unanswered
/// included; so does a pass none of whose items is left open to answer. A deferred item stays
/// open with the pass's note, is not answered again, and is due again after `retry_s`.
#[test]
fn answers_complete_a_pass() {
    let pinned = Pinned {
        timeout_s: 60,
        min_interval_s: 0,
        retry_s: 3,
    };
    let (a, b) = ("reclaimable:/a@h@1", "managed_merged:/b@h@1");
    let e = Env::new(
        pinned,
        &[
            answer(a, "deferred", "the act was refused: rejected (not_in_use)"),
            answer(a, "resolved", "again"),
        ],
    );
    e.raise(a, InboxKind::Reclaimable);
    e.raise(b, InboxKind::ManagedMerged);
    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(0), "{doc}");
    let pass = pass_of(&doc);
    assert_eq!(pass.outcome, Some(PassOutcome::Completed), "b unanswered");
    let deferred = e.ran(&pass.id, 0);
    assert_eq!(deferred.code, Some(0), "{}", deferred.stdout);
    let again: serde_json::Value = serde_json::from_str(&e.ran(&pass.id, 1).stdout).unwrap();
    assert_eq!(error_code(&again), "invalid_argument", "answered once");
    let item = e.item(a);
    assert_eq!((item.status, item.resolution), (InboxStatus::Open, None));
    let answer = item.arbiter.unwrap().answer.unwrap();
    assert_eq!(answer.outcome, AnswerOutcome::Deferred);
    assert_eq!(answer.note, "the act was refused: rejected (not_in_use)");
    assert!(
        e.records(RecordKind::InboxResolved).is_empty(),
        "a deferral resolves nothing"
    );
    // Due again after `retry_s`, like any item the pass left open.
    let (code, doc) = e.arbiter_run();
    assert_eq!((code, error_code(&doc)), (Some(1), "not_found"));
    sleep_until(pass.ended_ms.unwrap() + 3000);
    e.configure(pinned, &[Step::Wait("go".into())]);
    let points = TestPoints::new(e.dirs.scope());
    let launcher = e.spawn_launcher(&points, &[]);
    let second = wait_for("the second pass", WAIT, || {
        e.last()
            .filter(|last| last.ended_ms.is_none() && last.id != pass.id)
    });
    // Both came due when the first pass's cooldown ran out: by key.
    assert_eq!(second.items, [b, a]);
    // The launcher records the pass before it assigns the items, and starts the session after:
    // closed before then, they would leave nothing to assign, and no pass.
    e.session(&second.id);
    // Both items are closed while the session runs: nothing is left to answer.
    for key in [a, b] {
        inbox::resolve(
            &e.config(),
            &e.state,
            key,
            ResolutionOutcome::Resolved,
            None,
        )
        .unwrap();
    }
    e.signal("go");
    e.launcher_succeeded(launcher);
    assert_eq!(e.last().unwrap().outcome, Some(PassOutcome::Completed));
}

/// A19: passes take due items in the order they became due, so an item behind more than
/// `max_items` others is assigned within `ceil(n / max_items) + 1` passes even when every pass
/// leaves its items open and they come due again before it is reached.
#[test]
fn every_due_item_gets_a_pass() {
    const N: usize = 41;
    let pinned = Pinned {
        timeout_s: 60,
        min_interval_s: 0,
        retry_s: 1,
    };
    let e = Env::new(pinned, &[Step::DeferAll]);
    let keys: Vec<String> = (0..N)
        .map(|i| format!("reclaimable:/w{i:02}@h@1"))
        .collect();
    for key in &keys {
        e.raise(key, InboxKind::Reclaimable);
    }
    let max_items = cardinal::config::MAX_ITEMS;
    let passes = N.div_ceil(max_items) + 1;
    let mut assigned: Vec<Vec<String>> = Vec::new();
    for i in 0..passes {
        if i >= N / max_items {
            // From the pass that reaches the last item on, every item a pass left open is due
            // again, so retries compete with it however fast the earlier passes ran.
            sleep_until(e.last().unwrap().ended_ms.unwrap() + 1000);
        }
        let (code, doc) = e.arbiter_run();
        assert_eq!(code, Some(0), "{doc}");
        let pass = pass_of(&doc);
        assert_eq!(pass.outcome, Some(PassOutcome::Completed));
        assert_eq!(pass.items.len(), max_items);
        assigned.push(pass.items);
    }
    let never: Vec<_> = keys
        .iter()
        .filter(|key| !assigned.iter().flatten().any(|k| k == *key))
        .collect();
    assert!(
        never.is_empty(),
        "never assigned: {never:?}; passes: {assigned:?}"
    );
}

/// `cardinal inbox resolve KEY --outcome OUTCOME --note NOTE --assigned` as a session step.
fn answer(key: &str, outcome: &str, note: &str) -> Step {
    Step::Run(
        [
            "inbox",
            "resolve",
            key,
            "--outcome",
            outcome,
            "--note",
            note,
            "--assigned",
        ]
        .map(String::from)
        .to_vec(),
    )
}

/// The key of the `arbiter_failing` occurrence of the run of failures `first` starts.
fn failing_key(first: &ArbiterPass) -> String {
    format!("arbiter_failing:{}@{}", first.id, first.ended_ms.unwrap())
}

/// The keys of the `arbiter_failing` items, as a set: `created_ms` is the wall clock when an
/// item was stored, which items stored within a millisecond share, so no order of the items
/// is the order they were raised in.
fn failing_keys(e: &Env) -> BTreeSet<String> {
    failing(e).into_iter().map(|item| item.key).collect()
}

/// The `arbiter_failing` items.
fn failing(e: &Env) -> Vec<InboxItem> {
    e.items()
        .into_iter()
        .filter(|item| item.kind == InboxKind::ArbiterFailing)
        .collect()
}

/// A `last.json` that exists but cannot be read or parsed starts no pass and answers nothing:
/// the launcher, `--assigned` listing, and `--assigned` resolution fail with
/// `state_unavailable`, and nothing changes.
#[test]
fn unreadable_history_starts_nothing() {
    let pinned = Pinned {
        timeout_s: 60,
        min_interval_s: 0,
        retry_s: 0,
    };
    let e = Env::new(pinned, &[Step::DeferAll]);
    e.raise("reclaimable:/a@h@1", InboxKind::Reclaimable);
    // Assigned to the pass `p`, as far as the item says.
    e.raise("reclaimable:/b@h@1", InboxKind::Reclaimable);
    let b_key = ["reclaimable:/b@h@1".to_owned()];
    inbox::assign(&e.state, "p", &b_key, state::now_ms()).unwrap();
    let b_before = e.item(&b_key[0]);
    let garbage = b"{\"schema\":\"cardinal.arbiter-pass.v1\",";
    std::fs::write(e.state.arbiter_last(), garbage).unwrap();
    let (code, doc) = e.arbiter_run();
    assert_eq!((code, error_code(&doc)), (Some(1), "state_unavailable"));
    let (code, doc) = e.cardinal(&["inbox", "--assigned"], Some("p"));
    assert_eq!((code, error_code(&doc)), (Some(1), "state_unavailable"));
    let resolve = [
        "inbox",
        "resolve",
        "reclaimable:/b@h@1",
        "--outcome",
        "resolved",
        "--assigned",
    ];
    let (code, doc) = e.cardinal(&resolve, Some("p"));
    assert_eq!((code, error_code(&doc)), (Some(1), "state_unavailable"));
    assert_eq!(e.item(&b_key[0]), b_before, "unchanged");
    assert_eq!(std::fs::read(e.state.arbiter_last()).unwrap(), garbage);
    assert_eq!(
        std::fs::read_dir(&e.sessions).unwrap().count(),
        0,
        "no session ran"
    );
    let a = e.item("reclaimable:/a@h@1");
    assert_eq!((a.status, a.arbiter), (InboxStatus::Open, None));

    // A pass recorded in a file this user cannot read.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::remove_file(e.state.arbiter_last()).unwrap();
        let (code, doc) = e.arbiter_run();
        assert_eq!(code, Some(0), "{doc}");
        let pass = pass_of(&doc);
        let last = e.state.arbiter_last();
        std::fs::set_permissions(&last, std::fs::Permissions::from_mode(0o000)).unwrap();
        // A user who reads it anyway (root) has nothing to check here.
        if std::fs::read(&last).is_err() {
            let (code, doc) = e.arbiter_run();
            assert_eq!((code, error_code(&doc)), (Some(1), "state_unavailable"));
            let sessions = std::fs::read_dir(&e.sessions).unwrap().count();
            assert_eq!(sessions, 1, "only the first pass ran a session");
            assert_eq!(e.item("reclaimable:/a@h@1").arbiter.unwrap().pass, pass.id);
        }
        std::fs::set_permissions(&last, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

/// A pass whose due items were all closed before the launcher assigned them runs no session
/// and leaves no trace: the launcher puts the previous `last.json` back, or removes it when no
/// pass ran before, and exits with `not_found`. Killed before it does, its pass is an orphan
/// like any other: running until `timeout_s` after its start, then recorded `abandoned`. A
/// pass that could assign only some of its items lists just those.
#[test]
fn no_session_without_items() {
    let pinned = Pinned {
        timeout_s: KILLED_TIMEOUT_S,
        min_interval_s: 0,
        retry_s: 3600,
    };
    let e = Env::new(pinned, &[Step::DeferAll]);
    let points = TestPoints::new(e.dirs.scope());
    let close = |key: &str| {
        inbox::resolve(
            &e.config(),
            &e.state,
            key,
            ResolutionOutcome::Resolved,
            None,
        )
        .unwrap();
    };
    // Each launcher below pauses at `arbiter.recorded` until released.
    let rearm = || {
        std::fs::remove_file(cardinal::testpoint::release_file(
            points.dir(),
            ARBITER_RECORDED,
        ))
        .unwrap();
    };

    // No pass ran before: the launcher removes the `last.json` it wrote.
    e.raise("reclaimable:/x@h@1", InboxKind::Reclaimable);
    let mut launcher = e.spawn_launcher(&points, &[ARBITER_RECORDED]);
    points.wait_reached(ARBITER_RECORDED, launcher.id(), WAIT);
    let first = e.last().unwrap();
    close("reclaimable:/x@h@1");
    points.release(ARBITER_RECORDED);
    assert_eq!(launcher.wait().unwrap().code(), Some(1), "not_found");
    assert!(!e.state.arbiter_last().exists(), "no last.json");
    assert!(!e.session_ran(&first.id), "no session");
    rearm();

    // Killed between recording the pass and putting the previous `last.json` back.
    e.raise("reclaimable:/a@h@1", InboxKind::Reclaimable);
    let mut launcher = e.spawn_launcher(&points, &[ARBITER_RECORDED]);
    points.wait_reached(ARBITER_RECORDED, launcher.id(), WAIT);
    let orphan = e.last().unwrap();
    close("reclaimable:/a@h@1");
    launcher.kill();
    assert!(
        state::now_ms() < orphan.started_ms + KILLED_TIMEOUT_MS,
        "within timeout_s"
    );
    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(0), "{doc}");
    assert_eq!(pass_of(&doc), orphan, "running until timeout_s");
    sleep_until(orphan.started_ms + KILLED_TIMEOUT_MS);
    let (code, doc) = e.arbiter_run();
    assert_eq!((code, error_code(&doc)), (Some(1), "not_found"));
    let recorded = e.last().unwrap();
    assert_eq!(recorded.id, orphan.id);
    assert_eq!(recorded.outcome, Some(PassOutcome::Abandoned));
    assert_eq!(
        recorded.ended_ms,
        Some(orphan.started_ms + KILLED_TIMEOUT_MS)
    );
    assert!(!e.session_ran(&orphan.id), "no session");

    // The launcher puts the previous `last.json` back.
    let before = std::fs::read(e.state.arbiter_last()).unwrap();
    e.raise("reclaimable:/b@h@1", InboxKind::Reclaimable);
    let mut launcher = e.spawn_launcher(&points, &[ARBITER_RECORDED]);
    points.wait_reached(ARBITER_RECORDED, launcher.id(), WAIT);
    let pass = e.last().unwrap();
    assert_eq!(pass.items, ["reclaimable:/b@h@1"]);
    close("reclaimable:/b@h@1");
    points.release(ARBITER_RECORDED);
    assert_eq!(launcher.wait().unwrap().code(), Some(1), "not_found");
    assert!(!e.session_ran(&pass.id), "no session");
    assert_eq!(
        std::fs::read(e.state.arbiter_last()).unwrap(),
        before,
        "the previous last.json"
    );
    assert_eq!(e.item("reclaimable:/b@h@1").arbiter, None);
    let passes: Vec<_> = e
        .records(RecordKind::ArbiterPass)
        .into_iter()
        .map(|record| pass_of(&record.data.into()).id)
        .collect();
    assert_eq!(passes, [orphan.id], "no record of the pass");
    rearm();

    // One of two items closed before it was assigned: the pass lists the other.
    e.raise("reclaimable:/d@h@1", InboxKind::Reclaimable);
    e.raise("reclaimable:/e@h@1", InboxKind::Reclaimable);
    let launcher = e.spawn_launcher(&points, &[ARBITER_RECORDED]);
    points.wait_reached(ARBITER_RECORDED, launcher.id(), WAIT);
    assert_eq!(
        e.last().unwrap().items,
        ["reclaimable:/d@h@1", "reclaimable:/e@h@1"]
    );
    close("reclaimable:/e@h@1");
    points.release(ARBITER_RECORDED);
    e.launcher_succeeded(launcher);
    let partial = e.last().unwrap();
    assert_eq!(partial.items, ["reclaimable:/d@h@1"]);
    assert_eq!(partial.outcome, Some(PassOutcome::Completed));
    assert!(e.session_ran(&partial.id));
}

/// A session answers each item of its pass once: an escalation is not answered again.
#[test]
fn answered_once() {
    let pinned = Pinned {
        timeout_s: 60,
        min_interval_s: 0,
        retry_s: 3600,
    };
    let key = "closed_pr_dirty:/a#1@1";
    let steps = [
        answer(key, "escalated", "first"),
        answer(key, "escalated", "second"),
        answer(key, "resolved", "third"),
    ];
    let e = Env::new(pinned, &steps);
    e.raise(key, InboxKind::ClosedPrDirty);
    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(0), "{doc}");
    let pass = pass_of(&doc);
    let escalated = e.ran(&pass.id, 0);
    assert_eq!(escalated.code, Some(0), "{}", escalated.stdout);
    for step in [1, 2] {
        let again = e.ran(&pass.id, step);
        assert_eq!(again.code, Some(1), "step {step}");
        let again: serde_json::Value = serde_json::from_str(&again.stdout).unwrap();
        assert_eq!(error_code(&again), "invalid_argument", "step {step}");
    }
    let item = e.item(key);
    assert_eq!(item.status, InboxStatus::Escalated);
    assert_eq!(item.resolution.unwrap().note, "first");
}

/// An `--assigned` answer that waited for `inbox/.lock` until after its pass's deadline is
/// refused with `not_assigned` and changes nothing: the pass is checked under the lock, right
/// before the write.
#[test]
fn late_answer_refused() {
    let pinned = Pinned {
        timeout_s: KILLED_TIMEOUT_S,
        min_interval_s: 0,
        retry_s: 3600,
    };
    let e = Env::new(pinned, &[Step::Hold("release".into())]);
    let _leftovers = cardinal_test_support::process::LeftoverGuard::new(e.dirs.scope());
    e.raise("reclaimable:/a@h@1", InboxKind::Reclaimable);
    let points = TestPoints::new(e.dirs.scope());
    let mut launcher = e.spawn_launcher(&points, &[]);
    let pass = wait_for("the pass in last.json", WAIT, || e.last());
    e.session(&pass.id);
    // No launcher writes the pass's outcome or end: the pass runs until its deadline.
    launcher.kill();
    let deadline = pass.started_ms + KILLED_TIMEOUT_MS;

    let held = state::lock(
        &e.state.inbox_lock(),
        LockMode::Exclusive,
        Instant::now() + WAIT,
    )
    .unwrap();
    let resolve = e.spawn_answer(&points, &pass.id, "reclaimable:/a@h@1");
    points.wait_reached(ARBITER_ANSWERING, resolve.id(), WAIT);
    assert!(
        state::now_ms() < deadline,
        "the answer waits for the lock within timeout_s"
    );
    sleep_until(deadline + 200);
    drop(held);
    let output = resolve.wait_with_output().unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        (output.status.code(), error_code(&doc)),
        (Some(1), "not_assigned")
    );
    let a = e.item("reclaimable:/a@h@1");
    assert_eq!((a.status, a.resolution), (InboxStatus::Open, None));
    e.signal("release");
}

/// A20: a launcher killed after it recorded the third failure in a row and before it raised
/// `arbiter_failing` leaves it to the next launcher, which raises it once, keyed by the run's
/// first failed pass; a fourth failure raises none, a completed pass ends it, and the next run
/// of three failures raises a new one.
#[test]
fn failing_raised_once() {
    let pinned = Pinned {
        timeout_s: 60,
        min_interval_s: 0,
        retry_s: 0,
    };
    let e = Env::new(pinned, &[Step::Exit(2)]);
    e.raise("reclaimable:/a@h@1", InboxKind::Reclaimable);
    for _ in 0..2 {
        let (code, doc) = e.arbiter_run();
        assert_eq!(code, Some(0), "{doc}");
    }
    e.configure(pinned, &[Step::Wait("go".into()), Step::Exit(2)]);
    let points = TestPoints::new(e.dirs.scope());
    let mut launcher = e.spawn_launcher(&points, &[]);
    let pass = wait_for("the third pass in last.json", WAIT, || {
        e.last().filter(|last| last.ended_ms.is_none())
    });
    e.session(&pass.id);
    // With this lock held, the launcher records the end without the outcomes, then waits for
    // the lock to raise the item: killed there.
    let held = state::lock(
        &e.state.inbox_lock(),
        LockMode::Exclusive,
        Instant::now() + WAIT,
    )
    .unwrap();
    e.signal("go");
    let third = wait_for("the third failure in last.json", WAIT, || {
        e.last().filter(|last| last.ended_ms.is_some())
    });
    launcher.kill();
    drop(held);
    e.configure(pinned, &[Step::Exit(2)]);
    assert_eq!(third.failures_in_a_row, 3);
    assert!(failing(&e).is_empty(), "the killed launcher raised nothing");

    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(0), "{doc}");
    assert_eq!(pass_of(&doc).failures_in_a_row, 4);
    let first = pass_of(&e.records(RecordKind::ArbiterPass)[0].data.clone().into());
    let key = failing_key(&first);
    assert_eq!(
        failing_keys(&e),
        BTreeSet::from([key.clone()]),
        "raised once, for the third failure"
    );
    assert_eq!(e.item(&key).status, InboxStatus::Open);

    e.configure(pinned, &[Step::DeferAll]);
    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(0), "{doc}");
    let completed = pass_of(&doc);
    assert_eq!(completed.failures_in_a_row, 0);
    let ended = e.item(&key);
    assert_eq!(ended.status, InboxStatus::Resolved);
    assert_eq!(ended.ended_ms, completed.ended_ms);

    e.configure(pinned, &[Step::Exit(2)]);
    let mut streak = Vec::new();
    for _ in 0..3 {
        let (code, doc) = e.arbiter_run();
        assert_eq!(code, Some(0), "{doc}");
        streak.push(pass_of(&doc));
    }
    let second = failing_key(&streak[0]);
    assert_eq!(
        failing_keys(&e),
        BTreeSet::from([key, second.clone()]),
        "a new occurrence for the second run of failures"
    );
    assert_eq!(e.item(&second).status, InboxStatus::Open);
}

/// A20: a completed pass whose launcher exits after it wrote the outcome into its items and
/// before it recorded the end in `last.json` is recorded `abandoned`, so the run of failures
/// goes on, under the one `arbiter_failing` it raised.
#[test]
fn one_alert_per_run_of_failures() {
    let pinned = Pinned {
        timeout_s: KILLED_TIMEOUT_S,
        min_interval_s: 0,
        retry_s: 0,
    };
    let e = Env::new(pinned, &[Step::Exit(2)]);
    e.raise("reclaimable:/a@h@1", InboxKind::Reclaimable);
    let mut failed = Vec::new();
    for _ in 0..3 {
        let (code, doc) = e.arbiter_run();
        assert_eq!(code, Some(0), "{doc}");
        failed.push(pass_of(&doc));
    }
    let key = failing_key(&failed[0]);
    assert_eq!(failing_keys(&e), BTreeSet::from([key.clone()]));

    e.configure(pinned, &[Step::DeferAll]);
    let points = TestPoints::new(e.dirs.scope());
    let mut launcher = e.spawn_launcher_with(&points, &[], Some(ARBITER_OUTCOMES));
    assert_eq!(
        launcher.wait().unwrap().code(),
        Some(1),
        "exits at arbiter.outcomes"
    );
    let exited = e.last().unwrap();
    assert_eq!(exited.ended_ms, None, "no end recorded");
    let a = e.item("reclaimable:/a@h@1").arbiter.unwrap();
    assert_eq!(
        (a.pass, a.outcome),
        (exited.id.clone(), Some(PassOutcome::Completed))
    );
    assert_eq!(
        failing_keys(&e),
        BTreeSet::from([key.clone()]),
        "the alert is still open"
    );

    sleep_until(exited.started_ms + KILLED_TIMEOUT_MS);
    e.configure(pinned, &[Step::Exit(2)]);
    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(0), "{doc}");
    assert_eq!(
        pass_of(&doc).failures_in_a_row,
        5,
        "the abandoned pass and this one"
    );
    assert_eq!(
        failing_keys(&e),
        BTreeSet::from([key.clone()]),
        "no second alert"
    );
    assert_eq!(e.item(&key).status, InboxStatus::Open);
}

/// A20: a pass recorded `completed` ends the open `arbiter_failing` also when its outcome could
/// not be written into its items (a failure injected after the launcher read their answers),
/// so the next run of three failures raises a new one.
#[test]
fn a_completed_pass_ends_the_alert() {
    let pinned = Pinned {
        timeout_s: 60,
        min_interval_s: 0,
        retry_s: 0,
    };
    let e = Env::new(pinned, &[Step::Exit(2)]);
    e.raise("reclaimable:/a@h@1", InboxKind::Reclaimable);
    let mut failed = Vec::new();
    for _ in 0..3 {
        let (code, doc) = e.arbiter_run();
        assert_eq!(code, Some(0), "{doc}");
        failed.push(pass_of(&doc));
    }
    let key = failing_key(&failed[0]);
    assert_eq!(failing_keys(&e), BTreeSet::from([key.clone()]));

    // The session answers, and the launcher, having read the answer, fails to write the
    // outcome into the item.
    e.configure(pinned, &[Step::DeferAll]);
    let points = TestPoints::new(e.dirs.scope());
    let launcher = e.spawn_launcher_with(&points, &[], Some(ARBITER_ITEMS_WRITING));
    e.launcher_succeeded(launcher);
    let completed = e.last().unwrap();
    assert_eq!(completed.outcome, Some(PassOutcome::Completed));
    assert_eq!(e.item("reclaimable:/a@h@1").arbiter.unwrap().outcome, None);
    let ended = e.item(&key);
    assert_eq!(ended.status, InboxStatus::Resolved);
    assert_eq!(ended.ended_ms, completed.ended_ms);

    e.configure(pinned, &[Step::Exit(2)]);
    let mut run = Vec::new();
    for _ in 0..3 {
        let (code, doc) = e.arbiter_run();
        assert_eq!(code, Some(0), "{doc}");
        run.push(pass_of(&doc));
    }
    let second = failing_key(&run[0]);
    assert_eq!(failing_keys(&e), BTreeSet::from([key, second.clone()]));
    assert_eq!(e.item(&second).status, InboxStatus::Open);
}

/// An `--assigned` answer waiting for `inbox/.lock` while the launcher ends its pass is refused,
/// also when the launcher could not write the outcome into the item: the launcher records the
/// end in `last.json` before it releases the lock.
#[cfg(unix)]
#[test]
fn an_ending_pass_takes_no_answer() {
    use std::os::unix::fs::PermissionsExt as _;

    let pinned = Pinned {
        timeout_s: 60,
        min_interval_s: 0,
        retry_s: 3600,
    };
    let e = Env::new(pinned, &[Step::Wait("go".into())]);
    e.raise("reclaimable:/a@h@1", InboxKind::Reclaimable);
    let points = TestPoints::new(e.dirs.scope());
    let launcher = e.spawn_launcher(&points, &[ARBITER_OUTCOMES]);
    let pass = wait_for("the pass in last.json", WAIT, || e.last());
    e.session(&pass.id);
    // No item can be written: the launcher's outcome write fails.
    let inbox_dir = e.state.inbox_dir();
    let mode = |bits| std::fs::Permissions::from_mode(bits);
    std::fs::set_permissions(&inbox_dir, mode(0o555)).unwrap();
    let probe = inbox_dir.join("probe");
    let unwritable = std::fs::write(&probe, b"").is_err();
    e.signal("go");
    points.wait_reached(ARBITER_OUTCOMES, launcher.id(), WAIT);
    let answer = e.spawn_answer(&points, &pass.id, "reclaimable:/a@h@1");
    points.wait_reached(ARBITER_ANSWERING, answer.id(), WAIT);
    points.release(ARBITER_OUTCOMES);
    e.launcher_succeeded(launcher);
    let output = answer.wait_with_output().unwrap();
    std::fs::set_permissions(&inbox_dir, mode(0o755)).unwrap();
    // A user who writes there anyway (root) has nothing to check here.
    if !unwritable {
        std::fs::remove_file(&probe).unwrap();
        return;
    }
    let doc: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        (output.status.code(), error_code(&doc)),
        (Some(1), "not_assigned")
    );
    let a = e.item("reclaimable:/a@h@1");
    assert_eq!((a.status, a.resolution), (InboxStatus::Open, None));
    assert_eq!(a.arbiter.unwrap().outcome, None, "the outcome write failed");
    // The session answered nothing.
    assert_eq!(
        e.last().unwrap().outcome,
        Some(PassOutcome::Failed(inbox::NO_ANSWER.into()))
    );
}

/// A20: when a session exits 0 while `inbox/.lock` is held past the launcher's wait, nothing
/// checks its answers, and its pass ends `failed:unverified`, a failure in the run; a pass that
/// failed on its own keeps its outcome.
#[test]
fn unchecked_answers_fail_the_pass() {
    let pinned = Pinned {
        timeout_s: 60,
        min_interval_s: 0,
        retry_s: 0,
    };
    let e = Env::new(pinned, &[]);
    e.raise("reclaimable:/a@h@1", InboxKind::Reclaimable);
    let mut ended = Vec::new();
    for (go, steps) in [
        ("go1", vec![Step::Wait("go1".into())]),
        ("go2", vec![Step::Wait("go2".into()), Step::Exit(2)]),
    ] {
        e.configure(pinned, &steps);
        let points = TestPoints::new(e.dirs.scope());
        let launcher = e.spawn_launcher(&points, &[ARBITER_OUTCOMES]);
        let pass = wait_for("the pass in last.json", WAIT, || {
            e.last().filter(|last| last.ended_ms.is_none())
        });
        e.session(&pass.id);
        let held = state::lock(
            &e.state.inbox_lock(),
            LockMode::Exclusive,
            Instant::now() + WAIT,
        )
        .unwrap();
        e.signal(go);
        // The launcher records the end only once its wait for the lock has failed.
        points.wait_reached(ARBITER_OUTCOMES, launcher.id(), WAIT);
        drop(held);
        points.release(ARBITER_OUTCOMES);
        e.launcher_succeeded(launcher);
        let last = e.last().unwrap();
        assert_eq!(last.id, pass.id);
        let a = e.item("reclaimable:/a@h@1").arbiter.unwrap();
        assert_eq!((a.pass, a.outcome), (pass.id, None), "no item was written");
        ended.push((last.outcome.unwrap(), last.failures_in_a_row));
    }
    assert_eq!(
        ended,
        [
            (PassOutcome::Failed(inbox::UNVERIFIED.into()), 1),
            (PassOutcome::Failed("exit 2".into()), 2),
        ]
    );
}

/// A20: a session that exits 0 while one of its pass's items does not parse, or on Unix cannot
/// be read, has an answer nobody can check: its pass ends `failed:unverified` and continues the
/// run of failures, whose `arbiter_failing` the next launcher raises once the inbox reads
/// again (the inbox applies no change over an item it cannot read), instead of completing and
/// ending the run.
#[test]
fn an_unreadable_item_fails_the_pass() {
    let pinned = Pinned {
        timeout_s: 60,
        min_interval_s: 0,
        retry_s: 0,
    };
    let key = "reclaimable:/a@h@1";
    let e = Env::new(pinned, &[Step::Exit(2)]);
    e.raise(key, InboxKind::Reclaimable);
    for _ in 0..2 {
        let (code, doc) = e.arbiter_run();
        assert_eq!(code, Some(0), "{doc}");
    }
    let file = e.state.inbox_item(key, 0);
    // A pass whose session exits 0 once `spoil` made the item unreadable; `restore` undoes it
    // after the launcher exited.
    let pass_with = |spoil: &dyn Fn(), restore: &dyn Fn()| -> ArbiterPass {
        e.configure(pinned, &[Step::Wait("go".into())]);
        let before = e.last().unwrap().id;
        let points = TestPoints::new(e.dirs.scope());
        let mut launcher = e.spawn_launcher(&points, &[]);
        let pass = wait_for("the next pass in last.json", WAIT, || {
            e.last()
                .filter(|last| last.ended_ms.is_none() && last.id != before)
        });
        e.session(&pass.id);
        spoil();
        e.signal("go");
        let _ = launcher.wait();
        restore();
        std::fs::remove_file(e.sessions.join("go")).unwrap();
        e.last().unwrap()
    };
    let unverified = Some(PassOutcome::Failed(inbox::UNVERIFIED.into()));

    let saved = std::fs::read(&file).unwrap();
    let pass = pass_with(&|| std::fs::write(&file, b"{\"schema\":").unwrap(), &|| {
        std::fs::write(&file, &saved).unwrap();
    });
    assert_eq!(
        (pass.outcome, pass.failures_in_a_row),
        (unverified.clone(), 3)
    );
    e.configure(pinned, &[Step::Exit(2)]);
    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(0), "{doc}");
    assert_eq!(pass_of(&doc).failures_in_a_row, 4);
    let open = || {
        failing(&e)
            .into_iter()
            .filter(|item| item.ended_ms.is_none())
            .count()
    };
    assert_eq!(open(), 1, "the run of failures raised its alert");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let mode = |bits| std::fs::Permissions::from_mode(bits);
        let pass = pass_with(
            &|| std::fs::set_permissions(&file, mode(0o000)).unwrap(),
            &|| std::fs::set_permissions(&file, mode(0o644)).unwrap(),
        );
        // A user who reads it anyway (root) has nothing to check here.
        if pass.outcome != Some(PassOutcome::Completed) {
            assert_eq!((pass.outcome, pass.failures_in_a_row), (unverified, 5));
            assert_eq!(open(), 1, "the alert stays open");
        }
    }
}

/// A19: a launcher waits a few seconds for `arbiter/.lock`, which a trigger holds while it
/// settles the last pass. Released meanwhile, it runs its pass; still held, it prints the
/// running pass when `last.json` records one without an end, and otherwise fails as busy,
/// never printing the last pass, which ended, as its own.
#[test]
fn the_lock_is_waited_for_briefly() {
    let pinned = Pinned {
        timeout_s: 60,
        min_interval_s: 0,
        retry_s: 0,
    };
    let e = Env::new(pinned, &[Step::DeferAll]);
    e.raise("reclaimable:/a@h@1", InboxKind::Reclaimable);
    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(0), "{doc}");
    let first = pass_of(&doc);
    let held = state::lock(
        &e.state.arbiter_lock(),
        LockMode::Exclusive,
        Instant::now() + WAIT,
    )
    .unwrap();

    let start = Instant::now();
    let (code, doc) = e.arbiter_run();
    assert!(
        start.elapsed() >= Duration::from_secs(3),
        "the launcher waits for the lock"
    );
    assert_eq!((code, error_code(&doc)), (Some(1), "state_unavailable"));
    assert_eq!(e.last(), Some(first.clone()), "no pass started");

    let running = ArbiterPass {
        id: "running".into(),
        ended_ms: None,
        outcome: None,
        ..first.clone()
    };
    std::fs::write(
        e.state.arbiter_last(),
        serde_json::to_vec(&running).unwrap(),
    )
    .unwrap();
    let (code, doc) = e.arbiter_run();
    assert_eq!(code, Some(0), "{doc}");
    assert_eq!(pass_of(&doc), running, "the running pass");
    std::fs::write(e.state.arbiter_last(), serde_json::to_vec(&first).unwrap()).unwrap();

    let points = TestPoints::new(e.dirs.scope());
    let launcher = e.spawn_launcher(&points, &[]);
    std::thread::sleep(Duration::from_millis(500));
    drop(held);
    e.launcher_succeeded(launcher);
    let second = e.last().unwrap();
    assert_ne!(second.id, first.id, "a pass of its own");
    assert_eq!(second.outcome, Some(PassOutcome::Completed));
}
