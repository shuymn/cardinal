//! Live executions for unit X1c's tests (`docs/architecture.md` sections 9.7 and 13.7): a
//! granted owner's `act` on an ignored directory of a linked worktree that meets every
//! check, run as the `cardinal` binary through `tests/plan_support`, paused, failed, or
//! killed at the pause points of section 13.1.

#![allow(
    dead_code,
    reason = "each test crate uses a different subset of the helpers"
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use cardinal::act::ActOptions;
use cardinal::contract::{ActResult, ActStatus, Caller, Plan, PlanKind};
use cardinal::testpoint::{reached_file, release_file};
use cardinal_test_support::{Child, Scope};

use crate::plan_support::{self, RUN_LIMIT, Setup, write_synced};
use crate::support::testpoints::TestPoints;

/// `host.toml`'s grant to the owner of `reclaim_dir` plans.
pub const GRANT: &str = "[act]\nexecute = { owner = [\"reclaim_dir\"] }\n";

/// How long a test waits for a paused `act` to reach its point.
pub const WAIT: Duration = Duration::from_secs(30);

/// `act` options of the owner.
pub fn owner() -> ActOptions {
    ActOptions {
        dry_run: false,
        owner_approved: false,
        squash_merged: false,
        caller: Caller::Owner,
    }
}

/// A linked worktree whose ignored directory `rel` holds files, planned for reclamation.
pub struct Live {
    pub s: Setup,
    pub name: String,
    pub wt: PathBuf,
    pub rel: String,
    pub plan: Plan,
}

impl Live {
    /// A setup granting the owner, a linked worktree `name`, and `rel` holding `files`, each
    /// with its path below `rel` as contents, planned with every check passing.
    pub fn new(name: &str, rel: &str, files: &[&str]) -> Self {
        Self::with_host(GRANT, name, rel, files, |_, _| {})
    }

    /// [`Live::new`] with `host` in `host.toml`, and `prepare` run on the setup and the
    /// worktree before the files are written and the target planned.
    pub fn with_host(
        host: &str,
        name: &str,
        rel: &str,
        files: &[&str],
        prepare: impl FnOnce(&Setup, &Path),
    ) -> Self {
        Self::in_setup(Setup::with_host(host), name, rel, files, prepare)
    }

    /// [`Live::with_host`] in the setup `s`, made by the test.
    pub fn in_setup(
        s: Setup,
        name: &str,
        rel: &str,
        files: &[&str],
        prepare: impl FnOnce(&Setup, &Path),
    ) -> Self {
        let wt = s.worktree(name);
        // The worktree's setup and the files, an hour old, are idle without waiting out the
        // quiet period (section 9.8); what `prepare` writes keeps its own times.
        s.age_worktree(&wt);
        prepare(&s, &wt);
        for file in files {
            write_synced(&wt, &format!("{rel}/{file}"), file.as_bytes());
        }
        if !files.is_empty() {
            plan_support::age_tree(&wt.join(rel), Duration::from_hours(1));
        }
        let plan = s.plan(PlanKind::ReclaimDir, &wt.join(rel));
        assert!(plan.invariants_ok, "{:#?}", plan.checks);
        Self {
            s,
            name: name.to_owned(),
            wt,
            rel: rel.to_owned(),
            plan,
        }
    }

    pub fn target(&self) -> PathBuf {
        self.wt.join(&self.rel)
    }

    /// `<state>/trash/<plan>`.
    pub fn entry(&self) -> PathBuf {
        self.s.state.trash_entry(&self.plan.id)
    }

    /// The target as moved into the trash entry.
    pub fn t(&self) -> PathBuf {
        self.entry().join("t")
    }

    /// The worktree's administrative directory in the main repository's git dir.
    pub fn git_dir(&self) -> PathBuf {
        self.s.main.join(".git/worktrees").join(&self.name)
    }

    /// The worktree's index file.
    pub fn index(&self) -> PathBuf {
        self.git_dir().join("index")
    }

    /// Plans the target again, as a later pass or the owner would.
    pub fn replan(&mut self) {
        self.plan = self.s.plan(PlanKind::ReclaimDir, &self.target());
        assert!(self.plan.invariants_ok, "{:#?}", self.plan.checks);
    }

    /// The owner's `act` on the plan.
    pub fn act(&self) -> ActResult {
        self.s.act(&self.plan.id, &owner()).expect("an act result")
    }

    /// The owner's `act` paused at `point`, `change` called once it is there.
    pub fn act_at(&self, point: &str, change: impl FnOnce()) -> ActResult {
        self.s.act_at(&self.plan.id, &owner(), point, change)
    }

    /// Starts the owner's `act` paused at `pause` and failing at `fail`, with its test points.
    pub fn spawn(&self, pause: &[&str], fail: Option<&str>) -> Paused {
        let scope = Scope::new();
        let points = TestPoints::new(&scope);
        let mut command = self.s.act_command(&self.plan.id, &owner());
        points.apply(&mut command, pause, fail);
        let child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start cardinal act");
        Paused {
            _scope: scope,
            points,
            child,
        }
    }

    /// Starts the owner's `act` paused at `pause`, with the write of `kind` failing with
    /// `error` (`ENOSPC` or `EDQUOT`) `times` (`once` or `always`), section 13.7's fake.
    pub fn spawn_failing_write(
        &self,
        pause: &[&str],
        kind: &str,
        error: &str,
        times: &str,
    ) -> Paused {
        let scope = Scope::new();
        let points = TestPoints::new(&scope);
        std::fs::write(
            points.dir().join("write-failures"),
            format!("{kind}\t{error}\t{times}\n"),
        )
        .unwrap();
        let mut command = self.s.act_command(&self.plan.id, &owner());
        points.apply(&mut command, pause, None);
        let child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start cardinal act");
        Paused {
            _scope: scope,
            points,
            child,
        }
    }

    /// Writes the state dir's reserve file.
    pub fn write_reserve(&self) {
        std::fs::write(self.s.state.reserve_file(), vec![0_u8; 64 * 1024]).unwrap();
    }

    /// Kills the owner's `act` with SIGKILL once it reached `point`.
    pub fn kill_at(&self, point: &str) {
        let mut paused = self.spawn(&[point], None);
        paused.wait(point);
        paused.child.kill();
    }

    /// Every entry below the target as [`plan_support::listing`] reads it.
    pub fn files(&self) -> BTreeMap<PathBuf, String> {
        plan_support::listing(&self.target())
    }

    /// How many `reclaim_stranded` items the state's inbox holds.
    pub fn stranded_items(&self) -> usize {
        std::fs::read_dir(self.s.state.inbox_dir()).map_or(0, |dir| {
            dir.filter_map(Result::ok)
                .filter(|e| {
                    std::fs::read_to_string(e.path())
                        .is_ok_and(|text| text.contains("reclaim_stranded"))
                })
                .count()
        })
    }

    /// Writes `host.toml` with `host` after the scene's own settings, as `act` reads it next.
    pub fn write_host(&self, host: &str) {
        let path = self.s.config_dir.join("host.toml");
        let current = std::fs::read_to_string(&path).unwrap();
        let kept = current.split("[act]").next().expect("host.toml").to_owned();
        std::fs::write(&path, format!("{kept}{host}")).unwrap();
    }
}

/// An `act` process started with its test points.
pub struct Paused {
    _scope: Scope,
    pub points: TestPoints,
    pub child: Child,
}

impl Paused {
    /// Waits until the process reached `point`.
    pub fn wait(&self, point: &str) {
        self.points.wait_reached(point, self.child.id(), WAIT);
    }

    /// Releases `point`.
    pub fn release(&self, point: &str) {
        self.points.release(point);
    }

    /// Makes `point` pause again: its release and the process's reached file are removed, so
    /// the next time the process gets there it stops.
    pub fn rearm(&self, point: &str) {
        for file in [
            release_file(self.points.dir(), point),
            reached_file(self.points.dir(), point, self.child.id()),
        ] {
            std::fs::remove_file(file).unwrap();
        }
    }

    /// [`Paused::finish`], with whether the process reached `point` before it ended.
    pub fn finish_reached(self, points: &[&str], point: &str) -> (ActResult, bool) {
        for point in points {
            self.points.release(point);
        }
        let pid = self.child.id();
        let output = self
            .child
            .wait_with_output()
            .expect("wait for cardinal act");
        let reached = self.points.reached(point, pid);
        (
            plan_support::act_result(&output).expect("an act result"),
            reached,
        )
    }

    /// Releases every point in `points` and returns the result the process printed.
    pub fn finish(self, points: &[&str]) -> ActResult {
        self.finish_logged(points).0
    }

    /// [`Paused::finish`], with the write log the process kept (`state::logged`): one JSON
    /// object per operation.
    pub fn finish_logged(self, points: &[&str]) -> (ActResult, Vec<serde_json::Value>) {
        for point in points {
            self.points.release(point);
        }
        let output = self
            .child
            .wait_with_output()
            .expect("wait for cardinal act");
        let log = std::fs::read_to_string(self.points.dir().join("writes.log"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let result = plan_support::act_result(&output).expect("an act result");
        (result, log)
    }
}

/// Asserts that `result` restored the target: `status` with `reasons`, the target's files
/// `before` back in place, and no trash entry.
#[track_caller]
pub fn assert_restored(
    x: &Live,
    result: &ActResult,
    status: ActStatus,
    reasons: &[&str],
    before: &BTreeMap<PathBuf, String>,
) {
    assert_eq!(result.status, status, "{result:#?}");
    assert_eq!(result.reasons, reasons, "{result:#?}");
    assert_eq!(&x.files(), before, "{result:#?}");
    assert!(!x.entry().exists(), "{result:#?}");
    assert!(result.trash.is_none(), "{result:#?}");
}

/// How long one `cardinal` command may take.
pub const LIMIT: Duration = RUN_LIMIT;
