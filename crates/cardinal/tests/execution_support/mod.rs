//! Executions as a crash leaves them, for the tests of the deletion and of recovery
//! (`docs/architecture.md` section 13.7). Unit X1b's library is called by no command, so these
//! tests write an execution's records and trash entry the way an `act` that died leaves them,
//! and run recovery, or a claim's publication, through the library in a helper process
//! (section 13.3), whose pause points and fakes come from its environment (section 13.1).
//!
//! [`Execution::new`] makes a worktree directory holding a target and plans it with a new plan
//! ID; [`Execution::commit`] leaves it as `act` leaves it at `act.committed`: the execution
//! directory with its `act.lock`, the manifest of the target as a walk reads it, a
//! `committed` claim, and the target moved into the trash entry's `t` beside an empty hold.
//! [`Execution::take`] moves an entry into the hold, as a crash at `act.taken` leaves it.

#![allow(
    dead_code,
    reason = "each test crate uses a different subset of the helpers"
)]

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cardinal::act::delete::rel_path;
use cardinal::act::recover::{self, Recovered};
use cardinal::contract::{
    ActClaim, ActClaimSchema, ChainDir, ClaimRepo, ClaimStage, ClaimTarget, ClaimWorktree,
    EntryPath, EntryType, ManifestEntry, RootId,
};
use cardinal::plan::EntryKind;
use cardinal::state::StateDir;
use cardinal_test_support::{Command, Scope};

use crate::support::helper::{self, Mode};
use crate::support::testpoints::TestPoints;

/// The generic profile of every execution test.
pub const PROFILE: &str =
    "[data]\nregenerable = [\"node_modules\"]\nprotected = [\"fuzz/corpus\"]\n";

/// The custom helper mode that runs recovery.
const RECOVER_MODE: &str = "recover";

/// The custom helper mode that publishes a claim.
const PUBLISH_MODE: &str = "publish";

/// How long one recovery may take before the test fails.
pub const LIMIT: Duration = Duration::from_secs(60);

/// An execution of one target in a worktree directory of its own scope.
pub struct Execution {
    pub scope: Scope,
    pub state: StateDir,
    pub config: PathBuf,
    pub worktree: PathBuf,
    /// The target relative to the worktree.
    pub target_rel: PathBuf,
    pub plan: String,
    pub points: TestPoints,
    pub caller: String,
    pub manifest: Vec<ManifestEntry>,
}

impl Execution {
    /// A worktree `w/` whose target `w/<target_rel>` holds `entries`: a name ending in `/` is a
    /// directory, any other a file holding its own name; the generic [`PROFILE`] configured.
    pub fn new(target_rel: &str, entries: &[&str]) -> Self {
        let scope = Scope::new();
        let state = StateDir::open(&scope.path("state")).expect("open the state dir");
        let config = scope.path("config");
        std::fs::create_dir_all(config.join("repos")).unwrap();
        std::fs::write(config.join("repos/app.toml"), PROFILE).unwrap();
        let worktree = scope.path("w");
        let target = worktree.join(target_rel);
        std::fs::create_dir_all(&target).unwrap();
        for entry in entries {
            let path = target.join(entry.trim_end_matches('/'));
            if entry.ends_with('/') {
                std::fs::create_dir_all(&path).unwrap();
            } else {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, entry.as_bytes()).unwrap();
            }
        }
        let points = TestPoints::new(&scope);
        Self {
            scope,
            state,
            config,
            worktree,
            target_rel: PathBuf::from(target_rel),
            plan: cardinal::state::new_id(),
            points,
            caller: "owner".to_owned(),
            manifest: Vec::new(),
        }
    }

    /// The target at its planned path.
    pub fn target(&self) -> PathBuf {
        self.worktree.join(&self.target_rel)
    }

    /// The trash entry.
    pub fn entry(&self) -> PathBuf {
        self.state.trash_entry(&self.plan)
    }

    /// `t`, the target as moved.
    pub fn t(&self) -> PathBuf {
        self.state.trash_target(&self.plan)
    }

    /// `h`, the hold.
    pub fn h(&self) -> PathBuf {
        self.state.trash_hold(&self.plan)
    }

    /// The manifest the walk of the target reads now, each entry indexed by its position.
    pub fn walk_manifest(&self) -> Vec<ManifestEntry> {
        let walk = cardinal::plan::walk(&self.worktree, &self.target()).expect("walk the target");
        walk.all()
            .enumerate()
            .map(|(i, entry)| {
                let rel = entry.rel.strip_prefix(&self.target_rel).unwrap();
                ManifestEntry {
                    i: i as u64,
                    path: EntryPath::Utf8(rel.to_str().unwrap().to_owned()),
                    dev: entry.dev,
                    ino: entry.ino,
                    kind: match entry.kind {
                        EntryKind::Dir => EntryType::Dir,
                        EntryKind::File => EntryType::File,
                        EntryKind::Symlink => EntryType::Symlink,
                        other => panic!("a {other:?} in the target"),
                    },
                    size: entry.size,
                    mtime_ns: i64::try_from(entry.mtime_ns).unwrap(),
                    nlink: entry.nlink,
                }
            })
            .collect()
    }

    /// The claim of this execution at `stage`.
    pub fn claim(&self, stage: ClaimStage) -> ActClaim {
        let root = std::fs::metadata(&self.worktree).unwrap();
        let mut chain = Vec::new();
        let mut at = self.worktree.clone();
        let names: Vec<_> = self.target_rel.iter().collect();
        for name in &names[..names.len() - 1] {
            at.push(name);
            let meta = std::fs::metadata(&at).unwrap();
            chain.push(ChainDir {
                name: name.to_str().unwrap().to_owned(),
                dev: meta.dev(),
                ino: meta.ino(),
            });
        }
        let root_entry = &self.manifest[0];
        ActClaim {
            schema: ActClaimSchema,
            plan_id: self.plan.clone(),
            caller: self.caller.clone(),
            stage,
            created_ms: 1,
            updated_ms: 2,
            worktree: ClaimWorktree {
                path: self.worktree.display().to_string(),
                root: RootId {
                    dev: root.dev(),
                    ino: root.ino(),
                },
            },
            repo: ClaimRepo {
                common_dir: self.worktree.join(".git").display().to_string(),
                remote: None,
            },
            chain,
            target: ClaimTarget {
                name: names[names.len() - 1].to_str().unwrap().to_owned(),
                dev: root_entry.dev,
                ino: root_entry.ino,
                kind: root_entry.kind,
            },
            manifest_entries: self.manifest.len() as u64,
        }
    }

    /// The execution directory with its `act.lock`, as `act` creates it.
    pub fn exclude(&self) {
        std::fs::create_dir_all(self.state.execution_dir(&self.plan)).unwrap();
        std::fs::write(self.state.execution_lock(&self.plan), b"").unwrap();
    }

    /// Leaves the execution as `act` leaves it at `act.committed`: its directory and
    /// `act.lock`, the manifest, the `committed` claim, the target moved to `t`, and an empty
    /// hold.
    pub fn commit(&mut self) {
        self.exclude();
        self.manifest = self.walk_manifest();
        recover::write_manifest(&self.state, &self.plan, &self.manifest).unwrap();
        let published = recover::publish_claim(&self.state, &self.claim(ClaimStage::Committed));
        assert!(
            matches!(published, Ok(recover::Published::Durable)),
            "{published:?}"
        );
        std::fs::create_dir_all(self.h()).unwrap();
        std::fs::rename(self.target(), self.t()).unwrap();
    }

    /// The manifest entry at `rel`, relative to the target.
    pub fn manifest_entry(&self, rel: &str) -> &ManifestEntry {
        self.manifest
            .iter()
            .find(|e| rel_path(e) == Path::new(rel))
            .unwrap_or_else(|| panic!("{rel} is not in the manifest"))
    }

    /// `h/<index>` of the entry at `rel`.
    pub fn held(&self, rel: &str) -> PathBuf {
        self.h().join(self.manifest_entry(rel).i.to_string())
    }

    /// Moves the entry at `rel` from `t` into the hold under its index, as a crash at
    /// `act.taken` leaves it.
    pub fn take(&self, rel: &str) {
        std::fs::rename(self.t().join(rel), self.held(rel)).unwrap();
    }

    /// A helper process that runs recovery with the caller's grant held or withdrawn and
    /// writes its outcome to the returned path; its pause points and fakes are set on it.
    pub fn recover_command(&self, granted: bool) -> (Command, PathBuf) {
        recover_command(
            &self.scope,
            &self.points,
            (&self.state, &self.config),
            &self.plan,
            granted,
        )
    }

    /// Runs recovery with the caller's grant held or withdrawn, to its end, and returns its
    /// outcome.
    pub fn recover(&self, granted: bool) -> serde_json::Value {
        let (mut command, out) = self.recover_command(granted);
        let output = command.output_within(LIMIT).expect("run recovery");
        assert!(output.status.success(), "recovery failed: {output:?}");
        outcome(&out)
    }

    /// Publishes `claim` in a helper process with this execution's fakes, and returns the
    /// outcome: `durable`, `not_synced`, or `error`, with `why`.
    pub fn publish(&self, claim: &ActClaim) -> serde_json::Value {
        let out = self
            .scope
            .path(&format!("outcome-{}.json", cardinal::state::new_id()));
        let request = serde_json::json!({
            "root": self.scope.root(),
            "state": self.state.root(),
            "claim": claim,
            "out": out,
        });
        let mode = Mode::Custom {
            name: PUBLISH_MODE,
            arg: request.to_string(),
        };
        let mut command = helper::command(&self.scope, &mode, self.scope.root());
        command.env("CARDINAL_TEST_DIR", self.points.dir());
        let output = command.output_within(LIMIT).expect("run the publication");
        assert!(
            output.status.success(),
            "the publication failed: {output:?}"
        );
        outcome(&out)
    }

    /// Every entry under the worktree, the trash, and the executions, with its contents:
    /// `dir`, the bytes of a file, or `-> target` for a link.
    pub fn listing(&self) -> BTreeMap<PathBuf, String> {
        let mut out = BTreeMap::new();
        for root in [
            self.worktree.clone(),
            self.state.trash_dir(),
            self.state.executions_dir(),
            self.scope.path("outside"),
        ] {
            list(self.scope.root(), &root, &mut out);
        }
        out
    }

    /// Makes the deleter's next batch due now (section 13.7's fake clock).
    pub fn advance_clock(&self) {
        std::fs::write(self.points.dir().join("clock"), b"60000").unwrap();
    }

    /// Makes the write of `kind` fail with `error` (`ENOSPC` or `EDQUOT`) `times` (`once` or
    /// `always`).
    pub fn fail_write(&self, kind: &str, error: &str, times: &str) {
        std::fs::write(
            self.points.dir().join("write-failures"),
            format!("{kind}\t{error}\t{times}\n"),
        )
        .unwrap();
    }

    /// Writes the state dir's reserve file.
    pub fn write_reserve(&self) {
        std::fs::write(self.state.reserve_file(), vec![0_u8; 64 * 1024]).unwrap();
    }

    /// The write log the helper processes wrote: one JSON object per operation.
    pub fn write_log(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(self.points.dir().join("writes.log"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

/// A helper process of `scope` that runs recovery of `plan` in the state dir `state` with the
/// configuration directory `config`, the caller's grant held or withdrawn, and the pause
/// points and fakes of `points`; it writes its outcome to the returned path.
pub fn recover_command(
    scope: &Scope,
    points: &TestPoints,
    (state, config): (&StateDir, &Path),
    plan: &str,
    granted: bool,
) -> (Command, PathBuf) {
    let out = scope.path(&format!("outcome-{}.json", cardinal::state::new_id()));
    let request = serde_json::json!({
        "root": scope.root(),
        "state": state.root(),
        "config": config,
        "plan": plan,
        "granted": granted,
        "out": out,
    });
    let mode = Mode::Custom {
        name: RECOVER_MODE,
        arg: request.to_string(),
    };
    let mut command = helper::command(scope, &mode, scope.root());
    command.env("CARDINAL_TEST_DIR", points.dir());
    (command, out)
}

/// Adds `dir` and everything below it to `out`, by path relative to `root`, without following
/// links.
fn list(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, String>) {
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return;
    };
    let rel = dir.strip_prefix(root).unwrap().to_path_buf();
    if meta.is_dir() {
        out.insert(rel, "dir".to_owned());
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        for name in names {
            list(root, &dir.join(name), out);
        }
    } else if meta.is_symlink() {
        let target = std::fs::read_link(dir).unwrap();
        out.insert(rel, format!("-> {}", target.display()));
    } else {
        out.insert(
            rel,
            String::from_utf8_lossy(&std::fs::read(dir).unwrap()).into_owned(),
        );
    }
}

/// The outcome a recovery helper wrote to `out`.
pub fn outcome(out: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(out).expect("the recovery's outcome")).unwrap()
}

/// Runs the recovery or the publication a helper process was started for, writes its outcome,
/// and exits; returns at once otherwise.
pub fn run_if_requested() {
    publish_if_requested();
    let Some(request) = helper::custom_request(RECOVER_MODE) else {
        return;
    };
    let request: serde_json::Value = serde_json::from_str(&request).expect("a recovery request");
    let path = |key: &str| PathBuf::from(request[key].as_str().expect(key));
    let root = path("root");
    for key in ["state", "config", "out"] {
        assert!(path(key).starts_with(&root), "{key} leaves the scope");
    }
    let state = StateDir::open(&path("state")).expect("open the state dir");
    let granted = request["granted"].as_bool().expect("granted");
    let grant = |_: &cardinal::config::Config, _: &cardinal::contract::Caller| {
        if granted {
            Ok(())
        } else {
            Err("the caller's grant was withdrawn".to_owned())
        }
    };
    let plan = request["plan"].as_str().expect("plan");
    let doc = match recover::recover(&state, &path("config"), plan, &grant) {
        Ok(recovered) => describe(recovered),
        Err(err) => serde_json::json!({"outcome": "error", "why": err.to_string()}),
    };
    write_outcome(&path("out"), &doc);
}

/// Publishes the claim a helper process was started for, writes its outcome, and exits;
/// returns at once otherwise.
fn publish_if_requested() {
    let Some(request) = helper::custom_request(PUBLISH_MODE) else {
        return;
    };
    let request: serde_json::Value = serde_json::from_str(&request).expect("a publication request");
    let path = |key: &str| PathBuf::from(request[key].as_str().expect(key));
    let root = path("root");
    for key in ["state", "out"] {
        assert!(path(key).starts_with(&root), "{key} leaves the scope");
    }
    let state = StateDir::open(&path("state")).expect("open the state dir");
    let claim: ActClaim = serde_json::from_value(request["claim"].clone()).expect("a claim");
    let doc = match recover::publish_claim(&state, &claim) {
        Ok(recover::Published::Durable) => serde_json::json!({"outcome": "durable"}),
        Ok(recover::Published::NotSynced(err)) => {
            serde_json::json!({"outcome": "not_synced", "why": err.to_string()})
        }
        Err(err) => serde_json::json!({"outcome": "error", "why": err.to_string()}),
    };
    write_outcome(&path("out"), &doc);
}

/// Writes `doc` to `out` whole and exits the helper process.
fn write_outcome(out: &Path, doc: &serde_json::Value) -> ! {
    let tmp = out.with_extension("tmp");
    std::fs::write(&tmp, doc.to_string()).expect("write the outcome");
    std::fs::rename(&tmp, out).expect("publish the outcome");
    std::process::exit(0);
}

/// `recovered` as JSON.
fn describe(recovered: Recovered) -> serde_json::Value {
    use serde_json::json;
    match recovered {
        Recovered::InProgress => json!({"outcome": "in_progress"}),
        Recovered::NotFound => json!({"outcome": "not_found"}),
        Recovered::Finished => json!({"outcome": "finished"}),
        Recovered::Absent => json!({"outcome": "absent"}),
        Recovered::Unreadable { error, trash } => {
            json!({"outcome": "unreadable", "why": error, "trash": trash})
        }
        Recovered::Stranded { why, trash } => {
            json!({"outcome": "stranded", "why": why, "trash": trash})
        }
        Recovered::Done(result) => json!({"outcome": "done", "result": result}),
    }
}
