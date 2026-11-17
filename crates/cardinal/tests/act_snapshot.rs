//! Snapshot deletion (A10, C14, C20; `docs/architecture.md` section 9.5): `act` deletes only
//! entries equal to the snapshot taken before its checks, enters only directories whose
//! identity equals the snapshot, removes directories only once empty, keeps a worktree's
//! `.git` unless nothing else remains, and checks HEAD and branch again right before
//! deleting. Each race pauses `act` in a helper process at its test point, makes the change
//! once the point is reached, and releases it.
//!
//! Observation comes from the fake in `tests/fakes` until the integration gate; it reads
//! HEAD and branch from the worktree's git files, as git would. Deletion is Unix-only in
//! phase 0; on Windows `act` stops at `dry_run`.

#![cfg_attr(
    not(unix),
    allow(
        dead_code,
        unused_imports,
        reason = "deletion is Unix-only in phase 0, and so are the tests that delete"
    )
)]

mod fakes;
mod support;

#[cfg(target_os = "linux")]
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use cardinal::contract::{ActResult, ActStatus, IgnoredClass, PlanKind};
use fakes::{Setup, arbiter, write};
use support::helper::ChildGuard;
use support::testpoints::TestPoints;

#[test]
fn helper() {
    fakes::run_act_if_requested();
}

const WAIT: Duration = Duration::from_secs(20);

/// Runs `act` of `plan` in a helper paused at `point`, calls `change` once the helper has
/// reached it, releases it, and returns its result.
fn act_with_change_at(s: &Setup, plan: &str, point: &str, change: impl FnOnce()) -> ActResult {
    let points = TestPoints::new();
    let out = points.dir().join("result.json");
    let mut command = s.act_command(plan, &arbiter(), &out);
    points.apply(&mut command, &[point], None);
    let mut child = ChildGuard::new(command.spawn().unwrap());
    points.wait_reached(point, child.id(), WAIT);
    change();
    points.release(point);
    assert!(child.child().wait().unwrap().success());
    fakes::read_result(&out)
}

fn skipped(result: &ActResult, path: &Path) -> bool {
    result.skipped_changed.contains(&path.display().to_string())
}

/// A tracked file edited after the snapshot, before the checks: the checks see the worktree
/// dirty and nothing is touched.
#[cfg(unix)]
#[test]
fn edit_before_checks_is_rejected() {
    let s = Setup::new();
    let wt = s.worktree("edited", &[("node_modules", IgnoredClass::Regenerable)]);
    write(&wt, "node_modules/pkg/index.js", "x");
    let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
    let result = act_with_change_at(&s, &plan.id, cardinal::testpoint::ACT_SNAPSHOTTED, || {
        write(&wt, "src/lib.rs", "// edited after the snapshot\n");
        // What `git status` now reports.
        s.fake
            .update(&wt, |w| w.facts.dirty = cardinal::contract::Tri::Yes);
    });
    assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
    assert_eq!(result.reasons, ["clean"]);
    assert!(
        wt.join("node_modules/pkg/index.js").exists(),
        "nothing touched"
    );
    assert!(wt.join(".git").exists());
}

/// Changes after the checks: an edited tracked file, a new file, and new protected data all
/// survive with their directories and are listed; the worktree keeps its `.git`, so git
/// still works in it.
#[cfg(unix)]
#[test]
fn changes_after_checks_survive() {
    let s = Setup::new();
    let wt = s.worktree("busy", &[("node_modules", IgnoredClass::Regenerable)]);
    s.fx.commit_file(&wt, "src/other.rs", "// other\n", "other");
    s.fx.push(&wt, &s.origin, "busy");
    write(&wt, "node_modules/pkg/index.js", "x");
    let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
    assert!(plan.invariants_ok, "{:#?}", plan.checks);
    let result = act_with_change_at(&s, &plan.id, cardinal::testpoint::ACT_CHECKED, || {
        write(&wt, "src/lib.rs", "// edited after the checks, longer\n");
        write(&wt, "src/new.rs", "// new\n");
        write(&wt, "fuzz/corpus/seed", "precious");
    });
    assert_eq!(result.status, ActStatus::Partial, "{result:#?}");
    for kept in ["src/lib.rs", "src/new.rs", "fuzz/corpus/seed"] {
        assert!(wt.join(kept).exists(), "{kept}");
    }
    assert_eq!(
        std::fs::read_to_string(wt.join("src/lib.rs")).unwrap(),
        "// edited after the checks, longer\n"
    );
    assert!(skipped(&result, &wt.join("src/lib.rs")), "{result:#?}");
    assert!(skipped(&result, &wt.join("src/new.rs")));
    assert!(skipped(&result, &wt.join("fuzz")));
    assert!(
        !wt.join("src/other.rs").exists(),
        "unchanged entries are deleted"
    );
    assert!(!wt.join("node_modules").exists());
    assert!(wt.join(".git").is_file(), "the worktree keeps its .git");
    let status = s.fx.git(&wt, &["status", "--porcelain"]);
    assert!(status.contains(" D src/other.rs"), "{status}");
    assert!(s.fake.removed().is_empty(), "the worktree stays registered");
    assert!(result.freed_bytes.unwrap() > 0);
}

/// An inspected directory replaced by a symbolic link to protected data after the checks:
/// the link is not followed, the protected data is intact, and the subtree is listed.
#[cfg(unix)]
#[test]
fn directory_swapped_for_a_link_is_not_followed() {
    let s = Setup::new();
    let wt = s.worktree("swapped", &[("node_modules", IgnoredClass::Regenerable)]);
    write(&wt, "node_modules/pkg/index.js", "x");
    write(&wt, "node_modules/other/index.js", "y");
    let precious = s.fx.path("precious");
    write(&precious, "seed", "keep me");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"));
    let result = act_with_change_at(&s, &plan.id, cardinal::testpoint::ACT_CHECKED, || {
        std::fs::rename(wt.join("node_modules/pkg"), s.fx.path("moved-pkg")).unwrap();
        std::os::unix::fs::symlink(&precious, wt.join("node_modules/pkg")).unwrap();
    });
    assert_eq!(result.status, ActStatus::Partial, "{result:#?}");
    assert!(
        skipped(&result, &wt.join("node_modules/pkg")),
        "{result:#?}"
    );
    assert_eq!(
        std::fs::read_to_string(precious.join("seed")).unwrap(),
        "keep me"
    );
    assert!(
        std::fs::symlink_metadata(wt.join("node_modules/pkg"))
            .unwrap()
            .is_symlink()
    );
    assert!(!wt.join("node_modules/other").exists());
}

/// A directory on the way to the target replaced by a symbolic link after the checks:
/// nothing is reached through the link, and the target is listed as skipped.
#[cfg(unix)]
#[test]
fn link_on_the_way_is_not_followed() {
    let s = Setup::new();
    let wt = s.worktree("detour", &[("web/node_modules", IgnoredClass::Regenerable)]);
    write(&wt, "web/node_modules/pkg/index.js", "x");
    let elsewhere = s.fx.path("elsewhere");
    write(&elsewhere, "node_modules/pkg/index.js", "keep me");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("web/node_modules"));
    let result = act_with_change_at(&s, &plan.id, cardinal::testpoint::ACT_CHECKED, || {
        std::fs::rename(wt.join("web"), s.fx.path("old-web")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, wt.join("web")).unwrap();
    });
    assert_eq!(result.status, ActStatus::Partial, "{result:#?}");
    assert!(
        skipped(&result, &wt.join("web/node_modules")),
        "{result:#?}"
    );
    assert!(elsewhere.join("node_modules/pkg/index.js").exists());
}

/// An inspected directory replaced by another directory: its identity differs from the
/// snapshot, so it is not entered and what is in it survives.
#[cfg(unix)]
#[test]
fn replaced_directory_is_not_entered() {
    let s = Setup::new();
    let wt = s.worktree("replaced", &[("node_modules", IgnoredClass::Regenerable)]);
    write(&wt, "node_modules/pkg/index.js", "x");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"));
    let result = act_with_change_at(&s, &plan.id, cardinal::testpoint::ACT_CHECKED, || {
        std::fs::rename(wt.join("node_modules/pkg"), s.fx.path("old-pkg")).unwrap();
        // Same name, same contents, a new directory.
        write(&wt, "node_modules/pkg/index.js", "x");
    });
    assert_eq!(result.status, ActStatus::Partial, "{result:#?}");
    assert!(skipped(&result, &wt.join("node_modules/pkg")));
    assert!(wt.join("node_modules/pkg/index.js").exists());
}

/// The flags that give `unshare` a private mount namespace where `mount --bind` works:
/// `-m` as root, `-r -m` (a user namespace) otherwise; `None`, with the reason printed, where
/// neither does. The probe's mount ends with its namespace.
#[cfg(target_os = "linux")]
fn mount_namespace(s: &Setup) -> Option<&'static [&'static str]> {
    let probe = s.fx.path("probe");
    std::fs::create_dir_all(&probe).unwrap();
    for flags in [&["-m"][..], &["-r", "-m"]] {
        let mut command = std::process::Command::new("unshare");
        command
            .args(flags)
            .args(["mount", "--bind"])
            .arg(&probe)
            .arg(&probe);
        let probed = fakes::confined(&s.fx, command, s.fx.root()).output();
        if probed.is_ok_and(|o| o.status.success()) {
            return Some(flags);
        }
    }
    eprintln!(
        "skipped: no private mount namespace for a bind mount (neither `unshare -m` as root \
         nor `unshare -rm` works here)"
    );
    None
}

/// Runs `act` of `plan` in a helper inside a private mount namespace where each `(source,
/// target)` of `binds` is bind-mounted first. The mounts are undone after the helper, and end
/// with the namespace on every other path (a failed mount, a panic, a kill).
#[cfg(target_os = "linux")]
fn act_with_binds(s: &Setup, flags: &[&str], plan: &str, binds: &[(&Path, &Path)]) -> ActResult {
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let mut mounts = String::new();
    let mut unmounts = String::new();
    for (source, target) in binds {
        let _ = writeln!(
            mounts,
            "mount --bind {} {} || exit 97",
            quote(source),
            quote(target)
        );
        unmounts.insert_str(0, &format!("umount {}\n", quote(target)));
    }
    let script = format!("{mounts}\"$@\"\nstatus=$?\n{unmounts}exit $status\n");
    let out = s.fx.path(&format!("result-{plan}.json"));
    let act = s.act_command(plan, &arbiter(), &out);
    let mut command = std::process::Command::new("unshare");
    command
        .args(flags)
        .args(["sh", "-c", &script, "sh"])
        .arg(act.get_program())
        .args(act.get_args());
    for (key, value) in act.get_envs() {
        if let Some(value) = value {
            command.env(key, value);
        }
    }
    let status = fakes::confined(&s.fx, command, s.fx.root())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(
        status.success(),
        "the act helper in the namespace: {status}"
    );
    fakes::read_result(&out)
}

/// A7 on Linux: a bind mount on the same filesystem is a mount point. `act` refuses a target
/// that is one and does not enter one below its target, and the bound data survives both.
#[cfg(target_os = "linux")]
#[test]
fn bind_mounts_are_not_entered() {
    let s = Setup::new();
    let Some(flags) = mount_namespace(&s) else {
        return;
    };
    let owner = s.fx.path("owner");
    write(&owner, "data/keep", "precious");
    let wt = s.worktree(
        "bound",
        &[
            ("target", IgnoredClass::Regenerable),
            ("node_modules", IgnoredClass::Regenerable),
        ],
    );
    write(&wt, "target/debug/app", "bin");
    write(&wt, "node_modules/pkg/index.js", "x");
    std::fs::create_dir_all(wt.join("node_modules/cache")).unwrap();
    let data = owner.join("data");

    let target = wt.join("target");
    let plan = s.plan(PlanKind::ReclaimDir, &target);
    let result = act_with_binds(&s, flags, &plan.id, &[(&data, &target)]);
    assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
    assert_eq!(result.reasons, ["plain_target"]);
    assert!(data.join("keep").exists(), "the bound data survives");
    assert!(wt.join("target/debug/app").exists(), "nothing touched");

    let cache = wt.join("node_modules/cache");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"));
    let result = act_with_binds(&s, flags, &plan.id, &[(&data, &cache)]);
    assert_eq!(result.status, ActStatus::Partial, "{result:#?}");
    assert!(skipped(&result, &cache), "{result:#?}");
    assert!(data.join("keep").exists(), "the bound data survives");
    assert!(
        !wt.join("node_modules/pkg").exists(),
        "the rest is reclaimed"
    );
}

/// HEAD and branch are read again right before deleting: switching the worktree to another
/// branch with the same tree after the checks expires the plan with nothing deleted, for a
/// worktree plan and for a directory inside it.
#[cfg(unix)]
#[test]
fn head_change_stops_deletion() {
    for (n, kind) in [PlanKind::ReclaimWorktree, PlanKind::ReclaimDir]
        .into_iter()
        .enumerate()
    {
        let s = Setup::new();
        let wt = s.worktree("switched", &[("node_modules", IgnoredClass::Regenerable)]);
        write(&wt, "node_modules/pkg/index.js", "x");
        let target = match kind {
            PlanKind::ReclaimWorktree => wt.clone(),
            PlanKind::ReclaimDir => wt.join("node_modules"),
        };
        let plan = s.plan(kind, &target);
        let branch = format!("other-{n}");
        let result = act_with_change_at(&s, &plan.id, cardinal::testpoint::ACT_CHECKED, || {
            s.fx.git(&wt, &["switch", "-q", "-c", &branch]);
        });
        assert_eq!(result.status, ActStatus::Expired, "{kind:?}: {result:#?}");
        assert_eq!(result.reasons, ["identity_changed:branch"], "{kind:?}");
        assert!(wt.join("node_modules/pkg/index.js").exists(), "{kind:?}");
        assert!(wt.join("src/lib.rs").exists(), "{kind:?}");
    }
}

/// C20: `act` writes nothing inside the repository: after reclaiming a directory of one
/// worktree and a whole sibling worktree, every other entry of the main worktree (its
/// `.git` included) and of the first worktree is unchanged, and nothing was added.
#[cfg(unix)]
#[test]
fn nothing_written_in_repo() {
    let s = Setup::new();
    let wt = s.worktree("keep", &[("node_modules", IgnoredClass::Regenerable)]);
    write(&wt, "node_modules/pkg/index.js", "x");
    let sibling = s.worktree("gone", &[]);
    let main_before = fakes::listing(&s.main);
    let wt_before = fakes::listing(&wt);

    let dir = s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"));
    let whole = s.plan(PlanKind::ReclaimWorktree, &sibling);
    for plan in [&dir, &whole] {
        let result = s.act(&plan.id, &arbiter()).unwrap();
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
    }

    assert_eq!(fakes::listing(&s.main), main_before);
    let expected: std::collections::BTreeMap<_, _> = wt_before
        .into_iter()
        .filter(|(rel, _)| !rel.starts_with("node_modules"))
        .collect();
    assert_eq!(fakes::listing(&wt), expected);
    assert!(!sibling.exists());
}

/// On Windows, phase 0 `act` only dry-runs: repeating it keeps reporting `dry_run`, claims
/// nothing, and deletes nothing.
#[cfg(windows)]
#[test]
fn windows_act_stops_at_dry_run() {
    let s = Setup::new();
    let wt = s.worktree("windows", &[("target", IgnoredClass::Regenerable)]);
    write(&wt, "target/debug/app.exe", "bin");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
    for _ in 0..2 {
        let result = s.act(&plan.id, &arbiter()).unwrap();
        assert_eq!(result.status, ActStatus::DryRun, "{result:#?}");
    }
    assert!(!s.state.plan_claim(&plan.id).exists());
    assert!(wt.join("target/debug/app.exe").exists());
}
