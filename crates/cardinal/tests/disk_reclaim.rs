//! `disk-reclaim`'s listing (C10, A16, `docs/architecture.md` sections 10.4 and 13.6), driven
//! through `cardinal reconcile` by the readings fake, with the lines at 1, 2, and 3 MiB
//! (critical, warning, recovery). Each pass's first read is step 4's and, under pressure,
//! its second step 6's, the final reading the report is computed from. Listing deletes
//! nothing (`order_and_stop` compares the worktrees before and after).

mod disk_support;
mod observe_common;
mod plan_support;
mod support;

use std::time::Duration;

use cardinal::contract::{DiskLevel, PlanKind, RecordKind};
use cardinal::state::LockMode;
use disk_support::{Disk, MIB, blocked, fill, own_errors, report, targets};

const LINES: (u64, u64, u64) = (1, 2, 3);
const KIB: u64 = 1024;
const RECOVERY: u64 = 3 * MIB;

/// A pass's targets are listed in order, oldest worktree first, until their exclusive bytes
/// would lift the final reading above the recovery line; each listed target gets a `planned`
/// record, and nothing runs `act`.
#[test]
fn order_and_stop() {
    let d = Disk::new(LINES);
    let root = d.s.fx.root().to_path_buf();
    let old = d.s.worktree("old");
    let new = d.s.worktree("new");
    fill(&old, "target/big.bin", 512 * 1024);
    fill(&old, "node_modules/pkg/index.js", 256 * 1024);
    fill(&new, "target/big.bin", 1024 * 1024);
    disk_support::age(&old, Duration::from_hours(2));
    // Under pressure at step 4; 700 KiB short of the recovery line at step 6.
    let last = RECOVERY - 700 * KIB;
    d.fixture_reads(&[Some(3 * MIB / 2), Some(last)]);
    let before = [plan_support::listing(&old), plan_support::listing(&new)];

    let pass = d.reconcile();
    assert_eq!(own_errors(&pass), Vec::<String>::new());
    let observed: cardinal::contract::Inventory =
        serde_json::from_slice(&std::fs::read(d.s.state.reconcile_observation()).unwrap()).unwrap();
    let activity = |wt: &std::path::Path| observe_common::worktree(&observed, wt).last_activity_ms;
    assert!(
        activity(&old).unwrap() < activity(&new).unwrap(),
        "{:?} {:?}",
        activity(&old),
        activity(&new)
    );
    let report = report(&pass);
    assert_eq!(report.reading.free_bytes, last);
    assert_eq!(
        targets(report, &root),
        ["old/target", "old/node_modules"],
        "{report:#?}"
    );
    assert_eq!(report.shortfall_bytes, 0);
    let listed: u64 = report.targets.iter().map(|t| t.exclusive_bytes).sum();
    assert!(listed > 700 * KIB, "{report:#?}");
    assert!(report.targets[0].exclusive_bytes < 700 * KIB, "{report:#?}");

    let planned = d.records(RecordKind::Planned);
    let ids: Vec<&str> = planned
        .iter()
        .map(|r| r.plan_id.as_deref().unwrap())
        .collect();
    let listed_ids: Vec<&str> = report.targets.iter().map(|t| t.plan_id.as_str()).collect();
    assert_eq!(ids, listed_ids);
    for record in &planned {
        assert_eq!(record.data["caller"], "unit:disk-reclaim");
        assert_eq!(record.data["status"], "dry_run");
        assert_eq!(
            record.data["reasons"],
            serde_json::json!(["execution_not_designed"])
        );
        assert_eq!(record.data["checks_ms"], report.checks_ms);
        let keys = &record.data["provenance"]["config"];
        assert!(
            keys.as_array()
                .unwrap()
                .iter()
                .any(|k| k["key"] == "unit.disk-reclaim.act" && k["value"] == false),
            "{record:#?}"
        );
    }
    assert_eq!(d.records(RecordKind::Act), Vec::new());
    assert_eq!(
        [plan_support::listing(&old), plan_support::listing(&new)],
        before,
        "nothing is deleted"
    );
}

/// Within a worktree, the most exclusive bytes first: a `node_modules` whose files are mostly
/// hard links into a store outside comes after a smaller directory of its own. A target whose
/// regular files' bytes are all aliased frees none of them, whatever blocks its directories
/// hold (ext4's): it is not listed but counted in `blocked` as `aliased`.
#[test]
fn exclusive_bytes_first() {
    let d = Disk::new(LINES);
    let root = d.s.fx.root().to_path_buf();
    let a = d.s.worktree("a");
    let b = d.s.worktree("b");
    let store = d.s.fx.path("store");
    fill(&store, "big.js", 1024 * 1024);
    fill(&store, "only.js", 128 * 1024);
    fill(&a, "target/out.bin", 200 * 1024);
    fill(&a, "node_modules/pkg/own.js", 64 * 1024);
    std::fs::hard_link(store.join("big.js"), a.join("node_modules/pkg/big.js")).unwrap();
    std::fs::create_dir_all(b.join("node_modules/pkg")).unwrap();
    std::fs::hard_link(store.join("only.js"), b.join("node_modules/pkg/only.js")).unwrap();
    let last = 3 * MIB / 2;
    d.fixture_reads(&[Some(last)]);

    let pass = d.reconcile();
    assert_eq!(own_errors(&pass), Vec::<String>::new());
    let report = report(&pass);
    assert_eq!(
        targets(report, &root),
        ["a/target", "a/node_modules"],
        "{report:#?}"
    );
    let (target, modules) = (&report.targets[0], &report.targets[1]);
    assert!(
        target.exclusive_bytes > modules.exclusive_bytes,
        "{report:#?}"
    );
    assert!(modules.exclusive_bytes < 1024 * 1024, "{report:#?}");
    assert_eq!(
        blocked(report, &root),
        [("b/node_modules".to_owned(), "aliased".to_owned())]
    );
    assert!(report.blocked[0].bytes >= 128 * 1024, "{report:#?}");
    assert_eq!(
        report.shortfall_bytes,
        RECOVERY - last - target.exclusive_bytes - modules.exclusive_bytes
    );
}

/// A target in a worktree in use and one holding protected data are skipped, each counted in
/// `blocked` with the check that failed first; the control is listed.
#[test]
fn in_use_and_protected_skipped() {
    let d = Disk::new(LINES);
    let root = d.s.fx.root().to_path_buf();
    let busy = d.s.worktree("busy");
    let fuzzing = d.s.worktree("fuzzing");
    let free = d.s.worktree("free");
    for wt in [&busy, &fuzzing, &free] {
        fill(wt, "target/out.bin", 128 * 1024);
    }
    fill(&fuzzing, "target/a/b/fuzz/corpus/seed", 4 * 1024);
    let key = cardinal::state::worktree_key(&busy);
    let _in_use = cardinal::state::try_lock(&d.s.state.worktree_lock(&key), LockMode::Shared)
        .unwrap()
        .expect("the in-use lock is free");
    d.fixture_reads(&[Some(3 * MIB / 2)]);

    let pass = d.reconcile();
    assert_eq!(own_errors(&pass), Vec::<String>::new());
    let report = report(&pass);
    assert_eq!(targets(report, &root), ["free/target"], "{report:#?}");
    assert_eq!(
        blocked(report, &root),
        [
            ("busy/target".to_owned(), "not_in_use".to_owned()),
            ("fuzzing/target".to_owned(), "no_protected".to_owned()),
        ]
    );
    assert!(report.blocked.iter().all(|b| b.bytes >= 128 * 1024));
}

/// `disk-reclaim` reports in `critical` as in `warning`.
#[test]
fn runs_in_critical() {
    let d = Disk::new(LINES);
    let root = d.s.fx.root().to_path_buf();
    let a = d.s.worktree("a");
    fill(&a, "target/out.bin", 128 * 1024);
    d.fixture_reads(&[Some(MIB / 2)]);

    let pass = d.reconcile();
    assert_eq!(own_errors(&pass), Vec::<String>::new());
    assert_eq!(d.state().state, DiskLevel::Critical);
    let report = report(&pass);
    assert_eq!(targets(report, &root), ["a/target"]);
    assert_eq!(
        report.shortfall_bytes,
        RECOVERY - MIB / 2 - report.targets[0].exclusive_bytes
    );
}

/// The shortfall is computed against the final reading, step 6's, here lower than step 4's.
#[test]
fn shortfall_against_the_final_reading() {
    let d = Disk::new(LINES);
    let a = d.s.worktree("a");
    fill(&a, "target/out.bin", 300 * 1024);
    let (first, last) = (19 * MIB / 10, 12 * MIB / 10);
    d.fixture_reads(&[Some(first), Some(last)]);

    let pass = d.reconcile();
    assert_eq!(own_errors(&pass), Vec::<String>::new());
    let report = report(&pass);
    assert_eq!(report.reading.free_bytes, last);
    let state = d.state();
    assert_eq!(report.reading.at_ms, state.readings[1].at_ms);
    assert_eq!(
        report.shortfall_bytes,
        RECOVERY - last - report.targets[0].exclusive_bytes
    );
}

/// One set of checks: the plans come from the pass's one observation, which asks gh what
/// one `inventory` asks, and carry their time; the report and every `planned` record carry
/// that time, and no `act` runs.
#[test]
fn one_set_of_checks() {
    let d = Disk::new(LINES);
    let a = d.s.worktree("a");
    fill(&a, "target/out.bin", 64 * 1024);
    fill(&a, "node_modules/x.js", 64 * 1024);
    d.fixture_reads(&[Some(3 * MIB / 2)]);
    let gh_before = d.s.gh_calls().len();
    let created_before = d.records(RecordKind::PlanCreated).len();

    let pass = d.reconcile();
    assert_eq!(own_errors(&pass), Vec::<String>::new());
    let report = report(&pass);
    let pass_gh = d.s.gh_calls().len() - gh_before;
    let created = d.records(RecordKind::PlanCreated);
    // A whole-worktree plan for `a`, and one for each of its two entries.
    assert_eq!(created.len() - created_before, 3, "{created:#?}");
    for target in &report.targets {
        let plan = cardinal::plan::load(&d.s.state, &target.plan_id).unwrap();
        assert!(plan.created_ms >= report.checks_ms, "{plan:#?}");
    }
    for record in d.records(RecordKind::Planned) {
        assert_eq!(record.data["checks_ms"], report.checks_ms);
    }
    assert_eq!(d.records(RecordKind::Act), Vec::new());

    let gh_inventory = d.s.gh_calls().len();
    let out =
        d.s.cardinal()
            .arg("inventory")
            .arg("--repo")
            .arg(&d.s.main)
            .output()
            .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(pass_gh, d.s.gh_calls().len() - gh_inventory);
}

/// A16: a volume under pressure whose readings all fail in a pass is not reported on, and
/// its state stays; the control, on a volume read in the same pass, is reported.
#[test]
fn unreadable_volume_skipped() {
    let d = Disk::new(LINES);
    let root = d.s.fx.root().to_path_buf();
    let lost = d.s.worktree("lost");
    let kept = d.s.worktree("kept");
    fill(&lost, "target/out.bin", 64 * 1024);
    fill(&kept, "target/out.bin", 64 * 1024);
    let pressure = Some(3 * MIB / 2);
    // `lost` is a volume of its own, watched once a full observation names it: read in the
    // second pass, then never again.
    d.readings(&[(&lost, &[pressure, pressure, None]), (&root, &[pressure])]);
    let first = d.reconcile();
    assert_eq!(
        first.disk.len(),
        1,
        "only the state dir's volume: {first:#?}"
    );
    let second = d.reconcile();
    assert_eq!(second.disk_reports.len(), 2, "{second:#?}");

    let third = d.reconcile();
    let lost_key = second
        .disk
        .iter()
        .find(|s| s.mount == lost.display().to_string())
        .unwrap()
        .volume
        .clone();
    assert_eq!(third.disk_reports.len(), 1, "{third:#?}");
    let control = &third.disk_reports[0];
    assert_ne!(control.volume, lost_key);
    assert_eq!(targets(control, &root), ["kept/target"]);
    let state = third.disk.iter().find(|s| s.volume == lost_key).unwrap();
    assert_eq!(state.state, DiskLevel::Warning);
    assert!(
        own_errors(&third)
            .iter()
            .any(|e| e.contains(&lost.display().to_string())),
        "{third:#?}"
    );
}

/// A worktree on the volume that observation read only in part (a directory git status
/// cannot open, so its ignored entries are unknown) is named in the report with the reason:
/// that nothing of it is listed does not read as nothing reclaimable. The control, read in
/// full, is listed and not named.
#[cfg(unix)]
#[test]
fn a_worktree_observed_in_part_is_named() {
    use std::os::unix::fs::PermissionsExt;

    /// Gives the directory its permissions back, so the setup can be removed.
    struct Readable(std::path::PathBuf);
    impl Drop for Readable {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    let d = Disk::new(LINES);
    let root = d.s.fx.root().to_path_buf();
    let hidden = d.s.worktree("hidden");
    let seen = d.s.worktree("seen");
    for wt in [&hidden, &seen] {
        fill(wt, "target/out.bin", 128 * 1024);
    }
    let private = hidden.join("private");
    std::fs::create_dir(&private).unwrap();
    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o000)).unwrap();
    let _readable = Readable(private.clone());
    if std::fs::read_dir(&private).is_ok() {
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            b"skipped a_worktree_observed_in_part_is_named: the directory stays readable (root)\n",
        );
        return;
    }
    d.fixture_reads(&[Some(3 * MIB / 2)]);

    let pass = d.reconcile();
    let report = report(&pass);
    assert_eq!(targets(report, &root), ["seen/target"], "{report:#?}");
    let hidden = std::fs::canonicalize(&hidden)
        .unwrap()
        .display()
        .to_string();
    let named: Vec<&str> = report
        .incomplete
        .iter()
        .map(|i| i.worktree.as_str())
        .collect();
    assert_eq!(named, [hidden.as_str()], "{report:#?}");
    assert!(
        report.incomplete[0]
            .reason
            .contains("could not open directory 'private/'"),
        "{report:#?}"
    );
}

/// A repository's error (a base that is not a ref name) leaves each of its worktrees read in
/// part: a linked worktree on a volume of its own is named in that volume's report, and a
/// plan made for it alone carries the error in its provenance.
#[test]
fn a_repository_error_names_each_worktree() {
    let d = Disk::new(LINES);
    let away = d.s.worktree("away");
    d.s.profile("base", "[git]\nbase = ':missing'\n");
    // The linked worktree is a volume of its own under pressure, watched once a first pass
    // has observed it; the main worktree's volume is not.
    d.readings(&[
        (d.s.fx.root(), &[Some(10 * MIB)]),
        (&away, &[Some(3 * MIB / 2)]),
    ]);
    d.reconcile();

    let pass = d.reconcile();
    let report = report(&pass);
    let named: Vec<(&str, bool)> = report
        .incomplete
        .iter()
        .map(|i| (i.worktree.as_str(), i.reason.contains("not a ref name")))
        .collect();
    assert_eq!(named, [(away.to_str().unwrap(), true)], "{report:#?}");

    let plan = d.s.try_plan(PlanKind::ReclaimWorktree, &away).unwrap();
    assert!(
        plan.provenance
            .errors
            .iter()
            .any(|e| e.message.contains("not a ref name")),
        "{:#?}",
        plan.provenance
    );
}

/// A `planned` record that cannot be appended is reported in the pass's errors; the report
/// still lists the target.
#[test]
fn a_planned_record_that_fails_is_reported() {
    let d = Disk::new(LINES);
    let root = d.s.fx.root().to_path_buf();
    let a = d.s.worktree("a");
    fill(&a, "target/out.bin", 128 * 1024);
    d.fixture_reads(&[Some(3 * MIB / 2)]);
    let mut command = d.reconcile_command();
    d.points.apply(&mut command, &[], Some("record.opened"));

    let pass = disk_support::pass(
        &command
            .output_within(plan_support::RUN_LIMIT)
            .expect("run cardinal reconcile"),
    );
    let report = report(&pass);
    assert_eq!(targets(report, &root), ["a/target"], "{report:#?}");
    let id = &report.targets[0].plan_id;
    assert!(
        own_errors(&pass)
            .iter()
            .any(|e| e.starts_with(&format!("disk_reclaim: a planned record of {id}: "))),
        "{pass:#?}"
    );
    assert_eq!(d.records(RecordKind::Planned), Vec::new());
}
