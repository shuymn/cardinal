//! Session leases (`docs/architecture.md` section 9.8): the writers (A40), the expiry in the
//! inbox refresh (A43's pruning), and the readers: the lease and the quiet period in
//! `not_in_use` (A41), the report week's case refused (A42), and the quiescent control executed
//! (A43). The readers' tests set `linger_s` and `quiet_s` to seconds, since fixtures are fresh,
//! and wait them out where a test needs an idle worktree.
//!
//! Hooks run as children of a stand-in agent, this test binary hard-linked under the name its
//! test needs (`claude`, or another name for the control) and run in [`AGENT_MODE`]: it runs
//! one hook per request file the test writes, so one agent process can send `SessionStart`,
//! `SubagentStop`, and `SessionEnd` in turn, and stays alive until its scope ends. A hook run
//! directly by the test has the test process as its parent, which is not `claude`.

#![cfg(unix)]

mod disk_support;
mod hook_support;
mod live_support;
mod observe_common;
mod plan_support;
mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use std::time::{Duration, Instant};

use cardinal::config::Config;
use cardinal::contract::{
    LEASE_MAX_ENTRIES, LeaseAgent, LeaseEntry, LeaseEvent, Record, RecordKind, SessionLease,
};
use cardinal::state::{self, LockMode, StateDir};
use cardinal_test_support::fixture::Fixture;
use cardinal_test_support::process::wait_for;
use cardinal_test_support::{Child, Scope, bin_exe};
use hook_support::output_with_stdin;
use serde_json::{Value, json};
use support::cardinal::Dirs;
use support::helper::{self, Mode};
use support::testpoints::TestPoints;

/// The custom helper mode of the stand-in agent.
const AGENT_MODE: &str = "lease-agent";

#[test]
fn helper() {
    run_agent_if_requested();
    support::helper::run_if_requested();
}

/// One hook run a stand-in agent is asked for, in `<dir>/run.<n>.json`.
#[derive(serde::Serialize, serde::Deserialize)]
struct HookRun {
    event: String,
    input: Value,
    /// Through `sh -c`, as Claude Code runs hooks.
    via_shell: bool,
    /// Through `sh -c` that puts the hook in the background in a group of its own and exits,
    /// writing the hook's PID to `<dir>/hook.<n>.pid`: the hook is re-parented.
    orphaned: bool,
    env: Vec<(String, String)>,
    cardinal: PathBuf,
}

/// Runs the stand-in agent a helper process was started for: for n = 0, 1, …, waits for
/// `<dir>/run.<n>.json`, runs its hook with the input on stdin, and writes the exit status to
/// `<dir>/done.<n>`. Exits once no request comes for 60 s.
fn run_agent_if_requested() {
    let Some(dir) = helper::custom_request(AGENT_MODE) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let scope = Scope::new();
    for n in 0.. {
        let request = dir.join(format!("run.{n}.json"));
        let start = Instant::now();
        while !request.exists() {
            if start.elapsed() > Duration::from_secs(60) {
                std::process::exit(0);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let run: HookRun =
            serde_json::from_slice(&std::fs::read(&request).unwrap()).expect("a hook run");
        let input = dir.join(format!("input.{n}.json"));
        std::fs::write(&input, run.input.to_string()).unwrap();
        let cardinal = run.cardinal.display().to_string();
        let mut hook = if run.orphaned {
            // bash without startup files, leading a session of its own, gives the job its own
            // group with `set -m` (dash refuses job control without a terminal), so the hook
            // outlives the shell's group, which is cleaned up when the shell exits.
            let mut shell = scope.command("bash", scope.root());
            shell
                .args([
                    "--norc",
                    "--noprofile",
                    "-c",
                    &format!(
                        "set -m; '{cardinal}' hook claude {} < '{}' & echo $! > '{}'",
                        run.event,
                        input.display(),
                        dir.join(format!("hook.{n}.pid")).display()
                    ),
                ])
                .new_session();
            shell
        } else if run.via_shell {
            // `; true` keeps the shell alive as the hook's parent.
            scope.shell(
                &format!(
                    "'{cardinal}' hook claude {} < '{}'; true",
                    run.event,
                    input.display()
                ),
                scope.root(),
            )
        } else {
            let mut hook = scope.command(&run.cardinal, scope.root());
            hook.args(["hook", "claude", &run.event])
                .stdin(std::fs::File::open(&input).unwrap());
            hook
        };
        for (key, value) in &run.env {
            hook.env(key, value);
        }
        let status = hook
            .stdout(std::process::Stdio::null())
            .status()
            .expect("run the hook");
        let tmp = dir.join(format!("done.{n}.tmp"));
        std::fs::write(&tmp, status.code().unwrap_or(-1).to_string()).unwrap();
        std::fs::rename(&tmp, dir.join(format!("done.{n}"))).unwrap();
    }
}

/// A stand-in agent process named `name`.
struct Agent {
    child: Child,
    dir: PathBuf,
    next: usize,
}

impl Agent {
    /// Starts this test binary, hard-linked (or copied) as `name`, as a stand-in agent in
    /// `scope`.
    fn start(scope: &Scope, name: &str) -> Self {
        static STARTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let bin = scope.path(&format!("bin-{name}"));
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join(name);
        let test_binary = std::env::current_exe().unwrap();
        if !exe.exists() && std::fs::hard_link(&test_binary, &exe).is_err() {
            std::fs::copy(&test_binary, &exe).unwrap();
        }
        let n = STARTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = scope.path(&format!("agent-{name}-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        let mode = Mode::Custom {
            name: AGENT_MODE,
            arg: dir.display().to_string(),
        };
        let child = scope
            .command(&exe, scope.root())
            .args(helper::args(&mode))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("start the stand-in agent");
        Self {
            child,
            dir,
            next: 0,
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Asks for one hook run and returns its number.
    fn request(
        &mut self,
        dirs: &Dirs,
        event: &str,
        input: &Value,
        how: How,
        env: &[(String, String)],
    ) -> usize {
        let n = self.next;
        self.next += 1;
        let mut all_env = vec![
            (
                "CARDINAL_STATE_DIR".to_owned(),
                dirs.state().display().to_string(),
            ),
            (
                "CARDINAL_CONFIG_DIR".to_owned(),
                dirs.config().display().to_string(),
            ),
        ];
        all_env.extend_from_slice(env);
        let run = HookRun {
            event: event.to_owned(),
            input: input.clone(),
            via_shell: how == How::Shell,
            orphaned: how == How::Orphaned,
            env: all_env,
            cardinal: bin_exe("cardinal"),
        };
        let tmp = self.dir.join(format!("run.{n}.tmp"));
        std::fs::write(&tmp, serde_json::to_vec(&run).unwrap()).unwrap();
        std::fs::rename(&tmp, self.dir.join(format!("run.{n}.json"))).unwrap();
        n
    }

    /// Waits for run `n` to end; returns its exit status.
    fn wait(&self, n: usize) -> String {
        let done = self.dir.join(format!("done.{n}"));
        wait_for("the hook run", Duration::from_secs(60), || {
            std::fs::read_to_string(&done).ok()
        })
    }

    /// Runs `event` with `input` as this agent's child and waits for it; asserts exit 0.
    fn run(&mut self, dirs: &Dirs, event: &str, input: &Value, how: How) {
        let n = self.request(dirs, event, input, how, &[]);
        assert_eq!(self.wait(n), "0", "{event} exits 0");
    }

    /// The generation the hook should record for this agent.
    fn generation(&self) -> LeaseAgent {
        LeaseAgent {
            pid: self.pid(),
            start_s: cardinal::os::start_time_s(self.pid()).unwrap(),
            start_us: cardinal::os::start_us(self.pid()),
            boot_id: cardinal::os::boot_id().unwrap(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum How {
    Direct,
    Shell,
    Orphaned,
}

/// A hook input of `event` for `session` in `cwd`.
fn input(event: &str, session: &str, cwd: &Path, transcript: Option<&Path>) -> Value {
    json!({
        "session_id": session,
        "transcript_path": transcript,
        "cwd": cwd,
        "permission_mode": "default",
        "hook_event_name": event,
        "source": "startup",
        "reason": "other",
    })
}

/// `host.toml` with `extra` and the disk lines of the hook tests.
fn configure(dirs: &Dirs, extra: &str) {
    hook_support::configure(dirs, &hook_support::host(hook_support::RUNNER, extra), &[]);
}

/// Runs `cardinal hook claude <event>` with `input` as the test's child (its parent is not
/// `claude`); asserts exit 0 and no output.
fn run_hook(dirs: &Dirs, event: &str, input: &Value) {
    let output = output_with_stdin(
        dirs.cmd().args(["hook", "claude", event]),
        input.to_string().as_bytes(),
    );
    assert_eq!(output.status.code(), Some(0), "{event}");
    assert!(output.stdout.is_empty(), "{event}");
}

fn state(dirs: &Dirs) -> StateDir {
    StateDir::open(dirs.state()).unwrap()
}

fn key(root: &Path) -> String {
    state::worktree_key(&dunce::canonicalize(root).unwrap())
}

fn lease_path(dirs: &Dirs, root: &Path) -> PathBuf {
    state(dirs).lease(&key(root))
}

/// The lease of `root`, if any.
fn lease(dirs: &Dirs, root: &Path) -> Option<SessionLease> {
    let bytes = std::fs::read(lease_path(dirs, root)).ok()?;
    Some(serde_json::from_slice(&bytes).expect("a session lease"))
}

fn entry<'a>(lease: &'a SessionLease, session: &str) -> &'a LeaseEntry {
    lease
        .sessions
        .iter()
        .find(|e| e.session_id == session)
        .unwrap_or_else(|| panic!("no entry {session} in {lease:#?}"))
}

/// Writes the lease of `root`, marked `overflow` when it passes the cap, as a writer marks it.
fn write_lease(dirs: &Dirs, root: &Path, sessions: Vec<LeaseEntry>) {
    let overflow = sessions.len() > LEASE_MAX_ENTRIES;
    write_lease_marked(dirs, root, sessions, overflow);
}

fn write_lease_marked(dirs: &Dirs, root: &Path, sessions: Vec<LeaseEntry>, overflow: bool) {
    let path = lease_path(dirs, root);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let lease = SessionLease {
        schema: cardinal::contract::SessionLeaseSchema,
        worktree: dunce::canonicalize(root).unwrap().display().to_string(),
        overflow,
        sessions,
    };
    std::fs::write(path, serde_json::to_vec(&lease).unwrap()).unwrap();
}

fn records(dirs: &Dirs) -> Vec<Record> {
    cardinal::record::read_all(&state(dirs)).unwrap()
}

fn plain(
    session: &str,
    agent: Option<LeaseAgent>,
    last_event_ms: u64,
    ended_ms: Option<u64>,
) -> LeaseEntry {
    LeaseEntry {
        session_id: session.to_owned(),
        agent,
        uncertain: false,
        transcript_path: None,
        last_event: LeaseEvent::SessionEnd,
        last_event_ms,
        ended_ms,
    }
}

/// A repository with a commit, a linked worktree nested inside it, and a linked worktree
/// beside it.
struct Repo {
    fixture: Fixture,
    main: PathBuf,
    nested: PathBuf,
    beside: PathBuf,
}

fn repo() -> Repo {
    let fixture = Fixture::new();
    let main = fixture.init("main");
    fixture.commit_file(&main, "src/lib.rs", "x", "init");
    let nested = fixture.worktree_add(&main, "main/.wt/nested", "nested");
    let beside = fixture.worktree_add(&main, "beside", "beside");
    Repo {
        fixture,
        main,
        nested,
        beside,
    }
}

/// A40: each event upserts the session's entry in the worktree of its `cwd`, a subdirectory
/// mapped to the worktree's root, and in each worktree around a nested one; outside any
/// worktree nothing is written.
#[test]
fn hooks_write_entries() {
    let dirs = Dirs::new();
    configure(&dirs, "");
    let repo = repo();
    let deep = repo.nested.join("src/deep");
    std::fs::create_dir_all(&deep).unwrap();
    let transcript = repo.fixture.path("t/s1.jsonl");

    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "s1", &deep, Some(&transcript)),
    );
    for root in [&repo.nested, &repo.main] {
        let lease = lease(&dirs, root).expect("a lease of each enclosing worktree");
        assert_eq!(
            lease.worktree,
            dunce::canonicalize(root).unwrap().display().to_string()
        );
        let e = entry(&lease, "s1");
        assert_eq!(e.last_event, LeaseEvent::SessionStart);
        assert_eq!(
            e.transcript_path.as_deref(),
            Some(transcript.to_str().unwrap())
        );
        assert_eq!(e.agent, None, "the test process is not claude");
        assert_eq!(e.ended_ms, None);
    }
    assert!(
        lease(&dirs, &repo.beside).is_none(),
        "the worktree beside is not touched"
    );

    run_hook(
        &dirs,
        "subagent-stop",
        &input("SubagentStop", "s1", &repo.beside, None),
    );
    let e = entry(&lease(&dirs, &repo.beside).unwrap(), "s1").clone();
    assert_eq!(e.last_event, LeaseEvent::SubagentStop);
    run_hook(
        &dirs,
        "session-end",
        &input("SessionEnd", "s1", &deep, None),
    );
    let lease_now = lease(&dirs, &repo.nested).unwrap();
    let e = entry(&lease_now, "s1");
    assert_eq!(e.last_event, LeaseEvent::SessionEnd);
    assert_eq!(
        e.transcript_path.as_deref(),
        Some(transcript.to_str().unwrap()),
        "kept"
    );
    assert_eq!(lease_now.sessions.len(), 1);

    // Control: a cwd outside any worktree writes nothing.
    let outside = repo.fixture.path("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let count = || {
        std::fs::read_dir(state(&dirs).leases_dir())
            .unwrap()
            .count()
    };
    let before = count();
    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "s2", &outside, None),
    );
    assert_eq!(count(), before);
    let errors: Vec<_> = records(&dirs)
        .into_iter()
        .filter(|r| r.kind == RecordKind::HookError)
        .collect();
    assert!(errors.is_empty(), "{errors:#?}");
}

/// A40: a `cwd` inside a submodule maps to the submodule and to its superproject.
#[test]
fn submodule_cwd_holds_the_superproject() {
    let dirs = Dirs::new();
    configure(&dirs, "");
    let fixture = Fixture::new();
    let superproject = fixture.init("super");
    fixture.commit_file(&superproject, "README", "x", "init");
    // A submodule as git checks one out: its git dir in the superproject's `modules/`, named by
    // a `.git` file.
    let sub = fixture.init("super/sub");
    fixture.commit_file(&sub, "lib.rs", "x", "init");
    let modules = superproject.join(".git/modules");
    std::fs::create_dir_all(&modules).unwrap();
    std::fs::rename(sub.join(".git"), modules.join("sub")).unwrap();
    std::fs::write(sub.join(".git"), "gitdir: ../.git/modules/sub\n").unwrap();

    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "s", &sub, None),
    );
    entry(&lease(&dirs, &sub).unwrap(), "s");
    entry(&lease(&dirs, &superproject).unwrap(), "s");
    // Control: the superproject's own cwd does not reach into the submodule.
    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "t", &superproject, None),
    );
    assert!(
        lease(&dirs, &sub)
            .unwrap()
            .sessions
            .iter()
            .all(|e| e.session_id != "t")
    );
}

/// A40: the agent is the hook's parent, or its parent's parent through a shell, when it is
/// `claude`; `SessionEnd` from the same process ends the entry.
#[test]
fn agent_is_the_claude_parent() {
    let dirs = Dirs::new();
    configure(&dirs, "");
    let repo = repo();
    let mut agent = Agent::start(dirs.scope(), "claude");
    agent.run(
        &dirs,
        "session-start",
        &input("SessionStart", "direct", &repo.main, None),
        How::Direct,
    );
    agent.run(
        &dirs,
        "session-start",
        &input("SessionStart", "shell", &repo.main, None),
        How::Shell,
    );
    let lease_now = lease(&dirs, &repo.main).unwrap();
    for session in ["direct", "shell"] {
        assert_eq!(
            entry(&lease_now, session).agent.as_ref(),
            Some(&agent.generation()),
            "{session}"
        );
    }
    agent.run(
        &dirs,
        "session-end",
        &input("SessionEnd", "shell", &repo.main, None),
        How::Shell,
    );
    assert!(
        entry(&lease(&dirs, &repo.main).unwrap(), "shell")
            .ended_ms
            .is_some()
    );
}

/// A40: a parent that is not `claude` is no agent; the same binary named `claude` is (control).
#[test]
fn no_agent_that_is_not_claude() {
    let dirs = Dirs::new();
    configure(&dirs, "");
    let repo = repo();
    let mut other = Agent::start(dirs.scope(), "node");
    let mut claude = Agent::start(dirs.scope(), "claude");
    other.run(
        &dirs,
        "session-start",
        &input("SessionStart", "other", &repo.main, None),
        How::Shell,
    );
    claude.run(
        &dirs,
        "session-start",
        &input("SessionStart", "claude", &repo.main, None),
        How::Shell,
    );
    let lease_now = lease(&dirs, &repo.main).unwrap();
    assert_eq!(entry(&lease_now, "other").agent, None);
    assert_eq!(
        entry(&lease_now, "claude").agent.as_ref(),
        Some(&claude.generation())
    );
}

/// A40: a hook whose shell exited before it looked for its agent was re-parented, and records
/// none; the same run with the shell alive records `claude` (control).
#[test]
fn no_agent_past_a_reparented_hook() {
    let dirs = Dirs::new();
    configure(&dirs, "");
    let repo = repo();
    let points = TestPoints::new(dirs.scope());
    let env = vec![
        (
            "CARDINAL_TEST_DIR".to_owned(),
            points.dir().display().to_string(),
        ),
        (
            "CARDINAL_TEST_PAUSE".to_owned(),
            cardinal::testpoint::HOOK_LEASE_AGENT.to_owned(),
        ),
    ];
    let mut agent = Agent::start(dirs.scope(), "claude");
    let n = agent.request(
        &dirs,
        "session-start",
        &input("SessionStart", "orphan", &repo.main, None),
        How::Orphaned,
        &env,
    );
    assert_eq!(agent.wait(n), "0", "the shell exits at once");
    let hook: u32 = wait_for("the hook's PID", Duration::from_secs(10), || {
        std::fs::read_to_string(agent.dir.join(format!("hook.{n}.pid")))
            .ok()?
            .trim()
            .parse()
            .ok()
    });
    points.wait_reached(
        cardinal::testpoint::HOOK_LEASE_AGENT,
        hook,
        Duration::from_secs(30),
    );
    let parent = cardinal::os::parent_pid(hook);
    assert_ne!(parent, Some(agent.pid()));
    points.release(cardinal::testpoint::HOOK_LEASE_AGENT);
    let found = wait_for("the orphan's entry", Duration::from_secs(30), || {
        lease(&dirs, &repo.main).filter(|l| l.sessions.iter().any(|e| e.session_id == "orphan"))
    });
    assert_eq!(
        entry(&found, "orphan").agent,
        None,
        "re-parented to {parent:?}"
    );

    agent.run(
        &dirs,
        "session-start",
        &input("SessionStart", "control", &repo.main, None),
        How::Shell,
    );
    assert_eq!(
        entry(&lease(&dirs, &repo.main).unwrap(), "control")
            .agent
            .as_ref(),
        Some(&agent.generation())
    );
}

/// A40: `SessionStart` from a newer process resumes an ended entry: its generation is set and
/// `ended_ms` cleared.
#[test]
fn resume_clears_the_end() {
    let dirs = Dirs::new();
    configure(&dirs, "");
    let repo = repo();
    let mut agent = Agent::start(dirs.scope(), "claude");
    let boot = cardinal::os::boot_id().unwrap();
    let older = LeaseAgent {
        pid: 1,
        start_s: 1,
        start_us: Some(1_000_000),
        boot_id: boot,
    };
    let now = state::now_ms();
    write_lease(
        &dirs,
        &repo.main,
        vec![plain("s", Some(older), now, Some(now))],
    );
    agent.run(
        &dirs,
        "session-start",
        &input("SessionStart", "s", &repo.main, None),
        How::Shell,
    );
    let e = entry(&lease(&dirs, &repo.main).unwrap(), "s").clone();
    assert_eq!(
        (e.agent.as_ref(), e.ended_ms),
        (Some(&agent.generation()), None)
    );
}

/// A generation started 100 s after `agent`, in the same boot.
fn later(agent: &LeaseAgent) -> LeaseAgent {
    LeaseAgent {
        start_s: agent.start_s + 100,
        start_us: agent.start_us.map(|us| us + 100_000_000),
        ..agent.clone()
    }
}

/// This test process's generation.
fn me() -> LeaseAgent {
    let pid = std::process::id();
    LeaseAgent {
        pid,
        start_s: cardinal::os::start_time_s(pid).unwrap(),
        start_us: cardinal::os::start_us(pid),
        boot_id: cardinal::os::boot_id().unwrap(),
    }
}

/// A40: a late `SessionEnd` of an older process ends nothing; the current process's own end
/// does (control).
#[test]
fn a_late_end_of_an_older_turn_ends_nothing() {
    let dirs = Dirs::new();
    configure(&dirs, "");
    let repo = repo();
    let mut old = Agent::start(dirs.scope(), "claude");
    old.run(
        &dirs,
        "session-start",
        &input("SessionStart", "s", &repo.main, None),
        How::Shell,
    );
    // A newer process of the session: a later start time in the same boot.
    let newer = later(&old.generation());
    let now = state::now_ms();
    write_lease(
        &dirs,
        &repo.main,
        vec![plain("s", Some(newer.clone()), now, None)],
    );
    old.run(
        &dirs,
        "session-end",
        &input("SessionEnd", "s", &repo.main, None),
        How::Shell,
    );
    let e = entry(&lease(&dirs, &repo.main).unwrap(), "s").clone();
    assert_eq!((e.agent.as_ref(), e.ended_ms), (Some(&newer), None));
    assert_eq!(e.last_event, LeaseEvent::SessionEnd);

    write_lease(
        &dirs,
        &repo.main,
        vec![plain("s", Some(old.generation()), now, None)],
    );
    old.run(
        &dirs,
        "session-end",
        &input("SessionEnd", "s", &repo.main, None),
        How::Shell,
    );
    assert!(
        entry(&lease(&dirs, &repo.main).unwrap(), "s")
            .ended_ms
            .is_some()
    );
}

/// A40: a delayed `SessionStart` of an older process changes neither the generation nor an
/// end; it only updates the entry's time.
#[test]
fn a_late_start_of_an_older_process_changes_nothing() {
    let dirs = Dirs::new();
    configure(&dirs, "");
    let repo = repo();
    let mut old = Agent::start(dirs.scope(), "claude");
    let newer = later(&old.generation());
    let then = state::now_ms() - 1000;
    write_lease(
        &dirs,
        &repo.main,
        vec![plain("s", Some(newer.clone()), then, Some(then))],
    );
    old.run(
        &dirs,
        "session-start",
        &input("SessionStart", "s", &repo.main, None),
        How::Shell,
    );
    let e = entry(&lease(&dirs, &repo.main).unwrap(), "s").clone();
    assert_eq!((e.agent.as_ref(), e.ended_ms), (Some(&newer), Some(then)));
    assert!(e.last_event_ms > then);
}

/// A40: an entry `SubagentStop` creates has no agent and has ended, so it only lingers; the
/// same agent's `SessionStart` records it (control).
#[test]
fn subagent_stop_only_lingers() {
    let dirs = Dirs::new();
    configure(&dirs, "");
    let repo = repo();
    let mut agent = Agent::start(dirs.scope(), "claude");
    agent.run(
        &dirs,
        "subagent-stop",
        &input("SubagentStop", "sub", &repo.main, None),
        How::Shell,
    );
    agent.run(
        &dirs,
        "session-start",
        &input("SessionStart", "start", &repo.main, None),
        How::Shell,
    );
    let lease_now = lease(&dirs, &repo.main).unwrap();
    let sub = entry(&lease_now, "sub");
    assert_eq!((sub.agent.as_ref(), sub.ended_ms.is_some()), (None, true));
    assert_eq!(
        entry(&lease_now, "start").agent.as_ref(),
        Some(&agent.generation())
    );
}

/// A40: with the lease lock held, each hook exits 0 by its deadline, writes nothing, and records
/// `hook_error` (`lease_lock_timeout`); the event's own record still follows.
#[test]
fn locked_file_never_blocks_the_hook() {
    let dirs = Dirs::new();
    configure(&dirs, "");
    let repo = repo();
    let state = state(&dirs);
    std::fs::create_dir_all(state.leases_dir()).unwrap();
    let lock = state::lock(
        &state.lease_lock(&key(&repo.main)),
        LockMode::Exclusive,
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();
    for (event, name, wait_ms) in [
        ("session-start", "SessionStart", 1000),
        ("subagent-stop", "SubagentStop", 1000),
        ("session-end", "SessionEnd", 250),
    ] {
        let before = records(&dirs).len();
        run_hook(&dirs, event, &input(name, "s", &repo.main, None));
        let new: Vec<Record> = records(&dirs).split_off(before);
        let error = new
            .iter()
            .find(|r| r.kind == RecordKind::HookError)
            .unwrap_or_else(|| panic!("{event}: {new:#?}"));
        assert_eq!(error.data["reason"], "lease_lock_timeout", "{event}");
        let waited = error.data["waited_ms"].as_u64().unwrap();
        assert!(
            (wait_ms..wait_ms + 750).contains(&waited),
            "{event} waited {waited} ms"
        );
        assert_eq!(
            new.len(),
            if event == "session-start" { 1 } else { 2 },
            "{event}: {new:#?}"
        );
    }
    assert!(lease(&dirs, &repo.main).is_none(), "nothing written");
    drop(lock);
    // Control: with the lock free the same hook writes its entry and no error.
    let before = records(&dirs).len();
    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "s", &repo.main, None),
    );
    entry(&lease(&dirs, &repo.main).unwrap(), "s");
    assert_eq!(records(&dirs).len(), before);
}

/// A40: a write that fails (`ENOSPC`) leaves the file as it was and records `hook_error`
/// (`lease_write_failed`).
#[test]
fn failed_write_leaves_the_file() {
    let dirs = Dirs::new();
    configure(&dirs, "");
    let repo = repo();
    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "first", &repo.main, None),
    );
    let points = TestPoints::new(dirs.scope());
    std::fs::write(
        points.dir().join("write-failures"),
        "lease\tENOSPC\talways\n",
    )
    .unwrap();
    let output = output_with_stdin(
        dirs.cmd()
            .env("CARDINAL_TEST_DIR", points.dir())
            .args(["hook", "claude", "session-start"]),
        input("SessionStart", "second", &repo.main, None)
            .to_string()
            .as_bytes(),
    );
    assert_eq!(output.status.code(), Some(0));
    let lease_now = lease(&dirs, &repo.main).unwrap();
    assert_eq!(lease_now.sessions.len(), 1, "{lease_now:#?}");
    entry(&lease_now, "first");
    let error = records(&dirs)
        .into_iter()
        .find(|r| r.kind == RecordKind::HookError)
        .unwrap();
    assert_eq!(error.data["reason"], "lease_write_failed");
}

/// A40: over 1,024 entries a writer folds lingering entries, never a live one, and marks the
/// file `overflow` when live entries alone pass the cap.
#[test]
fn fold_keeps_live_entries() {
    let dirs = Dirs::new();
    configure(&dirs, "");
    let repo = repo();
    // The test process is alive with this generation.
    let me = me();
    let now = state::now_ms();
    let max = cardinal::contract::LEASE_MAX_ENTRIES;
    let mut sessions: Vec<LeaseEntry> = (0..max - 10)
        .map(|i| plain(&format!("live{i}"), Some(me.clone()), now, None))
        .collect();
    sessions.extend((0..20).map(|i| {
        plain(
            &format!("linger{i}"),
            None,
            now - 1000 + i,
            Some(now - 1000 + i),
        )
    }));
    write_lease(&dirs, &repo.main, sessions);
    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "new", &repo.main, None),
    );
    let lease_now = lease(&dirs, &repo.main).unwrap();
    assert_eq!(lease_now.sessions.len(), max);
    assert!(!lease_now.overflow);
    assert_eq!(
        lease_now
            .sessions
            .iter()
            .filter(|e| e.session_id.starts_with("live"))
            .count(),
        max - 10
    );
    entry(&lease_now, "new");
    // The oldest twelve lingering entries became one; the youngest eight stay.
    let folded = entry(&lease_now, cardinal::state::lease::FOLDED);
    assert_eq!(folded.last_event_ms, now - 1000 + 11);
    for i in 12..20 {
        entry(&lease_now, &format!("linger{i}"));
    }

    // Only live entries over the cap: nothing folds, and the file overflows.
    let sessions: Vec<LeaseEntry> = (0..=max)
        .map(|i| plain(&format!("live{i}"), Some(me.clone()), now, None))
        .collect();
    write_lease(&dirs, &repo.main, sessions);
    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "new", &repo.main, None),
    );
    let lease_now = lease(&dirs, &repo.main).unwrap();
    assert_eq!(lease_now.sessions.len(), max + 2);
    assert!(lease_now.overflow);
}

/// A40: a corrupt file is replaced with the writer's entry and an `uncertain` entry lingering
/// from the corrupt file's mtime, with `lease_corrupt` recorded.
#[test]
fn corrupt_file_is_uncertain() {
    let dirs = Dirs::new();
    configure(&dirs, "");
    let repo = repo();
    let path = lease_path(&dirs, &repo.main);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{\"schema\": \"cardinal.session-lease.v1\", \"sess").unwrap();
    let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
    let mtime_ms = u64::try_from(
        mtime
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "s", &repo.main, None),
    );
    let lease_now = lease(&dirs, &repo.main).unwrap();
    assert_eq!(lease_now.sessions.len(), 2);
    entry(&lease_now, "s");
    let uncertain = lease_now.sessions.iter().find(|e| e.uncertain).unwrap();
    assert_eq!(uncertain.last_event_ms, mtime_ms);
    let corrupt = records(&dirs)
        .into_iter()
        .find(|r| r.kind == RecordKind::LeaseCorrupt)
        .unwrap();
    assert_eq!(corrupt.data["mtime_ms"], json!(mtime_ms));
    assert_eq!(corrupt.session_id.as_deref(), Some("s"));
}

/// No lease from `PreToolUse`.
#[test]
fn pre_tool_use_writes_no_lease() {
    let dirs = Dirs::new();
    configure(&dirs, "");
    let repo = repo();
    let output = output_with_stdin(
        dirs.cmd().args(["hook", "claude", "pre-tool-use"]),
        hook_support::input("echo hi", &repo.main)
            .to_string()
            .as_bytes(),
    );
    assert_eq!(output.status.code(), Some(0));
    assert!(!state(&dirs).leases_dir().exists());
    // Control: the session hook in the same worktree writes one.
    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "s", &repo.main, None),
    );
    assert!(lease(&dirs, &repo.main).is_some());
}

/// Refreshes the inbox of `dirs`, which expires leases.
fn refresh(dirs: &Dirs) {
    let config = Config::load(dirs.config()).unwrap();
    cardinal::inbox::list(&config, &state(dirs), None, true).unwrap();
}

/// A43's pruning: the refresh drops expired entries and keeps live and lingering ones, removes
/// a file left empty while its lock file stays, and keeps an entry whose transcript cannot be
/// read; raising `linger_s` afterwards revives nothing.
#[test]
fn expired_entries_pruned() {
    let dirs = Dirs::new();
    configure(&dirs, "[sessions]\nlinger_s = 2\n");
    let repo = repo();
    let me = me();
    // Another process that took the PID started later, as finely as either start is read.
    let gone = LeaseAgent {
        start_s: me.start_s + 1,
        start_us: me.start_us.map(|us| us + 1_000_000),
        ..me.clone()
    };
    let other_boot = LeaseAgent {
        boot_id: "another-boot".into(),
        ..me.clone()
    };
    let now = state::now_ms();
    let old = now - 10_000;
    let locked_dir = repo.fixture.path("locked");
    std::fs::create_dir_all(&locked_dir).unwrap();
    let unreadable = locked_dir.join("t.jsonl");
    write_lease(
        &dirs,
        &repo.main,
        vec![
            plain("expired", None, old, Some(old)),
            plain("reused-pid", Some(gone), old, None),
            plain("earlier-boot", Some(other_boot), old, None),
            plain("live", Some(me.clone()), old, None),
            plain("lingering", None, now, Some(now)),
            LeaseEntry {
                transcript_path: Some(unreadable.display().to_string()),
                ..plain("unknown", None, old, Some(old))
            },
        ],
    );
    write_lease(
        &dirs,
        &repo.beside,
        vec![plain("expired", None, old, Some(old))],
    );
    std::fs::set_permissions(&locked_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    refresh(&dirs);
    std::fs::set_permissions(&locked_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    let kept: Vec<String> = lease(&dirs, &repo.main)
        .unwrap()
        .sessions
        .into_iter()
        .map(|e| e.session_id)
        .collect();
    assert_eq!(kept, ["live", "lingering", "unknown"]);
    assert!(
        lease(&dirs, &repo.beside).is_none(),
        "a file left empty is removed"
    );
    assert!(
        state(&dirs).lease_lock(&key(&repo.beside)).exists(),
        "lock files stay"
    );

    // Raising linger_s revives nothing that was dropped.
    configure(&dirs, "[sessions]\nlinger_s = 7200\n");
    refresh(&dirs);
    let kept: Vec<String> = lease(&dirs, &repo.main)
        .unwrap()
        .sessions
        .into_iter()
        .map(|e| e.session_id)
        .collect();
    assert_eq!(kept, ["live", "lingering", "unknown"]);
    assert!(lease(&dirs, &repo.beside).is_none());
}

/// A43: an `uncertain` entry is dropped `linger_s` after the corrupt file's mtime, and a corrupt
/// file older than `linger_s` is removed with `lease_corrupt_removed`; a fresh corrupt file and
/// a held lock keep their files (controls).
#[test]
fn uncertain_entry_dropped_after_the_linger() {
    let dirs = Dirs::new();
    configure(&dirs, "[sessions]\nlinger_s = 2\n");
    let repo = repo();
    let now = state::now_ms();
    let uncertain = |ms: u64| LeaseEntry {
        uncertain: true,
        ..plain("cardinal:uncertain", None, ms, Some(ms))
    };
    write_lease(
        &dirs,
        &repo.main,
        vec![uncertain(now - 10_000), plain("s", None, now, Some(now))],
    );
    write_lease(&dirs, &repo.nested, vec![uncertain(now)]);
    // A corrupt file, aged past the linger, and a fresh one.
    let stale = lease_path(&dirs, &repo.beside);
    std::fs::write(&stale, b"not json").unwrap();
    let past = std::time::SystemTime::now() - Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(past)
        .unwrap();
    let fixture2 = Fixture::new();
    let fresh_root = fixture2.init("fresh");
    let fresh = lease_path(&dirs, &fresh_root);
    std::fs::write(&fresh, b"not json").unwrap();
    // A held lock keeps an expired file as it is.
    let held_root = fixture2.init("held");
    write_lease(
        &dirs,
        &held_root,
        vec![plain("expired", None, now - 10_000, Some(now - 10_000))],
    );
    let held = state::lock(
        &state(&dirs).lease_lock(&key(&held_root)),
        LockMode::Exclusive,
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();

    refresh(&dirs);
    drop(held);
    let kept: Vec<String> = lease(&dirs, &repo.main)
        .unwrap()
        .sessions
        .into_iter()
        .map(|e| e.session_id)
        .collect();
    assert_eq!(kept, ["s"]);
    assert!(
        lease(&dirs, &repo.nested).unwrap().sessions[0].uncertain,
        "still within the linger"
    );
    assert!(!stale.exists());
    assert!(fresh.exists());
    assert_eq!(lease(&dirs, &held_root).unwrap().sessions.len(), 1);
    let removed: Vec<Record> = records(&dirs)
        .into_iter()
        .filter(|r| r.kind == RecordKind::LeaseCorruptRemoved)
        .collect();
    assert_eq!(removed.len(), 1, "{removed:#?}");
    assert_eq!(removed[0].data["path"], json!(stale));
}

/// `cardinal inbox` on `dirs`, which refreshes.
fn helper_refresh(dirs: &Dirs) -> cardinal_test_support::Command {
    let mut command = dirs.cmd();
    command.arg("inbox").stdout(std::process::Stdio::null());
    command
}

/// Sets the mtime of `path` to `ago` before now.
fn age(path: &Path, ago: Duration) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() - ago)
        .unwrap();
}

fn of_kind(dirs: &Dirs, kind: RecordKind) -> Vec<Record> {
    records(dirs)
        .into_iter()
        .filter(|r| r.kind == kind)
        .collect()
}

/// Section 9.8, "Expiry": the refresh reads, locks, and removes only a regular file named by a
/// worktree key; another `.json` file there and a key-named symbolic link stay, with no lock
/// file made for them, where a key-named corrupt regular file is removed (control).
#[test]
fn expiry_touches_only_lease_files() {
    let dirs = Dirs::new();
    configure(&dirs, "[sessions]\nlinger_s = 2\n");
    let repo = repo();
    let state = state(&dirs);
    let leases = state.leases_dir();
    std::fs::create_dir_all(&leases).unwrap();
    let old = Duration::from_secs(60);
    let export = leases.join("export.json");
    std::fs::write(&export, b"not json").unwrap();
    age(&export, old);
    let target = repo.fixture.path("target.json");
    std::fs::write(&target, b"not json").unwrap();
    age(&target, old);
    let link = lease_path(&dirs, &repo.main);
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let corrupt = lease_path(&dirs, &repo.beside);
    std::fs::write(&corrupt, b"not json").unwrap();
    age(&corrupt, old);

    refresh(&dirs);
    assert!(export.exists(), "another file stays");
    assert!(
        !leases.join("export.lock").exists(),
        "and gets no lock file"
    );
    assert!(link.symlink_metadata().is_ok(), "a link stays");
    assert!(target.exists());
    assert!(!state.lease_lock(&key(&repo.main)).exists());
    assert!(
        !corrupt.exists(),
        "control: a stale corrupt lease is removed"
    );
    let removed = of_kind(&dirs, RecordKind::LeaseCorruptRemoved);
    assert_eq!(removed.len(), 1, "{removed:#?}");
    assert_eq!(removed[0].data["path"], json!(corrupt));
}

/// Section 9.8: the refresh reads each agent under the lease lock, never from the table it took
/// before. An agent that registers while the refresh is paused after its table, in a file at
/// the cap, stays live after the refresh and is not folded.
#[test]
fn refresh_reads_agents_under_the_lock() {
    use cardinal::testpoint::INBOX_OBSERVED;

    let dirs = Dirs::new();
    configure(&dirs, "");
    let repo = repo();
    let me = me();
    let now = state::now_ms();
    let mut sessions: Vec<LeaseEntry> = (0..LEASE_MAX_ENTRIES - 1)
        .map(|i| plain(&format!("live{i}"), Some(me.clone()), now, None))
        .collect();
    sessions.push(plain("linger", None, now - 1000, Some(now - 1000)));
    write_lease(&dirs, &repo.main, sessions);

    let points = TestPoints::new(dirs.scope());
    let mut command = helper_refresh(&dirs);
    points.apply(&mut command, &[INBOX_OBSERVED], None);
    let mut refresh = command.spawn().unwrap();
    points.wait_reached(INBOX_OBSERVED, refresh.id(), Duration::from_secs(30));
    let mut agent = Agent::start(dirs.scope(), "claude");
    agent.run(
        &dirs,
        "session-start",
        &input("SessionStart", "new", &repo.main, None),
        How::Shell,
    );
    assert!(
        lease(&dirs, &repo.main).unwrap().overflow,
        "one lingering entry alone does not fold"
    );
    points.release(INBOX_OBSERVED);
    assert!(refresh.wait().unwrap().success());

    let after = lease(&dirs, &repo.main).unwrap();
    assert_eq!(
        entry(&after, "new").agent.as_ref(),
        Some(&agent.generation())
    );
    assert!(
        after
            .sessions
            .iter()
            .all(|e| e.session_id != cardinal::state::lease::FOLDED),
        "nothing folded"
    );
    assert_eq!(after.sessions.len(), LEASE_MAX_ENTRIES + 1);
}

/// Section 9.8, "Generations": a hook's event time never lowers `last_event_ms`, and an older
/// process's event keeps the entry's transcript path and last event kind.
#[test]
fn late_events_roll_nothing_back() {
    let dirs = Dirs::new();
    configure(&dirs, "");
    let repo = repo();
    let mut old = Agent::start(dirs.scope(), "claude");
    let newer = later(&old.generation());
    let future = state::now_ms() + 3_600_000;
    write_lease(
        &dirs,
        &repo.main,
        vec![LeaseEntry {
            transcript_path: Some("/t/new.jsonl".into()),
            last_event: LeaseEvent::SessionStart,
            ..plain("s", Some(newer.clone()), future, None)
        }],
    );
    old.run(
        &dirs,
        "session-end",
        &input(
            "SessionEnd",
            "s",
            &repo.main,
            Some(Path::new("/t/old.jsonl")),
        ),
        How::Shell,
    );
    let e = entry(&lease(&dirs, &repo.main).unwrap(), "s").clone();
    assert_eq!(e.last_event_ms, future, "never lowered");
    assert_eq!(e.transcript_path.as_deref(), Some("/t/new.jsonl"));
    assert_eq!(e.last_event, LeaseEvent::SessionStart);
    assert_eq!((e.agent.as_ref(), e.ended_ms), (Some(&newer), None));
}

/// Section 9.8, "Generations": of two processes of a session started within one second, a
/// delayed start of the older one never replaces the newer.
#[test]
fn same_second_starts_keep_the_newer() {
    let dirs = Dirs::new();
    configure(&dirs, "");
    let repo = repo();
    let (mut older, mut newer) = (0..20)
        .find_map(|_| {
            let older = Agent::start(dirs.scope(), "claude");
            let newer = Agent::start(dirs.scope(), "claude");
            (older.generation().start_s == newer.generation().start_s).then_some((older, newer))
        })
        .expect("two agents started within one second");
    newer.run(
        &dirs,
        "session-start",
        &input("SessionStart", "s", &repo.main, None),
        How::Shell,
    );
    older.run(
        &dirs,
        "session-start",
        &input("SessionStart", "s", &repo.main, None),
        How::Shell,
    );
    assert_eq!(
        entry(&lease(&dirs, &repo.main).unwrap(), "s")
            .agent
            .as_ref(),
        Some(&newer.generation())
    );
}

/// Section 9.8, "Writers": a `cwd` that cannot be resolved (`EACCES`) records `hook_error`
/// (`lease_discovery_incomplete`) and still writes the worktree around it.
#[test]
fn unreadable_cwd_writes_what_it_found() {
    let dirs = Dirs::new();
    configure(&dirs, "");
    let repo = repo();
    let blocked = repo.main.join("blocked");
    let deep = blocked.join("deep");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "s", &deep, None),
    );
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();
    entry(&lease(&dirs, &repo.main).unwrap(), "s");
    let errors = of_kind(&dirs, RecordKind::HookError);
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert_eq!(errors[0].data["reason"], "lease_discovery_incomplete");
}

/// Section 9.8: a hook reads `linger_s` from `host.toml` alone, so a broken repository profile
/// changes nothing; without a readable `host.toml` it drops nothing but writes its own entry.
/// With a valid one the same entry is dropped (control).
#[test]
fn config_errors_prune_nothing() {
    let dirs = Dirs::new();
    configure(&dirs, "[sessions]\nlinger_s = 14400\n");
    std::fs::write(dirs.config().join("repos/broken.toml"), "not = [toml").unwrap();
    let repo = repo();
    let now = state::now_ms();
    let hours = |h: u64| now - h * 3_600_000;
    write_lease(
        &dirs,
        &repo.main,
        vec![plain("three-hours", None, hours(3), Some(hours(3)))],
    );
    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "s", &repo.main, None),
    );
    entry(&lease(&dirs, &repo.main).unwrap(), "three-hours");

    std::fs::write(dirs.config().join("host.toml"), "[sessions\n").unwrap();
    write_lease(
        &dirs,
        &repo.main,
        vec![plain("ten-hours", None, hours(10), Some(hours(10)))],
    );
    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "s", &repo.main, None),
    );
    let kept = lease(&dirs, &repo.main).unwrap();
    entry(&kept, "ten-hours");
    entry(&kept, "s");

    configure(&dirs, "");
    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "s", &repo.main, None),
    );
    let pruned = lease(&dirs, &repo.main).unwrap();
    assert!(
        pruned.sessions.iter().all(|e| e.session_id != "ten-hours"),
        "{pruned:#?}"
    );
}

/// Section 9.8, "A corrupt file": `lease_corrupt` is recorded only once the replacement is
/// written; the `uncertain` entry of a file already past `linger_s` lingers from the repair;
/// and a file over the cap without `overflow` is corrupt.
#[test]
fn corrupt_file_reported_after_its_repair() {
    let dirs = Dirs::new();
    configure(&dirs, "");
    let repo = repo();
    let path = lease_path(&dirs, &repo.main);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"not json").unwrap();
    age(&path, Duration::from_hours(3));

    let points = TestPoints::new(dirs.scope());
    std::fs::write(
        points.dir().join("write-failures"),
        "lease\tENOSPC\talways\n",
    )
    .unwrap();
    let output = output_with_stdin(
        dirs.cmd()
            .env("CARDINAL_TEST_DIR", points.dir())
            .args(["hook", "claude", "session-start"]),
        input("SessionStart", "s", &repo.main, None)
            .to_string()
            .as_bytes(),
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(std::fs::read(&path).unwrap(), b"not json");
    assert_eq!(of_kind(&dirs, RecordKind::LeaseCorrupt), []);
    assert_eq!(
        of_kind(&dirs, RecordKind::HookError)[0].data["reason"],
        "lease_write_failed"
    );

    let repaired_at = state::now_ms();
    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "s", &repo.main, None),
    );
    let repaired = lease(&dirs, &repo.main).unwrap();
    let uncertain = repaired.sessions.iter().find(|e| e.uncertain).unwrap();
    assert!(uncertain.last_event_ms >= repaired_at, "{uncertain:#?}");
    assert_eq!(of_kind(&dirs, RecordKind::LeaseCorrupt).len(), 1);

    let over: Vec<LeaseEntry> = (0..=LEASE_MAX_ENTRIES)
        .map(|i| plain(&format!("s{i}"), None, repaired_at, Some(repaired_at)))
        .collect();
    write_lease_marked(&dirs, &repo.beside, over, false);
    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "s", &repo.beside, None),
    );
    let replaced = lease(&dirs, &repo.beside).unwrap();
    assert_eq!(replaced.sessions.len(), 2, "{:#?}", replaced.sessions);
    assert_eq!(of_kind(&dirs, RecordKind::LeaseCorrupt).len(), 2);
}

/// Section 9.8: a lease file larger than `LEASE_MAX_BYTES` is never assessed. The hook leaves
/// it as it was and records `hook_error`, and the refresh leaves it too; a small file beside it
/// takes the hook's entry (control).
#[test]
fn oversized_lease_is_left_intact() {
    let dirs = Dirs::new();
    configure(&dirs, "");
    let repo = repo();
    let long_ago = state::now_ms() - 10 * 3_600_000;
    // Expired: assessed, it would be dropped.
    let huge = plain(
        &"a".repeat(usize::try_from(cardinal::state::lease::LEASE_MAX_BYTES).unwrap()),
        None,
        long_ago,
        Some(long_ago),
    );
    write_lease(&dirs, &repo.main, vec![huge]);
    let path = lease_path(&dirs, &repo.main);
    let before = std::fs::read(&path).unwrap();
    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "s", &repo.main, None),
    );
    assert!(std::fs::read(&path).unwrap() == before, "the hook left it");
    let errors = of_kind(&dirs, RecordKind::HookError);
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert_eq!(errors[0].data["reason"], "lease_write_failed");
    refresh(&dirs);
    assert!(
        std::fs::read(&path).unwrap() == before,
        "the refresh left it"
    );

    // A file just within the bound that the hook's entry would take past it is not written.
    let max = usize::try_from(cardinal::state::lease::LEASE_MAX_BYTES).unwrap();
    let now = state::now_ms();
    let sized = |len: usize| vec![plain(&"b".repeat(len), None, now, Some(now))];
    write_lease(&dirs, &repo.main, sized(1));
    let overhead = usize::try_from(std::fs::metadata(&path).unwrap().len()).unwrap() - 1;
    write_lease(&dirs, &repo.main, sized(max - overhead - 16));
    let within = std::fs::read(&path).unwrap();
    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "s", &repo.main, None),
    );
    assert!(
        std::fs::read(&path).unwrap() == within,
        "not written past the bound"
    );
    assert_eq!(of_kind(&dirs, RecordKind::HookError).len(), 2);

    write_lease(
        &dirs,
        &repo.beside,
        vec![plain("old", None, long_ago, Some(long_ago))],
    );
    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "s", &repo.beside, None),
    );
    let small = lease(&dirs, &repo.beside).unwrap();
    assert_eq!(small.sessions.len(), 1, "{small:#?}");
    entry(&small, "s");
}

/// A FIFO at a lease's path never blocks the hook or the refresh: it is not a regular file,
/// so it is left as it is and reported, while a worktree beside it gets its lease (control).
#[test]
fn a_lease_that_is_not_a_regular_file_never_blocks() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::FileTypeExt;

    let dirs = Dirs::new();
    configure(&dirs, "");
    let repo = repo();
    let path = lease_path(&dirs, &repo.main);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c` is a valid NUL-terminated path.
    let made = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
    assert_eq!(made, 0, "mkfifo: {}", std::io::Error::last_os_error());

    let started = std::time::Instant::now();
    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "s", &repo.main, None),
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert!(
        std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_fifo()
    );
    let errors = of_kind(&dirs, RecordKind::HookError);
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert_eq!(errors[0].data["reason"], "lease_write_failed");
    refresh(&dirs);
    assert!(
        std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_fifo()
    );

    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "s", &repo.beside, None),
    );
    entry(&lease(&dirs, &repo.beside).unwrap(), "s");
}

/// Section 9.8: a `host.toml` that is a symbolic link whose target is missing is a config that
/// cannot be read, never an absent one: the hook prunes nothing. With no `host.toml` entry at
/// all the defaults hold and the entry past their linger goes (control).
#[test]
fn dangling_host_config_prunes_nothing() {
    let dirs = Dirs::new();
    configure(&dirs, "[sessions]\nlinger_s = 86400\n");
    let repo = repo();
    let host = dirs.config().join("host.toml");
    std::fs::remove_file(&host).unwrap();
    std::os::unix::fs::symlink(dirs.config().join("host.toml.new"), &host).unwrap();
    let ten_hours = state::now_ms() - 10 * 3_600_000;
    write_lease(
        &dirs,
        &repo.main,
        vec![plain("ten-hours", None, ten_hours, Some(ten_hours))],
    );
    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "s", &repo.main, None),
    );
    let kept = lease(&dirs, &repo.main).unwrap();
    entry(&kept, "ten-hours");
    entry(&kept, "s");
    // The refresh reads the same dangling entry as unreadable and expires nothing either.
    refresh(&dirs);
    entry(&lease(&dirs, &repo.main).unwrap(), "ten-hours");

    std::fs::remove_file(&host).unwrap();
    run_hook(
        &dirs,
        "session-start",
        &input("SessionStart", "s", &repo.main, None),
    );
    let pruned = lease(&dirs, &repo.main).unwrap();
    assert!(
        pruned.sessions.iter().all(|e| e.session_id != "ten-hours"),
        "{pruned:#?}"
    );
}

/// Section 9.8, "A corrupt file": the `uncertain` entry of a file already past `linger_s`
/// lingers from the repair, after the lock wait, not from the event the hook took before it.
#[test]
fn uncertain_entry_lingers_from_the_publication() {
    use std::io::Write as _;
    use std::process::Stdio;

    let dirs = Dirs::new();
    configure(&dirs, "");
    let repo = repo();
    let path = lease_path(&dirs, &repo.main);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"not json").unwrap();
    age(&path, Duration::from_hours(3));
    let lock = state::lock(
        &state(&dirs).lease_lock(&key(&repo.main)),
        LockMode::Exclusive,
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();
    let mut hook = dirs
        .cmd()
        .args(["hook", "claude", "session-start"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = hook.stdin.take().unwrap();
    stdin
        .write_all(
            input("SessionStart", "s", &repo.main, None)
                .to_string()
                .as_bytes(),
        )
        .unwrap();
    drop(stdin);
    // The hook waits for the lock up to 1 s; it is released well within that.
    std::thread::sleep(Duration::from_millis(400));
    let released_ms = state::now_ms();
    drop(lock);
    assert_eq!(hook.wait().unwrap().code(), Some(0));
    let repaired = lease(&dirs, &repo.main).unwrap();
    let uncertain = repaired.sessions.iter().find(|e| e.uncertain).unwrap();
    assert!(
        uncertain.last_event_ms >= released_ms,
        "{} before the release at {released_ms}",
        uncertain.last_event_ms
    );
    assert!(
        entry(&repaired, "s").last_event_ms < released_ms,
        "the event's own time"
    );
}

/// Section 9.8, "Expiry": the refresh checks the file type under the lock before it reads or
/// removes. A lease swapped for a symbolic link after the listing stays, its target untouched;
/// without the swap the same expired lease is removed (control).
#[test]
fn expiry_rechecks_the_file_under_the_lock() {
    use cardinal::testpoint::INBOX_LEASE_LOCKED;

    let dirs = Dirs::new();
    configure(&dirs, "");
    let repo = repo();
    let long_ago = state::now_ms() - 10 * 3_600_000;
    let expired = || vec![plain("old", None, long_ago, Some(long_ago))];
    write_lease(&dirs, &repo.main, expired());
    let path = lease_path(&dirs, &repo.main);
    let outside = dirs.scope().path("outside.json");
    std::fs::copy(&path, &outside).unwrap();
    let target = std::fs::read(&outside).unwrap();

    let points = TestPoints::new(dirs.scope());
    let mut command = helper_refresh(&dirs);
    points.apply(&mut command, &[INBOX_LEASE_LOCKED], None);
    let mut paused = command.spawn().unwrap();
    points.wait_reached(INBOX_LEASE_LOCKED, paused.id(), Duration::from_secs(30));
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&outside, &path).unwrap();
    points.release(INBOX_LEASE_LOCKED);
    assert!(paused.wait().unwrap().success());
    assert!(
        std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link stays"
    );
    assert!(std::fs::read(&outside).unwrap() == target, "its target too");

    std::fs::remove_file(&path).unwrap();
    write_lease(&dirs, &repo.main, expired());
    refresh(&dirs);
    assert!(!path.exists(), "removed without the swap");
}

/// A record appended whose rotation then fails is not counted as dropped: the hook's own
/// record carries no `dropped_records` while its `lease_corrupt` record is there.
#[test]
fn a_failed_rotation_drops_nothing() {
    let dirs = Dirs::new();
    configure(&dirs, "[records]\nmax_bytes = 1\nkeep = 2\n");
    let repo = repo();
    let state = state(&dirs);
    // The rotation renames the records over a directory that is not empty: it fails.
    let rotated = state.rotated_records(1);
    std::fs::create_dir_all(rotated.join("full")).unwrap();
    let path = lease_path(&dirs, &repo.main);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"not json").unwrap();
    run_hook(
        &dirs,
        "subagent-stop",
        &input("SubagentStop", "s", &repo.main, None),
    );
    // Out of the way of the reader, which reads rotated records too.
    std::fs::remove_dir_all(&rotated).unwrap();
    assert_eq!(of_kind(&dirs, RecordKind::LeaseCorrupt).len(), 1);
    let stopped = of_kind(&dirs, RecordKind::SubagentStopped);
    assert_eq!(stopped.len(), 1, "{stopped:#?}");
    assert_eq!(stopped[0].data.get("dropped_records"), None, "{stopped:#?}");
}

// --- Readers (A41–A43) ----------------------------------------------------------------------

use cardinal::contract::{ActStatus, CheckResult, Plan, PlanKind};
use plan_support::Setup;

/// `[sessions]` covering the fixture with `linger_s` and `quiet_s`.
fn sessions(s: &Setup, linger_s: u64, quiet_s: u64) -> String {
    format!(
        "[sessions]\ncovered = ['{}']\nlinger_s = {linger_s}\nquiet_s = {quiet_s}\n",
        dunce::canonicalize(s.fx.root()).unwrap().display()
    )
}

/// A plan setup whose `[sessions]` covers the fixture with these bounds.
fn bounded(linger_s: u64, quiet_s: u64) -> Setup {
    let mut s = Setup::new();
    let host = sessions(&s, linger_s, quiet_s);
    s.scene.observe_config(&host);
    s
}

/// Waits out `secs`, a bound of the test's own `[sessions]`.
fn wait_out(secs: u64) {
    std::thread::sleep(Duration::from_millis(secs * 1000 + 200));
}

/// Writes the lease of the worktree at `root` in `s`'s state dir.
fn put_lease(s: &Setup, root: &Path, sessions: Vec<LeaseEntry>, overflow: bool) {
    let path = s
        .state
        .lease(&state::worktree_key(&dunce::canonicalize(root).unwrap()));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let lease = SessionLease {
        schema: cardinal::contract::SessionLeaseSchema,
        worktree: dunce::canonicalize(root).unwrap().display().to_string(),
        overflow,
        sessions,
    };
    std::fs::write(path, serde_json::to_vec(&lease).unwrap()).unwrap();
}

/// `not_in_use` of `plan`: its result and detail.
fn in_use(plan: &Plan) -> (CheckResult, String) {
    let check = plan
        .checks
        .iter()
        .find(|c| c.name == "not_in_use")
        .expect("not_in_use");
    (check.result, check.detail.clone())
}

/// The `reclaim_dir` plan of `wt`'s `node_modules`.
fn plan_modules(s: &Setup, wt: &Path) -> Plan {
    s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"))
}

fn now_ms() -> u64 {
    state::now_ms()
}

/// Moves `path`'s modification time an hour into the past.
fn backdate(path: &Path) {
    plan_support::backdate(path);
}

/// A worktree `name` with a `node_modules` of one file, which a plan finds idle once `wait`
/// seconds have passed.
fn idle_worktree(s: &Setup, name: &str, wait: u64) -> PathBuf {
    let wt = s.worktree(name);
    plan_support::write(&wt, "node_modules/pkg/index.js", "x");
    wait_out(wait);
    wt
}

/// A41: a live entry (the test process is its agent, with its start time and boot ID) holds
/// its worktree, named; a worktree with no lease, idle, passes (control).
#[test]
fn live_session_refuses() {
    let s = bounded(60, 1);
    let free = idle_worktree(&s, "free", 0);
    let wt = idle_worktree(&s, "live", 1);
    put_lease(&s, &wt, vec![plain("s-live", Some(me()), 1, None)], false);
    let (result, detail) = in_use(&plan_modules(&s, &wt));
    assert_eq!(result, CheckResult::Fail, "{detail}");
    assert!(detail.contains("session s-live is live"), "{detail}");
    assert_eq!(in_use(&plan_modules(&s, &free)).0, CheckResult::Pass);
}

/// A41: an entry that ended within `linger_s` holds its worktree; one that ended earlier does
/// not (control).
#[test]
fn ended_session_lingers() {
    let s = bounded(60, 1);
    let wt = idle_worktree(&s, "ended", 1);
    put_lease(
        &s,
        &wt,
        vec![plain("s-ended", None, now_ms(), Some(now_ms()))],
        false,
    );
    let (result, detail) = in_use(&plan_modules(&s, &wt));
    assert_eq!(result, CheckResult::Fail, "{detail}");
    assert!(detail.contains("session s-ended lingers"), "{detail}");
    let old = now_ms() - 120_000;
    put_lease(&s, &wt, vec![plain("s-ended", None, old, Some(old))], false);
    assert_eq!(in_use(&plan_modules(&s, &wt)).0, CheckResult::Pass);
}

/// An entry whose events are older than `linger_s`, its transcript at `transcript`.
fn stale_with(transcript: &Path) -> LeaseEntry {
    let old = now_ms() - 120_000;
    LeaseEntry {
        transcript_path: Some(transcript.display().to_string()),
        ..plain("s-crashed", None, old, Some(old))
    }
}

/// A41: a session that died without `SessionEnd` lingers from its transcript's mtime; a
/// transcript older than `linger_s` holds nothing (control).
#[test]
fn crash_lingers_from_the_transcript() {
    let s = bounded(60, 1);
    let wt = idle_worktree(&s, "crashed", 1);
    let transcript = s.fx.path("transcripts/s-crashed.jsonl");
    plan_support::write(s.fx.root(), "transcripts/s-crashed.jsonl", "{}\n");
    put_lease(&s, &wt, vec![stale_with(&transcript)], false);
    assert_eq!(in_use(&plan_modules(&s, &wt)).0, CheckResult::Fail);
    backdate(&transcript);
    assert_eq!(in_use(&plan_modules(&s, &wt)).0, CheckResult::Pass);
}

/// A41: a subagent's transcript, beside the session's (`<transcript>/subagents/`), holds the
/// worktree while the main transcript stays silent; backdated, it holds nothing (control).
#[test]
fn subagent_transcript_holds() {
    let s = bounded(60, 1);
    let wt = idle_worktree(&s, "subagent", 1);
    let transcript = s.fx.path("transcripts/s-crashed.jsonl");
    plan_support::write(s.fx.root(), "transcripts/s-crashed.jsonl", "{}\n");
    plan_support::write(
        s.fx.root(),
        "transcripts/s-crashed/subagents/a.jsonl",
        "{}\n",
    );
    backdate(&transcript);
    put_lease(&s, &wt, vec![stale_with(&transcript)], false);
    assert_eq!(in_use(&plan_modules(&s, &wt)).0, CheckResult::Fail);
    backdate(&s.fx.path("transcripts/s-crashed/subagents/a.jsonl"));
    assert_eq!(in_use(&plan_modules(&s, &wt)).0, CheckResult::Pass);
}

/// A41: a PID another process took, told by its start time, is not live, and an entry with it
/// and old events holds nothing; the same PID with its own start time holds (control).
#[test]
fn reused_pid_is_not_live() {
    let s = bounded(60, 1);
    let wt = idle_worktree(&s, "reused", 1);
    let old = now_ms() - 120_000;
    put_lease(
        &s,
        &wt,
        vec![plain("s-old", Some(later(&me())), old, None)],
        false,
    );
    assert_eq!(in_use(&plan_modules(&s, &wt)).0, CheckResult::Pass);
    put_lease(&s, &wt, vec![plain("s-old", Some(me()), old, None)], false);
    assert_eq!(in_use(&plan_modules(&s, &wt)).0, CheckResult::Fail);
}

/// A41: a lease file that does not parse, one marked `overflow`, and an `uncertain` entry
/// within `linger_s` leave `not_in_use` unknown; an `uncertain` entry past `linger_s` holds
/// nothing (control).
#[test]
fn torn_lease_is_unknown() {
    let s = bounded(60, 1);
    let wt = idle_worktree(&s, "torn", 1);
    let path = s
        .state
        .lease(&state::worktree_key(&dunce::canonicalize(&wt).unwrap()));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{\"schema\": \"cardinal.session-lease.v1\", \"sess").unwrap();
    let (result, detail) = in_use(&plan_modules(&s, &wt));
    assert_eq!(result, CheckResult::Unknown, "{detail}");
    assert!(detail.contains("corrupt"), "{detail}");

    let old = now_ms() - 120_000;
    put_lease(&s, &wt, vec![plain("s-old", None, old, Some(old))], true);
    let (result, detail) = in_use(&plan_modules(&s, &wt));
    assert_eq!(result, CheckResult::Unknown, "{detail}");
    assert!(detail.contains("overflow"), "{detail}");

    let uncertain = |at: u64| LeaseEntry {
        uncertain: true,
        ..plain("cardinal:uncertain", None, at, Some(at))
    };
    put_lease(&s, &wt, vec![uncertain(now_ms())], false);
    let (result, detail) = in_use(&plan_modules(&s, &wt));
    assert_eq!(result, CheckResult::Unknown, "{detail}");
    assert!(detail.contains("uncertain"), "{detail}");
    put_lease(&s, &wt, vec![uncertain(old)], false);
    assert_eq!(in_use(&plan_modules(&s, &wt)).0, CheckResult::Pass);
}

/// Writes `n` files `<rel>/f<i>` under `root`, each backdated.
fn old_files(root: &Path, rel: &str, n: usize) {
    for i in 0..n {
        let file = format!("{rel}/f{i}");
        plan_support::write(root, &file, "x");
        backdate(&root.join(file));
    }
    backdate(&root.join(rel));
}

/// A41: each cap passed without a recent signal leaves `not_in_use` unknown, never a partial
/// newest mtime: more than 1,000 paths `git status` lists, more than 4,096 files of a session's
/// `subagents/`, and more than 4,096 entries in an ignored entry; at each cap it passes
/// (control).
#[test]
fn caps_are_unknown() {
    let s = bounded(60, 1);
    let subagents = idle_worktree(&s, "subagents", 0);
    // Status paths.
    let wt = s.worktree("paths");
    plan_support::write(&wt, "node_modules/pkg/index.js", "x");
    old_files(&wt, "untracked", 1000);
    wait_out(1);
    assert_eq!(in_use(&plan_modules(&s, &wt)).0, CheckResult::Pass);
    old_files(&wt, "more", 1);
    let (result, detail) = in_use(&plan_modules(&s, &wt));
    assert_eq!(result, CheckResult::Unknown, "{detail}");
    assert!(detail.contains("more than 1000"), "{detail}");

    // Subagent transcripts.
    let wt = subagents;
    let transcript = s.fx.path("transcripts/s-many.jsonl");
    plan_support::write(s.fx.root(), "transcripts/s-many.jsonl", "{}\n");
    backdate(&transcript);
    old_files(s.fx.root(), "transcripts/s-many/subagents", 4096);
    put_lease(&s, &wt, vec![stale_with(&transcript)], false);
    assert_eq!(in_use(&plan_modules(&s, &wt)).0, CheckResult::Pass);
    plan_support::write(s.fx.root(), "transcripts/s-many/subagents/f4096", "x");
    backdate(&s.fx.path("transcripts/s-many/subagents/f4096"));
    let (result, detail) = in_use(&plan_modules(&s, &wt));
    assert_eq!(result, CheckResult::Unknown, "{detail}");
    assert!(detail.contains("more than 4096 files"), "{detail}");

    // An ignored entry's top-level entries.
    let wt = s.worktree("ignored");
    plan_support::write(&wt, "node_modules/pkg/index.js", "x");
    old_files(&wt, "fuzz/corpus", 4096);
    backdate(&wt.join("fuzz"));
    wait_out(1);
    assert_eq!(in_use(&plan_modules(&s, &wt)).0, CheckResult::Pass);
    plan_support::write(&wt, "fuzz/corpus/f4096", "x");
    backdate(&wt.join("fuzz/corpus/f4096"));
    backdate(&wt.join("fuzz/corpus"));
    let (result, detail) = in_use(&plan_modules(&s, &wt));
    assert_eq!(result, CheckResult::Unknown, "{detail}");
    assert!(detail.contains("more than 4096 entries"), "{detail}");
}

/// A41's signals, each alone: a worktree idle for `quiet_s` passes (the control, first), and
/// after `change` its plan fails `not_in_use` naming `named`.
mod quiet_signals {
    use std::path::Path;

    use super::{CheckResult, bounded, in_use, plan_modules, wait_out};
    use crate::plan_support::{self, Setup};

    const QUIET_S: u64 = 4;

    fn case(change: impl FnOnce(&Setup, &Path) -> std::path::PathBuf, named: &str) {
        case_with(|_| {}, change, named);
    }

    /// [`case`] with `prepare` run on the worktree before the quiet period is waited out.
    fn case_with(
        prepare: impl FnOnce(&Path),
        change: impl FnOnce(&Setup, &Path) -> std::path::PathBuf,
        named: &str,
    ) {
        let s = bounded(60, QUIET_S);
        std::fs::write(s.main.join(".git/info/exclude"), "dist/\nlocal.env\n").unwrap();
        let wt = s.worktree("signal");
        plan_support::write(&wt, "node_modules/pkg/index.js", "x");
        prepare(&wt);
        wait_out(QUIET_S);
        assert_eq!(in_use(&plan_modules(&s, &wt)).0, CheckResult::Pass);
        let wt = change(&s, &wt);
        let (result, detail) = in_use(&plan_modules(&s, &wt));
        assert_eq!(result, CheckResult::Fail, "{detail}");
        assert!(detail.contains("not quiet"), "{detail}");
        assert!(detail.contains(named), "{named}: {detail}");
    }

    #[test]
    fn index_write() {
        case(
            |s, wt| {
                plan_support::write(wt, "staged.rs", "x");
                s.fx.add(wt, "staged.rs");
                plan_support::backdate(&wt.join("staged.rs"));
                wt.to_path_buf()
            },
            "/index",
        );
    }

    /// A commit modifying a tracked file, the file and the index backdated: the reflog
    /// alone is the signal, its mtime set ahead so that it stays recent however late the plan
    /// reads it.
    #[test]
    fn commit() {
        case(
            |s, wt| {
                s.fx.commit_file(wt, "src/lib.rs", "// more\n", "more");
                plan_support::backdate(&wt.join("src/lib.rs"));
                plan_support::backdate(&s.main.join(".git/worktrees/signal/index"));
                plan_support::ahead(&s.main.join(".git/worktrees/signal/logs/HEAD"));
                wt.to_path_buf()
            },
            "logs/HEAD",
        );
    }

    /// The reflog's last entry alone: an entry written an hour ahead, `logs/HEAD`'s own mtime
    /// set back after it, is the signal, named as the last entry.
    #[test]
    fn reflog_entry() {
        case(
            |s, wt| {
                use std::io::Write as _;
                let reflog = s.main.join(".git/worktrees/signal/logs/HEAD");
                let zeros = "0".repeat(40);
                let at_s = super::now_ms() / 1000 + 3600;
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&reflog)
                    .unwrap()
                    .write_all(
                        format!("{zeros} {zeros} A U Thor <a@example.com> {at_s} +0000\tx\n")
                            .as_bytes(),
                    )
                    .unwrap();
                plan_support::backdate(&reflog);
                wt.to_path_buf()
            },
            "the last entry of",
        );
    }

    #[test]
    fn tracked_edit() {
        case(
            |_, wt| {
                plan_support::write(wt, "src/lib.rs", "// edited\n");
                wt.to_path_buf()
            },
            "src/lib.rs",
        );
    }

    #[test]
    fn tracked_deletion() {
        case(
            |_, wt| {
                std::fs::remove_file(wt.join("src/lib.rs")).unwrap();
                wt.to_path_buf()
            },
            "a tracked deletion",
        );
    }

    #[test]
    fn untracked_file() {
        case(
            |_, wt| {
                plan_support::write(wt, "notes.txt", "x");
                wt.to_path_buf()
            },
            "notes.txt",
        );
    }

    #[test]
    fn target_entry() {
        case(
            |_, wt| {
                plan_support::write(wt, "node_modules/pkg/new.js", "x");
                plan_support::backdate(&wt.join("node_modules/pkg"));
                wt.to_path_buf()
            },
            "node_modules/pkg/new.js",
        );
    }

    /// A write in place deep inside another regenerable target (`target/`), below its
    /// top-level entries: the plan of `node_modules` reads every target's entries.
    #[test]
    fn another_target_entry() {
        case_with(
            |wt| plan_support::write(wt, "target/debug/deps/libx.rlib", "x"),
            |_, wt| {
                use std::io::Write as _;
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(wt.join("target/debug/deps/libx.rlib"))
                    .unwrap()
                    .write_all(b"y")
                    .unwrap();
                plan_support::ahead(&wt.join("target/debug/deps/libx.rlib"));
                wt.to_path_buf()
            },
            "target/debug/deps/libx.rlib",
        );
    }

    #[test]
    fn ignored_output() {
        case(
            |_, wt| {
                plan_support::write(wt, "dist/out.js", "x");
                wt.to_path_buf()
            },
            "dist",
        );
    }

    /// An ignored file no regenerable pattern matches: its own mtime is a signal.
    #[test]
    fn ignored_file() {
        case(
            |_, wt| {
                plan_support::write(wt, "local.env", "x");
                wt.to_path_buf()
            },
            "local.env",
        );
    }

    #[test]
    fn worktree_move() {
        case(
            |s, wt| {
                let to = s.fx.path("moved");
                s.fx.worktree_move(&s.main, wt, &to);
                to
            },
            "gitdir",
        );
    }
}

/// Section 9.8: in `cardinal plan`'s plans of every target, a write in place deep inside one
/// regenerable target (`target/debug/deps/libx.rlib`) fails the other's (`node_modules`)
/// `not_in_use`, read from the walk the plans already took; before the write both pass
/// (control).
#[test]
fn another_target_holds_every_plan() {
    use std::io::Write as _;
    let s = bounded(60, 2);
    let wt = s.worktree("siblings");
    plan_support::write(&wt, "node_modules/pkg/index.js", "x");
    plan_support::write(&wt, "target/debug/deps/libx.rlib", "x");
    wait_out(2);
    let modules = |s: &Setup| {
        let plans = s.plan_all().plans;
        let plan = plans
            .iter()
            .find(|p| p.target.path.ends_with("siblings/node_modules"))
            .expect("the node_modules plan")
            .clone();
        in_use(&plan)
    };
    assert_eq!(modules(&s).0, CheckResult::Pass);
    std::fs::OpenOptions::new()
        .append(true)
        .open(wt.join("target/debug/deps/libx.rlib"))
        .unwrap()
        .write_all(b"y")
        .unwrap();
    plan_support::ahead(&wt.join("target/debug/deps/libx.rlib"));
    let (result, detail) = modules(&s);
    assert_eq!(result, CheckResult::Fail, "{detail}");
    assert!(detail.contains("target/debug/deps/libx.rlib"), "{detail}");
}

/// Section 9.8: the entries below another regenerable target are read whole, or the plans of
/// the worktree read them `unknown`. In the main worktree, `target/debug` replaced by a new
/// directory after the walk listed `target/` and before it lists `debug` (`walk.dir_checked`)
/// leaves `node_modules`'s `not_in_use` unknown in `plan reclaim-dir` and in `cardinal plan`,
/// never quiet from the entries recorded; removed there, with the read held past `quiet_s`,
/// it is activity now. Nothing changed at the pause, both plans pass (control).
#[test]
fn another_target_is_read_whole() {
    let s = bounded(60, 1);
    let main = s.main.clone();
    let debug = main.join("target/debug");
    // Every entry an hour old; `node_modules` holds no directory, so the first directory the
    // plans' walks open is `target/debug`.
    let reset = || {
        let _ = std::fs::remove_dir_all(main.join("target"));
        plan_support::write(&main, "node_modules/index.js", "x");
        plan_support::write(&main, "target/debug/deps/libx.rlib", "x");
        for path in [
            "node_modules/index.js",
            "target/debug/deps/libx.rlib",
            "target/debug/deps",
            "target/debug",
        ] {
            backdate(&main.join(path));
        }
    };
    let replace = || {
        let aside = main.join("target/debug.old");
        std::fs::rename(&debug, &aside).unwrap();
        std::fs::create_dir(&debug).unwrap();
        backdate(&debug);
        backdate(&aside);
    };
    let remove = || {
        std::fs::remove_dir_all(&debug).unwrap();
        wait_out(1);
    };
    let point = cardinal::testpoint::WALK_DIR_CHECKED;
    let modules = main.join("node_modules");
    let one = |change: &dyn Fn()| {
        reset();
        in_use(&s.plan_at(PlanKind::ReclaimDir, &modules, point, change))
    };
    let all = |change: &dyn Fn()| {
        reset();
        let plans = s.plan_all_at(point, change).plans;
        let plan = plans
            .iter()
            .find(|p| Path::new(&p.target.path) == modules)
            .expect("the node_modules plan");
        in_use(plan)
    };
    reset();
    wait_out(1);
    for read in [&all as &dyn Fn(&dyn Fn()) -> (CheckResult, String), &one] {
        assert_eq!(read(&|| {}).0, CheckResult::Pass);
        let (result, detail) = read(&replace);
        assert_eq!(result, CheckResult::Unknown, "{detail}");
        assert!(detail.contains("target/debug"), "{detail}");
        let (result, detail) = read(&remove);
        assert_eq!(result, CheckResult::Fail, "{detail}");
        assert!(detail.contains("target/debug"), "{detail}");
    }
}

/// Section 9.8: a regenerable target's top-level entries, below a target whose whole walk the
/// plans read, are read outside both caps: a `node_modules` of 4,097 files, every entry past
/// `quiet_s`, leaves `node_modules`'s `not_in_use` `pass` (an ignored entry no regenerable
/// pattern matches past the cap is `caps_are_unknown`'s).
#[test]
fn a_large_regenerable_target_is_read_past_the_caps() {
    let s = bounded(60, 1);
    let large = s.worktree("large");
    let dir = large.join("node_modules");
    std::fs::create_dir_all(&dir).unwrap();
    for n in 0..=cardinal::observe::quiet::MAX_ENTRIES_PER_IGNORED {
        std::fs::write(dir.join(format!("f{n}")), "x").unwrap();
    }
    wait_out(1);
    let (result, detail) = in_use(&plan_modules(&s, &large));
    assert_eq!(result, CheckResult::Pass, "{detail}");
}

/// Section 9.8: a plan's reading of its own target counts what the walk found removed while
/// it read. `target` renamed out of the worktree after the walk opened it and read its names
/// (`walk.dir_listed`), an empty directory put at its name, every time an hour old and no
/// directory below it left unlisted: the identity recheck finds the name no longer the
/// directory listed, and `plan reclaim-dir`'s `not_in_use` fails, never `pass` from the old
/// directory's entries. Nothing changed at the pause,
/// it passes (control).
#[test]
fn a_target_replaced_while_it_was_read_is_not_quiet() {
    let s = bounded(60, 1);
    let main = s.main.clone();
    let target = main.join("target");
    let aside = s.fx.root().join("target-aside");
    let reset = || {
        let _ = std::fs::remove_dir_all(&target);
        let _ = std::fs::remove_dir_all(&aside);
        plan_support::write(&main, "target/app", "x");
        for path in ["target/app", "target"] {
            backdate(&main.join(path));
        }
    };
    let replace = || {
        std::fs::rename(&target, &aside).unwrap();
        std::fs::create_dir(&target).unwrap();
        backdate(&target);
    };
    let point = cardinal::testpoint::WALK_DIR_LISTED;
    let read = |change: &dyn Fn()| {
        reset();
        in_use(&s.plan_at(PlanKind::ReclaimDir, &target, point, change))
    };
    reset();
    wait_out(1);
    let (result, detail) = read(&|| {});
    assert_eq!(result, CheckResult::Pass, "{detail}");
    let (result, detail) = read(&replace);
    assert_eq!(result, CheckResult::Fail, "{detail}");
    assert!(detail.contains("removed while it was read"), "{detail}");
}

/// Section 9.8, Linux: an ignored file whose name is not UTF-8, written now, is read by the
/// name git listed, not a lossy spelling that names nothing: `not_in_use` fails; backdated,
/// it passes (control).
#[cfg(target_os = "linux")]
#[test]
fn a_name_that_is_not_utf8_is_read() {
    use std::os::unix::ffi::OsStrExt as _;
    let s = bounded(60, 2);
    std::fs::write(s.main.join(".git/info/exclude"), "*.log\n").unwrap();
    let wt = s.worktree("bytes");
    plan_support::write(&wt, "node_modules/pkg/index.js", "x");
    let name = wt.join(std::ffi::OsStr::from_bytes(b"build-\xff.log"));
    std::fs::write(&name, "x").unwrap();
    backdate(&name);
    wait_out(2);
    assert_eq!(in_use(&plan_modules(&s, &wt)).0, CheckResult::Pass);
    std::fs::write(&name, "y").unwrap();
    plan_support::ahead(&name);
    let (result, detail) = in_use(&plan_modules(&s, &wt));
    assert_eq!(result, CheckResult::Fail, "{detail}");
    assert!(detail.contains(".log"), "{detail}");
}

/// Section 9.8: a FIFO at the worktree's `logs/HEAD`, its mtime an hour old, is no reflog: the
/// plan reads it without waiting in it, and `not_in_use` is unknown, naming it; a regular
/// reflog there passes (control).
#[test]
fn a_reflog_that_is_not_a_regular_file_never_blocks() {
    use std::os::unix::ffi::OsStrExt as _;
    let s = bounded(60, 1);
    let wt = idle_worktree(&s, "fifo", 1);
    assert_eq!(in_use(&plan_modules(&s, &wt)).0, CheckResult::Pass);
    let reflog = s.main.join(".git/worktrees/fifo/logs/HEAD");
    let kept = std::fs::read(&reflog).unwrap();
    std::fs::remove_file(&reflog).unwrap();
    let c = std::ffi::CString::new(reflog.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c` is a valid NUL-terminated path.
    let made = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
    assert_eq!(made, 0, "mkfifo: {}", std::io::Error::last_os_error());
    let hour_ago = libc::timespec {
        tv_sec: libc::time_t::try_from(now_ms() / 1000 - 3600).unwrap(),
        tv_nsec: 0,
    };
    // SAFETY: `c` is a valid NUL-terminated path and `times` two `timespec`s; a FIFO's times
    // are set without opening it, which would wait for a writer.
    let set =
        unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), [hour_ago, hour_ago].as_ptr(), 0) };
    assert_eq!(set, 0, "utimensat: {}", std::io::Error::last_os_error());
    let (result, detail) = in_use(&plan_modules(&s, &wt));
    assert_eq!(result, CheckResult::Unknown, "{detail}");
    assert!(detail.contains("logs/HEAD: not a regular file"), "{detail}");
    std::fs::remove_file(&reflog).unwrap();
    std::fs::write(&reflog, kept).unwrap();
    backdate(&reflog);
    assert_eq!(in_use(&plan_modules(&s, &wt)).0, CheckResult::Pass);
}

/// A41: a signal that cannot be read, here an ignored directory whose top-level entries
/// cannot be listed, leaves `not_in_use` unknown, never idle, naming it; readable again, the
/// worktree passes (control).
#[test]
fn unreadable_signal_is_unknown() {
    use std::os::unix::fs::PermissionsExt as _;
    let s = bounded(60, 1);
    let wt = s.worktree("unreadable");
    plan_support::write(&wt, "node_modules/pkg/index.js", "x");
    plan_support::write(&wt, "fuzz/corpus/a", "x");
    wait_out(1);
    let corpus = wt.join("fuzz/corpus");
    std::fs::set_permissions(&corpus, std::fs::Permissions::from_mode(0o000)).unwrap();
    let (result, detail) = in_use(&plan_modules(&s, &wt));
    std::fs::set_permissions(&corpus, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(result, CheckResult::Unknown, "{detail}");
    assert!(detail.contains("fuzz/corpus"), "{detail}");
    assert_eq!(in_use(&plan_modules(&s, &wt)).0, CheckResult::Pass);
}

/// A41: `act`'s move and restore change the target's own times and its parent's mtime, never
/// read: a target renamed away and back, its own mtime now, is idle, while a write into its entries is not
/// (control).
#[test]
fn a_restored_target_is_not_held_by_its_own_times() {
    let s = bounded(60, 2);
    let wt = idle_worktree(&s, "restored", 2);
    std::fs::rename(wt.join("node_modules"), wt.join("node_modules.moved")).unwrap();
    std::fs::rename(wt.join("node_modules.moved"), wt.join("node_modules")).unwrap();
    // A rename keeps a directory's own mtime on some file systems and not others: set it now.
    std::fs::File::open(wt.join("node_modules"))
        .unwrap()
        .set_modified(std::time::SystemTime::now())
        .unwrap();
    assert_eq!(in_use(&plan_modules(&s, &wt)).0, CheckResult::Pass);
    plan_support::write(&wt, "node_modules/pkg/index.js", "y");
    plan_support::ahead(&wt.join("node_modules/pkg/index.js"));
    assert_eq!(in_use(&plan_modules(&s, &wt)).0, CheckResult::Fail);
}

/// A41: an entry the refresh dropped under the old `linger_s` stays dropped when `linger_s`
/// is raised: the worktree is idle under the new bound too; the entry held before it expired
/// (control).
#[test]
fn raised_linger_revives_nothing() {
    let mut s = bounded(2, 1);
    let wt = idle_worktree(&s, "raised", 1);
    put_lease(
        &s,
        &wt,
        vec![plain("s-gone", None, now_ms(), Some(now_ms()))],
        false,
    );
    assert_eq!(in_use(&plan_modules(&s, &wt)).0, CheckResult::Fail);
    wait_out(2);
    let refreshed = s
        .cardinal()
        .arg("inbox")
        .output_within(plan_support::RUN_LIMIT);
    assert!(refreshed.unwrap().status.success());
    assert!(
        !s.state
            .lease(&state::worktree_key(&dunce::canonicalize(&wt).unwrap()))
            .exists()
    );
    s.scene.clear_observe_config();
    let host = sessions(&s, 7200, 1);
    s.scene.observe_config(&host);
    assert_eq!(in_use(&plan_modules(&s, &wt)).0, CheckResult::Pass);
}

/// A41: a worktree whose `not_in_use` is unknown at three consecutive full observations raises
/// `in_use_unknown` naming the input, not before, and the item ends once a full observation
/// reads `pass` or `fail`.
#[test]
fn unknown_raises_an_item() {
    use cardinal::contract::{InboxItem, InboxKind, InboxStatus};
    let mut d = disk_support::Disk::with_host((1, 2, 3), "");
    let host = sessions(&d.s, 3600, 1);
    d.s.scene.observe_config(&host);
    let wt = d.s.worktree("unknown");
    let path =
        d.s.state
            .lease(&state::worktree_key(&dunce::canonicalize(&wt).unwrap()));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"torn").unwrap();
    d.fixture_reads(&[Some(3 * disk_support::MIB / 2)]);
    wait_out(observe_common::QUIET_SESSIONS_S);
    let items = |d: &disk_support::Disk| -> Vec<InboxItem> {
        cardinal::inbox::list(
            &d.s.config(),
            &d.s.state,
            Some(cardinal::inbox::StatusFilter::All),
            false,
        )
        .unwrap()
        .items
        .into_iter()
        .filter(|i| i.kind == InboxKind::InUseUnknown)
        .collect()
    };
    for _ in 0..2 {
        d.reconcile();
        assert_eq!(items(&d), Vec::new());
    }
    d.reconcile();
    let raised = items(&d);
    assert_eq!(raised.len(), 1, "{raised:#?}");
    assert_eq!(raised[0].status, InboxStatus::Open);
    assert_eq!(raised[0].evidence["streak"], 3);
    assert!(
        raised[0].evidence["input"]
            .as_str()
            .unwrap()
            .contains("session lease"),
        "{raised:#?}"
    );
    std::fs::remove_file(&path).unwrap();
    d.reconcile();
    let ended = items(&d);
    assert_eq!(ended.len(), 1);
    assert!(ended[0].ended_ms.is_some(), "{ended:#?}");
    assert_eq!(ended[0].status, InboxStatus::Resolved);
}

/// The disk setup of A42 and A43: lines at 1, 2, and 3 MiB with the whole fixture one volume
/// read under the warning line, `disk-reclaim` and the owner granted, and `[sessions]` with
/// these bounds covering the fixture. The main worktree holds a regenerable root
/// `node_modules` and a package's.
fn pressured(linger_s: u64, quiet_s: u64) -> disk_support::Disk {
    let grant = "[unit.disk-reclaim]\nact = true\n[act]\nexecute = { owner = [\"reclaim_dir\"], \
                 \"unit:disk-reclaim\" = [\"reclaim_dir\"] }\n";
    let mut d = disk_support::Disk::with_host((1, 2, 3), grant);
    let host = sessions(&d.s, linger_s, quiet_s);
    d.s.scene.observe_config(&host);
    plan_support::write(&d.s.main, "node_modules/react/index.js", "root");
    plan_support::write(&d.s.main, "packages/a/node_modules/lib/index.js", "package");
    d.fixture_reads(&[Some(3 * disk_support::MIB / 2)]);
    d
}

/// `session-start`, then `session-end`, for one session with its `cwd` in `dir`, run by the
/// test: their agent, the test process, is not `claude`, so the entry has none, as when the
/// agent was released.
fn between_turns(d: &disk_support::Disk, dir: &Path) {
    for event in ["session-start", "session-end"] {
        let input = input(
            if event == "session-start" {
                "SessionStart"
            } else {
                "SessionEnd"
            },
            "s-turns",
            dir,
            None,
        );
        let output = output_with_stdin(
            d.s.cardinal().args(["hook", "claude", event]),
            input.to_string().as_bytes(),
        );
        assert_eq!(output.status.code(), Some(0), "{output:?}");
    }
}

/// Asserts `pass` listed and ran none of the main worktree's targets and blocked both under
/// `not_in_use`, and that the owner's granted `act` on the root one reports `rejected`
/// (`not_in_use`) and leaves it; returns the detail of that `not_in_use`.
fn assert_refused(d: &disk_support::Disk, pass: &cardinal::contract::Reconcile) -> String {
    let report = disk_support::report(pass);
    assert_eq!(report.targets, Vec::new(), "{report:#?}");
    let blocked = disk_support::blocked(report, d.s.fx.root());
    for target in ["repo/node_modules", "repo/packages/a/node_modules"] {
        assert!(
            blocked.contains(&(target.to_owned(), "not_in_use".to_owned())),
            "{target}: {blocked:?}"
        );
    }
    let plan =
        d.s.plan(PlanKind::ReclaimDir, &d.s.main.join("node_modules"));
    let result = d.s.act(&plan.id, &live_support::owner()).unwrap();
    assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
    assert_eq!(result.reasons, ["not_in_use"]);
    assert!(d.s.main.join("node_modules/react/index.js").exists());
    in_use_check(&result.checks)
}

/// The detail of `not_in_use` among `checks`.
fn in_use_check(checks: &[cardinal::contract::Check]) -> String {
    checks
        .iter()
        .find(|c| c.name == "not_in_use")
        .map_or_else(String::new, |c| c.detail.clone())
}

/// Moves the modification time of every entry under `dir` and `dir` itself an hour into the
/// past, never through a link.
fn backdate_tree(dir: &Path) {
    for item in std::fs::read_dir(dir).unwrap() {
        let path = item.unwrap().path();
        let meta = std::fs::symlink_metadata(&path).unwrap();
        if meta.is_dir() {
            backdate_tree(&path);
        } else if meta.is_file() {
            backdate(&path);
        }
    }
    backdate(dir);
}

/// A42, the report week's case: a session in the main worktree sent `SessionEnd` less than
/// `linger_s` before a pass under pressure, with no process of it left and the worktree
/// quiet; the pass lists none of its targets, and the owner's `act` is refused.
#[test]
fn session_between_turns_refused() {
    let d = pressured(3600, 1);
    between_turns(&d, &d.s.main.join("packages/a"));
    wait_out(1);
    let pass = d.reconcile();
    let detail = assert_refused(&d, &pass);
    assert!(detail.contains("session s-turns"), "{detail}");
}

/// A42, without hooks: no lease file and a commit in the main worktree within `quiet_s`, as
/// the reflog showed, every other signal (the targets, the committed file, the index) an hour
/// old: the pass lists none of its targets, and the owner's `act` is refused naming the
/// reflog. The reflog's mtime is set an hour ahead, which counts as now, so a slow run reads
/// it as recent too.
#[test]
fn unhooked_commit_refused() {
    let d = pressured(1, 3600);
    d.s.fx
        .commit_file(&d.s.main, "src/main.rs", "fn main() { }\n", "edit");
    backdate_tree(&d.s.main);
    std::fs::File::open(d.s.main.join(".git/logs/HEAD"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() + Duration::from_hours(1))
        .unwrap();
    let pass = d.reconcile();
    let detail = assert_refused(&d, &pass);
    assert!(detail.contains("logs/HEAD"), "{detail}");
}

/// A43: with both bounds at 2 s and waited out, the case of A42 and a second worktree no
/// session touched are executed in one pass, and the pass's refresh prunes the expired lease
/// file.
#[test]
fn quiescent_control_executes() {
    let d = pressured(2, 2);
    let other = d.s.worktree("untouched");
    plan_support::write(&other, "node_modules/x/index.js", "x");
    between_turns(&d, &d.s.main.join("packages/a"));
    let lease = d.s.state.lease(&state::worktree_key(
        &dunce::canonicalize(&d.s.main).unwrap(),
    ));
    assert!(lease.exists());
    wait_out(2);
    let pass = d.reconcile();
    let report = disk_support::report(&pass);
    let executed: Vec<String> = disk_support::targets(report, d.s.fx.root());
    for target in [
        "repo/node_modules",
        "repo/packages/a/node_modules",
        "untouched/node_modules",
    ] {
        assert!(
            executed.contains(&target.to_owned()),
            "{target}: {report:#?}"
        );
    }
    assert!(
        report
            .targets
            .iter()
            .all(|t| t.status == Some(ActStatus::Executed)),
        "{report:#?}"
    );
    assert!(!d.s.main.join("node_modules").exists());
    assert!(!other.join("node_modules").exists());
    assert!(!lease.exists(), "the expired lease file is pruned");
}
