//! What a report reads of the target's file system (`docs/architecture.md` sections 8.5,
//! 9.2, and 13.6): links are never followed, files with names outside the target count as
//! aliased bytes, which removing the target would not free, and in no lower bound of the
//! freed bytes (`exclusive_bytes`), which a dry run reports as its `freed_bytes`
//! (`act_execute::freed_bytes` reads the bound of clones on APFS and of extents on ext4); and
//! a mount below the target refuses it (A7), each beside a control. Nothing is changed.

#![cfg(unix)]

mod observe_common;
mod plan_support;
mod support;

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use cardinal::contract::{ActStatus, CheckResult, ExclusiveMeasure, Plan, PlanKind};
use plan_support::{Setup, arbiter, write, write_synced};

/// The bytes allocated to the file at `path`.
fn allocated(path: &Path) -> u64 {
    std::fs::symlink_metadata(path).unwrap().blocks() * 512
}

/// Asserts that `plan`'s bound is `own`, the blocks of the target's regular files written in
/// place with all their names inside, under the measure of the volume the tests run on: their
/// private size on APFS and their extents on ext2–4 are their blocks; under `links` the
/// directories' and links' blocks add to them; other file systems give none.
fn assert_bound(plan: &Plan, own: u64) {
    match plan.exclusive_measure {
        Some(ExclusiveMeasure::PrivateSize | ExclusiveMeasure::Extents) => {
            assert_eq!(plan.exclusive_bytes, Some(own), "{plan:#?}");
        }
        Some(ExclusiveMeasure::Links) => {
            assert!(plan.exclusive_bytes.unwrap() >= own, "{plan:#?}");
        }
        _ => assert_eq!(plan.exclusive_bytes, None, "{plan:#?}"),
    }
}

/// The checks of `checks` that did not pass.
fn failed(checks: &[cardinal::contract::Check]) -> Vec<&str> {
    checks
        .iter()
        .filter(|c| c.result != CheckResult::Pass)
        .map(|c| c.name.as_str())
        .collect()
}

/// A symbolic link in the target to a directory outside is not followed: the outside
/// data is neither counted nor touched. A hard link in the target to a file outside counts in
/// `aliased_bytes`.
#[test]
fn links_in_the_target_are_not_followed_and_hard_links_are_aliased() {
    let s = Setup::new();
    let wt = s.worktree("links");
    let outside = s.fx.path("outside");
    let big = "x".repeat(1 << 20);
    write(&outside, "dir/big", &big);
    write(&outside, "shared.bin", &"y".repeat(256 * 1024));
    write_synced(&wt, "target/debug/app", b"bin");
    std::os::unix::fs::symlink(outside.join("dir"), wt.join("target/linked")).unwrap();
    std::fs::hard_link(outside.join("shared.bin"), wt.join("target/shared.bin")).unwrap();
    let before = plan_support::listing(&wt);

    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
    assert!(plan.invariants_ok, "{:#?}", plan.checks);
    let shared = allocated(&outside.join("shared.bin"));
    assert_eq!(plan.aliased_bytes, Some(shared), "{plan:#?}");
    let total = plan.allocated_bytes.unwrap();
    assert!(total >= shared && total < big.len() as u64, "{plan:#?}");
    // The file linked from outside counts nothing, and the outside data is not reached.
    assert_bound(&plan, allocated(&wt.join("target/debug/app")));

    let result = s.act(&plan.id, &arbiter()).unwrap();
    assert_eq!(result.status, ActStatus::DryRun, "{result:#?}");
    assert_eq!(result.freed_bytes, plan.exclusive_bytes, "{result:#?}");
    assert_eq!(plan_support::listing(&wt), before, "nothing touched");
    assert_eq!(
        std::fs::read_to_string(outside.join("dir/big")).unwrap(),
        big
    );
}

/// A pnpm-like `node_modules`: packages linked into `.pnpm`, their files hard-linked from a
/// store outside the worktree. The store's files count in `aliased_bytes`, in the plan and in
/// `inventory --sizes`, and what is the target's own is the rest; the bound counts none of the
/// store's files, and a dry run reports it.
#[test]
fn a_pnpm_like_store_counts_as_aliased() {
    let s = Setup::new();
    let wt = s.worktree("pnpm");
    let store = s.fx.path("store");
    let mut store_bytes = 0;
    for name in ["a", "b"] {
        let file = store.join(format!("{name}.js"));
        write(&store, &format!("{name}.js"), &name.repeat(64 * 1024));
        store_bytes += allocated(&file);
        let package = wt.join(format!(
            "node_modules/.pnpm/{name}@1.0.0/node_modules/{name}"
        ));
        std::fs::create_dir_all(&package).unwrap();
        std::fs::hard_link(&file, package.join("index.js")).unwrap();
        std::os::unix::fs::symlink(
            format!(".pnpm/{name}@1.0.0/node_modules/{name}"),
            wt.join(format!("node_modules/{name}")),
        )
        .unwrap();
    }
    write_synced(&wt, "node_modules/.modules.yaml", &vec![b'm'; 16 * 1024]);

    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"));
    assert!(plan.invariants_ok, "{:#?}", plan.checks);
    assert_eq!(plan.aliased_bytes, Some(store_bytes), "{plan:#?}");
    let own = plan.allocated_bytes.unwrap() - store_bytes;
    assert!(
        own >= allocated(&wt.join("node_modules/.modules.yaml")),
        "{plan:#?}"
    );

    let inventory = s.inventory_with(true);
    let entry = observe_common::worktree(&inventory, &wt)
        .ignored
        .iter()
        .find(|e| e.path == wt.join("node_modules").display().to_string())
        .unwrap()
        .clone();
    assert_eq!(entry.aliased_bytes, Some(store_bytes), "{entry:#?}");
    assert_eq!(entry.allocated_bytes, plan.allocated_bytes, "{entry:#?}");
    assert_bound(&plan, allocated(&wt.join("node_modules/.modules.yaml")));
    assert_eq!(
        (entry.exclusive_bytes, entry.exclusive_measure),
        (plan.exclusive_bytes, plan.exclusive_measure),
        "{entry:#?}"
    );

    let result = s.act(&plan.id, &arbiter()).unwrap();
    assert_eq!(result.status, ActStatus::DryRun, "{result:#?}");
    assert_eq!(result.freed_bytes, plan.exclusive_bytes, "{result:#?}");
    assert!(store.join("a.js").exists());
}

/// A file with two names inside the target and one outside counts nothing in the bound, and
/// with all three inside counts once.
#[test]
fn a_file_counts_once_and_only_with_all_its_names_inside() {
    let s = Setup::new();
    let wt = s.worktree("names");
    let outside = s.fx.path("outside");
    write_synced(&outside, "shared.bin", &vec![b's'; 64 * 1024]);
    std::fs::create_dir_all(wt.join("target/b")).unwrap();
    std::fs::hard_link(outside.join("shared.bin"), wt.join("target/a.bin")).unwrap();
    std::fs::hard_link(outside.join("shared.bin"), wt.join("target/b/a.bin")).unwrap();
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
    assert_bound(&plan, 0);

    write_synced(&wt, "node_modules/one.bin", &vec![b'o'; 64 * 1024]);
    for name in ["two.bin", "pkg/three.bin"] {
        let path = wt.join("node_modules").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::hard_link(wt.join("node_modules/one.bin"), path).unwrap();
    }
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"));
    assert_eq!(plan.aliased_bytes, Some(0), "{plan:#?}");
    assert_bound(&plan, allocated(&wt.join("node_modules/one.bin")));
}

/// The bound of a plan whose walk left the directory `node_modules/cache` unread: null, never
/// a number that leaves out what the directory holds, with the reason in the provenance.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn assert_unread_below(plan: &Plan) {
    assert_eq!(
        (plan.exclusive_bytes, plan.exclusive_measure),
        (None, Some(ExclusiveMeasure::ReadFailed)),
        "{plan:#?}"
    );
    assert!(
        plan.provenance
            .errors
            .iter()
            .any(|e| e.source == "exclusive_bytes" && e.message.contains("node_modules/cache")),
        "{:#?}",
        plan.provenance
    );
}

/// A7: a mount below the target, a disk image attached at a directory inside it, refuses the
/// target by `plain_target` alone, naming the directory; nothing on the mount is read, so the
/// plan reads no bound of the freed bytes. The control without the mount reports `dry_run`.
#[cfg(target_os = "macos")]
#[test]
fn mount_below_the_target_refused() {
    let s = Setup::new();
    let wt = s.worktree("mounted-below");
    write(&wt, "node_modules/pkg/index.js", "x");
    let cache = wt.join("node_modules/cache");
    std::fs::create_dir_all(&cache).unwrap();
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"));
    assert!(plan.invariants_ok, "the control: {:#?}", plan.checks);
    let control = s.act(&plan.id, &arbiter()).unwrap();
    assert_eq!(control.status, ActStatus::DryRun, "{control:#?}");

    let Some(mut image) = s.fx.scope().attach_image(&cache) else {
        return;
    };
    let mounted = s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"));
    assert_eq!(
        failed(&mounted.checks),
        ["plain_target"],
        "{:#?}",
        mounted.checks
    );
    assert_unread_below(&mounted);
    let result = s.act(&mounted.id, &arbiter()).unwrap();
    assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
    assert_eq!(result.reasons, ["plain_target"]);
    let check = result
        .checks
        .iter()
        .find(|c| c.name == "plain_target")
        .unwrap();
    assert!(check.detail.contains("node_modules/cache"), "{check:#?}");
    image.detach().expect("detach the image");
    assert!(wt.join("node_modules/pkg/index.js").exists());
}

/// A7 on Linux: a bind mount below the target (in a mount namespace of the test's own)
/// refuses it by `plain_target` alone, and the plan reads no bound of the freed bytes;
/// `act_snapshot::bind_mounts_are_not_entered` runs the same through `act`. Here `plan`
/// itself runs in the namespace.
#[cfg(target_os = "linux")]
#[test]
fn mount_below_the_target_refused() {
    use std::io::Write as _;
    let s = Setup::new();
    let wt = s.worktree("mounted-below");
    write(&wt, "node_modules/pkg/index.js", "x");
    let cache = wt.join("node_modules/cache");
    std::fs::create_dir_all(&cache).unwrap();
    let data = s.fx.path("owner/data");
    write(&data, "keep", "x");
    let mut probe = s.fx.command("true", s.fx.root());
    s.fx.scope().mount_namespace(&mut probe, &[(&data, &cache)]);
    if !probe.status().is_ok_and(|status| status.success()) {
        assert!(
            std::env::var("CI").as_deref() != Ok("true"),
            "CI runs every bind-mount test"
        );
        let _ = writeln!(
            std::io::stderr(),
            "skipped mount_below_the_target_refused: no private mount namespace"
        );
        return;
    }
    let mut command = s.cardinal();
    command
        .arg("plan")
        .arg("reclaim-dir")
        .arg(wt.join("node_modules"));
    s.fx.scope()
        .mount_namespace(&mut command, &[(&data, &cache)]);
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let plan: cardinal::contract::Plan = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(failed(&plan.checks), ["plain_target"], "{:#?}", plan.checks);
    assert_unread_below(&plan);
    let control = s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"));
    assert!(control.invariants_ok, "{:#?}", control.checks);
    assert_eq!(std::fs::read_to_string(data.join("keep")).unwrap(), "x");
    let _ = writeln!(
        std::io::stderr(),
        "mount_below_the_target_refused: ran in a private mount namespace"
    );
}
