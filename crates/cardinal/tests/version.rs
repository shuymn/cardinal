//! The source commit `cardinal --version` names: `build/commit.rs`, which `build.rs` runs,
//! read in fixture repositories with git started through the scope as the build script
//! starts it, with the same options and added variables. The real binary's output is checked
//! in `tests/cli.rs`.
//!
//! Cargo runs a build script again only when a path it printed with `rerun-if-changed` on
//! its last run changes, so each change of HEAD is checked against the paths the read before
//! it watched. Cargo compares modification times; the checks compare contents, which change
//! with every write that matters here and do not depend on the file system's time
//! resolution.

#[path = "../build/commit.rs"]
mod commit;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use cardinal_test_support::fixture::Fixture;

use commit::{Commit, Source};

/// [`commit::read`] in `dir`, with git started through the fixture's scope.
fn read(fx: &Fixture, dir: &Path) -> Source {
    commit::read(dir, |args| git(fx, dir, args))
}

fn git(fx: &Fixture, dir: &Path, args: &[&str]) -> Option<String> {
    let mut command = fx.command("git", dir);
    command.args(commit::GIT_OPTIONS).args(args);
    for (name, value) in commit::GIT_ADDED {
        command.env(name, value);
    }
    let output = command.output().expect("start git");
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).expect("git output is UTF-8"))
}

/// A repository `name` holding the crate at `crates/cardinal`, as cardinal's does, with one
/// commit.
fn checkout(fx: &Fixture, name: &str) -> PathBuf {
    let repo = fx.init(name);
    fx.commit_file(
        &repo,
        "crates/cardinal/Cargo.toml",
        "[package]\n",
        "the crate",
    );
    repo
}

fn crate_dir(worktree: &Path) -> PathBuf {
    worktree.join("crates/cardinal")
}

/// HEAD's commit in `worktree` as git abbreviates it to at least 9 digits, and its date.
fn head(fx: &Fixture, worktree: &Path, dirty: bool) -> Commit {
    let id = fx.rev_parse(worktree, "HEAD");
    let date = fx.log(worktree, "%cs");
    Commit {
        short: id[..9].to_owned(),
        date: date.lines().next().unwrap().to_owned(),
        dirty,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Missing,
    Dir,
    File(Vec<u8>),
}

/// Every watched path, and each path below a watched directory, with its contents.
fn state(watch: &[PathBuf]) -> BTreeMap<PathBuf, Entry> {
    fn add(path: &Path, out: &mut BTreeMap<PathBuf, Entry>) {
        if path.is_dir() {
            out.insert(path.to_path_buf(), Entry::Dir);
            for entry in std::fs::read_dir(path).unwrap() {
                add(&entry.unwrap().path(), out);
            }
        } else {
            let entry = std::fs::read(path).map_or(Entry::Missing, Entry::File);
            out.insert(path.to_path_buf(), entry);
        }
    }
    let mut out = BTreeMap::new();
    for path in watch {
        add(path, &mut out);
    }
    out
}

/// Runs `change`, which must make HEAD in `dir` name another commit, and asserts that a path
/// the read before it watched changed.
fn assert_rerun(fx: &Fixture, dir: &Path, change: impl FnOnce()) {
    let before = read(fx, dir);
    let seen = state(&before.watch);
    change();
    let after = read(fx, dir);
    assert_ne!(after.commit, before.commit, "the change kept HEAD's commit");
    assert_ne!(
        state(&before.watch),
        seen,
        "HEAD names another commit, and none of {:?} changed",
        before.watch
    );
}

#[test]
fn a_main_checkout_names_head() {
    let fx = Fixture::new();
    let repo = checkout(&fx, "repo");
    let source = read(&fx, &crate_dir(&repo));
    assert_eq!(source.commit, Some(head(&fx, &repo, false)));
    let git_dir = repo.join(".git");
    assert!(source.watch.contains(&git_dir.join("HEAD")), "{source:?}");
    assert!(
        source.watch.contains(&git_dir.join("refs/heads/main")),
        "{source:?}"
    );
}

#[test]
fn a_linked_worktree_names_its_own_head() {
    let fx = Fixture::new();
    let repo = checkout(&fx, "repo");
    let wt = fx.worktree_add(&repo, "wt", "feature");
    fx.commit_file(
        &wt,
        "crates/cardinal/src.rs",
        "fn f() {}\n",
        "in the worktree",
    );
    let source = read(&fx, &crate_dir(&wt));
    let commit = source.commit.clone().unwrap();
    assert_eq!(commit, head(&fx, &wt, false));
    assert_ne!(commit, head(&fx, &repo, false));
    // HEAD lives in the worktree's git dir, the branch's ref in the repository's.
    let common = repo.join(".git");
    assert!(
        source.watch.contains(&common.join("worktrees/wt/HEAD")),
        "{source:?}"
    );
    assert!(
        source.watch.contains(&common.join("refs/heads/feature")),
        "{source:?}"
    );
}

#[test]
fn each_change_of_head_changes_a_watched_path() {
    let fx = Fixture::new();
    let repo = checkout(&fx, "repo");
    fx.branch(&repo, "other");
    let wt = fx.worktree_add(&repo, "wt", "feat/x");
    let dir = crate_dir(&wt);
    let mut n = 0;
    let mut commit = |worktree: &Path| {
        n += 1;
        fx.commit_file(worktree, "file", &n.to_string(), "change");
    };
    assert_rerun(&fx, &crate_dir(&repo), || commit(&repo));
    assert_rerun(&fx, &dir, || commit(&wt));
    assert_rerun(&fx, &dir, || fx.switch(&wt, "other"));
    assert_rerun(&fx, &dir, || fx.switch(&wt, "feat/x"));
    // Only packed, the branch's ref has no file until the next commit writes one.
    fx.pack_refs(&repo);
    assert_rerun(&fx, &dir, || commit(&wt));
    fx.detach(&wt);
    assert_rerun(&fx, &dir, || commit(&wt));
}

#[test]
fn a_worktrees_own_ref_in_the_chain_is_watched() {
    let fx = Fixture::new();
    let repo = checkout(&fx, "repo");
    let wt = fx.worktree_add(&repo, "wt", "linked");
    let dir = crate_dir(&wt);
    fx.commit_file(&wt, "file", "1", "in the worktree");
    // HEAD names `linked`, a symbolic ref to `refs/worktree/local`, which git keeps in the
    // worktree's own git dir.
    fx.update_ref(&wt, "refs/worktree/local", "HEAD");
    fx.symbolic_ref(&wt, "refs/heads/linked", "refs/worktree/local");
    let source = read(&fx, &dir);
    let local = repo.join(".git/worktrees/wt/refs/worktree/local");
    assert!(source.watch.contains(&local), "{source:?}");
    assert_rerun(&fx, &dir, || {
        fx.update_ref(&wt, "refs/worktree/local", "main");
    });
    assert_rerun(&fx, &dir, || {
        fx.commit_file(&wt, "file", "2", "through the worktree's own ref");
    });
}

#[test]
fn each_ref_of_a_symbolic_chain_is_watched() {
    let fx = Fixture::new();
    let repo = checkout(&fx, "repo");
    let dir = crate_dir(&repo);
    fx.branch(&repo, "old");
    // HEAD names `alias`, itself a symbolic ref to `main`.
    fx.symbolic_ref(&repo, "refs/heads/alias", "refs/heads/main");
    fx.symbolic_ref(&repo, "HEAD", "refs/heads/alias");
    fx.commit_file(&repo, "file", "1", "on main through alias");
    let source = read(&fx, &dir);
    let heads = repo.join(".git/refs/heads");
    for name in ["alias", "main"] {
        assert!(
            source.watch.contains(&heads.join(name)),
            "{name}: {source:?}"
        );
    }
    assert_rerun(&fx, &dir, || {
        fx.symbolic_ref(&repo, "refs/heads/alias", "refs/heads/old");
    });
    assert_rerun(&fx, &dir, || {
        fx.commit_file(&repo, "file", "2", "on old through alias");
    });
}

#[test]
fn the_first_commit_changes_a_watched_path() {
    let fx = Fixture::new();
    let repo = fx.init("repo");
    let dir = crate_dir(&repo);
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(read(&fx, &dir).commit, None);
    assert_rerun(&fx, &dir, || {
        fx.commit_file(&repo, "crates/cardinal/Cargo.toml", "", "first");
    });
}

#[test]
fn commits_in_other_worktrees_change_no_watched_path() {
    let fx = Fixture::new();
    let repo = checkout(&fx, "repo");
    let wt = fx.worktree_add(&repo, "wt", "feature");
    fx.pack_refs(&repo);
    fx.commit_file(&wt, "file", "1", "in the worktree");
    let source = read(&fx, &crate_dir(&wt));
    let seen = state(&source.watch);
    fx.commit_file(&repo, "file", "2", "in the main worktree");
    let other = fx.worktree_add(&repo, "other", "other");
    fx.commit_file(&other, "file", "3", "in another worktree");
    assert_eq!(state(&source.watch), seen, "{:?}", source.watch);
    assert_eq!(read(&fx, &crate_dir(&wt)).commit, source.commit);
}

#[test]
fn dirty_marks_tracked_files_that_differ_from_head() {
    let fx = Fixture::new();
    let repo = checkout(&fx, "repo");
    let dir = crate_dir(&repo);
    let dirty = || read(&fx, &dir).commit.unwrap().dirty;
    std::fs::write(repo.join("untracked"), "new").unwrap();
    assert!(!dirty(), "an untracked file is no change");
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    assert!(dirty(), "a changed working tree");
    fx.add(&repo, "crates/cardinal/Cargo.toml");
    assert!(dirty(), "a staged change");
    fx.commit(&repo, "commit it");
    assert!(!dirty(), "committed");
}

#[test]
fn sources_outside_a_checkout_of_cardinal_name_no_commit() {
    let fx = Fixture::new();
    // Vendored into another repository: its commit is not the sources'.
    let other = fx.init("other");
    fx.commit_file(&other, "vendor/cardinal/Cargo.toml", "", "vendored");
    // An unpacked crate package or source archive, in no repository.
    let plain = fx.path("cardinal-0.1.0");
    std::fs::create_dir_all(&plain).unwrap();
    for dir in [other.join("vendor/cardinal"), plain.clone()] {
        let source = read(&fx, &dir);
        assert_eq!(source.commit, None, "{}", dir.display());
        assert!(source.watch.is_empty(), "{}: {source:?}", dir.display());
    }
}

#[test]
fn a_crate_package_is_read_without_git() {
    let vcs_info = "{\"git\":{\"sha1\":\"0123456789abcdef0123456789abcdef01234567\"},\
                    \"path_in_vcs\":\"crates/cardinal\"}\n";
    // Every package has `Cargo.toml.orig`; one made from a checkout also has
    // `.cargo_vcs_info.json`, one made from an exported tree does not.
    for (marker, contents) in [
        ("Cargo.toml.orig", "[package]\n"),
        (".cargo_vcs_info.json", vcs_info),
    ] {
        let fx = Fixture::new();
        // A crate package committed into another repository at the path cardinal's uses: by
        // the layout alone, a checkout of cardinal.
        let other = checkout(&fx, "other");
        let dir = crate_dir(&other);
        assert!(read(&fx, &dir).commit.is_some(), "{marker}");
        let rel = format!("crates/cardinal/{marker}");
        fx.commit_file(&other, &rel, contents, "packaged");
        let source = commit::read(&dir, |args| panic!("git {args:?} ran beside {marker}"));
        assert_eq!(source.commit, None, "{marker}");
        assert!(source.watch.is_empty(), "{marker}: {source:?}");
    }
}

#[test]
fn a_failing_git_names_no_commit() {
    let fx = Fixture::new();
    let repo = checkout(&fx, "repo");
    let dir = crate_dir(&repo);
    let source = commit::read(&dir, |_| None);
    assert_eq!(source.commit, None);
    assert!(source.watch.is_empty(), "{source:?}");
    for failing in ["symbolic-ref", "rev-list", "status"] {
        let source = commit::read(&dir, |args| {
            if args[0] == failing {
                None
            } else {
                git(&fx, &dir, args)
            }
        });
        if failing == "symbolic-ref" {
            // Read as a detached HEAD: the commit is still HEAD's.
            assert_eq!(source.commit, Some(head(&fx, &repo, false)));
        } else {
            assert_eq!(source.commit, None, "{failing} failing");
        }
    }
}

#[test]
fn the_version_is_the_package_and_the_commit() {
    let commit = Commit {
        short: "0123456789".into(),
        date: "2026-11-19".into(),
        dirty: false,
    };
    assert_eq!(commit::version("1.2.3", None), "1.2.3");
    assert_eq!(
        commit::version("1.2.3", Some(&commit)),
        "1.2.3 (0123456789 2026-11-19)"
    );
    let dirty = Commit {
        dirty: true,
        ..commit
    };
    assert_eq!(
        commit::version("1.2.3", Some(&dirty)),
        "1.2.3 (0123456789-dirty 2026-11-19)"
    );
}

#[test]
fn git_gets_no_variable_that_points_at_another_repository() {
    let inherited: Vec<(OsString, OsString)> = [
        ("PATH", "/bin"),
        ("HOME", "/home/u"),
        ("GIT_DIR", "/decoy/.git"),
        ("GIT_WORK_TREE", "/decoy"),
        ("GIT_INDEX_FILE", "/decoy/index"),
        ("GIT_COMMON_DIR", "/decoy/.git"),
        ("GIT_OBJECT_DIRECTORY", "/decoy/objects"),
        ("GIT_CEILING_DIRECTORIES", "/"),
        ("GIT_OPTIONAL_LOCKS", "1"),
        ("BASH_ENV", "/decoy/env"),
        ("Path", "C:\\bin"),
        ("SystemRoot", "C:\\Windows"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect();
    let names = |windows| {
        commit::git_env(inherited.clone(), windows)
            .into_iter()
            .map(|(k, v)| format!("{}={}", k.to_string_lossy(), v.to_string_lossy()))
            .collect::<Vec<_>>()
    };
    let added = [
        "GIT_OPTIONAL_LOCKS=0",
        "GIT_NO_LAZY_FETCH=1",
        "GIT_TERMINAL_PROMPT=0",
    ];
    let unix: Vec<String> = ["PATH=/bin", "HOME=/home/u"]
        .into_iter()
        .chain(added)
        .map(String::from)
        .collect();
    assert_eq!(names(false), unix);
    let windows: Vec<String> = [
        "PATH=/bin",
        "HOME=/home/u",
        "Path=C:\\bin",
        "SystemRoot=C:\\Windows",
    ]
    .into_iter()
    .chain(added)
    .map(String::from)
    .collect();
    assert_eq!(names(true), windows);
}
