//! `disk-reclaim` on a real small volume (C10, `docs/architecture.md` sections 10.4 and 13.6):
//! the 256 MiB APFS image of `Scope::attach_image` on macOS, the 64 MiB tmpfs of
//! `Scope::tmpfs` on Linux, with its lines set as fractions of its size and worktrees on it
//! holding regenerable entries, one of them in use by a paused runner and one holding
//! protected data. A file written past the warning line puts the volume under pressure; the
//! pass reads the volume as it is, with no readings fake.

#![cfg(any(target_os = "macos", target_os = "linux"))]

mod disk_support;
mod observe_common;
mod plan_support;
mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use cardinal::contract::{DiskLevel, DiskReport, InboxItem, InboxKind, InboxStatus};
use cardinal::os::Measure;
use disk_support::{Disk, MIB, blocked, fill, own_errors, targets};

#[test]
fn helper() {
    support::helper::run_if_requested();
}

/// A linked worktree of `main` at `path` on a new branch `name`, made by git directly
/// because the volume can be outside the fixture's root.
fn worktree_at(d: &Disk, path: &Path, name: &str) -> PathBuf {
    let out =
        d.s.fx
            .command("git", &d.s.main)
            .args(["worktree", "add", "-q", "-b", name, "--"])
            .arg(path)
            .output()
            .unwrap();
    assert!(out.status.success(), "{out:?}");
    path.to_path_buf()
}

/// The one disk item, which must be a `disk_low` one with `status` carrying `report`.
fn low_item(d: &Disk, status: InboxStatus, report: &DiskReport) -> InboxItem {
    let items = d.disk_items();
    assert_eq!(items.len(), 1, "{items:#?}");
    let item = items.into_iter().next().unwrap();
    assert_eq!((item.kind, item.status), (InboxKind::DiskLow, status));
    assert_eq!(disk_support::carried_report(&item).as_ref(), Some(report));
    item
}

/// The pass finds the volume under pressure before it is full and reports its eligible
/// targets in order, the in-use worktree's entry and the protected `target/` in `blocked`,
/// and the shortfall against its own reading, and the volume's `disk_low` item carries that
/// report; once the writer's file is gone, the volume is back to `ok`, nothing is reported,
/// and cardinal ends the item.
#[test]
fn reported_before_full() {
    // Lines below any free space for every other volume (the state dir's).
    let mut d = Disk::new((0, 1, 2));
    // The volume, in a scope of its own so that the setup's configuration can still change.
    #[cfg(target_os = "macos")]
    let scope = cardinal_test_support::Scope::new();
    #[cfg(target_os = "macos")]
    let root = scope.path("volume");
    #[cfg(target_os = "macos")]
    let Some(_image) = ({
        std::fs::create_dir(&root).unwrap();
        scope.attach_image(&root)
    }) else {
        return;
    };
    #[cfg(target_os = "linux")]
    let Some(scope) = cardinal_test_support::Scope::tmpfs() else {
        return;
    };
    #[cfg(target_os = "linux")]
    let root = scope.root().to_path_buf();
    let mount = cardinal::os::volume(&root).unwrap().mount;
    let total = cardinal::os::disk(&root, Measure::Usable)
        .unwrap()
        .total_bytes;
    let (critical, warning, recovery) = (total / 8 / MIB, total / 4 / MIB, total / 2 / MIB);
    d.s.scene.observe_config(&format!(
        "[budget]\ncpu = 4\n[[disk.volumes]]\nmount = '{}'\ncritical_mib = {critical}\n\
         warning_mib = {warning}\nrecovery_mib = {recovery}",
        mount.display()
    ));

    let old = worktree_at(&d, &root.join("old"), "old");
    let new = worktree_at(&d, &root.join("new"), "new");
    let busy = worktree_at(&d, &root.join("busy"), "busy");
    let fuzzing = worktree_at(&d, &root.join("fuzzing"), "fuzzing");
    fill(&old, "target/debug/app", 2 * MIB);
    fill(&new, "node_modules/pkg/index.js", MIB);
    fill(&busy, "target/debug/app", MIB);
    fill(&fuzzing, "target/debug/app", MIB);
    fill(&fuzzing, "target/a/b/fuzz/corpus/seed", 4096);
    disk_support::age(&old, Duration::from_hours(2));
    let jobs = support::jobs::Jobs::new(&scope, &d.s.scene.state_dir, &d.s.scene.config_dir);
    d.s.config();
    let job = jobs.running(&busy, 1);
    // A first pass observes the worktrees; from the next, their volume is watched.
    let first = d.reconcile();
    assert!(
        first.disk.iter().all(|s| Path::new(&s.mount) != mount),
        "{first:#?}"
    );

    // Past the warning line, halfway to the critical line.
    let free = cardinal::os::disk(&root, Measure::Usable)
        .unwrap()
        .free_bytes;
    let goal = (critical + warning) * MIB / 2;
    fill(&root, "writer.bin", free - goal);

    let pass = d.reconcile();
    assert_eq!(own_errors(&pass), Vec::<String>::new());
    let state = pass
        .disk
        .iter()
        .find(|s| Path::new(&s.mount) == mount)
        .unwrap_or_else(|| panic!("{pass:#?}"));
    assert_eq!(state.state, DiskLevel::Warning, "{state:#?}");
    assert_eq!(pass.disk_reports.len(), 1, "{pass:#?}");
    let report = &pass.disk_reports[0];
    assert_eq!(report.volume, state.volume);
    let reading = report.reading.free_bytes;
    assert!(
        reading < warning * MIB && reading >= critical * MIB,
        "{report:#?}"
    );
    assert_eq!(
        targets(report, &root),
        ["old/target", "new/node_modules"],
        "{report:#?}"
    );
    assert_eq!(
        blocked(report, &root),
        [
            ("busy/target".to_owned(), "not_in_use".to_owned()),
            ("fuzzing/target".to_owned(), "no_protected".to_owned()),
        ]
    );
    let exclusive: u64 = report.targets.iter().map(|t| t.exclusive_bytes).sum();
    assert_eq!(report.shortfall_bytes, recovery * MIB - reading - exclusive);
    assert!(old.join("target/debug/app").exists());
    assert!(fuzzing.join("target/a/b/fuzz/corpus/seed").exists());
    low_item(&d, InboxStatus::Open, report);

    std::fs::remove_file(root.join("writer.bin")).unwrap();
    let after = d.reconcile();
    let state = after
        .disk
        .iter()
        .find(|s| Path::new(&s.mount) == mount)
        .unwrap();
    assert_eq!(state.state, DiskLevel::Ok, "{state:#?}");
    assert!(after.disk_reports.is_empty(), "{after:#?}");
    let low = low_item(&d, InboxStatus::Resolved, report);
    assert_eq!(low.resolution.unwrap().by, "cardinal");
    drop(job);
}

/// An ignored regenerable entry that is a symbolic link is on the volume of the directory
/// holding it, whatever it points to: a link into another volume and a dangling one are both
/// in the report of their worktree's volume, blocked.
#[test]
fn a_link_is_reported_on_its_own_volume() {
    // Lines above any free space: the fixture's volume, the state dir's, is under pressure.
    let d = Disk::new((1, 1 << 30, 1 << 31));
    // The volume the link points into, which no worktree is on, so it is not watched.
    #[cfg(target_os = "macos")]
    let scope = cardinal_test_support::Scope::new();
    #[cfg(target_os = "macos")]
    let elsewhere = scope.path("volume");
    #[cfg(target_os = "macos")]
    let Some(_image) = ({
        std::fs::create_dir(&elsewhere).unwrap();
        scope.attach_image(&elsewhere)
    }) else {
        return;
    };
    #[cfg(target_os = "linux")]
    let Some(scope) = cardinal_test_support::Scope::tmpfs() else {
        return;
    };
    #[cfg(target_os = "linux")]
    let elsewhere = scope.root().to_path_buf();
    std::fs::create_dir(elsewhere.join("modules")).unwrap();
    fill(&elsewhere, "modules/pkg/index.js", MIB);

    let root = d.s.fx.root().to_path_buf();
    let wt = d.s.worktree("links");
    std::os::unix::fs::symlink(elsewhere.join("modules"), wt.join("node_modules")).unwrap();
    std::os::unix::fs::symlink(elsewhere.join("gone"), wt.join("target")).unwrap();
    // `.gitignore`'s `node_modules/` and `target/` match directories only.
    std::fs::write(d.s.main.join(".git/info/exclude"), "node_modules\ntarget\n").unwrap();

    let pass = d.reconcile();
    assert_eq!(own_errors(&pass), Vec::<String>::new());
    assert_eq!(pass.disk_reports.len(), 1, "{pass:#?}");
    let report = &pass.disk_reports[0];
    let listed: Vec<String> = blocked(report, &root)
        .into_iter()
        .map(|(target, _)| target)
        .collect();
    assert_eq!(
        listed,
        ["links/node_modules", "links/target"],
        "{report:#?}"
    );
    assert!(targets(report, &root).is_empty(), "{report:#?}");
}
