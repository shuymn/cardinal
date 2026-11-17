//! A3: observation writes nothing inside any repository, `.git` included
//! (`docs/architecture.md` sections 8.1 and 17, item 8).

mod observe_common;
mod support;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use observe_common::{Scene, merged_json};

/// Every entry below `dir` with its type, length, modification time, and file contents.
type Listing = BTreeMap<String, (bool, u64, SystemTime, Vec<u8>)>;

fn listing(dir: &Path) -> Listing {
    let mut out = Listing::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let rel = path
                .strip_prefix(dir)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let bytes = if meta.is_file() {
                std::fs::read(&path).unwrap()
            } else {
                Vec::new()
            };
            if meta.is_dir() {
                stack.push(path);
            }
            out.insert(
                rel,
                (meta.is_dir(), meta.len(), meta.modified().unwrap(), bytes),
            );
        }
    }
    out
}

/// Sets the modification time of `path` one hour back, so its index entry is stale and an
/// index refresh would rewrite the index.
fn age(path: &Path) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
}

/// Stops a filesystem monitor daemon a failing run would have started, on every exit path.
struct StopMonitor<'a>(&'a Scene);

impl Drop for StopMonitor<'_> {
    fn drop(&mut self) {
        let _ = self
            .0
            .fx
            .git_output(&self.0.repo, &["fsmonitor--daemon", "stop"]);
    }
}

#[test]
fn index_and_refs_unchanged() {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    let head = scene.commit(&wt, "feature.txt");
    scene.push(&wt, "feat");
    let other = scene.worktree("other", "other");
    scene.commit(&other, "other.txt");
    scene.set_merged(&head, &merged_json(1, &head));
    std::fs::create_dir_all(wt.join("target/debug")).unwrap();
    std::fs::write(wt.join("target/debug/out"), b"bin").unwrap();
    std::fs::write(scene.repo.join(".gitignore"), "target/\n").unwrap();
    for dir in [&scene.repo, &wt, &other] {
        age(&dir.join("README.md"));
    }
    scene.profile("data", "[data]\nregenerable = ['target']\n");
    // A filesystem monitor configured for the repository must not start: its daemon writes
    // inside `.git`.
    scene
        .fx
        .git(&scene.repo, &["config", "core.fsmonitor", "true"]);
    let _daemon = StopMonitor(&scene);

    let roots = [scene.repo.clone(), wt.clone(), other.clone()];
    let before: Vec<Listing> = roots.iter().map(|r| listing(r)).collect();
    for args in [&["inventory"][..], &["inventory", "--sizes"]] {
        let output = scene
            .cardinal()
            .args(args)
            .arg("--repo")
            .arg(&scene.repo)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let doc: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(doc["repos"][0]["worktrees"].as_array().unwrap().len(), 3);
    }
    scene.observe(&wt).unwrap();
    let after: Vec<Listing> = roots.iter().map(|r| listing(r)).collect();
    for ((root, before), after) in roots.iter().zip(&before).zip(&after) {
        let changed: Vec<&String> = before
            .keys()
            .chain(after.keys())
            .filter(|k| before.get(*k) != after.get(*k))
            .collect();
        assert!(
            changed.is_empty(),
            "observation changed {changed:?} in {}",
            root.display()
        );
    }

    // Stop a daemon if one started, and prove none is left running for the repository.
    scene
        .fx
        .git_output(&scene.repo, &["fsmonitor--daemon", "stop"]);
    assert!(
        !scene
            .fx
            .git_output(&scene.repo, &["fsmonitor--daemon", "status"])
            .status
            .success(),
        "no filesystem monitor daemon remains"
    );
    // Turn the monitor off for the control below.
    scene
        .fx
        .git(&scene.repo, &["config", "--unset", "core.fsmonitor"]);

    // Control: the same status without disabling optional locks rewrites the index, so the
    // comparison above would have seen it.
    scene.fx.git(&wt, &["status", "--porcelain"]);
    let index_after_git = listing(&scene.repo.join(".git"));
    let before_git = &before[0];
    assert!(
        index_after_git
            .iter()
            .any(|(k, v)| k.contains("index") && before_git.get(&format!(".git/{k}")) != Some(v)),
        "a plain git status refreshes an index"
    );
}

/// A worktree with a split index is not read at all: reading a split index freshens its
/// shared index file inside `.git` even without optional locks. What needs the index is
/// unknown, so the worktree is never a candidate.
#[test]
fn split_index_is_left_unread() {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    let head = scene.commit(&wt, "feature.txt");
    scene.push(&wt, "feat");
    scene.merge_into_base(&head);
    scene.fx.git(&wt, &["update-index", "--split-index"]);
    let shared: Vec<_> = listing(&scene.repo.join(".git"))
        .keys()
        .filter(|k| k.contains("sharedindex."))
        .map(|k| scene.repo.join(".git").join(k))
        .collect();
    assert!(!shared.is_empty(), "git wrote a shared index");
    for file in &shared {
        age(file);
    }

    let before = listing(&scene.repo);
    let output = scene
        .cardinal()
        .args(["inventory", "--repo"])
        .arg(&scene.repo)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let inv = scene.inventory();
    scene.observe(&wt).unwrap();
    let after = listing(&scene.repo);
    let changed: Vec<&String> = before
        .keys()
        .chain(after.keys())
        .filter(|k| before.get(*k) != after.get(*k))
        .collect();
    assert!(changed.is_empty(), "observation changed {changed:?}");
    assert_no_monitor(&scene, &scene.repo);
    assert_no_monitor(&scene, &wt);

    let w = observe_common::worktree(&inv, &wt);
    assert_eq!(w.facts.dirty, cardinal::contract::Tri::Unknown);
    assert!(!w.reclaim.candidate);
    assert!(w.reclaim.failed.contains(&"observed".to_owned()));
    assert!(
        inv.errors.iter().any(|e| e.message.contains("split index")),
        "{:?}",
        inv.errors
    );

    // Control: a plain status freshens the shared index, so the comparison above would have
    // seen it.
    scene.fx.git(&wt, &["status", "--porcelain"]);
    let control = listing(&scene.repo);
    assert!(
        shared.iter().any(|file| {
            let key = file
                .strip_prefix(&scene.repo)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            before.get(&key) != control.get(&key)
        }),
        "a plain git status freshens the shared index"
    );
}

/// Every `sharedindex.*` file below `dir`, aged so that a freshen shows in its mtime.
fn aged_shared_indexes(dir: &Path) -> Vec<std::path::PathBuf> {
    let shared: Vec<_> = listing(dir)
        .keys()
        .filter(|k| k.contains("sharedindex."))
        .map(|k| dir.join(k))
        .collect();
    assert!(
        !shared.is_empty(),
        "git wrote a shared index below {}",
        dir.display()
    );
    for file in &shared {
        age(file);
    }
    shared
}

/// Runs inventory through the binary and the library, and returns the library's.
fn observe_all(scene: &Scene) -> cardinal::contract::Inventory {
    let output = scene
        .cardinal()
        .args(["inventory", "--repo"])
        .arg(&scene.repo)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    scene.inventory()
}

/// No filesystem monitor daemon runs for the repository at `dir`.
fn assert_no_monitor(scene: &Scene, dir: &Path) {
    assert!(
        !scene
            .fx
            .git_output(dir, &["fsmonitor--daemon", "status"])
            .status
            .success(),
        "a filesystem monitor daemon runs for {}",
        dir.display()
    );
}

fn assert_unchanged(before: &Listing, after: &Listing) {
    let changed: Vec<&String> = before
        .keys()
        .chain(after.keys())
        .filter(|k| before.get(*k) != after.get(*k))
        .collect();
    assert!(changed.is_empty(), "observation changed {changed:?}");
}

/// A configured base in revision syntax (`:path` reads the index) is refused as a ref name
/// and never handed to git; the base is a failed step.
#[test]
fn a_configured_base_is_only_a_ref_name() {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    scene
        .fx
        .git(&scene.repo, &["update-index", "--split-index"]);
    aged_shared_indexes(&scene.repo.join(".git"));
    scene.profile("base", "[git]\nbase = ':missing'\n");

    let before = listing(&scene.repo);
    let inv = observe_all(&scene);
    assert_unchanged(&before, &listing(&scene.repo));
    assert_eq!(inv.repos[0].base, None);
    assert!(
        inv.errors
            .iter()
            .any(|e| e.message.contains("not a ref name")),
        "{:?}",
        inv.errors
    );
    let w = observe_common::worktree(&inv, &wt);
    assert!(w.reclaim.failed.contains(&"observed".to_owned()));
    assert_no_monitor(&scene, &scene.repo);
}

/// The parent's status never enters a submodule, whose own split index reading would
/// freshen. The gitlink alone keeps the worktree from being a candidate.
#[test]
fn a_submodule_split_index_is_left_unread() {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    let dep = scene.fx.init("feat/dep");
    scene.fx.commit_file(&dep, "lib.txt", "lib\n", "dep");
    scene.fx.git(&wt, &["add", "dep"]);
    scene.fx.git(&wt, &["commit", "-q", "-m", "add dep"]);
    let head = scene.head(&wt);
    scene.push(&wt, "feat");
    scene.merge_into_base(&head);
    scene.fx.git(&dep, &["update-index", "--split-index"]);
    let shared = aged_shared_indexes(&dep.join(".git"));
    age(&dep.join("lib.txt"));

    let before = listing(&wt);
    let inv = observe_all(&scene);
    assert_unchanged(&before, &listing(&wt));
    let w = observe_common::worktree(&inv, &wt);
    assert_eq!(w.facts.dirty, cardinal::contract::Tri::No);
    assert!(
        w.reclaim
            .failed
            .contains(&"no_nested_repository".to_owned())
    );
    assert!(!w.reclaim.candidate);
    assert_no_monitor(&scene, &wt);
    assert_no_monitor(&scene, &dep);

    // Control: a status that enters submodules freshens the submodule's shared index.
    scene.fx.git(&wt, &["status", "--porcelain"]);
    let control = listing(&wt);
    assert!(
        shared.iter().any(|file| {
            let key = file
                .strip_prefix(&wt)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            before.get(&key) != control.get(&key)
        }),
        "a plain git status enters the submodule"
    );
}
