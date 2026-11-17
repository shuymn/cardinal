//! A repository for the plan, act, and explain tests, observed by the real `observe` and
//! `admission` modules through the `cardinal` binary (`docs/architecture.md` sections 13.3 and
//! 14.1): a main worktree with a bare remote it pushes to, `origin` reported as a GitHub
//! URL with its remote-tracking refs written as a push and fetch would leave them, and the
//! fake `gh` of [`Scene`] answering for GitHub.
//!
//! [`Setup::worktree`] makes a linked worktree that meets every reclaim condition: clean,
//! pushed with an upstream, no open pull request, and its HEAD the head of a merged pull
//! request. A test changes a fact through the fixture (a commit, a branch switch, a new
//! file), through gh's answers, or with a real reservation, never by rewriting what
//! observation reports. `plan` and `act` run as the `cardinal` binary, which uses
//! `plan::Real`; every `act` first checks that its plan's target lies inside the fixture.

#![allow(
    dead_code,
    reason = "each test crate uses a different subset of the helpers"
)]

use std::cell::Cell;
use std::collections::BTreeMap;
use std::ops::Deref;
use std::path::{Component, Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use cardinal::act::ActOptions;
use cardinal::contract::{ActResult, Error, ErrorDoc, Plan, PlanKind, PlanList};
use cardinal::state::StateDir;
use cardinal_test_support::fixture::BareRepo;
use cardinal_test_support::{Command, Scope};

use crate::observe_common::{Scene, merged_json};
use crate::support::testpoints::TestPoints;

/// The generic profile every plan and act test uses.
pub const PROFILE: &str = r#"
[data]
regenerable = ["node_modules", "target", ".DS_Store"]
protected = ["fuzz/corpus", "fuzz/artifacts", "proptest-regressions"]
"#;

/// What the main repository ignores, so worktrees stay clean around their ignored data.
const GITIGNORE: &str = "node_modules/\ntarget/\n.DS_Store\nfuzz/corpus/\nfuzz/artifacts/\n";

/// The scene, its main worktree, the bare remote branches are pushed to, and the state dir.
pub struct Setup {
    pub scene: Scene,
    pub main: PathBuf,
    pub origin: BareRepo,
    pub state: StateDir,
    /// The number of the next merged pull request gh reports.
    next_pr: Cell<u64>,
}

impl Deref for Setup {
    type Target = Scene;

    fn deref(&self) -> &Scene {
        &self.scene
    }
}

impl Setup {
    pub fn new() -> Self {
        Self::with_host("")
    }

    /// A setup whose `host.toml` has `host` after its `[observe]` table: more `[observe]`
    /// keys first, then other tables.
    pub fn with_host(host: &str) -> Self {
        let mut scene = Scene::new();
        if !host.is_empty() {
            scene.observe_config(host);
        }
        scene.profile("generic", PROFILE);
        let main = scene.repo.clone();
        scene
            .fx
            .commit_file(&main, ".gitignore", GITIGNORE, "ignore build output");
        scene
            .fx
            .commit_file(&main, "src/main.rs", "fn main() {}\n", "start");
        let origin = scene.fx.init_bare("origin.git");
        let state = scene.state();
        let setup = Self {
            scene,
            main,
            origin,
            state,
            next_pr: Cell::new(1),
        };
        setup.publish(&setup.main, "main");
        setup
    }

    /// Pushes `branch` of `dir` to the bare remote and records it as pushed: `origin/<branch>`
    /// at its HEAD, and that ref as its upstream.
    pub fn publish(&self, dir: &Path, branch: &str) {
        self.fx.push(dir, &self.origin, branch);
        self.scene.push(dir, branch);
    }

    /// Makes gh report the HEAD of `dir` as the head of a merged pull request; returns it.
    pub fn merge(&self, dir: &Path) -> String {
        let head = self.head(dir);
        let number = self.next_pr.get();
        self.next_pr.set(number + 1);
        self.set_merged(&head, &merged_json(number, &head));
        head
    }

    /// A linked worktree `name` on a new branch `name` with one tracked file of its own,
    /// pushed, and merged per gh: it meets every reclaim condition.
    pub fn worktree(&self, name: &str) -> PathBuf {
        let wt = self.fx.worktree_add(&self.main, name, name);
        self.fx
            .commit_file(&wt, "src/lib.rs", &format!("// {name}\n"), "work");
        self.publish(&wt, name);
        self.merge(&wt);
        wt
    }

    /// `cardinal plan reclaim-worktree|reclaim-dir TARGET`.
    pub fn try_plan(&self, kind: PlanKind, target: &Path) -> Result<Plan, Error> {
        let sub = match kind {
            PlanKind::ReclaimWorktree => "reclaim-worktree",
            PlanKind::ReclaimDir => "reclaim-dir",
        };
        let output = self
            .cardinal()
            .args(["plan", sub])
            .arg(target)
            .output()
            .expect("run cardinal plan");
        document(&output, &[0])
    }

    /// [`Setup::try_plan`]; panics if the plan cannot be created.
    pub fn plan(&self, kind: PlanKind, target: &Path) -> Plan {
        self.try_plan(kind, target)
            .unwrap_or_else(|err| panic!("plan {kind:?} {}: {err:?}", target.display()))
    }

    /// `cardinal plan --repo MAIN`.
    pub fn plan_all(&self) -> PlanList {
        let output = self
            .cardinal()
            .arg("plan")
            .arg("--repo")
            .arg(&self.main)
            .output()
            .expect("run cardinal plan");
        document(&output, &[0]).expect("a plan list")
    }

    /// `cardinal act PLAN` with `options`; panics first if the plan targets anything outside
    /// the fixture.
    pub fn act_command(&self, plan: &str, options: &ActOptions) -> Command {
        assert_plans_inside(self.fx.root(), &self.state, plan);
        let mut command = self.cardinal();
        command.args(["act", plan, "--caller", &options.caller.to_string()]);
        if options.dry_run {
            command.arg("--dry-run");
        }
        if options.owner_approved {
            command.arg("--owner-approved");
        }
        command
    }

    /// Runs [`Setup::act_command`] and returns its result: exit 0 for `dry_run`, 3 for a
    /// refusal, 1 with `cardinal.error.v1` for an error.
    pub fn act(&self, plan: &str, options: &ActOptions) -> Result<ActResult, Error> {
        let output = self
            .act_command(plan, options)
            .output()
            .expect("run cardinal act");
        act_result(&output)
    }

    /// Runs [`Setup::act_command`] paused at `point`, calls `change` once it has reached it,
    /// releases it, and returns its result.
    pub fn act_at(
        &self,
        plan: &str,
        options: &ActOptions,
        point: &str,
        change: impl FnOnce(),
    ) -> ActResult {
        // A test-point directory of its own, so no release file of an earlier race is left.
        let scope = Scope::new();
        let points = TestPoints::new(&scope);
        let mut command = self.act_command(plan, options);
        points.apply(&mut command, &[point], None);
        let child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start cardinal act");
        points.wait_reached(point, child.id(), Duration::from_secs(20));
        change();
        points.release(point);
        act_result(&child.wait_with_output().expect("wait for cardinal act"))
            .expect("an act result")
    }
}

/// The `act` result an `act` process printed.
pub fn act_result(output: &Output) -> Result<ActResult, Error> {
    let result: Result<ActResult, Error> = document(output, &[0, 3]);
    if let Ok(result) = &result {
        let code = if result.status.is_refusal() { 3 } else { 0 };
        assert_eq!(output.status.code(), Some(code), "{result:#?}");
    }
    result
}

/// The document a `cardinal` process printed to stdout when it exited with one of `codes`,
/// else the error it printed with exit 1.
fn document<T: serde::de::DeserializeOwned>(output: &Output, codes: &[i32]) -> Result<T, Error> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let code = output.status.code();
    if code == Some(1) {
        let doc: ErrorDoc = serde_json::from_str(&stdout)
            .unwrap_or_else(|err| panic!("an error document: {err}: {stdout}"));
        return Err(Error::new(doc.error.code, doc.error.message));
    }
    assert!(
        code.is_some_and(|code| codes.contains(&code)),
        "exit {code:?}: {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_str(&stdout).unwrap_or_else(|err| panic!("a document: {err}: {stdout}")))
}

/// `act` options of the arbiter.
pub fn arbiter() -> ActOptions {
    ActOptions {
        dry_run: false,
        owner_approved: false,
        caller: cardinal::contract::Caller::Arbiter,
    }
}

/// Panics unless `path` is an absolute path inside the fixture root `root`, without `..`.
pub fn assert_inside(root: &Path, path: &Path, what: &str) {
    assert!(
        path.is_absolute()
            && !path.components().any(|c| c == Component::ParentDir)
            && path.starts_with(root),
        "refusing: {what} {} is outside the fixture root {}",
        path.display(),
        root.display()
    );
}

/// Panics unless every plan in `state` whose ID starts with `prefix` targets a worktree and a
/// path inside the fixture root `root`. A prefix no plan has passes: `act` then finds none.
pub fn assert_plans_inside(root: &Path, state: &StateDir, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(state.plans_dir()) else {
        return;
    };
    for entry in entries {
        let id = entry.unwrap().file_name().to_string_lossy().into_owned();
        if !id.starts_with(prefix) {
            continue;
        }
        let Ok(bytes) = std::fs::read(state.plan_file(&id)) else {
            continue;
        };
        let plan: Plan = serde_json::from_slice(&bytes).expect("a stored plan");
        assert_inside(root, Path::new(&plan.target.worktree), "act worktree");
        assert_inside(root, Path::new(&plan.target.path), "act target");
    }
}

/// Every entry below `root`, `.git` included, without following symbolic links: path
/// relative to `root`, then type, length, and modification time for files and links.
pub fn listing(root: &Path) -> BTreeMap<PathBuf, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let rel = path.strip_prefix(root).unwrap().to_path_buf();
            if meta.is_dir() {
                out.insert(rel, "dir".into());
                walk(root, &path, out);
            } else {
                let kind = if meta.is_symlink() { "link" } else { "file" };
                out.insert(
                    rel,
                    format!("{kind} {} {:?}", meta.len(), meta.modified().ok()),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// Writes `contents` to `root/rel`, creating its directories.
pub fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}
