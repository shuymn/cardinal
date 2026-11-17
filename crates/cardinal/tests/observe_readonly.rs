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
