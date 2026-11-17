//! `scripts/repo-guard` detects writes to a repository (`docs/architecture.md` section 13.4):
//! snapshot a fixture, change it the way a test that escaped its fixture would, and verify
//! must fail; without a change it passes. On Windows the script runs in Git Bash, as
//! `scripts/test` does.

use std::path::{Path, PathBuf};
use std::process::Output;

use cardinal_test_support::fixture::Fixture;

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/repo-guard")
}

fn guard_with(
    fixture: &Fixture,
    env: &[(&str, &str)],
    mode: &str,
    repo: &Path,
    file: Option<&Path>,
) -> Output {
    guard_args(fixture, env, &[mode], repo, file)
}

fn guard_args(
    fixture: &Fixture,
    env: &[(&str, &str)],
    args: &[&str],
    repo: &Path,
    file: Option<&Path>,
) -> Output {
    let mut command = fixture.command("bash", fixture.root());
    command.arg(script()).args(args).arg(repo);
    if let Some(file) = file {
        command.arg(file);
    }
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("run repo-guard")
}

fn guard(fixture: &Fixture, mode: &str, repo: &Path, file: &Path) -> Output {
    guard_with(fixture, &[], mode, repo, Some(file))
}

fn assert_ok(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A repository with one commit, a linked worktree, and a snapshot of it.
fn snapshotted() -> (Fixture, PathBuf, PathBuf) {
    let fixture = Fixture::new();
    let repo = fixture.init("repo");
    fixture.commit_file(&repo, "a.txt", "a\n", "first");
    fixture.worktree_add(&repo, "wt", "feature");
    let file = fixture.path("snapshot");
    assert_ok(&guard(&fixture, "snapshot", &repo, &file));
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
    fixture.status(&repo);
    fixture.log(&repo, "%H");
    assert_ok(&guard(&fixture, "verify", &repo, &file));
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
    assert_ok(&guard(&fixture, "verify", &repo, &file));

    // Seen from the linked worktree, a commit on its own branch counts, one in the main
    // worktree does not.
    let wt_file = fixture.path("wt-snapshot");
    assert_ok(&guard(&fixture, "snapshot", &wt, &wt_file));
    fixture.commit_file(&repo, "m.txt", "m\n", "in the main worktree");
    assert_ok(&guard(&fixture, "verify", &wt, &wt_file));
    fixture.commit_file(&wt, "w2.txt", "w\n", "in this worktree");
    assert!(!guard(&fixture, "verify", &wt, &wt_file).status.success());
}

/// Another worktree that switches branch between snapshot and verify, to a new branch or to a
/// detached HEAD, leaves the branches it had checked out at either point out of both states;
/// a commit on this worktree's own branch still counts.
#[test]
fn other_worktrees_switching_branch_are_ignored() {
    let (fixture, repo, file) = snapshotted();
    let wt = fixture.path("wt");
    fixture.commit_file(&wt, "w.txt", "w\n", "on feature");
    fixture.branch(&wt, "other");
    fixture.switch(&wt, "other");
    fixture.commit_file(&wt, "o.txt", "o\n", "on other");
    fixture.config_set(&wt, "branch.other.note", "x");
    assert_ok(&guard(&fixture, "verify", &repo, &file));

    assert_ok(&guard(&fixture, "snapshot", &repo, &file));
    fixture.commit_file(&wt, "o2.txt", "o\n", "on other again");
    fixture.detach(&wt);
    assert_ok(&guard(&fixture, "verify", &repo, &file));

    fixture.commit_file(&repo, "m.txt", "m\n", "on main");
    let output = guard(&fixture, "verify", &repo, &file);
    assert!(
        !output.status.success(),
        "a commit on this worktree's branch was ignored"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("refs/heads/main"), "{stderr}");
}

#[test]
fn in_ci_other_worktrees_count_too() {
    let fixture = Fixture::new();
    let repo = fixture.init("repo");
    fixture.commit_file(&repo, "a.txt", "a\n", "first");
    let wt = fixture.worktree_add(&repo, "wt", "feature");
    let file = fixture.path("snapshot");
    let ci = [("CI", "true")];
    assert_ok(&guard_with(&fixture, &ci, "snapshot", &repo, Some(&file)));
    assert_ok(&guard_with(&fixture, &ci, "verify", &repo, Some(&file)));
    fixture.commit_file(&wt, "w.txt", "w\n", "in the other worktree");
    assert!(
        !guard_with(&fixture, &ci, "verify", &repo, Some(&file))
            .status
            .success(),
        "a commit in another worktree was ignored with CI=true"
    );
}

#[test]
fn pushes_and_upstreams_of_other_worktrees_are_ignored() {
    let (fixture, repo, file) = snapshotted();
    let bare = fixture.init_bare("remote.git");
    let wt = fixture.path("wt");
    // The push sets the upstream config; a push or fetch through a named remote also
    // writes the remote-tracking ref.
    fixture.push(&wt, &bare, "feature");
    fixture.update_ref(&wt, "refs/remotes/origin/feature", "HEAD");
    fixture.config_set(&wt, "branch.feature.note", "x");
    assert_ok(&guard(&fixture, "verify", &repo, &file));

    // The same for this worktree's own branch is a change.
    fixture.push(&repo, &bare, "main");
    assert!(!guard(&fixture, "verify", &repo, &file).status.success());
}

#[test]
fn config_of_a_branch_whose_name_extends_another_worktrees_branch_is_detected() {
    // The linked worktree has `feature` checked out; `feature.fix` is a different branch.
    let (fixture, repo, file) = snapshotted();
    fixture.config_set(&repo, "branch.feature.note", "x");
    assert_ok(&guard(&fixture, "verify", &repo, &file));
    fixture.config_set(&repo, "branch.feature.fix.note", "x");
    let output = guard(&fixture, "verify", &repo, &file);
    assert!(!output.status.success(), "branch.feature.fix was ignored");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("branch.feature.fix.note=x"), "{stderr}");
}

#[test]
fn new_ref_is_detected() {
    assert_detected("a new branch", |fixture, repo| {
        fixture.branch(repo, "other");
    });
}

#[test]
fn new_tag_is_detected() {
    assert_detected("a new tag", |fixture, repo| {
        fixture.tag(repo, "v1");
    });
}

#[test]
fn refs_outside_git_namespaces_are_ignored() {
    let (fixture, repo, file) = snapshotted();
    fixture.update_ref(&repo, "refs/tool/checkpoint", "HEAD");
    assert_ok(&guard(&fixture, "verify", &repo, &file));
}

#[test]
fn local_config_is_detected() {
    assert_detected("a local config entry", |fixture, repo| {
        fixture.config_set(repo, "cardinal.probe", "x");
    });
}

#[test]
fn stash_is_detected() {
    assert_detected("a stash", |fixture, repo| {
        std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
        fixture.stash_push(repo);
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
        fixture.add(repo, "c.txt");
    });
}

#[test]
fn hook_content_is_detected() {
    assert_detected("a new hook", |_, repo| {
        std::fs::write(repo.join(".git/hooks/pre-commit"), "#!/bin/sh\n").unwrap();
    });
}

#[cfg(unix)]
#[test]
fn hook_mode_is_detected() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let repo = fixture.init("repo");
    let hook = repo.join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\n").unwrap();
    let file = fixture.path("snapshot");
    assert_ok(&guard(&fixture, "snapshot", &repo, &file));
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!guard(&fixture, "verify", &repo, &file).status.success());
}

#[test]
fn info_is_detected() {
    assert_detected("info/exclude", |_, repo| {
        std::fs::write(repo.join(".git/info/exclude"), "escaped\n").unwrap();
    });
}

#[test]
fn included_config_is_detected() {
    let fixture = Fixture::new();
    let repo = fixture.init("repo");
    let included = repo.join(".git/extra.config");
    std::fs::write(&included, "").unwrap();
    let config = repo.join(".git/config");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("[include]\n\tpath = extra.config\n");
    std::fs::write(&config, text).unwrap();
    let file = fixture.path("snapshot");
    assert_ok(&guard(&fixture, "snapshot", &repo, &file));
    std::fs::write(&included, "[cardinal]\n\tprobe = x\n").unwrap();
    assert!(
        !guard(&fixture, "verify", &repo, &file).status.success(),
        "a change in an included config file was not detected"
    );
}

#[test]
fn worktree_git_dir_files_are_detected() {
    let (fixture, _, _) = snapshotted();
    let wt = fixture.path("wt");
    let file = fixture.path("wt-snapshot");
    assert_ok(&guard(&fixture, "snapshot", &wt, &file));
    let git_dir = fixture.path("repo/.git/worktrees/wt");
    std::fs::write(git_dir.join("config.worktree"), "[cardinal]\n\tx = y\n").unwrap();
    assert!(!guard(&fixture, "verify", &wt, &file).status.success());
}

/// With `--checkout`, a test that writes into the checkout through a relative path changes
/// what the guard sees.
#[test]
fn checkout_change_is_detected_with_checkout() {
    for (name, mutate) in [
        ("an overwritten tracked file", "a.txt"),
        ("a new untracked file", "new.txt"),
    ] {
        let fixture = Fixture::new();
        let repo = fixture.init("repo");
        fixture.commit_file(&repo, "a.txt", "a\n", "first");
        let file = fixture.path("snapshot");
        let snapshot = ["snapshot", "--checkout"];
        assert_ok(&guard_args(&fixture, &[], &snapshot, &repo, Some(&file)));
        std::fs::write(repo.join(mutate), "escaped\n").unwrap();
        let verify = ["verify", "--checkout"];
        let output = guard_args(&fixture, &[], &verify, &repo, Some(&file));
        assert!(!output.status.success(), "{name} was not detected");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(mutate), "{name}: {stderr}");
    }
}

/// A file that already differs from HEAD before the snapshot is compared by content.
#[test]
fn checkout_change_to_a_modified_file_is_detected() {
    let fixture = Fixture::new();
    let repo = fixture.init("repo");
    fixture.commit_file(&repo, "a.txt", "a\n", "first");
    std::fs::write(repo.join("a.txt"), "edited before the run\n").unwrap();
    let file = fixture.path("snapshot");
    assert_ok(&guard_args(
        &fixture,
        &[],
        &["snapshot", "--checkout"],
        &repo,
        Some(&file),
    ));
    std::fs::write(repo.join("a.txt"), "edited during the run\n").unwrap();
    let output = guard_args(&fixture, &[], &["verify", "--checkout"], &repo, Some(&file));
    assert!(!output.status.success(), "a second edit was not detected");
}

/// Locally and confined, the checkout is the developer's to edit while the tests run; in CI
/// it is compared without `--checkout`.
#[test]
fn checkout_is_compared_in_ci_only_without_checkout() {
    for (env, detected) in [(&[][..], false), (&[("CI", "true")][..], true)] {
        let fixture = Fixture::new();
        let repo = fixture.init("repo");
        fixture.commit_file(&repo, "a.txt", "a\n", "first");
        let file = fixture.path("snapshot");
        assert_ok(&guard_with(&fixture, env, "snapshot", &repo, Some(&file)));
        std::fs::write(repo.join("a.txt"), "escaped\n").unwrap();
        let output = guard_with(&fixture, env, "verify", &repo, Some(&file));
        assert_eq!(!output.status.success(), detected, "{env:?}");
    }
}

/// Appends `text` to a file.
fn append(path: &Path, text: &str) {
    let mut contents = std::fs::read_to_string(path).unwrap_or_default();
    contents.push_str(text);
    std::fs::write(path, contents).unwrap();
}

/// Outside the git dir, so only the include graph leads to them; git resolves the nested
/// relative path against the including file.
#[test]
fn nested_included_config_is_detected() {
    let fixture = Fixture::new();
    let repo = fixture.init("repo");
    let includes = fixture.path("includes");
    std::fs::create_dir_all(&includes).unwrap();
    let first = includes.join("a.config");
    append(
        &repo.join(".git/config"),
        &format!(
            "[include]\n\tpath = {}\n",
            first.to_string_lossy().replace('\\', "/")
        ),
    );
    std::fs::write(&first, "[include]\n\tpath = b.config\n").unwrap();
    let nested = includes.join("b.config");
    std::fs::write(&nested, "[cardinal]\n\tnested = 1\n").unwrap();
    let file = fixture.path("snapshot");
    assert_ok(&guard(&fixture, "snapshot", &repo, &file));
    // A comment changes no entry; the file's content still counts.
    append(&nested, "# changed\n");
    assert!(
        !guard(&fixture, "verify", &repo, &file).status.success(),
        "a change in a nested included config file was not detected; {} should be among the \
         files the snapshot hashed:\n{}",
        nested.display(),
        config_sections(&file)
    );
}

/// The config entries and hashed config files of a snapshot, for a failure message.
fn config_sections(snapshot: &Path) -> String {
    let text = std::fs::read_to_string(snapshot).expect("read the snapshot");
    let mut out = String::new();
    let mut inside = false;
    for line in text.lines() {
        if line.starts_with("## ") {
            inside = line == "## config" || line == "## config files";
        }
        if inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[test]
fn hooks_path_set_through_an_include_is_detected() {
    let fixture = Fixture::new();
    let repo = fixture.init("repo");
    let hooks = fixture.path("hooks-elsewhere");
    std::fs::create_dir_all(&hooks).unwrap();
    append(
        &repo.join(".git/config"),
        "[include]\n\tpath = hooks.config\n",
    );
    std::fs::write(
        repo.join(".git/hooks.config"),
        format!(
            "[core]\n\thooksPath = {}\n",
            hooks.to_string_lossy().replace('\\', "/")
        ),
    )
    .unwrap();
    let file = fixture.path("snapshot");
    assert_ok(&guard(&fixture, "snapshot", &repo, &file));
    std::fs::write(hooks.join("pre-commit"), "#!/bin/sh\n").unwrap();
    assert!(
        !guard(&fixture, "verify", &repo, &file).status.success(),
        "a hook in the directory an included core.hooksPath names was not detected"
    );
}

/// A relative `core.hooksPath` is relative to the worktree the hooks run in.
#[test]
fn worktree_relative_hooks_path_is_detected() {
    let (fixture, repo, _) = snapshotted();
    append(
        &repo.join(".git/config"),
        "[core]\n\thooksPath = githooks\n",
    );
    let wt = fixture.path("wt");
    let file = fixture.path("wt-snapshot");
    assert_ok(&guard(&fixture, "snapshot", &wt, &file));
    std::fs::create_dir_all(wt.join("githooks")).unwrap();
    std::fs::write(wt.join("githooks/pre-commit"), "#!/bin/sh\n").unwrap();
    assert!(
        !guard(&fixture, "verify", &wt, &file).status.success(),
        "a hook in the worktree's relative core.hooksPath was not detected"
    );
}

/// The reflog of the branch a worktree has checked out counts; another worktree's does not.
#[test]
fn reflog_of_the_checked_out_branch_is_detected() {
    let (fixture, repo, file) = snapshotted();
    std::fs::write(repo.join(".git/logs/refs/heads/feature"), "").unwrap();
    assert_ok(&guard(&fixture, "verify", &repo, &file));
    std::fs::write(repo.join(".git/logs/refs/heads/main"), "").unwrap();
    assert!(
        !guard(&fixture, "verify", &repo, &file).status.success(),
        "the main worktree's branch reflog was not detected"
    );

    let wt = fixture.path("wt");
    let wt_file = fixture.path("wt-snapshot");
    assert_ok(&guard(&fixture, "snapshot", &wt, &wt_file));
    std::fs::write(repo.join(".git/logs/refs/heads/feature"), "rewritten\n").unwrap();
    assert!(
        !guard(&fixture, "verify", &wt, &wt_file).status.success(),
        "the linked worktree's branch reflog was not detected"
    );
}

#[test]
fn inherited_git_variables_do_not_redirect_the_guard() {
    let fixture = Fixture::new();
    let repo = fixture.init("repo");
    fixture.commit_file(&repo, "a.txt", "a\n", "first");
    let decoy = fixture.init("decoy");
    fixture.commit_file(&decoy, "d.txt", "d\n", "decoy");
    let decoy_git = decoy.join(".git");
    let decoy_git = decoy_git.to_string_lossy();
    let decoy_index = decoy.join(".git/index");
    let decoy_index = decoy_index.to_string_lossy();
    let decoy_top = decoy.to_string_lossy();
    let env = [
        ("GIT_DIR", &*decoy_git),
        ("GIT_WORK_TREE", &*decoy_top),
        ("GIT_INDEX_FILE", &*decoy_index),
        ("GIT_COMMON_DIR", &*decoy_git),
    ];
    let output = guard_with(&fixture, &env, "identify", &repo, None);
    assert_ok(&output);
    let identity = String::from_utf8_lossy(&output.stdout);
    assert!(identity.contains("/repo/.git"), "{identity}");
    assert!(!identity.contains("decoy"), "{identity}");

    // The snapshot is of the repository, so a change in it is seen through the variables.
    let file = fixture.path("snapshot");
    assert_ok(&guard_with(&fixture, &env, "snapshot", &repo, Some(&file)));
    fixture.branch(&repo, "other");
    assert!(
        !guard_with(&fixture, &env, "verify", &repo, Some(&file))
            .status
            .success()
    );
}

#[test]
fn a_directory_below_the_top_level_is_refused() {
    let fixture = Fixture::new();
    let repo = fixture.init("repo");
    fixture.commit_file(&repo, "sub/a.txt", "a\n", "first");
    let output = guard_with(&fixture, &[], "identify", &repo.join("sub"), None);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("not the top level"));
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
