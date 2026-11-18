//! The squash-merge judgment (C12, A14; `docs/architecture.md` sections 8.2 and 9.3): a
//! worktree whose branch was squash-merged without a pull request has no merge evidence, so
//! its plan needs approval, and its `merge_evidence` says that `content_in_base` is `yes`.
//! `--squash-merged` turns that into evidence only when `act`'s own observation finds every
//! file the branch changed identical in the base, and never skips an invariant.

mod observe_common;
mod plan_support;
mod support;

use std::path::PathBuf;

use cardinal::act::ActOptions;
use cardinal::contract::{ActStatus, Caller, CheckResult, Plan, PlanKind};
use plan_support::{Setup, arbiter};

fn squash_merged(caller: Caller) -> ActOptions {
    ActOptions {
        squash_merged: true,
        caller,
        ..arbiter()
    }
}

/// A clean, pushed worktree whose branch changed `feature.txt`, and a base that holds the
/// same content in a squash commit, with no pull request gh knows of.
fn squashed(s: &Setup, name: &str) -> PathBuf {
    let wt = s.fx.worktree_add(&s.main, name, name);
    s.fx.commit_file(&wt, "feature.txt", &format!("{name}\n"), "work");
    s.fx.commit_file(&wt, "feature.txt", &format!("{name} done\n"), "more");
    s.publish(&wt, name);
    let squash =
        s.fx.commit_file(&s.main, "feature.txt", &format!("{name} done\n"), "squash");
    s.publish(&s.main, "main");
    s.merge_into_base(&squash);
    wt
}

fn merge_check(plan: &Plan) -> &cardinal::contract::Check {
    plan.checks
        .iter()
        .find(|c| c.name == "merge_evidence")
        .unwrap()
}

fn fact<'a>(check: &'a cardinal::contract::Check, name: &str) -> &'a serde_json::Value {
    &check
        .facts
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("{name} in {check:#?}"))
        .value
}

/// The control: without the flag the plan needs approval and its `merge_evidence` says the
/// content is in the base; the arbiter's and the owner's `--squash-merged` report `dry_run`.
#[test]
fn judged_squash_merge_reports_dry_run() {
    let s = Setup::new();
    let wt = squashed(&s, "squashed");
    let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
    assert!(plan.invariants_ok, "{:#?}", plan.checks);
    let check = merge_check(&plan);
    assert_eq!(check.result, CheckResult::Fail);
    assert!(
        check.detail.contains("content_in_base is yes"),
        "{check:#?}"
    );
    assert_eq!(fact(check, "worktree.facts.content_in_base"), "yes");

    let refused = s.act(&plan.id, &arbiter()).unwrap();
    assert_eq!(refused.status, ActStatus::Rejected);
    assert_eq!(refused.reasons, ["owner_approval_required"]);
    for caller in [Caller::Arbiter, Caller::Owner] {
        let result = s.act(&plan.id, &squash_merged(caller.clone())).unwrap();
        assert_eq!(result.status, ActStatus::DryRun, "{caller}: {result:#?}");
        assert_eq!(result.reasons, ["execution_not_designed"]);
        let check = result
            .checks
            .iter()
            .find(|c| c.name == "merge_evidence")
            .unwrap();
        assert_eq!(check.result, CheckResult::Pass);
        assert_eq!(fact(check, "act.squash_merged"), true);
    }
    assert!(wt.exists());
}

/// A branch whose changed files differ in the base gets no evidence from the judgment.
#[test]
fn content_not_in_the_base_is_no_evidence() {
    let s = Setup::new();
    let wt = s.fx.worktree_add(&s.main, "diverged", "diverged");
    s.fx.commit_file(&wt, "feature.txt", "mine\n", "work");
    s.publish(&wt, "diverged");
    let other =
        s.fx.commit_file(&s.main, "feature.txt", "theirs\n", "other");
    s.publish(&s.main, "main");
    s.merge_into_base(&other);
    let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
    assert_eq!(
        fact(merge_check(&plan), "worktree.facts.content_diff"),
        &serde_json::json!(["feature.txt"])
    );
    let result = s.act(&plan.id, &squash_merged(Caller::Arbiter)).unwrap();
    assert_eq!(result.status, ActStatus::Rejected);
    assert_eq!(result.reasons, ["owner_approval_required"]);
}

/// The judgment never skips an invariant: a dirty worktree whose content is in the base is
/// refused by `clean` alone.
#[test]
fn the_judgment_skips_no_invariant() {
    let s = Setup::new();
    let wt = squashed(&s, "dirty");
    std::fs::write(wt.join("feature.txt"), "edited\n").unwrap();
    let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
    let result = s.act(&plan.id, &squash_merged(Caller::Arbiter)).unwrap();
    assert_eq!(result.status, ActStatus::Rejected);
    assert_eq!(result.reasons, ["clean"]);
}

/// A14: `act` reads `content_in_base` again at its checks. The plan was made with
/// `content_in_base: "yes"`; at `act.snapshotted` only the local base ref moves, to a commit
/// that changes the file the branch changed, and the plan is refused for want of approval,
/// while the same run without the move reports `dry_run`.
#[test]
fn content_in_base_read_at_the_checks() {
    let s = Setup::new();
    let wt = squashed(&s, "moved");
    let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
    assert_eq!(
        fact(merge_check(&plan), "worktree.facts.content_in_base"),
        "yes"
    );
    let options = squash_merged(Caller::Arbiter);

    let unchanged = s.act_at(&plan.id, &options, "act.snapshotted", || {});
    assert_eq!(unchanged.status, ActStatus::DryRun, "{unchanged:#?}");

    let rewrite =
        s.fx.commit_file(&s.main, "feature.txt", "rewritten\n", "rewrite");
    let moved = s.act_at(&plan.id, &options, "act.snapshotted", || {
        s.merge_into_base(&rewrite);
    });
    assert_eq!(moved.status, ActStatus::Rejected, "{moved:#?}");
    assert_eq!(moved.reasons, ["owner_approval_required"]);
    let check = moved
        .checks
        .iter()
        .find(|c| c.name == "merge_evidence")
        .unwrap();
    assert_eq!(fact(check, "worktree.facts.content_in_base"), "no");
}
