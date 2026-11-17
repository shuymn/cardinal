//! `plan` and `act` against observed facts (`docs/architecture.md` sections 9.2–9.5): which
//! worktrees are plans without approval (C12), whole-target inspection (C13), identity
//! binding (C14), recomputed approval (A6), plain targets (A7), and managed worktrees (A8).
//! Every refusal is paired with a control that passes every check and reports `dry_run`.
//!
//! Observation and admission come from the fake in `tests/fakes` until the integration gate
//! runs these against the real modules. The tests that inspect targets run on Unix, where
//! `act` reads its snapshot through directory descriptors; `act_report_only` runs the
//! report-only contract on every platform.

#![cfg_attr(
    not(unix),
    allow(
        dead_code,
        unused_imports,
        reason = "the tests that inspect targets run on Unix"
    )
)]

mod fakes;
mod support;

use std::path::Path;

use cardinal::contract::{
    ActStatus, Approval, CheckResult, IgnoredClass, InUse, MergeEvidence, Plan, PlanKind, Tri,
};
use fakes::{Setup, arbiter, write};

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

fn owner_approved() -> cardinal::act::ActOptions {
    cardinal::act::ActOptions {
        owner_approved: true,
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
        let wt = s.worktree("done", &[("node_modules", IgnoredClass::Regenerable)]);
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
        let wt = s.worktree("merged-then-more", &[]);
        s.fx.commit_file(&wt, "src/more.rs", "// more\n", "more work");
        s.fx.push(&wt, &s.origin, "merged-then-more");
        s.fake.update(&wt, |w| {
            w.facts.merge_evidence = MergeEvidence::None;
            w.facts.merged_pr = None;
        });
        let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
        assert!(plan.invariants_ok);
        assert_eq!(plan.approval, Approval::Owner);
        assert_eq!(check(&plan.checks, "merge_evidence"), CheckResult::Fail);
        let result = s.act(&plan.id, &arbiter()).unwrap();
        assert_eq!(result.status, ActStatus::Rejected);
        assert_eq!(result.reasons, ["owner_approval_required"]);
        assert!(wt.join("src/more.rs").exists());
    }

    /// A branch name reused for new work after its PR merged gives no evidence either.
    #[test]
    fn reused_branch_name_needs_approval() {
        let s = Setup::new();
        let wt = s.worktree("reused", &[]);
        s.fake
            .update(&wt, |w| w.facts.merge_evidence = MergeEvidence::None);
        let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
        assert_eq!(plan.approval, Approval::Owner);
        let result = s.act(&plan.id, &arbiter()).unwrap();
        assert_eq!(result.status, ActStatus::Rejected);
        assert!(wt.join("src/lib.rs").exists());
    }

    /// No upstream is an invariant: not even the owner's approval reclaims it (G6).
    #[test]
    fn no_upstream_is_refused_even_with_approval() {
        let s = Setup::new();
        let wt = s.worktree("local-only", &[]);
        s.fake.update(&wt, |w| {
            w.facts.has_upstream = Tri::No;
            w.upstream = None;
        });
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
        let wt = s.worktree("gh-down", &[]);
        let mut world = s.fake.read();
        let observed = world
            .worktrees
            .iter_mut()
            .find(|o| Path::new(&o.worktree.path) == wt)
            .unwrap();
        observed.repo.gh = cardinal::contract::GhStatus::Unknown;
        observed.repo.gh_error = Some("gh exited with 1".into());
        observed.worktree.facts.open_pr = Tri::Unknown;
        observed.worktree.facts.merge_evidence = MergeEvidence::Unknown;
        s.fake.write(&world);
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

    #[test]
    fn dirty_unpushed_or_open_pr_is_refused() {
        type Change = fn(&mut cardinal::contract::Worktree);
        let s = Setup::new();
        let cases: [(&str, &str, Change); 3] = [
            ("dirty", "clean", |w| w.facts.dirty = Tri::Yes),
            ("unpushed", "no_unpushed", |w| w.facts.unpushed = Tri::Yes),
            ("open-pr", "no_open_pr", |w| w.facts.open_pr = Tri::Yes),
        ];
        for (name, failing, change) in cases {
            let wt = s.worktree(name, &[]);
            s.fake.update(&wt, change);
            let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
            let result = s.act(&plan.id, &owner_approved()).unwrap();
            assert_eq!(result.status, ActStatus::Rejected, "{name}");
            assert_eq!(result.reasons, [failing], "{name}");
            assert!(wt.join("src/lib.rs").exists(), "{name}");
        }
    }

    /// In use: a reservation (a waiting or crashed job), the in-use lock (a running job), or
    /// a process with its working directory inside.
    #[test]
    fn worktree_in_use_is_refused() {
        let s = Setup::new();
        let reserved = s.worktree("reserved", &[]);
        let key = cardinal::state::worktree_key(&reserved);
        let mut world = s.fake.read();
        world.in_use.insert(
            key,
            InUse {
                value: true,
                jobs: vec!["0123456789abcdef".into()],
            },
        );
        s.fake.write(&world);
        let locked = s.worktree("locked", &[]);
        let busy = s.worktree("busy", &[]);
        let plans: Vec<Plan> = [&reserved, &locked, &busy]
            .iter()
            .map(|wt| s.plan(PlanKind::ReclaimWorktree, wt))
            .collect();

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
        sleeper.kill();
    }
}

/// C14: a plan is bound to its target's identity, observed again whenever `act` evaluates it.
#[cfg(unix)]
mod identity {
    use super::*;

    #[test]
    fn new_worktree_at_the_same_path_expires() {
        let s = Setup::new();
        let wt = s.worktree("again", &[]);
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
        let wt = s.worktree("advanced", &[]);
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
        let wt = s.worktree("elsewhere", &[("target", IgnoredClass::Regenerable)]);
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
            !s.state.plan_claim(&plan.id).exists(),
            "an expired plan of another host is not claimed"
        );
    }
}

/// C13: `act` inspects the whole target at run time, descendants included. The protected
/// data appears after the plan was made, so only `act`'s own inspection can find it.
#[cfg(unix)]
#[test]
fn protected_deep_in_target_refused() {
    let s = Setup::new();
    let wt = s.worktree("deep", &[("target", IgnoredClass::Regenerable)]);
    write(&wt, "target/a/b/c.o", "obj");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
    assert!(plan.invariants_ok, "{:#?}", plan.checks);
    write(&wt, "target/a/b/fuzz/corpus/seed", "precious");
    let result = s.act(&plan.id, &arbiter()).unwrap();
    assert_eq!(result.status, ActStatus::Rejected);
    assert_eq!(result.reasons, ["no_protected"]);
    assert!(wt.join("target/a/b/fuzz/corpus/seed").exists());
    assert!(wt.join("target/a/b/c.o").exists(), "nothing touched");

    let control = s.worktree("shallow", &[("target", IgnoredClass::Regenerable)]);
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
    let wt = s.worktree(
        "fuzzing",
        &[
            ("fuzz/corpus", IgnoredClass::Protected),
            ("target", IgnoredClass::Regenerable),
        ],
    );
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
/// never widens what it may do.
#[cfg(unix)]
#[test]
fn approval_recomputed_when_merge_evidence_disappears() {
    let s = Setup::new();
    let wt = s.worktree("was-merged", &[]);
    let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
    assert_no_approval(&plan);
    s.fake
        .update(&wt, |w| w.facts.merge_evidence = MergeEvidence::None);
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

/// A7: another repository below the target's root is refused.
#[cfg(unix)]
#[test]
fn nested_repository_refused() {
    let s = Setup::new();
    let wt = s.worktree("vendored", &[("node_modules", IgnoredClass::Regenerable)]);
    write(&wt, "node_modules/pkg/index.js", "x");
    let dir = s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"));
    let whole = s.plan(PlanKind::ReclaimWorktree, &wt);
    assert!(dir.invariants_ok && whole.invariants_ok);
    write(&wt, "node_modules/pkg/.git/HEAD", "ref: refs/heads/main\n");
    for plan in [&dir, &whole] {
        let result = s.act(&plan.id, &owner_approved()).unwrap();
        assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
        assert_eq!(result.reasons, ["no_nested_repository"], "{result:#?}");
    }
    assert!(wt.join("node_modules/pkg/.git/HEAD").exists());
    assert!(wt.join("node_modules/pkg/index.js").exists());

    let control = s.worktree("plain", &[("node_modules", IgnoredClass::Regenerable)]);
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
    let wt = s.worktree("linked", &[("node_modules", IgnoredClass::Regenerable)]);
    write(&wt, "node_modules/pkg/index.js", "x");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"));
    assert!(plan.invariants_ok);
    let outside = s.fx.path("store");
    write(&outside, "pkg/index.js", "shared");
    std::fs::remove_dir_all(wt.join("node_modules")).unwrap();
    std::os::unix::fs::symlink(&outside, wt.join("node_modules")).unwrap();
    let result = s.act(&plan.id, &owner_approved()).unwrap();
    assert_eq!(result.status, ActStatus::Rejected);
    assert_eq!(result.reasons, ["plain_target"]);
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
    let deep = s.worktree("deep", &[("web/node_modules", IgnoredClass::Regenerable)]);
    write(&deep, "web/node_modules/pkg/index.js", "x");
    let nested = s.plan(PlanKind::ReclaimDir, &deep.join("web/node_modules"));
    assert!(nested.invariants_ok, "{:#?}", nested.checks);
    write(&outside, "node_modules/pkg/index.js", "shared");
    std::fs::remove_dir_all(deep.join("web")).unwrap();
    std::os::unix::fs::symlink(&outside, deep.join("web")).unwrap();
    let result = s.act(&nested.id, &owner_approved()).unwrap();
    assert_eq!(result.status, ActStatus::Rejected);
    assert_eq!(result.reasons, ["plain_target"]);
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
        },
        patterns: &cardinal::plan::DataLists::default(),
        managed: &[],
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
    let wt = s.worktree("mounted", &[("node_modules", IgnoredClass::Regenerable)]);
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

/// A8: both plan kinds are refused for a worktree managed by another app, whether
/// observation reports it or it lies under a configured managed root.
#[cfg(unix)]
#[test]
fn managed_worktree_refused() {
    let s = Setup::new();
    let wt = s.worktree("by-app", &[]);
    s.fake.update(&wt, |w| w.managed_by = Some("t3code".into()));
    let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
    assert_eq!(check(&plan.checks, "not_managed"), CheckResult::Fail);
    let result = s.act(&plan.id, &owner_approved()).unwrap();
    assert_eq!(result.status, ActStatus::Rejected);
    assert_eq!(result.reasons, ["not_managed"]);
    assert!(wt.join("src/lib.rs").exists());

    let control = s.worktree("by-hand", &[]);
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
    let host = format!(
        "[observe]\nmanaged = [{{ app = \"t3code\", root = \"{}\" }}]\n",
        s.fx.path("apps").display()
    );
    s.config = fakes::config(s.dirs.config(), Some(&host));
    // Observation does not report it; the configured root alone makes it managed.
    let wt = s.worktree("apps/wt", &[("node_modules", IgnoredClass::Regenerable)]);
    write(&wt, "node_modules/pkg/index.js", "x");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"));
    let result = s.act(&plan.id, &owner_approved()).unwrap();
    assert_eq!(result.status, ActStatus::Rejected);
    assert_eq!(result.reasons, ["not_managed"]);
    assert!(wt.join("node_modules/pkg/index.js").exists());

    let control = s.worktree("unmanaged", &[("node_modules", IgnoredClass::Regenerable)]);
    write(&control, "node_modules/pkg/index.js", "x");
    let plan = s.plan(PlanKind::ReclaimDir, &control.join("node_modules"));
    assert_eq!(
        s.act(&plan.id, &arbiter()).unwrap().status,
        ActStatus::DryRun
    );
    assert!(control.join("node_modules/pkg/index.js").exists());
}

/// `plan` with no target plans every candidate worktree and the regenerable entries of
/// worktrees that are not candidates.
#[cfg(unix)]
#[test]
fn plan_all_covers_candidates_and_regenerable_entries() {
    let s = Setup::new();
    let done = s.worktree("done", &[("node_modules", IgnoredClass::Regenerable)]);
    let busy = s.worktree("busy", &[("target", IgnoredClass::Regenerable)]);
    write(&busy, "target/x", "x");
    s.fake.update(&busy, |w| {
        w.facts.dirty = Tri::Yes;
        w.reclaim.candidate = false;
    });
    let list = cardinal::plan::create_all_with(&s.config, &s.state, &s.fake, &[]).unwrap();
    let targets: Vec<(PlanKind, String)> = list
        .plans
        .iter()
        .map(|p| (p.kind, p.target.path.clone()))
        .collect();
    assert_eq!(
        targets,
        [
            (PlanKind::ReclaimWorktree, done.display().to_string()),
            (
                PlanKind::ReclaimDir,
                busy.join("target").display().to_string()
            ),
        ]
    );
    assert!(list.errors.is_empty(), "{:?}", list.errors);
}

/// Plans are bound and stored; the CLI resolves unique prefixes.
#[test]
fn plan_is_stored_and_recorded() {
    let s = Setup::new();
    let wt = s.worktree("stored", &[("target", IgnoredClass::Regenerable)]);
    write(&wt, "target/x", "x");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
    assert_eq!(plan.host_id, s.state.host_id());
    assert_eq!(plan.target.worktree, wt.display().to_string());
    assert_eq!(plan.target.branch.as_deref(), Some("stored"));
    assert_eq!(
        plan.target.head,
        fakes::read_head(&wt).head,
        "bound to the observed HEAD"
    );
    let stored = cardinal::plan::load(&s.state, &plan.id[..6]).unwrap();
    assert_eq!(stored, plan);
    let records = cardinal::record::read_all(&s.state).unwrap();
    assert!(records.iter().any(|r| {
        r.kind == cardinal::contract::RecordKind::PlanCreated
            && r.plan_id.as_deref() == Some(&plan.id)
    }));
    let mut world = s.fake.read();
    world
        .worktrees
        .push(fakes::reclaimable(&s.main, &s.main, &[]));
    s.fake.write(&world);
    let err = cardinal::plan::create_with(
        &s.config,
        &s.state,
        &s.fake,
        PlanKind::ReclaimWorktree,
        &s.main,
    )
    .unwrap_err();
    assert_eq!(
        err.code,
        cardinal::contract::ErrorCode::InvalidArgument,
        "a main worktree is never reclaimed: {err}"
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
