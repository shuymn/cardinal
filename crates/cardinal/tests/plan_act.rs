//! `plan` and `act` against observed facts (`docs/architecture.md` sections 9.2–9.5): which
//! worktrees are plans without approval (C12), whole-target inspection (C13), identity
//! binding (C14), recomputed approval (A6), plain targets (A7), and managed worktrees (A8).
//! Every refusal is paired with a control that passes every check and reports `dry_run`.
//!
//! `plan` and `act` run as the `cardinal` binary on fixture repositories, observed by the real
//! `observe` and `admission` modules with GitHub answered by the fake `gh`
//! (`tests/plan_support`); a fact changes through the fixture, gh's answers, or a real
//! reservation. The tests that inspect targets run on Unix, where `act` reads its snapshot
//! through directory descriptors; `act_report_only` runs the report-only contract on every
//! platform.

#![cfg_attr(
    not(unix),
    allow(
        dead_code,
        unused_imports,
        reason = "the tests that inspect targets run on Unix"
    )
)]

mod observe_common;
mod plan_support;
mod support;

use std::path::Path;

use cardinal::contract::{ActStatus, Approval, CheckResult, InUse, Plan, PlanKind};
use plan_support::{Setup, arbiter, write};

#[test]
fn helper() {
    support::helper::run_if_requested();
}

fn check(plan_or_checks: &[cardinal::contract::Check], name: &str) -> CheckResult {
    plan_or_checks
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("no check {name}"))
        .result
}

/// `--owner-approved`, which only the owner gives (section 9.3).
fn owner_approved() -> cardinal::act::ActOptions {
    cardinal::act::ActOptions {
        owner_approved: true,
        caller: cardinal::contract::Caller::Owner,
        ..arbiter()
    }
}

/// A whole-worktree plan that the arbiter may act on without approval.
fn assert_no_approval(plan: &Plan) {
    assert!(plan.invariants_ok, "{:#?}", plan.checks);
    assert_eq!(plan.approval, Approval::None, "{:#?}", plan.checks);
}

/// C12: a whole worktree is a plan without approval only when every reclaim condition holds.
#[cfg(unix)]
mod candidates {
    use super::*;

    #[test]
    fn control_passes_without_approval() {
        let s = Setup::new();
        let wt = s.worktree("done");
        write(&wt, "node_modules/pkg/index.js", "x");
        let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
        assert_no_approval(&plan);
        assert!(plan.allocated_bytes.unwrap() > 0);
        let result = s.act(&plan.id, &arbiter()).unwrap();
        assert_eq!(result.status, ActStatus::DryRun, "{result:#?}");
        assert!(wt.join("node_modules/pkg/index.js").exists());
    }

    /// Observation matches merge evidence on the HEAD object: commits added after the merged
    /// PR's head leave no evidence.
    #[test]
    fn commits_after_merged_pr_need_approval() {
        let s = Setup::new();
        let wt = s.worktree("merged-then-more");
        s.fx.commit_file(&wt, "src/more.rs", "// more\n", "more work");
        s.publish(&wt, "merged-then-more");
        let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
        assert!(plan.invariants_ok, "{:#?}", plan.checks);
        assert_eq!(plan.approval, Approval::Owner);
        assert_eq!(check(&plan.checks, "merge_evidence"), CheckResult::Fail);
        let result = s.act(&plan.id, &arbiter()).unwrap();
        assert_eq!(result.status, ActStatus::Rejected);
        assert_eq!(result.reasons, ["owner_approval_required"]);
        assert!(wt.join("src/more.rs").exists());
    }

    /// A branch name reused for new work after its PR merged gives no evidence either: gh
    /// finds the merged PR, whose head is the branch's old HEAD.
    #[test]
    fn reused_branch_name_needs_approval() {
        let s = Setup::new();
        let wt = s.worktree("reused");
        let old = s.head(&wt);
        s.fx.commit_file(&wt, "src/new.rs", "// new work\n", "new work");
        s.publish(&wt, "reused");
        s.set_merged(&s.head(&wt), &observe_common::merged_json(99, &old));
        let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
        assert_eq!(plan.approval, Approval::Owner, "{:#?}", plan.checks);
        let result = s.act(&plan.id, &arbiter()).unwrap();
        assert_eq!(result.status, ActStatus::Rejected);
        assert_eq!(result.reasons, ["owner_approval_required"]);
        assert!(wt.join("src/lib.rs").exists());
    }

    /// No upstream is an invariant: not even the owner's approval reclaims it (G6). The
    /// branch's commits are on the remote, so nothing else fails.
    #[test]
    fn no_upstream_is_refused_even_with_approval() {
        let s = Setup::new();
        let wt = s.fx.worktree_add(&s.main, "local-only", "local-only");
        s.fx.commit_file(&wt, "src/lib.rs", "// local\n", "work");
        s.fx.update_ref(&wt, "refs/remotes/origin/local-only", &s.head(&wt));
        s.merge(&wt);
        let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
        assert!(!plan.invariants_ok);
        assert_eq!(check(&plan.checks, "has_upstream"), CheckResult::Fail);
        let result = s.act(&plan.id, &owner_approved()).unwrap();
        assert_eq!(result.status, ActStatus::Rejected);
        assert_eq!(result.reasons, ["has_upstream"]);
        assert!(wt.join("src/lib.rs").exists());
    }

    /// A failed gh call makes the PR facts unknown, which is never safe.
    #[test]
    fn gh_failure_is_refused_even_with_approval() {
        let s = Setup::new();
        let wt = s.worktree("gh-down");
        s.gh_fails("open", 1);
        s.gh_fails("merged", 1);
        let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
        assert!(!plan.invariants_ok);
        assert_eq!(check(&plan.checks, "observed"), CheckResult::Fail);
        assert_eq!(check(&plan.checks, "no_open_pr"), CheckResult::Unknown);
        let result = s.act(&plan.id, &owner_approved()).unwrap();
        assert_eq!(result.status, ActStatus::Rejected);
        assert!(
            result.reasons.contains(&"observed".to_owned()),
            "{result:#?}"
        );
        assert!(result.reasons.contains(&"no_open_pr".to_owned()));
        assert!(wt.join("src/lib.rs").exists());
    }

    /// A worktree that is dirty, has a commit nobody pushed, or has an open PR is refused, also
    /// with the owner's approval. The unpushed commit is not merged either, which only the
    /// approval covers.
    #[test]
    fn dirty_unpushed_or_open_pr_is_refused() {
        type Change = fn(&Setup, &Path, &str);
        let s = Setup::new();
        let cases: [(&str, &str, Change); 3] = [
            ("dirty", "clean", |_, wt, _| {
                write(wt, "src/lib.rs", "// edited\n");
            }),
            ("unpushed", "no_unpushed", |s, wt, _| {
                s.fx.commit_file(wt, "src/more.rs", "// more\n", "more work");
            }),
            ("open-pr", "no_open_pr", |s, wt, name| {
                let entry = observe_common::open_entry(7, name, &s.head(wt), false);
                s.set_open(&format!("[{entry}]"));
            }),
        ];
        for (name, failing, change) in cases {
            let wt = s.worktree(name);
            change(&s, &wt, name);
            let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
            let result = s.act(&plan.id, &owner_approved()).unwrap();
            assert_eq!(result.status, ActStatus::Rejected, "{name}");
            assert_eq!(result.reasons, [failing], "{name}");
            assert!(wt.join("src/lib.rs").exists(), "{name}");
        }
    }

    /// In use: a reservation (a job whose runner crashed with its group running), the in-use
    /// lock (a running job), or a process with its working directory inside. The budget
    /// admits the one job.
    #[test]
    fn worktree_in_use_is_refused() {
        let s = Setup::with_host("[budget]\ncpu = 1");
        let reserved = s.worktree("reserved");
        let locked = s.worktree("locked");
        let busy = s.worktree("busy");
        let plans: Vec<Plan> = [&reserved, &locked, &busy]
            .iter()
            .map(|wt| s.plan(PlanKind::ReclaimWorktree, wt))
            .collect();

        let jobs = support::jobs::Jobs::new(s.fx.scope(), &s.state_dir, &s.config_dir);
        let crashed = jobs.crashed(&reserved, 1);
        let lock_path = s
            .state
            .worktree_lock(&cardinal::state::worktree_key(&locked));
        let _shared = cardinal::state::try_lock(&lock_path, cardinal::state::LockMode::Shared)
            .unwrap()
            .unwrap();
        let mut sleeper = support::helper::spawn(
            s.fx.scope(),
            &support::helper::Mode::Sleep,
            &busy.join("src"),
        );
        cardinal_test_support::process::wait_for(
            "the sleeper's cwd",
            std::time::Duration::from_secs(10),
            || cardinal::os::cwd(sleeper.id()).filter(|cwd| cwd.starts_with(&busy)),
        );

        for (plan, wt) in plans.iter().zip([&reserved, &locked, &busy]) {
            let result = s.act(&plan.id, &owner_approved()).unwrap();
            assert_eq!(result.status, ActStatus::Rejected, "{}", wt.display());
            assert_eq!(result.reasons, ["not_in_use"], "{}", wt.display());
            assert!(wt.join("src/lib.rs").exists());
        }
        let facts = &result_facts(&s, &plans[0], "not_in_use");
        assert!(
            facts.contains(&crashed.job),
            "the reservation names the worktree: {facts}"
        );
        sleeper.kill();
    }

    /// The facts the latest `act` record of `plan` holds for check `name`, as JSON text.
    fn result_facts(s: &Setup, plan: &Plan, name: &str) -> String {
        let records = cardinal::record::read_all(&s.state).unwrap();
        let act = records
            .iter()
            .rev()
            .find(|r| {
                r.kind == cardinal::contract::RecordKind::Act
                    && r.plan_id.as_deref() == Some(&plan.id)
            })
            .expect("an act record");
        let check = act.data["checks"]
            .as_array()
            .and_then(|checks| checks.iter().find(|c| c["name"] == name))
            .expect("the check");
        check["facts"].to_string()
    }
}

/// C14: a plan is bound to its target's identity, observed again whenever `act` evaluates it.
#[cfg(unix)]
mod identity {
    use super::*;

    #[test]
    fn new_worktree_at_the_same_path_expires() {
        let s = Setup::new();
        let wt = s.worktree("again");
        let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
        // Hold the old root open so the new directory cannot reuse its inode.
        let _old_root = std::fs::File::open(&wt).unwrap();
        s.fx.worktree_remove(&s.main, &wt);
        s.fx.worktree_add_existing(&s.main, "again", "again");
        let result = s.act(&plan.id, &arbiter()).unwrap();
        assert_eq!(result.status, ActStatus::Expired);
        assert_eq!(result.reasons, ["identity_changed:root_id"]);
        assert!(wt.join("src/lib.rs").exists(), "nothing deleted");
    }

    #[test]
    fn head_advanced_while_clean_expires() {
        let s = Setup::new();
        let wt = s.worktree("advanced");
        let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
        s.fx.commit_file(&wt, "src/new.rs", "// new\n", "advance");
        let result = s.act(&plan.id, &arbiter()).unwrap();
        assert_eq!(result.status, ActStatus::Expired);
        assert_eq!(result.reasons, ["identity_changed:head"]);
        assert!(wt.join("src/new.rs").exists());
    }

    #[test]
    fn plan_of_another_host_expires() {
        let s = Setup::new();
        let wt = s.worktree("elsewhere");
        write(&wt, "target/x", "x");
        let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
        let file = s.state.plan_file(&plan.id);
        let text = std::fs::read_to_string(&file)
            .unwrap()
            .replace(s.state.host_id(), "ffffffffffffffff");
        std::fs::write(&file, text).unwrap();
        let result = s.act(&plan.id, &arbiter()).unwrap();
        assert_eq!(result.status, ActStatus::Expired);
        assert_eq!(result.reasons, ["identity_changed:host_id"]);
        assert!(wt.join("target/x").exists());
        assert!(
            !s.state.execution_claim(&plan.id).exists(),
            "an expired plan of another host is not claimed"
        );
    }
}

/// The detail of check `name` in `result`.
#[cfg(unix)]
fn detail<'a>(result: &'a cardinal::contract::ActResult, name: &str) -> &'a str {
    &result
        .checks
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("no check {name}"))
        .detail
}

/// C13: `act` inspects the whole target at run time, descendants included. The protected
/// data appears after the plan was made, so only `act`'s own inspection can find it, and is
/// removed once `act` has recorded its snapshot, so the observation `act` runs after the
/// snapshot no longer classifies it: the snapshot alone refuses the plan.
#[cfg(unix)]
#[test]
fn protected_deep_in_target_refused() {
    let s = Setup::new();
    let wt = s.worktree("deep");
    write(&wt, "target/a/b/c.o", "obj");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
    assert!(plan.invariants_ok, "{:#?}", plan.checks);
    write(&wt, "target/a/b/fuzz/corpus/seed", "precious");
    let result = s.act_at(
        &plan.id,
        &arbiter(),
        cardinal::testpoint::ACT_SNAPSHOTTED,
        || std::fs::remove_dir_all(wt.join("target/a/b/fuzz")).unwrap(),
    );
    assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
    assert_eq!(result.reasons, ["no_protected"], "{result:#?}");
    assert_eq!(
        detail(&result, "no_protected"),
        "protected data: target/a/b/fuzz/corpus"
    );
    assert!(wt.join("target/a/b/c.o").exists(), "nothing touched");

    let control = s.worktree("shallow");
    write(&control, "target/a/b/c.o", "obj");
    let plan = s.plan(PlanKind::ReclaimDir, &control.join("target"));
    let result = s.act(&plan.id, &arbiter()).unwrap();
    assert_eq!(result.status, ActStatus::DryRun, "{result:#?}");
    assert!(control.join("target/a/b/c.o").exists());
}

/// C13: a worktree holding protected data is refused, while its regenerable `target/` passes
/// on its own.
#[cfg(unix)]
#[test]
fn protected_worktree_refused_target_dir_passes() {
    let s = Setup::new();
    let wt = s.worktree("fuzzing");
    write(&wt, "fuzz/corpus/seed", "precious");
    write(&wt, "target/debug/app", "bin");
    let whole = s.plan(PlanKind::ReclaimWorktree, &wt);
    assert!(!whole.invariants_ok);
    let result = s.act(&whole.id, &owner_approved()).unwrap();
    assert_eq!(result.status, ActStatus::Rejected);
    assert_eq!(result.reasons, ["no_protected"]);
    assert!(wt.join("fuzz/corpus/seed").exists());
    assert!(wt.join("target/debug/app").exists());

    let dir = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
    let result = s.act(&dir.id, &arbiter()).unwrap();
    assert_eq!(result.status, ActStatus::DryRun, "{result:#?}");
    assert!(wt.join("target/debug/app").exists());
    assert!(wt.join("fuzz/corpus/seed").exists());
}

/// A6: `act` recomputes approval from the checks it evaluates; the plan's stored approval
/// never widens what it may do. gh stops reporting the merged PR after the plan was made.
#[cfg(unix)]
#[test]
fn approval_recomputed_when_merge_evidence_disappears() {
    let s = Setup::new();
    let wt = s.worktree("was-merged");
    let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
    assert_no_approval(&plan);
    s.unset_merged(&s.head(&wt));
    let result = s.act(&plan.id, &arbiter()).unwrap();
    assert_eq!(result.status, ActStatus::Rejected);
    assert_eq!(result.reasons, ["owner_approval_required"]);
    assert!(wt.join("src/lib.rs").exists());

    let approved = s.plan(PlanKind::ReclaimWorktree, &wt);
    assert_eq!(approved.approval, Approval::Owner);
    let result = s.act(&approved.id, &owner_approved()).unwrap();
    assert_eq!(result.status, ActStatus::DryRun, "{result:#?}");
    assert!(wt.join("src/lib.rs").exists());
}

/// A7: another repository below the target's root is refused. Its `.git` appears after the
/// plans were made and is removed once `act` has recorded its snapshot, so the observation
/// `act` runs after the snapshot no longer classifies it: the snapshot alone refuses each plan.
#[cfg(unix)]
#[test]
fn nested_repository_refused() {
    let s = Setup::new();
    let wt = s.worktree("vendored");
    write(&wt, "node_modules/pkg/index.js", "x");
    let dir = s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"));
    let whole = s.plan(PlanKind::ReclaimWorktree, &wt);
    assert!(dir.invariants_ok && whole.invariants_ok);
    for plan in [&dir, &whole] {
        write(&wt, "node_modules/pkg/.git/HEAD", "ref: refs/heads/main\n");
        let result = s.act_at(
            &plan.id,
            &owner_approved(),
            cardinal::testpoint::ACT_SNAPSHOTTED,
            || std::fs::remove_dir_all(wt.join("node_modules/pkg/.git")).unwrap(),
        );
        assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
        assert_eq!(result.reasons, ["no_nested_repository"], "{result:#?}");
        assert_eq!(
            detail(&result, "no_nested_repository"),
            "repositories inside: node_modules/pkg"
        );
    }
    assert!(wt.join("node_modules/pkg/index.js").exists());

    let control = s.worktree("plain");
    write(&control, "node_modules/pkg/index.js", "x");
    let plan = s.plan(PlanKind::ReclaimDir, &control.join("node_modules"));
    assert_eq!(
        s.act(&plan.id, &arbiter()).unwrap().status,
        ActStatus::DryRun
    );
    assert!(control.join("node_modules/pkg/index.js").exists());
}

/// A7: a target that is a symbolic link is refused, and what it points to is untouched.
#[cfg(unix)]
#[test]
fn symlink_target_refused() {
    let s = Setup::new();
    let wt = s.worktree("linked");
    write(&wt, "node_modules/pkg/index.js", "x");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"));
    assert!(plan.invariants_ok);
    let outside = s.fx.path("store");
    write(&outside, "pkg/index.js", "shared");
    std::fs::remove_dir_all(wt.join("node_modules")).unwrap();
    std::os::unix::fs::symlink(&outside, wt.join("node_modules")).unwrap();
    let result = s.act(&plan.id, &owner_approved()).unwrap();
    assert_eq!(result.status, ActStatus::Rejected);
    // git does not ignore a link with the name of an ignored directory: the target is no
    // longer a regenerable entry either.
    assert_eq!(result.reasons, ["regenerable", "plain_target"]);
    assert!(outside.join("pkg/index.js").exists());

    let link_plan = s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"));
    assert!(
        !link_plan.invariants_ok,
        "a link is planned as the link it is"
    );
    assert_eq!(check(&link_plan.checks, "plain_target"), CheckResult::Fail);
    assert_eq!(
        link_plan.target.path,
        wt.join("node_modules").display().to_string()
    );

    // A directory on the way to the target replaced by a link: nothing below it is reached.
    let deep = s.worktree("deep");
    write(&deep, "web/node_modules/pkg/index.js", "x");
    let nested = s.plan(PlanKind::ReclaimDir, &deep.join("web/node_modules"));
    assert!(nested.invariants_ok, "{:#?}", nested.checks);
    write(&outside, "node_modules/pkg/index.js", "shared");
    std::fs::remove_dir_all(deep.join("web")).unwrap();
    std::os::unix::fs::symlink(&outside, deep.join("web")).unwrap();
    let result = s.act(&nested.id, &owner_approved()).unwrap();
    assert_eq!(result.status, ActStatus::Rejected);
    // The target is not reached, so the type of its file system is not read either.
    assert_eq!(
        result.reasons,
        ["regenerable", "plain_target", "local_volume"]
    );
    assert!(outside.join("node_modules/pkg/index.js").exists());
}

/// A7: a mount point is refused. The check reads the target's device against its parent's,
/// shown here on a real mount point of the OS, where the walk reads nothing below it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn mount_point_is_not_a_plain_target() {
    let mount = Path::new(if cfg!(target_os = "linux") {
        "/proc"
    } else {
        "/dev"
    });
    let walk = cardinal::plan::walk(Path::new("/"), mount).unwrap();
    assert!(
        walk.entries.is_empty(),
        "nothing below another device is read"
    );
    let checks = plain_target_checks(Path::new("/"), mount, &walk);
    assert_eq!(check(&checks, "plain_target"), CheckResult::Fail);

    let tmp = tempfile::tempdir().unwrap();
    let dir = dunce::canonicalize(tmp.path()).unwrap();
    std::fs::create_dir(dir.join("plain")).unwrap();
    let walk = cardinal::plan::walk(&dir, &dir.join("plain")).unwrap();
    let checks = plain_target_checks(&dir, &dir.join("plain"), &walk);
    assert_eq!(check(&checks, "plain_target"), CheckResult::Pass);
}

fn plain_target_checks(
    worktree: &Path,
    target: &Path,
    walk: &cardinal::plan::Walk,
) -> Vec<cardinal::contract::Check> {
    cardinal::plan::evaluate(&cardinal::plan::Inspection {
        kind: PlanKind::ReclaimDir,
        worktree,
        target,
        observation: Err("not observed"),
        walk: Ok(walk),
        in_use: &cardinal::plan::InUseFacts {
            lock_held: Ok(false),
            reservations: Ok(InUse::default()),
            processes: Ok(Vec::new()),
            descriptors: None,
        },
        patterns: &cardinal::plan::DataLists::default(),
        managed: &[],
        squash_merged: false,
    })
}

/// A7 end to end: `act` refuses a regenerable directory that is a mount point (a disk image
/// attached there), and what is on it stays. The image is written as a file and attached
/// read-only: formatting a writable image writes to its device, which the test sandbox denies.
#[cfg(target_os = "macos")]
#[test]
fn mount_point_refused() {
    /// Detaches the image when the test fails part-way; a passing test detaches it itself and
    /// checks that the detach succeeded.
    struct Attached(Option<cardinal_test_support::Command>);
    impl Attached {
        fn detach(mut self) -> std::process::ExitStatus {
            self.0.take().unwrap().status().unwrap()
        }
    }
    impl Drop for Attached {
        fn drop(&mut self) {
            if let Some(mut detach) = self.0.take() {
                let _ = detach.status();
            }
        }
    }
    let s = Setup::new();
    let wt = s.worktree("mounted");
    std::fs::create_dir(wt.join("node_modules")).unwrap();
    let contents = s.fx.path("image");
    write(&contents, "pkg/index.js", "on the image");
    let image = s.fx.path("disk.dmg");
    let created =
        s.fx.command("hdiutil", s.fx.root())
            .args(["makehybrid", "-quiet", "-hfs", "-o"])
            .arg(&image)
            .arg(&contents)
            .status()
            .unwrap();
    assert!(created.success(), "hdiutil makehybrid");
    let attached =
        s.fx.command("hdiutil", s.fx.root())
            .args([
                "attach",
                "-quiet",
                "-nobrowse",
                "-noautoopen",
                "-mountpoint",
            ])
            .arg(wt.join("node_modules"))
            .arg(&image)
            .status()
            .unwrap();
    assert!(attached.success(), "hdiutil attach");
    let mut detach = s.fx.command("hdiutil", s.fx.root());
    detach
        .args(["detach", "-force", "-quiet"])
        .arg(wt.join("node_modules"));
    let attached = Attached(Some(detach));

    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"));
    assert_eq!(check(&plan.checks, "plain_target"), CheckResult::Fail);
    let result = s.act(&plan.id, &owner_approved()).unwrap();
    assert_eq!(result.status, ActStatus::Rejected);
    assert_eq!(result.reasons, ["plain_target"]);
    assert_eq!(
        std::fs::read_to_string(wt.join("node_modules/pkg/index.js")).unwrap(),
        "on the image"
    );
    assert!(attached.detach().success(), "hdiutil detach");
}

/// A8: both plan kinds are refused for a worktree managed by another app: one git has
/// locked, and one under a configured managed root.
#[cfg(unix)]
#[test]
fn managed_worktree_refused() {
    let s = Setup::new();
    let wt = s.worktree("by-app");
    s.fx.worktree_lock(&s.main, &wt);
    let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
    assert_eq!(check(&plan.checks, "not_managed"), CheckResult::Fail);
    let result = s.act(&plan.id, &owner_approved()).unwrap();
    assert_eq!(result.status, ActStatus::Rejected);
    assert_eq!(result.reasons, ["not_managed"]);
    assert!(wt.join("src/lib.rs").exists());

    let control = s.worktree("by-hand");
    let plan = s.plan(PlanKind::ReclaimWorktree, &control);
    assert_eq!(
        s.act(&plan.id, &arbiter()).unwrap().status,
        ActStatus::DryRun
    );
    assert!(control.join("src/lib.rs").exists());
}

#[cfg(unix)]
#[test]
fn managed_reclaim_dir_refused() {
    let mut s = Setup::new();
    let apps = s.fx.path("apps");
    s.scene.observe_config(&format!(
        "managed = [{{ app = \"t3code\", root = \"{}\" }}]",
        apps.display()
    ));
    let wt = s.worktree("apps/wt");
    write(&wt, "node_modules/pkg/index.js", "x");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"));
    let result = s.act(&plan.id, &owner_approved()).unwrap();
    assert_eq!(result.status, ActStatus::Rejected);
    assert_eq!(result.reasons, ["not_managed"]);
    assert!(wt.join("node_modules/pkg/index.js").exists());

    let control = s.worktree("unmanaged");
    write(&control, "node_modules/pkg/index.js", "x");
    let plan = s.plan(PlanKind::ReclaimDir, &control.join("node_modules"));
    assert_eq!(
        s.act(&plan.id, &arbiter()).unwrap().status,
        ActStatus::DryRun
    );
    assert!(control.join("node_modules/pkg/index.js").exists());
}

/// A24: `plan` with no target plans every linked worktree, whatever its checks find, and
/// every ignored entry matching a regenerable pattern; a failing plan carries its reason.
#[cfg(unix)]
#[test]
fn bulk_plans_every_worktree() {
    let s = Setup::new();
    let done = s.worktree("done");
    let busy = s.worktree("busy");
    write(&busy, "target/x", "x");
    write(&busy, "notes.txt", "not committed\n");
    let list = s.plan_all();
    let mut targets: Vec<(String, PlanKind, bool)> = list
        .plans
        .iter()
        .map(|p| (p.target.path.clone(), p.kind, p.invariants_ok))
        .collect();
    targets.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        targets,
        [
            (busy.display().to_string(), PlanKind::ReclaimWorktree, false),
            (
                busy.join("target").display().to_string(),
                PlanKind::ReclaimDir,
                true
            ),
            (done.display().to_string(), PlanKind::ReclaimWorktree, true),
        ]
    );
    let refused = list
        .plans
        .iter()
        .find(|p| {
            p.kind == PlanKind::ReclaimWorktree && p.target.path == busy.display().to_string()
        })
        .unwrap();
    let failed: Vec<&str> = refused
        .checks
        .iter()
        .filter(|c| c.result != CheckResult::Pass)
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(failed, ["clean"], "{:#?}", refused.checks);
    assert!(list.errors.is_empty(), "{:?}", list.errors);
}

/// A24: a `target/` holding protected data deep inside matches a regenerable pattern, so the
/// bulk `plan` plans it, and its plan is refused by `no_protected` alone, beside a control
/// `target/` that passes.
#[cfg(unix)]
#[test]
fn protected_target_dir_planned_and_refused() {
    let s = Setup::new();
    let wt = s.worktree("fuzzed");
    write(&wt, "target/a/b/fuzz/corpus/seed", "keep");
    let control = s.worktree("control");
    write(&control, "target/x", "x");
    let list = s.plan_all();
    let plan_of = |path: &Path| {
        list.plans
            .iter()
            .find(|p| p.target.path == path.display().to_string())
            .unwrap_or_else(|| panic!("{} not planned: {list:#?}", path.display()))
    };
    let refused = plan_of(&wt.join("target"));
    assert!(!refused.invariants_ok);
    let failed: Vec<&str> = refused
        .checks
        .iter()
        .filter(|c| c.result != CheckResult::Pass)
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(failed, ["no_protected"], "{:#?}", refused.checks);
    let result = s.act(&refused.id, &plan_support::arbiter()).unwrap();
    assert_eq!(result.reasons, ["no_protected"]);
    let passing = plan_of(&control.join("target"));
    assert!(passing.invariants_ok, "{:#?}", passing.checks);
    let result = s.act(&passing.id, &plan_support::arbiter()).unwrap();
    assert_eq!(result.status, ActStatus::DryRun);
    assert!(wt.join("target/a/b/fuzz/corpus/seed").exists());
}

/// Plans are bound and stored; the CLI resolves unique prefixes; a main worktree is never
/// planned.
#[test]
fn plan_is_stored_and_recorded() {
    let s = Setup::new();
    let wt = s.worktree("stored");
    write(&wt, "target/x", "x");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
    assert_eq!(plan.host_id, s.state.host_id());
    assert_eq!(plan.target.worktree, wt.display().to_string());
    assert_eq!(plan.target.branch.as_deref(), Some("stored"));
    assert_eq!(
        plan.target.head.as_deref(),
        Some(s.head(&wt).as_str()),
        "bound to the observed HEAD"
    );
    let stored = cardinal::plan::load(&s.state, &plan.id[..6]).unwrap();
    assert_eq!(stored, plan);
    let records = cardinal::record::read_all(&s.state).unwrap();
    assert!(records.iter().any(|r| {
        r.kind == cardinal::contract::RecordKind::PlanCreated
            && r.plan_id.as_deref() == Some(&plan.id)
    }));
    let err = s.try_plan(PlanKind::ReclaimWorktree, &s.main).unwrap_err();
    assert_eq!(
        err.code,
        cardinal::contract::ErrorCode::InvalidArgument,
        "a main worktree is never reclaimed: {err:?}"
    );
}

/// Through the binary, only on the fixture repository: `plan --repo` observes it through git,
/// and its main worktree alone, which is never reclaimed and holds no ignored data, gets no
/// plan.
#[test]
fn cli_plan_on_a_fixture_repo() {
    let s = Setup::new();
    let main = s.main.display().to_string();
    let assert = s
        .cardinal()
        .args(["plan", "--repo", &main])
        .assert()
        .code(0);
    let doc: serde_json::Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(doc["schema"], "cardinal.plan-list.v1", "{doc:#}");
    assert_eq!(doc["plans"], serde_json::json!([]), "{doc:#}");
    assert_eq!(doc["errors"], serde_json::json!([]), "{doc:#}");
}

/// C15: a process working anywhere in the worktree uses its `node_modules` and `target/`, so
/// a `reclaim_dir` plan counts the whole worktree: one in `src/` refuses the plan of
/// `node_modules` by `not_in_use` alone, beside a control with no process inside.
#[cfg(unix)]
#[test]
fn working_directory_in_the_worktree_refused() {
    let s = Setup::new();
    let busy = s.worktree("busy");
    write(&busy, "node_modules/pkg/index.js", "x");
    let control = s.worktree("idle");
    write(&control, "node_modules/pkg/index.js", "x");
    let plan = s.plan(PlanKind::ReclaimDir, &busy.join("node_modules"));
    let idle = s.plan(PlanKind::ReclaimDir, &control.join("node_modules"));
    let mut sleeper = support::helper::spawn(
        s.fx.scope(),
        &support::helper::Mode::Sleep,
        &busy.join("src"),
    );
    cardinal_test_support::process::wait_for(
        "the sleeper's cwd",
        std::time::Duration::from_secs(10),
        || cardinal::os::cwd(sleeper.id()).filter(|cwd| cwd.starts_with(&busy)),
    );
    let result = s.act(&plan.id, &arbiter()).unwrap();
    assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
    assert_eq!(result.reasons, ["not_in_use"]);
    assert!(detail(&result, "not_in_use").contains(&sleeper.id().to_string()));
    let result = s.act(&idle.id, &arbiter()).unwrap();
    assert_eq!(result.status, ActStatus::DryRun, "{result:#?}");
    sleeper.kill();
    assert!(busy.join("node_modules/pkg/index.js").exists());
}

/// A `plan` run from inside the worktree does not count itself: its own working directory is
/// in the worktree, and `not_in_use` passes.
#[cfg(unix)]
#[test]
fn the_planning_process_is_not_in_use() {
    let s = Setup::new();
    let wt = s.worktree("here");
    write(&wt, "target/x", "x");
    let plan = s
        .try_plan_in(PlanKind::ReclaimDir, &wt.join("target"), &wt.join("src"))
        .unwrap();
    assert_eq!(
        check(&plan.checks, "not_in_use"),
        CheckResult::Pass,
        "{plan:#?}"
    );
    assert!(plan.invariants_ok, "{plan:#?}");
}

/// What observation of the worktree could not read is in its plans' provenance: a directory
/// git status cannot open leaves the worktree's ignored data unknown, and the plan says why.
#[cfg(unix)]
#[test]
fn a_plan_says_what_observation_could_not_read() {
    use std::os::unix::fs::PermissionsExt;

    /// Gives the directory its permissions back, so the setup can be removed.
    struct Readable(std::path::PathBuf);
    impl Drop for Readable {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    let s = Setup::new();
    let wt = s.worktree("hidden");
    write(&wt, "target/x", "x");
    let private = wt.join("private");
    std::fs::create_dir(&private).unwrap();
    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o000)).unwrap();
    let _readable = Readable(private.clone());
    if std::fs::read_dir(&private).is_ok() {
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            b"skipped a_plan_says_what_observation_could_not_read: the directory stays readable (root)\n",
        );
        return;
    }
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
    assert!(
        plan.provenance
            .errors
            .iter()
            .any(|e| e.source == "observation"
                && e.message.contains("git status is incomplete")
                && e.message.contains("could not open directory 'private/'")),
        "{plan:#?}"
    );
}

/// A git directory (`HEAD`, `objects/`, `refs/`) below a regenerable entry.
#[cfg(unix)]
fn bare_repository(at: &Path) {
    write(at, "HEAD", "ref: refs/heads/main\n");
    write(at, "objects/info/packs", "");
    write(at, "refs/heads/.keep", "");
}

/// A7: a bare repository below the target, made after the plan and removed once `act` has
/// recorded its snapshot, refuses the plan by `no_nested_repository` alone; the control
/// without it passes.
#[cfg(unix)]
#[test]
fn bare_repository_refused() {
    let s = Setup::new();
    let wt = s.worktree("bare-below");
    write(&wt, "node_modules/pkg/index.js", "x");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"));
    assert!(plan.invariants_ok, "{:#?}", plan.checks);
    bare_repository(&wt.join("node_modules/cache.git"));
    let result = s.act_at(
        &plan.id,
        &arbiter(),
        cardinal::testpoint::ACT_SNAPSHOTTED,
        || std::fs::remove_dir_all(wt.join("node_modules/cache.git")).unwrap(),
    );
    assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
    assert_eq!(result.reasons, ["no_nested_repository"]);
    assert_eq!(
        detail(&result, "no_nested_repository"),
        "repositories inside: node_modules/cache.git"
    );
    let result = s.act(&plan.id, &arbiter()).unwrap();
    assert_eq!(result.status, ActStatus::DryRun, "the control: {result:#?}");
}

/// A7: a target that is itself a bare repository is refused by `no_nested_repository`, and
/// observation classifies it as a nested repository.
#[cfg(unix)]
#[test]
fn bare_repository_at_the_target_root_refused() {
    let s = Setup::new();
    let wt = s.worktree("bare-root");
    bare_repository(&wt.join("node_modules"));
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"));
    let failed: Vec<&str> = plan
        .checks
        .iter()
        .filter(|c| c.result != CheckResult::Pass)
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(failed, ["no_nested_repository"], "{:#?}", plan.checks);
    let result = s.act(&plan.id, &arbiter()).unwrap();
    assert_eq!(result.reasons, ["no_nested_repository"]);
    assert!(wt.join("node_modules/HEAD").exists());
}

/// Filtered paths (section 13.6): a same-size edit of a tracked file whose filter observation
/// does not run makes `dirty` `unknown` with `filtered_paths`; the worktree's plan is refused
/// by `clean` alone, which names the path and carries the reason as a fact, and a
/// `reclaim_dir` plan in it stays `observed` and reports `dry_run`.
#[cfg(unix)]
#[test]
fn filtered_paths_fail_clean_alone() {
    use std::os::unix::fs::PermissionsExt;
    let s = Setup::new();
    let script = s.fx.path("passthrough.sh");
    std::fs::write(&script, "#!/bin/sh\ncat\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    s.fx.clean_filter(&s.main, "store", &script);
    let wt = s.fx.worktree_add(&s.main, "filtered", "filtered");
    write(&wt, ".gitattributes", "*.bin filter=store\n");
    s.fx.add(&wt, ".gitattributes");
    s.fx.commit_file(&wt, "asset.bin", "original\n", "asset");
    s.publish(&wt, "filtered");
    s.merge(&wt);
    write(&wt, "node_modules/pkg/index.js", "x");
    write(&wt, "asset.bin", "modified\n");

    let whole = s.plan(PlanKind::ReclaimWorktree, &wt);
    let failed: Vec<&str> = whole
        .checks
        .iter()
        .filter(|c| c.result != CheckResult::Pass)
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(failed, ["clean"], "{:#?}", whole.checks);
    let result = s.act(&whole.id, &owner_approved()).unwrap();
    assert_eq!(result.reasons, ["clean"]);
    let clean = result.checks.iter().find(|c| c.name == "clean").unwrap();
    assert!(
        clean.detail.starts_with("filtered_paths") && clean.detail.contains("asset.bin"),
        "{clean:#?}"
    );
    let unknown = &clean
        .facts
        .iter()
        .find(|f| f.name == "worktree.facts.dirty_unknown")
        .unwrap()
        .value;
    assert_eq!(
        unknown,
        &serde_json::json!({"reason": "filtered_paths", "paths": ["asset.bin"]})
    );

    let dir = s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"));
    assert!(dir.invariants_ok, "{:#?}", dir.checks);
    let result = s.act(&dir.id, &arbiter()).unwrap();
    assert_eq!(result.status, ActStatus::DryRun, "{result:#?}");
    assert_eq!(check(&result.checks, "observed"), CheckResult::Pass);
}
