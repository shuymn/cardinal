//! A3: observation writes nothing inside any repository, `.git` included
//! (`docs/architecture.md` sections 8.1 and 17, item 8): no index refresh, no filesystem
//! monitor, no filter driver, and no lazy fetch of a partial clone's missing objects.

mod observe_common;
mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[cfg(unix)]
use cardinal::contract::{DirtyUnknown, DirtyUnknownReason};
use cardinal::contract::{Inventory, Tri};
use cardinal::observe::{InventoryOptions, Observer};
#[cfg(unix)]
use cardinal::testpoint::OBSERVE_FILTERS_LISTED;
use cardinal_test_support::fixture::Omit;
use cardinal_test_support::process::LeftoverGuard;
use observe_common::{Scene, merged_json};
#[cfg(unix)]
use support::testpoints::TestPoints;

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
    // A filesystem monitor configured for the repository must not start: its daemon writes
    // inside `.git`. Should one start, dropping `monitor` stops it, and `_leftovers`, dropped
    // after it, kills whatever of the scope still runs, the daemon included, and fails the test.
    let _leftovers = LeftoverGuard::new(scene.fx.scope());
    let monitor = scene.fx.fsmonitor(&scene.repo);

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

    assert_no_monitor(&scene, &scene.repo);
    // Turn the monitor off for the control below.
    drop(monitor);

    // Control: the same status without disabling optional locks rewrites the index, so the
    // comparison above would have seen it.
    scene.fx.status(&wt);
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
    scene.fx.split_index(&wt);
    let shared = aged_shared_indexes(&scene.repo.join(".git"));

    let before = listing(&scene.repo);
    let inv = observe_all(&scene);
    scene.observe(&wt).unwrap();
    assert_unchanged(&before, &listing(&scene.repo));
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
    scene.fx.status(&wt);
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
        !scene.fx.fsmonitor_running(dir),
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
    scene.fx.split_index(&scene.repo);
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
    scene.fx.add(&wt, "dep");
    scene.fx.commit(&wt, "add dep");
    let head = scene.head(&wt);
    scene.push(&wt, "feat");
    scene.merge_into_base(&head);
    scene.fx.split_index(&dep);
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
    scene.fx.status(&wt);
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

/// A clean filter that appends to `marker` and passes its input through, as Git LFS's
/// stores the file in `.git/lfs/objects`.
#[cfg(unix)]
fn writing_filter(scene: &Scene, name: &str, marker: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = scene.fx.path(&format!("{name}.sh"));
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho ran >>'{}'\ncat\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// A worktree with a tracked `asset.bin` that a required clean filter writing into the git
/// dir covers, edited at the same size, so its index entry needs a content check.
#[cfg(unix)]
fn filtered_edit(scene: &Scene) -> (PathBuf, PathBuf) {
    let wt = scene.worktree("feat", "feat");
    let marker = scene.repo.join(".git/filter-ran");
    let script = writing_filter(scene, "store", &marker);
    scene.fx.clean_filter(&scene.repo, "store", &script);
    std::fs::write(wt.join(".gitattributes"), "*.bin filter=store\n").unwrap();
    scene.fx.add(&wt, ".gitattributes");
    scene
        .fx
        .commit_file(&wt, "asset.bin", "original\n", "asset");
    scene.push(&wt, "feat");
    assert!(
        marker.exists(),
        "git ran the filter when the file was added"
    );
    std::fs::write(wt.join("asset.bin"), "modified\n").unwrap();
    (wt, marker)
}

/// No filter driver runs (section 8.1): a same-size edit of a filtered file, whose index
/// entry needs a content check, leaves the repository unchanged, although the driver is
/// marked `required` and its filter writes into the git dir. Without its filter the file
/// cannot be told from an unchanged one, so `dirty` is `unknown` with the path.
#[cfg(unix)]
#[test]
fn configured_filters_never_run() {
    let scene = Scene::new();
    let (wt, marker) = filtered_edit(&scene);

    let before = listing(&scene.repo);
    let inv = observe_all(&scene);
    let observed = scene.observe(&wt).unwrap();
    assert_unchanged(&before, &listing(&scene.repo));

    let filtered = Some(DirtyUnknown {
        reason: DirtyUnknownReason::FilteredPaths,
        paths: vec!["asset.bin".to_owned()],
    });
    for facts in [
        &observe_common::worktree(&inv, &wt).facts,
        &observed.worktree.facts,
    ] {
        assert_eq!(facts.dirty, Tri::Unknown);
        assert_eq!(facts.dirty_unknown, filtered);
    }
    // The main worktree has no filtered edit and stays known.
    let main = observe_common::worktree(&inv, &scene.repo);
    assert_eq!(
        (main.facts.dirty, &main.facts.dirty_unknown),
        (Tri::No, &None)
    );

    // Control: a plain status runs the filter, which writes into the git dir.
    let ran = std::fs::read(&marker).unwrap();
    scene.fx.status(&wt);
    assert_ne!(
        std::fs::read(&marker).unwrap(),
        ran,
        "a plain status runs the filter"
    );
}

/// A filter driver configured between the listing and `status` is found when observation
/// lists the drivers again, and fails that worktree's index facts (`filters_changed`). The
/// repository has one worktree: the pause point is process-wide, so with more, observed
/// concurrently, the driver could land before another worktree's first listing.
#[cfg(unix)]
#[test]
fn filter_added_during_observation() {
    let scene = Scene::new();
    let wt = scene.repo.clone();
    let points = TestPoints::new(scene.fx.scope());
    let mut command = scene.cardinal();
    command.args(["inventory", "--repo"]).arg(&scene.repo);
    points.apply(&mut command, &[OBSERVE_FILTERS_LISTED], None);
    let child = command
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    points.wait_reached(OBSERVE_FILTERS_LISTED, child.id(), Duration::from_secs(30));
    let marker = scene.fx.path("late-ran");
    let script = writing_filter(&scene, "late", &marker);
    scene.fx.clean_filter(&scene.repo, "late", &script);
    points.release(OBSERVE_FILTERS_LISTED);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let inv: Inventory = serde_json::from_slice(&output.stdout).unwrap();
    let w = observe_common::worktree(&inv, &wt);
    assert_eq!(w.facts.dirty, Tri::Unknown);
    assert!(w.reclaim.failed.contains(&"observed".to_owned()));
    let scope = format!("worktree:{}", w.path);
    assert!(
        inv.errors
            .iter()
            .any(|e| e.scope == scope && e.message.starts_with("filters_changed")),
        "{:?}",
        inv.errors
    );
}

/// `inventory --repo <repo>` through the library.
fn inventory_of(scene: &Scene, repo: &Path) -> Inventory {
    let config = scene.config();
    Observer::new(&config)
        .with_env(scene.env())
        .inventory(
            &scene.state(),
            &InventoryOptions {
                repos: vec![repo.to_path_buf()],
                sizes: false,
            },
        )
        .unwrap()
}

/// A partial clone, `--filter=tree:0` of a bare copy of the scene's repository, whose HEAD
/// moved, without a checkout, to a commit whose trees it never fetched: its `status`
/// needs objects only the promisor remote has.
fn partial_clone_needing_objects(scene: &Scene, name: &str) -> PathBuf {
    let origin = scene.fx.init_bare(&format!("{name}-origin.git"));
    scene.fx.push(&scene.repo, &origin, "main");
    let branch = format!("{name}-other");
    scene.fx.branch(&scene.repo, &branch);
    scene.fx.switch(&scene.repo, &branch);
    let other = scene.commit(&scene.repo, &format!("{name}.txt"));
    scene.fx.push(&scene.repo, &origin, &branch);
    scene.fx.switch(&scene.repo, "main");
    let clone = scene.fx.partial_clone(&origin, name, Omit::Trees);
    scene.fx.update_ref(&clone, "refs/heads/main", &other);
    clone
}

/// A blobless clone (`--filter=blob:none`) of a bare copy of the scene's repository whose
/// `main` moved, without a checkout, to a commit that changed `moved.txt`, a blob the clone
/// never fetched, and whose index renames `moved.txt` to `renamed.txt` with an edit. `status`
/// pairs the two by content (inexact rename detection), which needs the missing blob.
#[cfg(unix)]
fn staged_rename_needing_a_blob(scene: &Scene, name: &str) -> PathBuf {
    let text = (0..40).fold(String::new(), |text, n| text + &format!("line {n}\n"));
    scene
        .fx
        .commit_file(&scene.repo, "moved.txt", &text, "moved");
    let origin = scene.fx.init_bare(&format!("{name}-origin.git"));
    scene.fx.push(&scene.repo, &origin, "main");
    let branch = format!("{name}-other");
    scene.fx.branch(&scene.repo, &branch);
    scene.fx.switch(&scene.repo, &branch);
    let other = scene.fx.commit_file(
        &scene.repo,
        "moved.txt",
        &format!("{text}changed\n"),
        "change",
    );
    scene.fx.push(&scene.repo, &origin, &branch);
    scene.fx.switch(&scene.repo, "main");
    let clone = scene.fx.partial_clone(&origin, name, Omit::Blobs);
    scene.fx.update_ref(&clone, "refs/heads/main", &other);
    std::fs::remove_file(clone.join("moved.txt")).unwrap();
    std::fs::write(clone.join("renamed.txt"), format!("{text}edited\n")).unwrap();
    scene.fx.add(&clone, "moved.txt");
    scene.fx.add(&clone, "renamed.txt");
    clone
}

/// The files of `dir`'s object store's packs.
#[cfg(unix)]
fn packs(dir: &Path) -> Vec<String> {
    listing(&dir.join(".git/objects/pack"))
        .into_keys()
        .collect()
}

/// A git command that needs a missing object of a partial clone fetches it from the
/// promisor remote and writes a pack; observation runs git with `GIT_NO_LAZY_FETCH=1`, so
/// `status` fails instead: its facts are unknown, and the clone gains no pack.
#[test]
fn no_lazy_fetch() {
    let scene = Scene::new();
    let clone = partial_clone_needing_objects(&scene, "clone");

    let before = listing(&clone);
    let output = scene
        .cardinal()
        .args(["inventory", "--repo"])
        .arg(&clone)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let inv = inventory_of(&scene, &clone);
    assert_unchanged(&before, &listing(&clone));
    let w = observe_common::worktree(&inv, &clone);
    assert_eq!(w.facts.dirty, Tri::Unknown);
    assert!(w.reclaim.failed.contains(&"observed".to_owned()));
}

/// A git that ignores `GIT_NO_LAZY_FETCH`, `[observe] git` as a script that reports
/// `version` and drops the variable before it runs the real git.
#[cfg(unix)]
fn lazy_git(scene: &Scene, version: &str) -> PathBuf {
    let script = scene.fx.path(&format!("git-{version}"));
    scene.fx.scope().started_script(
        &script,
        &format!(
            "for arg; do [ \"$arg\" = --version ] && {{ echo 'git version {version}'; exit 0; }}; done\nunset GIT_NO_LAZY_FETCH\nexec git \"$@\"\n"
        ),
    );
    script
}

/// Before git 2.45 `GIT_NO_LAZY_FETCH` is ignored, so in a promisor repository observation
/// runs nothing that reads objects: the clone gains no pack, its facts are unknown, and the
/// reason is reported. The control, a git that claims 2.54 but ignores the variable all the
/// same, fetches: the scene does need the refusal.
#[cfg(unix)]
#[test]
fn an_old_git_reads_no_objects_of_a_partial_clone() {
    let mut scene = Scene::new();
    let old = lazy_git(&scene, "2.44.0");
    let clone = partial_clone_needing_objects(&scene, "clone");
    scene.observe_config(&format!("git = '{}'", old.display()));
    let before = listing(&clone);
    let inv = inventory_of(&scene, &clone);
    assert_unchanged(&before, &listing(&clone));
    let w = observe_common::worktree(&inv, &clone);
    assert_eq!(w.facts.dirty, Tri::Unknown);
    assert_eq!(
        w.facts.merge_evidence,
        cardinal::contract::MergeEvidence::Unknown
    );
    assert_eq!(inv.repos[0].base, None);
    assert!(
        inv.errors
            .iter()
            .any(|e| e.message.contains("git 2.44.0 would fetch missing objects")),
        "{:?}",
        inv.errors
    );

    let mut control = Scene::new();
    let lazy = lazy_git(&control, "2.54.0");
    let clone = partial_clone_needing_objects(&control, "control");
    control.observe_config(&format!("git = '{}'", lazy.display()));
    let before = packs(&clone);
    inventory_of(&control, &clone);
    assert_ne!(
        packs(&clone),
        before,
        "a git that ignores GIT_NO_LAZY_FETCH fetches the missing objects"
    );
}

/// git 2.45.0 reads `GIT_NO_LAZY_FETCH`, but the inexact rename detection `status` runs on a
/// staged rename fetches a missing blob without asking it (fixed in 2.45.1). With the git
/// under test, which honors the variable there too, the staged rename fetches nothing; with
/// a git reporting 2.45.0 that drops the variable, observation reads no objects at all.
#[cfg(unix)]
#[test]
fn rename_detection_fetches_nothing() {
    let scene = Scene::new();
    let clone = staged_rename_needing_a_blob(&scene, "clone");
    let before = listing(&clone);
    inventory_of(&scene, &clone);
    assert_unchanged(&before, &listing(&clone));

    let mut old = Scene::new();
    let git = lazy_git(&old, "2.45.0");
    let clone = staged_rename_needing_a_blob(&old, "clone");
    old.observe_config(&format!("git = '{}'", git.display()));
    let before = listing(&clone);
    let inv = inventory_of(&old, &clone);
    assert_unchanged(&before, &listing(&clone));
    assert!(
        inv.errors
            .iter()
            .any(|e| e.message.contains("git 2.45.0 would fetch missing objects")),
        "{:?}",
        inv.errors
    );
}

/// Replaces the one occurrence of `from` with `to` in the git config file `config`.
#[cfg(unix)]
fn edit_config(config: &Path, from: &str, to: &str) {
    let text = std::fs::read_to_string(config).unwrap();
    assert_eq!(text.matches(from).count(), 1, "{from:?} in {text}");
    std::fs::write(config, text.replacen(from, to, 1)).unwrap();
}

/// git registers a promisor remote from `remote.<name>.partialCloneFilter` alone, even where
/// `remote.<name>.promisor` is false, and fetches from it: below 2.45.1 observation reads no
/// objects there either.
#[cfg(unix)]
#[test]
fn a_partial_clone_filter_alone_makes_a_promisor() {
    let mut scene = Scene::new();
    let old = lazy_git(&scene, "2.44.0");
    let clone = partial_clone_needing_objects(&scene, "clone");
    let config = clone.join(".git/config");
    edit_config(&config, "\tpromisor = true\n", "\tpromisor = false\n");
    scene.observe_config(&format!("git = '{}'", old.display()));
    let before = listing(&clone);
    let inv = inventory_of(&scene, &clone);
    assert_unchanged(&before, &listing(&clone));
    let w = observe_common::worktree(&inv, &clone);
    assert_eq!(w.facts.dirty, Tri::Unknown);
    assert!(
        inv.errors
            .iter()
            .any(|e| e.message.contains("git 2.44.0 would fetch missing objects")),
        "{:?}",
        inv.errors
    );
}

/// A linked worktree's own configuration (`config.worktree`) can make its remote a promisor
/// where the repository's is not, so the check runs where each command runs: below 2.45.1
/// the linked worktree reads no objects, while the main worktree, whose configuration names
/// no promisor, is observed.
#[cfg(unix)]
#[test]
fn a_worktree_config_promisor_is_checked_in_that_worktree() {
    let mut scene = Scene::new();
    let old = lazy_git(&scene, "2.44.0");
    let origin = scene.fx.init_bare("origin.git");
    scene.fx.push(&scene.repo, &origin, "main");
    scene.fx.branch(&scene.repo, "other");
    scene.fx.switch(&scene.repo, "other");
    let other = scene.commit(&scene.repo, "other.txt");
    scene.fx.push(&scene.repo, &origin, "other");
    scene.fx.switch(&scene.repo, "main");
    let clone = scene.fx.partial_clone(&origin, "clone", Omit::Trees);
    let linked = scene.fx.worktree_add(&clone, "linked", "linked");
    // The linked worktree's HEAD moves to a commit whose trees the clone never fetched.
    scene.fx.update_ref(&clone, "refs/heads/linked", &other);
    let config = clone.join(".git/config");
    edit_config(&config, "\tpromisor = true\n", "\tpromisor = false\n");
    edit_config(
        &config,
        "\tpartialclonefilter = tree:0\n",
        "[extensions]\n\tworktreeConfig = true\n",
    );
    std::fs::write(
        clone.join(".git/worktrees/linked/config.worktree"),
        "[remote \"origin\"]\n\tpromisor = true\n",
    )
    .unwrap();
    scene.observe_config(&format!("git = '{}'", old.display()));

    let before = (listing(&clone), listing(&linked));
    let inv = inventory_of(&scene, &clone);
    assert_eq!(
        before,
        (listing(&clone), listing(&linked)),
        "nothing fetched"
    );
    let w = observe_common::worktree(&inv, &linked);
    assert_eq!(w.facts.dirty, Tri::Unknown);
    let scope = format!("worktree:{}", linked.display());
    assert!(
        inv.errors
            .iter()
            .any(|e| e.scope == scope
                && e.message.contains("git 2.44.0 would fetch missing objects")),
        "{:?}",
        inv.errors
    );
    let main = observe_common::worktree(&inv, &clone);
    assert_eq!(main.facts.dirty, Tri::No, "{:?}", inv.errors);
}

/// Without a configured filter driver no path is filtered, so a worktree-side modification is
/// plainly dirty and no attribute is looked up: with a git whose `check-attr` fails, `dirty`
/// is still `yes` and nothing fails.
#[cfg(unix)]
#[test]
fn no_attribute_lookup_without_a_driver() {
    let mut scene = Scene::new();
    let git = scene.fx.path("git-without-check-attr");
    scene.fx.scope().started_script(
        &git,
        "for arg; do [ \"$arg\" = check-attr ] && exit 1; done\nexec git \"$@\"\n",
    );
    let wt = scene.worktree("feat", "feat");
    scene
        .fx
        .commit_file(&wt, "asset.bin", "original\n", "asset");
    std::fs::write(wt.join("asset.bin"), "modified\n").unwrap();
    scene.observe_config(&format!("git = '{}'", git.display()));
    let inv = inventory_of(&scene, &scene.repo);
    let w = observe_common::worktree(&inv, &wt);
    assert_eq!(w.facts.dirty, Tri::Yes, "{:?}", inv.errors);
    assert!(
        !inv.errors.iter().any(|e| e.message.contains("check-attr")),
        "{:?}",
        inv.errors
    );
}
