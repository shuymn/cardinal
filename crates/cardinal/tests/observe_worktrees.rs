//! Worktree facts (`docs/architecture.md` section 8.2) and the observation side of C12:
//! merge evidence matched on the HEAD object ID, clean, unpushed, open pull request,
//! upstream, and observation failures as `unknown`. gh is a fake answering from files.

mod observe_common;
mod support;

use std::time::Instant;

use cardinal::contract::{GhStatus, MergeEvidence, Tri, Worktree};
use observe_common::{PROMPT, Scene, merged_json, open_entry, worktree};

fn reclaim(w: &Worktree) -> (bool, Vec<&str>, Vec<&str>) {
    (
        w.reclaim.candidate,
        w.reclaim.failed.iter().map(String::as_str).collect(),
        w.reclaim.unknown.iter().map(String::as_str).collect(),
    )
}

/// A linked worktree `feat` with one commit, pushed, and merged into the base.
fn merged_scene() -> (Scene, std::path::PathBuf, String) {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    let head = scene.commit(&wt, "feature.txt");
    scene.push(&wt, "feat");
    scene.merge_into_base(&head);
    (scene, wt, head)
}

#[test]
fn control_is_a_candidate() {
    let (scene, wt, head) = merged_scene();
    let inv = scene.inventory();
    assert_eq!(inv.repos.len(), 1);
    let repo = &inv.repos[0];
    assert_eq!(repo.github.as_deref(), Some(observe_common::GITHUB));
    assert_eq!(repo.gh, GhStatus::Ok);
    assert_eq!(repo.base.as_ref().unwrap().ref_name, "origin/main");
    assert_eq!(repo.base.as_ref().unwrap().oid, head);

    let w = worktree(&inv, &wt);
    assert!(!w.main);
    assert_eq!(w.head.as_deref(), Some(head.as_str()));
    assert_eq!(w.branch.as_deref(), Some("feat"));
    assert_eq!(w.upstream.as_deref(), Some("origin/feat"));
    assert_eq!(w.facts.dirty, Tri::No);
    assert_eq!(w.facts.has_upstream, Tri::Yes);
    assert_eq!(w.facts.unpushed, Tri::No);
    assert_eq!(w.facts.open_pr, Tri::No);
    assert_eq!(w.facts.merge_evidence, MergeEvidence::BaseAncestor);
    assert_eq!(reclaim(w), (true, vec![], vec![]));
    assert!(
        scene.gh_calls().iter().all(|c| !c.contains("merged")),
        "a base ancestor needs no merged pull request lookup: {:?}",
        scene.gh_calls()
    );

    let main = worktree(&inv, &scene.repo);
    assert!(main.main);
    assert!(
        !main.reclaim.candidate,
        "a main worktree is never a candidate"
    );
}

#[test]
fn merged_pull_request_head_is_evidence_and_its_commits_count_as_pushed() {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    let head = scene.commit(&wt, "feature.txt");
    // The branch was pushed, merged by squash, and deleted; only the pull request knows.
    scene
        .fx
        .git(&wt, &["branch", "--set-upstream-to=origin/main", "feat"]);
    scene.set_merged(&head, &merged_json(42, &head));
    let inv = scene.inventory();
    let w = worktree(&inv, &wt);
    assert_eq!(w.facts.merge_evidence, MergeEvidence::MergedPr);
    assert_eq!(w.facts.merged_pr.as_ref().unwrap().number, 42);
    assert_eq!(w.facts.unpushed, Tri::No, "the head exists on GitHub");
    assert_eq!(reclaim(w), (true, vec![], vec![]));
}

#[test]
fn commits_after_a_merged_pull_request_give_no_evidence() {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    let merged = scene.commit(&wt, "feature.txt");
    scene.push(&wt, "feat");
    let head = scene.commit(&wt, "after.txt");
    // A search for the new head still finds the pull request merged at the old one.
    scene.set_merged(&head, &merged_json(42, &merged));
    let inv = scene.inventory();
    let w = worktree(&inv, &wt);
    assert_eq!(w.facts.merge_evidence, MergeEvidence::None);
    assert_eq!(w.facts.merged_pr, None);
    assert_eq!(w.facts.unpushed, Tri::Yes);
    let (candidate, failed, unknown) = reclaim(w);
    assert!(!candidate);
    assert_eq!(failed, ["no_unpushed", "merge_evidence"]);
    assert_eq!(unknown, Vec::<&str>::new());
}

#[test]
fn a_reused_branch_name_gives_no_evidence() {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    let head = scene.commit(&wt, "again.txt");
    scene.push(&wt, "feat");
    let old = "0123456789abcdef0123456789abcdef01234567";
    scene.set_merged_default(&merged_json(7, old));
    let inv = scene.inventory();
    let w = worktree(&inv, &wt);
    assert_eq!(w.facts.merge_evidence, MergeEvidence::None);
    assert!(
        scene
            .gh_calls()
            .iter()
            .any(|c| c.contains("merged") && c.contains(&head)),
        "the lookup searches by head object ID: {:?}",
        scene.gh_calls()
    );
    assert_eq!(reclaim(w), (false, vec!["merge_evidence"], vec![]));
}

#[test]
fn dirty_worktrees_are_not_clean() {
    let (scene, wt, _) = merged_scene();
    std::fs::write(wt.join("feature.txt"), "changed\n").unwrap();
    let w = worktree(&scene.inventory(), &wt).clone();
    assert_eq!(w.facts.dirty, Tri::Yes);
    assert_eq!(reclaim(&w), (false, vec!["clean"], vec![]));

    std::fs::write(wt.join("feature.txt"), "feature.txt\n").unwrap();
    std::fs::write(wt.join("untracked.txt"), "new\n").unwrap();
    let w = worktree(&scene.inventory(), &wt).clone();
    assert_eq!(w.facts.dirty, Tri::Yes, "an untracked file is a change");
}

#[test]
fn unpushed_commits_are_reported() {
    let (scene, wt, _) = merged_scene();
    scene.commit(&wt, "local.txt");
    let w = worktree(&scene.inventory(), &wt).clone();
    assert_eq!(w.facts.unpushed, Tri::Yes);
    assert_eq!(w.facts.merge_evidence, MergeEvidence::None);
    assert!(w.reclaim.failed.contains(&"no_unpushed".to_owned()));
}

#[test]
fn open_pull_requests_match_by_branch_or_head_from_this_repository() {
    let scene = Scene::new();
    let by_name = scene.worktree("by-name", "by-name");
    scene.commit(&by_name, "a.txt");
    scene.push(&by_name, "by-name");
    let by_head = scene.worktree("by-head", "by-head");
    let head = scene.commit(&by_head, "b.txt");
    scene.push(&by_head, "by-head");
    let fork = scene.worktree("fork", "fork");
    let fork_head = scene.commit(&fork, "c.txt");
    scene.push(&fork, "fork");
    scene.set_open(&format!(
        "[{},{},{}]",
        open_entry(
            1,
            "by-name",
            "0000000000000000000000000000000000000000",
            false
        ),
        open_entry(2, "renamed", &head, false),
        open_entry(3, "fork", &fork_head, true),
    ));
    let inv = scene.inventory();
    assert_eq!(worktree(&inv, &by_name).facts.open_pr, Tri::Yes);
    assert_eq!(worktree(&inv, &by_head).facts.open_pr, Tri::Yes);
    assert_eq!(
        worktree(&inv, &fork).facts.open_pr,
        Tri::No,
        "a fork's branch of the same name is not this worktree's"
    );
    assert!(
        worktree(&inv, &by_name)
            .reclaim
            .failed
            .contains(&"no_open_pr".to_owned())
    );
}

#[test]
fn missing_upstream_and_detached_head_have_no_upstream() {
    let scene = Scene::new();
    let local = scene.worktree("local", "local");
    let detached = scene.worktree("detached", "detached");
    let head = scene.head(&detached);
    scene
        .fx
        .git(&detached, &["checkout", "-q", "--detach", &head]);
    let inv = scene.inventory();
    let w = worktree(&inv, &local);
    assert_eq!(w.upstream, None);
    assert_eq!(w.facts.has_upstream, Tri::No);
    assert!(w.reclaim.failed.contains(&"has_upstream".to_owned()));
    let w = worktree(&inv, &detached);
    assert_eq!(w.branch, None);
    assert_eq!(w.facts.has_upstream, Tri::No);
    assert_eq!(w.head.as_deref(), Some(head.as_str()));
}

/// Every gh failure makes the facts that depend on it `unknown`, never "safe", and the
/// repository reports `gh: unknown` with the reason.
#[test]
fn gh_failures_make_pull_request_facts_unknown() {
    type Case = (&'static str, fn(&Scene));
    let cases: [Case; 4] = [
        ("exit 1", |s| s.gh_fails("open", 1)),
        ("invalid JSON", |s| s.set_open("{not json")),
        ("a full page", |s| {
            let entries: Vec<String> = (0..500)
                .map(|n| {
                    open_entry(
                        n,
                        &format!("b{n}"),
                        "0000000000000000000000000000000000000000",
                        false,
                    )
                })
                .collect();
            s.set_open(&format!("[{}]", entries.join(",")));
        }),
        ("timeout", |s| s.gh_hangs("open")),
    ];
    for (name, break_gh) in cases {
        let (mut scene, wt, _) = merged_scene();
        scene.gh_timeout_ms = 2000;
        break_gh(&scene);
        let start = Instant::now();
        let inv = scene.inventory();
        assert!(start.elapsed() < PROMPT, "{name}: {:?}", start.elapsed());
        let repo = &inv.repos[0];
        assert_eq!(repo.gh, GhStatus::Unknown, "{name}");
        assert!(repo.gh_error.is_some(), "{name}");
        let w = worktree(&inv, &wt);
        assert_eq!(w.facts.open_pr, Tri::Unknown, "{name}");
        let (candidate, failed, unknown) = reclaim(w);
        assert!(!candidate, "{name}");
        assert_eq!(failed, ["observed"], "{name}");
        assert_eq!(unknown, ["no_open_pr"], "{name}");
    }
}

#[test]
fn a_failed_merged_lookup_makes_merge_evidence_unknown() {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    scene.commit(&wt, "feature.txt");
    scene.push(&wt, "feat");
    scene.gh_fails("merged", 1);
    let inv = scene.inventory();
    let w = worktree(&inv, &wt);
    assert_eq!(w.facts.merge_evidence, MergeEvidence::Unknown);
    assert_eq!(inv.repos[0].gh, GhStatus::Unknown);
    assert_eq!(
        reclaim(w),
        (false, vec!["observed"], vec!["merge_evidence"])
    );
}

#[test]
fn a_missing_base_makes_merge_evidence_unknown() {
    let scene = Scene::new();
    scene.fx.git(
        &scene.repo,
        &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"],
    );
    let wt = scene.worktree("feat", "feat");
    scene.commit(&wt, "feature.txt");
    scene.push(&wt, "feat");
    let inv = scene.inventory();
    assert_eq!(inv.repos[0].base, None);
    let w = worktree(&inv, &wt);
    assert_eq!(w.facts.merge_evidence, MergeEvidence::Unknown);
    assert!(w.reclaim.unknown.contains(&"merge_evidence".to_owned()));
}

#[test]
fn a_configured_base_wins() {
    let (scene, wt, _) = merged_scene();
    scene.fx.git(
        &scene.repo,
        &["update-ref", "refs/remotes/origin/develop", "HEAD~0"],
    );
    scene.profile(
        "app",
        &format!(
            "[match]\npaths = ['{}']\n\n[git]\nbase = 'origin/develop'\n",
            scene.repo.display()
        ),
    );
    let inv = scene.inventory();
    assert_eq!(inv.repos[0].profiles, ["app"]);
    assert_eq!(
        inv.repos[0].base.as_ref().unwrap().ref_name,
        "origin/develop"
    );
    assert_eq!(
        worktree(&inv, &wt).facts.merge_evidence,
        MergeEvidence::None,
        "origin/develop does not contain the feature"
    );
}

#[test]
fn a_remote_outside_github_makes_pull_request_facts_unknown() {
    let (scene, wt, _) = merged_scene();
    scene.fx.git(
        &scene.repo,
        &[
            "remote",
            "set-url",
            "origin",
            "https://example.com/acme/app.git",
        ],
    );
    let inv = scene.inventory();
    assert_eq!(inv.repos[0].github, None);
    assert_eq!(inv.repos[0].gh, GhStatus::NotApplicable);
    assert_eq!(worktree(&inv, &wt).facts.open_pr, Tri::Unknown);
    assert!(
        scene.gh_calls().is_empty(),
        "gh is not asked about other hosts"
    );
}

#[test]
fn a_git_failure_makes_worktree_facts_unknown() {
    let (scene, wt, _) = merged_scene();
    // The directory is gone; git still lists the worktree (prunable).
    std::fs::remove_dir_all(&wt).unwrap();
    let inv = scene.inventory();
    let w = worktree(&inv, &wt);
    assert_eq!(w.facts.dirty, Tri::Unknown);
    assert_eq!(w.facts.has_upstream, Tri::Unknown);
    assert!(!w.reclaim.candidate);
    assert!(w.reclaim.failed.contains(&"observed".to_owned()));
    assert!(
        inv.errors.iter().any(|e| e.scope.starts_with("worktree:")),
        "{:?}",
        inv.errors
    );
}

#[test]
fn a_git_that_cannot_start_is_reported_not_trusted() {
    let mut scene = Scene::new();
    scene.observe_config("git = 'cardinal-no-such-git'");
    let inv = scene.inventory();
    assert_eq!(inv.repos, []);
    assert!(
        inv.errors.iter().any(|e| e.scope.starts_with("repo:")),
        "{:?}",
        inv.errors
    );
    let err = scene.observe(&scene.repo).unwrap_err();
    assert!(err.message.contains("cardinal-no-such-git"), "{err}");
}

#[test]
fn managed_and_git_locked_worktrees_are_marked() {
    let (mut scene, wt, _) = merged_scene();
    let locked = scene.worktree("locked", "locked");
    scene.fx.git(
        &scene.repo,
        &["worktree", "lock", &locked.to_string_lossy()],
    );
    let root = scene.fx.path("feat");
    scene.observe_config(&format!(
        "managed = [{{ app = 't3code', root = '{}' }}]",
        root.display()
    ));
    let inv = scene.inventory();
    let w = worktree(&inv, &wt);
    assert_eq!(w.managed_by.as_deref(), Some("t3code"));
    assert_eq!(reclaim(w), (false, vec!["not_managed"], vec![]));
    assert_eq!(
        worktree(&inv, &locked).managed_by.as_deref(),
        Some("git-lock")
    );
}

#[test]
fn observe_worktree_finds_the_worktree_of_a_path_inside_it() {
    let (scene, wt, head) = merged_scene();
    std::fs::create_dir_all(wt.join("node_modules/x")).unwrap();
    let obs = scene.observe(&wt.join("node_modules/x")).unwrap();
    assert_eq!(
        obs.worktree.path,
        dunce::canonicalize(&wt).unwrap().to_string_lossy()
    );
    assert_eq!(obs.worktree.head.as_deref(), Some(head.as_str()));
    assert_eq!(obs.repo.worktrees, []);
    assert_eq!(obs.repo.gh, GhStatus::Ok);

    let outside = scene.fx.path("plain");
    std::fs::create_dir_all(&outside).unwrap();
    let err = scene.observe(&outside).unwrap_err();
    assert_eq!(err.code, cardinal::contract::ErrorCode::NotFound);
}

#[test]
fn tracked_protected_data_is_not_a_candidate() {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    scene.commit(&wt, "crate/proptest-regressions/case.txt");
    let head = scene.commit(&wt, "feature.txt");
    scene.push(&wt, "feat");
    scene.merge_into_base(&head);
    scene.profile("data", "[data]\nprotected = ['proptest-regressions']\n");
    let inv = scene.inventory();
    let w = worktree(&inv, &wt);
    assert_eq!(w.facts.dirty, Tri::No);
    assert_eq!(reclaim(w), (false, vec!["no_protected"], vec![]));
}

#[test]
fn a_gitlink_without_gitmodules_is_a_nested_repository() {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    let dep = scene.fx.init("feat/dep");
    scene.fx.commit_file(&dep, "lib.txt", "lib\n", "dep");
    scene.fx.git(&wt, &["add", "dep"]);
    scene.fx.git(&wt, &["commit", "-q", "-m", "add dep"]);
    assert!(!wt.join(".gitmodules").exists());
    let head = scene.head(&wt);
    scene.push(&wt, "feat");
    scene.merge_into_base(&head);
    let inv = scene.inventory();
    let w = worktree(&inv, &wt);
    assert_eq!(w.facts.dirty, Tri::No);
    assert_eq!(reclaim(w), (false, vec!["no_nested_repository"], vec![]));
}

#[test]
fn a_full_merged_page_without_the_head_is_unknown() {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    let head = scene.commit(&wt, "feature.txt");
    scene.push(&wt, "feat");
    let other = "0123456789abcdef0123456789abcdef01234567";
    let entries: Vec<String> = (0..20)
        .map(|n| format!(r#"{{"number":{n},"headRefOid":"{other}","mergedAt":null,"url":"u{n}"}}"#))
        .collect();
    scene.set_merged(&head, &format!("[{}]", entries.join(",")));
    let inv = scene.inventory();
    let w = worktree(&inv, &wt);
    assert_eq!(w.facts.merge_evidence, MergeEvidence::Unknown);
    assert_eq!(inv.repos[0].gh, GhStatus::Unknown);
    assert!(!w.reclaim.candidate);
}

/// A git step that fails makes `observed` fail even when other evidence would do: here
/// `merge-base` fails while gh reports the head merged.
#[cfg(unix)]
#[test]
fn a_failed_git_step_fails_observed() {
    use std::os::unix::fs::PermissionsExt;

    let mut scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    let head = scene.commit(&wt, "feature.txt");
    scene.push(&wt, "feat");
    scene.set_merged(&head, &merged_json(42, &head));
    let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .expect("git on PATH");
    let wrapper = scene.fx.path("git-wrapper");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\ncase \" $* \" in *\" merge-base \"*) echo broken >&2; exit 2;; esac\nexec '{}' \"$@\"\n",
            real_git.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    scene.observe_config(&format!("git = '{}'", wrapper.display()));

    let inv = scene.inventory();
    let w = worktree(&inv, &wt);
    assert_eq!(w.facts.merge_evidence, MergeEvidence::MergedPr);
    assert!(
        inv.errors.iter().any(|e| e.message.contains("merge-base")),
        "{:?}",
        inv.errors
    );
    assert_eq!(reclaim(w), (false, vec!["observed"], vec![]));
}
