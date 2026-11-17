//! `scripts/repo-guard` detects writes to a repository (`docs/architecture.md` section 13.4):
//! snapshot a fixture, mutate it, and verify must fail; without a mutation it passes.
#![cfg(unix)]

mod support;

use std::path::{Path, PathBuf};
use std::process::Output;

use support::fixture::Fixture;

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/repo-guard")
}

fn guard(fixture: &Fixture, mode: &str, repo: &Path, file: &Path) -> Output {
    fixture
        .command("bash", fixture.root())
        .arg(script())
        .arg(mode)
        .arg(repo)
        .arg(file)
        .output()
        .expect("run repo-guard")
}

/// A repository with one commit, a linked worktree, and a snapshot of it.
fn snapshotted() -> (Fixture, PathBuf, PathBuf) {
    let fixture = Fixture::new();
    let repo = fixture.init("repo");
    fixture.commit_file(&repo, "a.txt", "a\n", "first");
    fixture.worktree_add(&repo, "wt", "feature");
    let file = fixture.path("snapshot");
    let output = guard(&fixture, "snapshot", &repo, &file);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (fixture, repo, file)
}

fn assert_detected(name: &str, mutate: impl FnOnce(&Fixture, &Path)) {
    let (fixture, repo, file) = snapshotted();
    mutate(&fixture, &repo);
    let output = guard(&fixture, "verify", &repo, &file);
    assert!(!output.status.success(), "{name} was not detected");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("changed while the tests ran"),
        "{name}: {stderr}"
    );
}

#[test]
fn unchanged_repository_verifies() {
    let (fixture, repo, file) = snapshotted();
    // Reads do not count as changes.
    fixture.git(&repo, &["status", "--porcelain"]);
    fixture.git(&repo, &["log", "--oneline"]);
    let output = guard(&fixture, "verify", &repo, &file);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn commit_is_detected() {
    assert_detected("a commit", |fixture, repo| {
        fixture.commit_file(repo, "b.txt", "b\n", "second");
    });
}

#[test]
fn commits_in_other_worktrees_are_ignored() {
    let (fixture, repo, file) = snapshotted();
    let wt = fixture.path("wt");
    fixture.commit_file(&wt, "w.txt", "w\n", "in the other worktree");
    let output = guard(&fixture, "verify", &repo, &file);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Seen from the linked worktree, a commit on its own branch counts, one in the main
    // worktree does not.
    let wt_file = fixture.path("wt-snapshot");
    assert!(guard(&fixture, "snapshot", &wt, &wt_file).status.success());
    fixture.commit_file(&repo, "m.txt", "m\n", "in the main worktree");
    assert!(guard(&fixture, "verify", &wt, &wt_file).status.success());
    fixture.commit_file(&wt, "w2.txt", "w\n", "in this worktree");
    assert!(!guard(&fixture, "verify", &wt, &wt_file).status.success());
}

#[test]
fn pushes_and_upstreams_of_other_worktrees_are_ignored() {
    let (fixture, repo, file) = snapshotted();
    let bare = fixture.init_bare("remote.git");
    let bare = bare.to_string_lossy().into_owned();
    // Adding a remote is a config change of its own; snapshot after it.
    fixture.git(&repo, &["remote", "add", "origin", &bare]);
    assert!(guard(&fixture, "snapshot", &repo, &file).status.success());
    let wt = fixture.path("wt");
    fixture.git(&wt, &["push", "-q", "-u", "origin", "feature"]);
    fixture.git(&wt, &["config", "branch.feature.note", "x"]);
    let output = guard(&fixture, "verify", &repo, &file);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The same for this worktree's own branch is a change.
    fixture.git(&repo, &["push", "-q", "-u", "origin", "main"]);
    assert!(!guard(&fixture, "verify", &repo, &file).status.success());
}

#[test]
fn new_ref_is_detected() {
    assert_detected("a new branch", |fixture, repo| {
        fixture.git(repo, &["branch", "other"]);
    });
}

#[test]
fn new_tag_is_detected() {
    assert_detected("a new tag", |fixture, repo| {
        fixture.git(repo, &["tag", "v1"]);
    });
}

#[test]
fn refs_outside_git_namespaces_are_ignored() {
    let (fixture, repo, file) = snapshotted();
    fixture.git(
        repo.as_path(),
        &["update-ref", "refs/tool/checkpoint", "HEAD"],
    );
    let output = guard(&fixture, "verify", &repo, &file);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn local_config_is_detected() {
    assert_detected("a local config entry", |fixture, repo| {
        fixture.git(repo, &["config", "--local", "user.name", "Someone"]);
    });
}

#[test]
fn stash_is_detected() {
    assert_detected("a stash", |fixture, repo| {
        std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
        fixture.git(repo, &["stash", "push", "-q"]);
    });
}

#[test]
fn worktree_change_is_detected() {
    assert_detected("a new worktree", |fixture, repo| {
        fixture.worktree_add(repo, "wt2", "feature2");
    });
}

#[test]
fn index_change_is_detected() {
    assert_detected("a staged file", |fixture, repo| {
        std::fs::write(repo.join("c.txt"), "c\n").unwrap();
        fixture.git(repo, &["add", "--", "c.txt"]);
    });
}

#[test]
fn bad_usage_fails() {
    let fixture = Fixture::new();
    let output = fixture
        .command("bash", fixture.root())
        .arg(script())
        .arg("snapshot")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}
