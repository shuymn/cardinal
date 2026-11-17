//! A fake of the observation and admission boundary of `plan` and `act` (`plan::Sources`),
//! for the plan, act, and explain tests until they run against the real `observe` and
//! `admission` modules (`docs/architecture.md` section 14.1).
//!
//! The facts live in a JSON file that every call reads again, so a test can change them while
//! an `act` is paused at a test point. HEAD and branch are read from the worktree's git files
//! on disk (no git process), so a branch switch or a new commit made through the fixture is
//! seen as it would be by git. `act` runs in a helper process (this test binary re-run in the
//! custom helper mode `act`) when a test needs pause points.
//!
//! Every child starts through the spawn API of `cardinal_test_support` in a scope of the setup,
//! and every `act` first checks that its plan's target lies inside the fixture.

#![allow(
    dead_code,
    reason = "each test crate uses a different subset of the helpers"
)]

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use cardinal::act::{self, ActOptions};
use cardinal::config::Config;
use cardinal::contract::{
    ActResult, Error, ErrorCode, Facts, GhStatus, HostInfo, IgnoredClass, IgnoredEntry, InUse,
    Inventory, InventorySchema, MergeEvidence, Os, Plan, ReclaimSummary, Repo, Tri, Worktree,
};
use cardinal::observe::WorktreeObservation;
use cardinal::plan::Sources;
use cardinal::state::StateDir;
use cardinal_test_support::Command;
use cardinal_test_support::fixture::{BareRepo, Fixture};
use serde::{Deserialize, Serialize};

use crate::support::cardinal::Dirs;
use crate::support::helper::Mode;

/// One observed worktree with its repository.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Observed {
    pub repo: Repo,
    pub worktree: Worktree,
}

/// Everything the fake reports.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct World {
    pub worktrees: Vec<Observed>,
    /// Reservations by worktree key.
    pub in_use: BTreeMap<String, InUse>,
    /// When set, every observation fails with this message.
    pub observe_error: Option<String>,
}

/// [`Sources`] backed by a [`World`] file.
#[derive(Clone, Debug)]
pub struct FakeSources {
    pub file: PathBuf,
}

impl FakeSources {
    pub fn new(file: &Path, world: &World) -> Self {
        let fake = Self {
            file: file.to_path_buf(),
        };
        fake.write(world);
        fake
    }

    pub fn read(&self) -> World {
        serde_json::from_slice(&std::fs::read(&self.file).expect("read the world"))
            .expect("parse the world")
    }

    pub fn write(&self, world: &World) {
        std::fs::write(&self.file, serde_json::to_vec(world).unwrap()).expect("write the world");
    }

    /// Changes the facts of the worktree at `path`.
    pub fn update(&self, path: &Path, change: impl FnOnce(&mut Worktree)) {
        let mut world = self.read();
        let observed = world
            .worktrees
            .iter_mut()
            .find(|o| Path::new(&o.worktree.path) == path)
            .expect("an observed worktree");
        change(&mut observed.worktree);
        self.write(&world);
    }
}

impl Sources for FakeSources {
    fn observe_worktree(&self, path: &Path) -> Result<WorktreeObservation, Error> {
        let world = self.read();
        if let Some(message) = world.observe_error {
            return Err(Error::new(ErrorCode::Internal, message));
        }
        let observed = world
            .worktrees
            .into_iter()
            .filter(|o| path.starts_with(&o.worktree.path))
            .max_by_key(|o| o.worktree.path.len())
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "not in an observed worktree"))?;
        let root = PathBuf::from(&observed.worktree.path);
        if !root.exists() {
            return Err(Error::new(ErrorCode::NotFound, "the worktree is gone"));
        }
        let head = read_head(&root);
        let mut worktree = observed.worktree;
        worktree.head = head.head;
        worktree.branch = head.branch;
        Ok(WorktreeObservation {
            repo: observed.repo,
            worktree,
        })
    }

    fn inventory(&self, _repos: &[PathBuf]) -> Result<Inventory, Error> {
        let world = self.read();
        let mut repos: Vec<Repo> = Vec::new();
        for observed in world.worktrees {
            let mut worktree = observed.worktree;
            let head = read_head(Path::new(&worktree.path));
            worktree.head = head.head;
            worktree.branch = head.branch;
            match repos
                .iter_mut()
                .find(|r| r.common_dir == observed.repo.common_dir)
            {
                Some(repo) => repo.worktrees.push(worktree),
                None => repos.push(Repo {
                    worktrees: vec![worktree],
                    ..observed.repo
                }),
            }
        }
        Ok(Inventory {
            schema: InventorySchema,
            observed_ms: 0,
            host: HostInfo {
                id: "0000000000000000".into(),
                hostname: "fake".into(),
                os: Os::Linux,
                arch: "fake".into(),
                boot_s: 0,
            },
            budget: BTreeMap::new(),
            reserved: BTreeMap::new(),
            reservations: Vec::new(),
            repos,
            outlived: Vec::new(),
            errors: Vec::new(),
        })
    }

    fn in_use(&self, worktree_key: &str) -> Result<InUse, Error> {
        Ok(self
            .read()
            .in_use
            .get(worktree_key)
            .cloned()
            .unwrap_or_default())
    }
}

/// HEAD and branch of a worktree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Head {
    pub head: Option<String>,
    pub branch: Option<String>,
}

/// HEAD and branch from the git files of the worktree at `root`, as git resolves them for a
/// fixture (loose refs; fixtures never pack refs).
pub fn read_head(root: &Path) -> Head {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let Ok(text) = std::fs::read_to_string(&dot_git) else {
            return Head::default();
        };
        let Some(dir) = text.strip_prefix("gitdir:") else {
            return Head::default();
        };
        root.join(dir.trim())
    };
    let common = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(rel) => git_dir.join(rel.trim()),
        Err(_) => git_dir.clone(),
    };
    let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) else {
        return Head::default();
    };
    let head = head.trim();
    match head.strip_prefix("ref: ") {
        Some(reference) => Head {
            head: std::fs::read_to_string(common.join(reference))
                .ok()
                .map(|oid| oid.trim().to_owned()),
            branch: reference.strip_prefix("refs/heads/").map(str::to_owned),
        },
        None => Head {
            head: Some(head.to_owned()),
            branch: None,
        },
    }
}

/// A worktree as observation reports one that meets every reclaim condition: clean, pushed,
/// with an upstream, no open PR, HEAD an ancestor of the base, not managed.
pub fn reclaimable(main: &Path, worktree: &Path, ignored: &[(&str, IgnoredClass)]) -> Observed {
    let is_main = main == worktree;
    Observed {
        repo: Repo {
            common_dir: main.join(".git").display().to_string(),
            main_worktree: main.display().to_string(),
            profiles: Vec::new(),
            github: Some("owner/repo".into()),
            base: None,
            gh: GhStatus::Ok,
            gh_error: None,
            worktrees: Vec::new(),
        },
        worktree: Worktree {
            path: worktree.display().to_string(),
            main: is_main,
            head: None,
            branch: None,
            upstream: Some("origin/feature".into()),
            managed_by: None,
            facts: Facts {
                dirty: Tri::No,
                has_upstream: Tri::Yes,
                unpushed: Tri::No,
                open_pr: Tri::No,
                merge_evidence: MergeEvidence::BaseAncestor,
                merged_pr: None,
                in_use: InUse::default(),
            },
            ignored: ignored
                .iter()
                .map(|(rel, class)| IgnoredEntry {
                    path: worktree.join(rel).display().to_string(),
                    class: *class,
                    allocated_bytes: None,
                })
                .collect(),
            reclaim: ReclaimSummary {
                candidate: !is_main,
                failed: Vec::new(),
                unknown: Vec::new(),
            },
            allocated_bytes: None,
        },
    }
}

/// The generic profile every plan and act test uses.
pub const PROFILE: &str = r#"
[data]
regenerable = ["node_modules", "target", ".DS_Store"]
protected = ["fuzz/corpus", "fuzz/artifacts", "proptest-regressions"]
"#;

/// Writes the generic profile (and `host_toml`, if any) into `config_dir` and loads it.
pub fn config(config_dir: &Path, host_toml: Option<&str>) -> Config {
    std::fs::create_dir_all(config_dir.join("repos")).unwrap();
    std::fs::write(config_dir.join("repos/generic.toml"), PROFILE).unwrap();
    if let Some(host) = host_toml {
        std::fs::write(config_dir.join("host.toml"), host).unwrap();
    }
    Config::load(config_dir).expect("load the test config")
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

/// What a helper process needs to run `act` against the fake.
#[derive(Debug, Serialize, Deserialize)]
struct ActRequest {
    /// The fixture root every plan target must lie in.
    root: PathBuf,
    config: PathBuf,
    state: PathBuf,
    world: PathBuf,
    plan: String,
    dry_run: bool,
    owner_approved: bool,
    caller: String,
    out: PathBuf,
}

/// The custom helper mode that runs `act` against the fake.
const ACT_MODE: &str = "act";

/// A command that runs `act` against the fake in a helper process (this test binary, whose
/// `helper` test calls [`run_act_if_requested`]) in the fixture root; the result is written
/// to `out`. Panics if the plan targets anything outside the fixture.
pub fn act_command(
    fx: &Fixture,
    config_dir: &Path,
    state: &StateDir,
    fake: &FakeSources,
    plan: &str,
    options: &ActOptions,
    out: &Path,
) -> Command {
    assert_plans_inside(fx.root(), state, plan);
    let request = ActRequest {
        root: fx.root().to_path_buf(),
        config: config_dir.to_path_buf(),
        state: state.root().to_path_buf(),
        world: fake.file.clone(),
        plan: plan.to_owned(),
        dry_run: options.dry_run,
        owner_approved: options.owner_approved,
        caller: options.caller.to_string(),
        out: out.to_path_buf(),
    };
    let mode = Mode::Custom {
        name: ACT_MODE,
        arg: serde_json::to_string(&request).unwrap(),
    };
    crate::support::helper::command(fx.scope(), &mode, fx.root())
}

/// Runs the `act` a helper process was started for, then exits; returns at once otherwise.
pub fn run_act_if_requested() {
    let Some(request) = crate::support::helper::custom_request(ACT_MODE) else {
        return;
    };
    let request: ActRequest = serde_json::from_str(&request).expect("parse the act request");
    let config = Config::load(&request.config).expect("load the config");
    let state = StateDir::open(&request.state).expect("open the state dir");
    assert_plans_inside(&request.root, &state, &request.plan);
    let fake = FakeSources {
        file: request.world,
    };
    let options = ActOptions {
        dry_run: request.dry_run,
        owner_approved: request.owner_approved,
        caller: request.caller.parse().expect("a caller"),
    };
    let result = act::execute_with(&config, &state, &fake, &request.plan, &options);
    let doc = match result {
        Ok(result) => serde_json::to_value(result).unwrap(),
        Err(err) => serde_json::to_value(err.to_doc()).unwrap(),
    };
    std::fs::write(&request.out, serde_json::to_vec(&doc).unwrap()).expect("write the result");
    std::process::exit(0);
}

/// Reads the result a helper process wrote.
pub fn read_result(out: &Path) -> ActResult {
    serde_json::from_slice(&std::fs::read(out).expect("read the act result"))
        .expect("an act result")
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

/// A main repository with a bare `origin`, cardinal's config and state dirs, and the fake.
pub struct Setup {
    pub fx: Fixture,
    pub dirs: Dirs,
    pub config: Config,
    pub state: StateDir,
    pub fake: FakeSources,
    pub main: PathBuf,
    pub origin: BareRepo,
}

/// What the main repository ignores, so worktrees stay clean around their ignored data.
const GITIGNORE: &str = "node_modules/\ntarget/\n.DS_Store\nfuzz/corpus/\nfuzz/artifacts/\n";

impl Setup {
    pub fn new() -> Self {
        Self::with_host(None)
    }

    /// A setup whose `host.toml` is `host_toml`.
    pub fn with_host(host_toml: Option<&str>) -> Self {
        let fx = Fixture::new();
        let main = fx.init("main");
        fx.commit_file(&main, ".gitignore", GITIGNORE, "ignore build output");
        fx.commit_file(&main, "src/main.rs", "fn main() {}\n", "start");
        let origin = fx.init_bare("origin.git");
        fx.push(&main, &origin, "main");
        let dirs = Dirs::new();
        let config = config(dirs.config(), host_toml);
        let state = StateDir::open(dirs.state()).expect("open the state dir");
        let fake = FakeSources::new(&fx.path("world.json"), &World::default());
        Self {
            fx,
            dirs,
            config,
            state,
            fake,
            main,
            origin,
        }
    }

    /// A linked worktree `name` on a new pushed branch `name` with one tracked file, observed
    /// as reclaimable with the given ignored entries.
    pub fn worktree(&self, name: &str, ignored: &[(&str, IgnoredClass)]) -> PathBuf {
        let wt = self.fx.worktree_add(&self.main, name, name);
        self.fx
            .commit_file(&wt, "src/lib.rs", &format!("// {name}\n"), "work");
        self.fx.push(&wt, &self.origin, name);
        let mut world = self.fake.read();
        world.worktrees.push(reclaimable(&self.main, &wt, ignored));
        self.fake.write(&world);
        wt
    }

    /// Creates a plan through the fake; panics if it cannot be created.
    pub fn plan(
        &self,
        kind: cardinal::contract::PlanKind,
        target: &Path,
    ) -> cardinal::contract::Plan {
        cardinal::plan::create_with(&self.config, &self.state, &self.fake, kind, target)
            .expect("create the plan")
    }

    /// Runs `act` in this process through the fake; panics first if the plan targets
    /// anything outside the fixture.
    pub fn act(&self, plan: &str, options: &ActOptions) -> Result<ActResult, Error> {
        assert_plans_inside(self.fx.root(), &self.state, plan);
        act::execute_with(&self.config, &self.state, &self.fake, plan, options)
    }

    /// The `cardinal` binary on this setup's state and config, in the fixture root.
    pub fn cardinal(&self) -> Command {
        let mut command = self
            .fx
            .command(env!("CARGO_BIN_EXE_cardinal"), self.fx.root());
        command
            .env("CARDINAL_STATE_DIR", self.dirs.state())
            .env("CARDINAL_CONFIG_DIR", self.dirs.config());
        command
    }

    /// A helper process running `act` through the fake, writing its result to `out`.
    pub fn act_command(&self, plan: &str, options: &ActOptions, out: &Path) -> Command {
        act_command(
            &self.fx,
            self.dirs.config(),
            &self.state,
            &self.fake,
            plan,
            options,
            out,
        )
    }
}

/// `act` options of the arbiter.
pub fn arbiter() -> ActOptions {
    ActOptions {
        dry_run: false,
        owner_approved: false,
        caller: cardinal::contract::Caller::Arbiter,
    }
}
