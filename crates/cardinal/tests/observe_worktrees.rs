//! Worktree facts (`docs/architecture.md` section 8.2) and the observation side of C12:
//! merge evidence matched on the HEAD object ID, clean, unpushed, open pull request,
//! upstream, and observation failures as `unknown`. gh is a fake answering from files.

mod observe_common;

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
    scene.fx.set_upstream(&wt, "feat", "origin/main");
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
    scene.fx.detach(&detached);
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
    scene
        .fx
        .symbolic_ref_delete(&scene.repo, "refs/remotes/origin/HEAD");
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
    let main = scene.head(&scene.repo);
    scene
        .fx
        .update_ref(&scene.repo, "refs/remotes/origin/develop", &main);
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
    scene
        .fx
        .remote_set_url(&scene.repo, "origin", "https://example.com/acme/app.git");
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
    scene.fx.worktree_lock(&scene.repo, &locked);
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
    scene.fx.add(&wt, "dep");
    scene.fx.commit(&wt, "add dep");
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

/// A git step that fails makes `observed` fail even when other evidence would do: each of
/// `merge-base` and the two base queries fails (or prints something that is not a ref or an
/// object ID) while gh reports the head merged.
#[cfg(unix)]
#[test]
fn a_failed_git_step_fails_observed() {
    use std::os::unix::fs::PermissionsExt;

    let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .expect("git on PATH");
    let cases = [
        (
            "merge-base",
            "*\" merge-base \"*) echo broken >&2; exit 2;;",
        ),
        (
            "symbolic-ref",
            "*\" symbolic-ref \"*) echo fatal >&2; exit 128;;",
        ),
        (
            "rev-parse",
            "*\" rev-parse --verify \"*) echo fatal >&2; exit 128;;",
        ),
        (
            "rev-parse output",
            "*\" rev-parse --verify \"*) echo garbage; exit 0;;",
        ),
        ("signal", "*\" symbolic-ref \"*) kill -9 $$;;"),
        (
            "symbolic-ref output",
            "*\" symbolic-ref \"*) echo 'not a ref'; exit 0;;",
        ),
    ];
    for (name, arm) in cases {
        let mut scene = Scene::new();
        let wt = scene.worktree("feat", "feat");
        let head = scene.commit(&wt, "feature.txt");
        scene.push(&wt, "feat");
        scene.set_merged(&head, &merged_json(42, &head));
        let wrapper = scene.fx.path("git-wrapper");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\ncase \" $* \" in {arm} esac\nexec '{}' \"$@\"\n",
                real_git.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        scene.observe_config(&format!("git = '{}'", wrapper.display()));

        let inv = scene.inventory();
        let w = worktree(&inv, &wt);
        assert_eq!(w.facts.merge_evidence, MergeEvidence::MergedPr, "{name}");
        assert!(!inv.errors.is_empty(), "{name}: the failure is reported");
        assert_eq!(reclaim(w), (false, vec!["observed"], vec![]), "{name}");
    }
}

/// `git_dir` is the worktree's administrative entry, and `last_activity_ms` the modification
/// time of its index, which a commit moves forward (section 8.2).
#[test]
fn git_dir_and_last_activity_are_read() {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    let w = worktree(&scene.inventory(), &wt).clone();
    let admin = dunce::canonicalize(scene.repo.join(".git/worktrees/feat")).unwrap();
    assert_eq!(w.git_dir.as_deref(), Some(admin.to_str().unwrap()));
    let index_ms = |path: &std::path::Path| {
        let modified = std::fs::metadata(path).unwrap().modified().unwrap();
        u64::try_from(
            modified
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap()
    };
    assert_eq!(w.last_activity_ms, Some(index_ms(&admin.join("index"))));
    let main = worktree(&scene.inventory(), &scene.repo).clone();
    assert_eq!(
        main.last_activity_ms,
        Some(index_ms(&scene.repo.join(".git/index")))
    );
}

/// `closed_pr` (section 8.2): for a dirty worktree, the newest pull request of its branch
/// closed without merging whose head is HEAD or an ancestor of it; a merged one, one whose
/// head is not an ancestor, and one whose head is missing locally do not count. A clean
/// worktree with merge evidence does not ask.
#[test]
fn closed_pull_requests_are_read_for_dirty_or_unmerged_worktrees() {
    let (scene, wt, head) = merged_scene();
    let missing = "0123456789abcdef0123456789abcdef01234567";
    let other = scene.commit(&scene.repo, "elsewhere.txt");
    let entry = |number: u64, state: &str, oid: &str| {
        format!(
            r#"{{"number":{number},"state":"{state}","headRefOid":"{oid}","url":"https://github.com/acme/app/pull/{number}"}}"#
        )
    };
    scene.set_closed(&format!(
        "[{},{},{},{},{}]",
        entry(9, "MERGED", &head),
        entry(8, "CLOSED", missing),
        entry(7, "CLOSED", &other),
        entry(6, "CLOSED", &head),
        entry(5, "CLOSED", &head),
    ));
    let w = worktree(&scene.inventory(), &wt).clone();
    assert_eq!(w.facts.closed_pr, None, "clean and merged: not asked");
    assert!(
        !scene
            .gh_calls()
            .iter()
            .any(|c| c.contains("--state closed --head feat")),
        "{:?}",
        scene.gh_calls()
    );

    std::fs::write(wt.join("feature.txt"), "changed\n").unwrap();
    let w = worktree(&scene.inventory(), &wt).clone();
    assert_eq!(w.facts.dirty, Tri::Yes);
    let closed = w.facts.closed_pr.as_ref().expect("a closed pull request");
    assert_eq!(
        closed.number, 6,
        "the newest whose head is HEAD or below it"
    );
    assert!(
        scene
            .gh_calls()
            .iter()
            .any(|c| c.contains("--state closed") && c.contains("--head feat")),
        "{:?}",
        scene.gh_calls()
    );
    assert_eq!(reclaim(&w), (false, vec!["clean"], vec![]));
}

/// A full page of closed pull requests cannot tell "none" from "beyond the limit": the
/// lookup is a failed gh step (section 8.1).
#[test]
fn a_full_closed_page_is_unknown() {
    let (scene, wt, head) = merged_scene();
    let page: Vec<String> = (0..20)
        .map(|n| format!(r#"{{"number":{n},"state":"CLOSED","headRefOid":"{head}","url":"u{n}"}}"#))
        .collect();
    scene.set_closed(&format!("[{}]", page.join(",")));
    std::fs::write(wt.join("feature.txt"), "changed\n").unwrap();
    let inv = scene.inventory();
    let w = worktree(&inv, &wt);
    assert_eq!(w.facts.closed_pr, None);
    assert_eq!(inv.repos[0].gh, GhStatus::Unknown);
    assert!(w.reclaim.failed.contains(&"observed".to_owned()), "{w:#?}");
}

/// `content_in_base` (section 8.2): a branch whose changed files the base holds with the
/// same content, as after a squash merge, is `yes`; once the base changes one of them, `no`
/// with that path. It is read only without merge evidence.
#[test]
fn content_in_base_follows_a_squash_merge() {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    scene
        .fx
        .commit_file(&wt, "feature.txt", "the feature\n", "feature");
    scene.fx.commit_file(&wt, "notes.txt", "notes\n", "notes");
    scene.push(&wt, "feat");
    let w = worktree(&scene.inventory(), &wt).clone();
    assert_eq!(w.facts.merge_evidence, MergeEvidence::None);
    assert_eq!(w.facts.content_in_base, Tri::No);
    assert_eq!(w.facts.content_diff, ["feature.txt", "notes.txt"]);

    // The squash commit on the base, with an unrelated change after it.
    scene
        .fx
        .commit_file(&scene.repo, "feature.txt", "the feature\n", "squash");
    scene
        .fx
        .commit_file(&scene.repo, "notes.txt", "notes\n", "squash 2");
    let base = scene
        .fx
        .commit_file(&scene.repo, "other.txt", "other\n", "later");
    scene.merge_into_base(&base);
    let w = worktree(&scene.inventory(), &wt).clone();
    assert_eq!(w.facts.merge_evidence, MergeEvidence::None);
    assert_eq!(w.facts.content_in_base, Tri::Yes);
    assert_eq!(w.facts.content_diff, Vec::<String>::new());
    assert_eq!(reclaim(&w), (false, vec!["merge_evidence"], vec![]));

    let base = scene
        .fx
        .commit_file(&scene.repo, "notes.txt", "rewritten\n", "rewrite");
    scene.merge_into_base(&base);
    let w = worktree(&scene.inventory(), &wt).clone();
    assert_eq!(w.facts.content_in_base, Tri::No);
    assert_eq!(w.facts.content_diff, ["notes.txt"]);

    let (scene, wt, _) = merged_scene();
    let w = worktree(&scene.inventory(), &wt).clone();
    assert_eq!(w.facts.content_in_base, Tri::Unknown, "merged: not read");
}

/// `content_in_base` compares a gitlink as any path, also where `diff.ignoreSubmodules` hides
/// gitlinks from diffs: a branch that records an unpopulated gitlink at one commit, where the
/// base records another, is `no` with that path.
#[test]
fn a_gitlink_counts_in_content_in_base() {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    let start = scene.head(&scene.repo);
    scene.fx.stage_gitlink(&wt, "sub", &start);
    let head = scene.fx.commit(&wt, "record sub");
    scene.push(&wt, "feat");
    scene.fx.stage_gitlink(&scene.repo, "sub", &head);
    let base = scene.fx.commit(&scene.repo, "record sub elsewhere");
    scene.merge_into_base(&base);
    let w = worktree(&scene.inventory(), &wt).clone();
    assert_eq!(w.facts.merge_evidence, MergeEvidence::None);
    assert_eq!(w.facts.content_in_base, Tri::No);
    assert_eq!(w.facts.content_diff, ["sub"]);

    scene.fx.diff_ignore_submodules(&scene.repo);
    let w = worktree(&scene.inventory(), &wt).clone();
    assert_eq!(w.facts.content_in_base, Tri::No, "{w:#?}");
    assert_eq!(w.facts.content_diff, ["sub"]);
}

/// Removing the only gitlink from the index is a staged change, although status, which
/// ignores submodules, reports nothing, and the index then holds no gitlink.
#[test]
fn removing_the_only_gitlink_is_dirty() {
    let (scene, wt, head) = merged_scene();
    scene.fx.stage_gitlink(&wt, "sub", &head);
    let head = scene.fx.commit(&wt, "record sub");
    scene.push(&wt, "feat");
    scene.merge_into_base(&head);
    let w = worktree(&scene.inventory(), &wt).clone();
    assert_eq!(w.facts.dirty, Tri::No, "{w:#?}");

    scene.fx.rm_cached(&wt, "sub");
    let w = worktree(&scene.inventory(), &wt).clone();
    assert_eq!(w.facts.dirty, Tri::Yes, "{w:#?}");
    assert!(w.reclaim.failed.contains(&"clean".to_owned()), "{w:#?}");
}

/// A staged gitlink change is an uncommitted change, although status, which never enters a
/// submodule, does not report it (section 8.2); a submodule whose HEAD moved without being
/// staged is not this worktree's change.
#[test]
fn a_staged_gitlink_change_is_dirty() {
    let (scene, wt, _) = merged_scene();
    let sub = scene.fx.init("feat/sub");
    scene.fx.commit_file(&sub, "a.txt", "a\n", "a");
    scene.fx.add(&wt, "sub");
    scene.fx.commit(&wt, "add a gitlink");
    scene.push(&wt, "feat");
    scene.merge_into_base(&scene.head(&wt));
    let w = worktree(&scene.inventory(), &wt).clone();
    assert_eq!(w.facts.dirty, Tri::No, "{w:#?}");

    scene.fx.commit_file(&sub, "b.txt", "b\n", "b");
    let w = worktree(&scene.inventory(), &wt).clone();
    assert_eq!(
        w.facts.dirty,
        Tri::No,
        "a submodule's own commit is not staged"
    );

    scene.fx.add(&wt, "sub");
    let w = worktree(&scene.inventory(), &wt).clone();
    assert_eq!(w.facts.dirty, Tri::Yes, "the staged gitlink");
    assert!(w.reclaim.failed.contains(&"clean".to_owned()), "{w:#?}");
}
