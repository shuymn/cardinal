//! `disk-reclaim` executing under pressure (A31, C16; `docs/architecture.md` sections 9.7,
//! 10.4, and 13.7, "Under pressure"), driven through `cardinal reconcile` by the readings fake,
//! with the lines at 1, 2, and 3 MiB (critical, warning, recovery) and the whole fixture, the
//! state dir included, as one volume. Each pass's first read is step 4's, its second step 6's,
//! and each later one follows an `act`. With its grant and its `act` true, the unit runs `act`
//! on the volume's targets in the order of section 10.4, reads the volume after every `act`,
//! and stops above the recovery line, at a reading it cannot take, when the targets run out,
//! or once `reclaim_budget_s` is spent; without either, it lists and records `planned`.

#![cfg(unix)]

mod disk_support;
mod live_support;
mod observe_common;
mod plan_support;
mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use cardinal::contract::{ActStatus, DiskLevel, DiskReport, InboxKind, InboxStatus, RecordKind};
use cardinal::state::LockMode;
use cardinal::testpoint;
use disk_support::{Disk, MIB, fill, own_errors, report, targets};

#[test]
fn helper() {
    support::helper::run_if_requested();
}

const LINES: (u64, u64, u64) = (1, 2, 3);
const KIB: u64 = 1024;
const UNDER: Option<u64> = Some(3 * MIB / 2);
const ABOVE: Option<u64> = Some(7 * MIB / 2);

/// `[unit.disk-reclaim] act` true and `[act] execute` granting the unit.
const GRANTED: &str = "[unit.disk-reclaim]\nact = true\n\
                       [act]\nexecute = { \"unit:disk-reclaim\" = [\"reclaim_dir\"] }\n";

/// A setup whose `disk-reclaim` executes, with `extra` tables before the grant.
fn granted(extra: &str) -> Disk {
    Disk::with_host(LINES, &format!("{extra}{GRANTED}"))
}

/// Three worktrees, oldest first, each with a `target/` of its own bytes: `a` 512 KiB, `b`
/// 256 KiB, `c` 128 KiB.
fn three(d: &Disk) -> [PathBuf; 3] {
    let a = d.s.worktree("a");
    let b = d.s.worktree("b");
    let c = d.s.worktree("c");
    fill(&a, "target/a.bin", 512 * KIB);
    fill(&b, "target/b.bin", 256 * KIB);
    fill(&c, "target/c.bin", 128 * KIB);
    disk_support::age(&a, Duration::from_hours(3));
    disk_support::age(&b, Duration::from_hours(2));
    [a, b, c]
}

/// The status each target of `report` was run to, by path relative to the fixture.
fn statuses(d: &Disk, report: &DiskReport) -> Vec<(String, Option<ActStatus>)> {
    targets(report, d.s.fx.root())
        .into_iter()
        .zip(report.targets.iter().map(|t| t.status))
        .collect()
}

fn executed(rel: &str) -> (String, Option<ActStatus>) {
    (rel.to_owned(), Some(ActStatus::Executed))
}

/// How many readings the volume's state holds.
fn readings(d: &Disk) -> usize {
    d.state().readings.len()
}

/// Executes in order, the volume read after every `act`, and stops once a reading is above the
/// recovery line: the third target is untouched. Each executed target reports its status and
/// the bytes `act` expects it freed, the report carries the last reading, and every `act` is
/// recorded as the unit's, none as `planned`.
#[test]
fn executes_in_order_until_above_the_recovery_line() {
    let d = granted("");
    let [a, b, c] = three(&d);
    d.fixture_reads(&[UNDER, UNDER, Some(2 * MIB), ABOVE]);

    let pass = d.reconcile();
    assert_eq!(own_errors(&pass), Vec::<String>::new());
    let report = report(&pass);
    assert_eq!(
        statuses(&d, report),
        [executed("a/target"), executed("b/target")],
        "{report:#?}"
    );
    for target in &report.targets {
        let freed = target.freed_bytes.flatten().expect("freed bytes");
        assert!(freed >= target.exclusive_bytes, "{report:#?}");
    }
    assert_eq!(report.reading.free_bytes, ABOVE.unwrap());
    assert_eq!(report.shortfall_bytes, 0);
    assert!(!a.join("target").exists());
    assert!(!b.join("target").exists());
    assert!(c.join("target/c.bin").exists());
    assert!(
        !d.s.state
            .trash_dir()
            .join(&report.targets[0].plan_id)
            .exists()
    );
    assert_eq!(readings(&d), 4);
    assert_eq!(d.state().state, DiskLevel::Ok);
    let acts = d.records(RecordKind::Act);
    assert_eq!(acts.len(), 2, "{acts:#?}");
    assert!(acts.iter().all(|r| r.data["caller"] == "unit:disk-reclaim"));
    assert_eq!(d.records(RecordKind::Planned), Vec::new());
}

/// An episode that begins at step 4's reading and that the pass's executions end is reported
/// all the same: `disk_low`, and `disk_critical` when a reading found the volume critical, are
/// raised with the volume's report and ended at the reading above the recovery line, both
/// listed in the pass's inbox changes.
#[test]
fn an_episode_ended_within_the_pass_is_raised_and_ended() {
    for (first, kinds) in [
        (UNDER, vec![InboxKind::DiskLow]),
        (
            Some(MIB / 2),
            vec![InboxKind::DiskLow, InboxKind::DiskCritical],
        ),
    ] {
        let d = granted("");
        let [a, _, _] = three(&d);
        d.fixture_reads(&[first, first, ABOVE]);

        let pass = d.reconcile();
        assert!(!a.join("target").exists());
        assert_eq!(d.state().state, DiskLevel::Ok);
        let items = d.disk_items();
        assert_eq!(
            items.iter().map(|i| i.kind).collect::<Vec<_>>(),
            kinds,
            "{items:#?}"
        );
        for item in &items {
            assert!(item.ended_ms.is_some(), "{item:#?}");
            assert_eq!(item.status, InboxStatus::Resolved, "{item:#?}");
            let executed = &item.evidence["report"]["targets"][0]["status"];
            assert_eq!(*executed, "executed", "{item:#?}");
            assert!(pass.inbox.raised.contains(&item.key), "{pass:#?}");
            assert!(pass.inbox.cleared.contains(&item.key), "{pass:#?}");
        }
    }
}

/// An episode ended within the pass is published once, already ended: with the write of an
/// ended item failing, the pass leaves no `disk_low` open for the volume it lifted back to
/// `ok`, and reports the failure.
#[test]
fn an_episode_ended_within_the_pass_is_one_write() {
    let d = granted("");
    three(&d);
    d.fixture_reads(&[UNDER, UNDER, ABOVE]);
    std::fs::write(
        d.points.dir().join("write-failures"),
        "inbox-end\tENOSPC\talways\n",
    )
    .unwrap();

    let pass = d.reconcile();
    assert_eq!(d.state().state, DiskLevel::Ok);
    assert_eq!(d.disk_items(), Vec::new(), "{pass:#?}");
    assert!(
        pass.errors.iter().any(|e| e.scope == "disk_items"),
        "{pass:#?}"
    );
}

/// A `disk_low` open from an earlier pass that this pass's executions end keeps this pass's
/// report, the targets it executed.
#[test]
fn an_episode_ended_by_executions_keeps_their_report() {
    let d = granted("");
    let [a, _, _] = three(&d);
    d.fixture_reads(&[UNDER]);
    d.reconcile();
    let open = d.disk_items();
    assert_eq!(open.len(), 1, "{open:#?}");
    assert_eq!(open[0].ended_ms, None);
    // The first pass ran out of targets above nothing: refill one for the next.
    fill(&a, "target/a.bin", 512 * KIB);
    d.fixture_reads(&[UNDER, UNDER, ABOVE]);

    let pass = d.reconcile();
    let items = d.disk_items();
    assert_eq!(items.len(), 1, "{items:#?}");
    assert!(items[0].ended_ms.is_some(), "{pass:#?}");
    let reported = &items[0].evidence["report"];
    assert_eq!(
        reported["targets"][0]["status"], "executed",
        "{reported:#?}"
    );
    assert_eq!(
        reported["reading"]["free_bytes"],
        ABOVE.unwrap(),
        "{reported:#?}"
    );
}

/// An item that cannot be read when the pass takes its snapshot authorizes no end: the
/// snapshot is reported as incomplete, and an episode the pass sees begin and end while that
/// `disk_low` is still open, readable again by step 6, leaves it as it was.
#[test]
fn an_unreadable_item_ends_nothing() {
    use std::os::unix::fs::PermissionsExt as _;
    let d = granted("");
    let [a, _, _] = three(&d);
    d.fixture_reads(&[UNDER]);
    d.reconcile();
    let open = d.disk_items();
    assert_eq!(open.len(), 1, "{open:#?}");
    let file = d.s.state.inbox_item(&open[0].key, 0);
    assert!(file.exists(), "{open:#?}");
    fill(&a, "target/a.bin", 512 * KIB);
    d.fixture_reads(&[Some(MIB / 2), Some(MIB / 2), ABOVE]);
    let mode = |m| std::fs::set_permissions(&file, std::fs::Permissions::from_mode(m));
    mode(0o000).unwrap();
    let mut command = d.reconcile_command();
    d.points
        .apply(&mut command, &[testpoint::RECONCILE_OBSERVED], None);
    let child = command
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    d.points.wait_reached(
        testpoint::RECONCILE_OBSERVED,
        child.id(),
        live_support::WAIT,
    );
    mode(0o600).unwrap();
    d.points.release(testpoint::RECONCILE_OBSERVED);
    let pass = disk_support::pass(&child.wait_with_output().unwrap());

    assert!(
        pass.errors.iter().any(|e| e.scope == "inbox_before"),
        "{pass:#?}"
    );
    let low: Vec<_> = d
        .disk_items()
        .into_iter()
        .filter(|i| i.kind == InboxKind::DiskLow)
        .collect();
    assert_eq!(low.len(), 1, "{low:#?}");
    assert_eq!(low[0].ended_ms, None, "{low:#?}");
    assert_eq!(low[0].seen, open[0].seen, "{low:#?}");
}

/// A target that a job started using after step 5 is refused (`rejected`, `not_in_use`) and
/// kept, the volume is read after that refusal too, and the next target executes; the targets
/// running out ends the pass below the recovery line, with the shortfall the last reading
/// leaves.
#[test]
fn a_refused_target_is_read_after_and_the_next_executes() {
    let d = granted("");
    let [a, b, c] = three(&d);
    d.fixture_reads(&[UNDER]);
    let mut command = d.reconcile_command();
    d.points
        .apply(&mut command, &[testpoint::RECONCILE_OBSERVED], None);
    let child = command
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    d.points.wait_reached(
        testpoint::RECONCILE_OBSERVED,
        child.id(),
        live_support::WAIT,
    );
    let key = cardinal::state::worktree_key(&a);
    let in_use = cardinal::state::try_lock(&d.s.state.worktree_lock(&key), LockMode::Shared)
        .unwrap()
        .expect("the in-use lock is free");
    d.points.release(testpoint::RECONCILE_OBSERVED);
    let pass = disk_support::pass(&child.wait_with_output().unwrap());
    drop(in_use);

    let report = report(&pass);
    assert_eq!(
        statuses(&d, report),
        [
            ("a/target".to_owned(), Some(ActStatus::Rejected)),
            executed("b/target"),
            executed("c/target"),
        ],
        "{report:#?}"
    );
    assert!(a.join("target/a.bin").exists());
    assert!(!b.join("target").exists());
    assert!(!c.join("target").exists());
    // Steps 4 and 6, then one reading after each `act`.
    assert_eq!(readings(&d), 5);
    assert_eq!(
        report.shortfall_bytes,
        3 * MIB - UNDER.unwrap(),
        "{report:#?}"
    );
}

/// A reading that fails after an `act` stops the unit: the next target is untouched, and the
/// report carries the last reading that succeeded.
#[test]
fn an_unreadable_reading_stops_it() {
    let d = granted("");
    let [a, b, _] = three(&d);
    d.fixture_reads(&[UNDER, UNDER, None, UNDER]);

    let pass = d.reconcile();
    let report = report(&pass);
    assert_eq!(statuses(&d, report), [executed("a/target")], "{report:#?}");
    assert_eq!(report.reading.free_bytes, UNDER.unwrap());
    assert!(!a.join("target").exists());
    assert!(b.join("target/b.bin").exists());
    assert!(
        pass.errors.iter().any(|e| e.scope.starts_with("disk:")),
        "{pass:#?}"
    );
}

/// Once `reclaim_budget_s` is spent, no more `act` starts: the one under way, paused past the
/// budget, finishes, and the next target waits for a later pass.
#[test]
fn the_budget_lets_the_execution_under_way_finish() {
    let d = granted("[reconcile]\nreclaim_budget_s = 1\n");
    let [a, b, _] = three(&d);
    d.fixture_reads(&[UNDER]);
    let mut command = d.reconcile_command();
    d.points
        .apply(&mut command, &[testpoint::ACT_EXCLUDED], None);
    let child = command
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    d.points
        .wait_reached(testpoint::ACT_EXCLUDED, child.id(), live_support::WAIT);
    std::thread::sleep(Duration::from_millis(1200));
    d.points.release(testpoint::ACT_EXCLUDED);
    let pass = disk_support::pass(&child.wait_with_output().unwrap());

    let report = report(&pass);
    assert_eq!(statuses(&d, report), [executed("a/target")], "{report:#?}");
    assert!(!a.join("target").exists());
    assert!(b.join("target/b.bin").exists());
}

/// The gate and the grant are read again before each `act`: one withdrawn after the pass
/// observed, `[unit.disk-reclaim] act` set to false or `[act] execute` dropped, runs no `act`
/// and is reported.
#[test]
fn a_gate_or_grant_withdrawn_during_the_pass_runs_no_act() {
    for withdrawn in [
        "[unit.disk-reclaim]\nact = false\n\
         [act]\nexecute = { \"unit:disk-reclaim\" = [\"reclaim_dir\"] }\n",
        "[unit.disk-reclaim]\nact = true\n",
    ] {
        let mut d = granted("");
        let [a, _, _] = three(&d);
        d.fixture_reads(&[UNDER]);
        let mut command = d.reconcile_command();
        d.points
            .apply(&mut command, &[testpoint::RECONCILE_OBSERVED], None);
        let child = command
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        d.points.wait_reached(
            testpoint::RECONCILE_OBSERVED,
            child.id(),
            live_support::WAIT,
        );
        let main = d.s.main.display().to_string();
        d.s.scene.clear_observe_config();
        d.s.scene.observe_config(&format!(
            "repos = ['{main}']\n{}",
            disk_support::host(LINES, withdrawn)
        ));
        d.s.scene.config();
        d.points.release(testpoint::RECONCILE_OBSERVED);
        let pass = disk_support::pass(&child.wait_with_output().unwrap());

        assert_eq!(d.records(RecordKind::Act), Vec::new(), "{withdrawn}");
        assert!(report(&pass).targets.iter().all(|t| t.status.is_none()));
        assert!(a.join("target/a.bin").exists());
        assert!(
            own_errors(&pass)
                .iter()
                .any(|e| e.contains("may no longer execute")),
            "{pass:#?}"
        );
    }
}

/// A target whose bound is 0 or null (two clones of one file on macOS, an injected `xfs` on
/// Linux) runs after every target with a positive bound, in an older worktree included, and
/// the reading after it decides whether the pass goes on; a reading above the recovery line
/// after the positive target leaves it untouched.
#[test]
fn a_zero_bound_runs_last_and_the_reading_decides() {
    for (reads, ran) in [
        (vec![UNDER, UNDER, UNDER, ABOVE], 2),
        (vec![UNDER, UNDER, ABOVE], 1),
    ] {
        let d = granted("");
        let old = d.s.worktree("old");
        let new = d.s.worktree("new");
        zero_bound(&d, &old.join("target"));
        fill(&new, "target/out.bin", 128 * KIB);
        disk_support::age(&old, Duration::from_hours(2));
        d.fixture_reads(&reads);

        let pass = d.reconcile();
        let report = report(&pass);
        let expected = [executed("new/target"), executed("old/target")];
        assert_eq!(statuses(&d, report), expected[..ran], "{report:#?}");
        assert_eq!(
            report.targets.last().map(|t| t.plan_id.is_empty()),
            Some(false)
        );
        assert!(!new.join("target").exists());
        assert_eq!(old.join("target").exists(), ran == 1, "{report:#?}");
        if ran == 2 {
            assert_eq!(report.targets[1].exclusive_bytes, 0, "{report:#?}");
        }
    }
}

/// A target at `target` whose plan's bound is 0 or null: on macOS two clones of one file, on
/// Linux a file on an injected `xfs`.
fn zero_bound(d: &Disk, target: &Path) {
    #[cfg(target_os = "macos")]
    {
        fill(target, "one.bin", MIB);
        let c_path = |path: PathBuf| {
            std::ffi::CString::new(path.into_os_string().into_encoded_bytes()).unwrap()
        };
        let (from, to) = (
            c_path(target.join("one.bin")),
            c_path(target.join("two.bin")),
        );
        // SAFETY: both are NUL-terminated paths.
        let cloned = unsafe { libc::clonefile(from.as_ptr(), to.as_ptr(), 0) };
        assert_eq!(cloned, 0, "clonefile: {}", std::io::Error::last_os_error());
    }
    #[cfg(not(target_os = "macos"))]
    {
        fill(target, "one.bin", MIB);
        std::fs::write(
            d.points.dir().join("fs-types"),
            format!("xfs\t{}\n", target.display()),
        )
        .unwrap();
    }
    let _ = d;
}

/// On a volume other than the state dir's, the unit lists as without the grant: a `planned`
/// record per listed target with `execution_not_granted`, and nothing deleted.
#[test]
fn another_volume_lists_as_without_the_grant() {
    let d = granted("");
    let [a, b, c] = three(&d);
    let before: Vec<_> = [&a, &b, &c].map(|wt| plan_support::listing(wt)).into();
    d.readings(&[
        (d.s.fx.root(), &[UNDER]),
        (&d.s.scene.state_dir, &[Some(60 * MIB)]),
    ]);
    // A first pass observes the worktrees; from the next, their volume is watched.
    d.reconcile();

    let pass = d.reconcile();
    assert_eq!(own_errors(&pass), Vec::<String>::new());
    let report = report(&pass);
    assert!(
        report.targets.iter().all(|t| t.status.is_none()),
        "{report:#?}"
    );
    assert!(!report.targets.is_empty(), "{report:#?}");
    let after: Vec<_> = [&a, &b, &c].map(|wt| plan_support::listing(wt)).into();
    assert_eq!(after, before);
    assert_eq!(d.records(RecordKind::Act), Vec::new());
    let planned = d.records(RecordKind::Planned);
    assert_eq!(planned.len(), report.targets.len());
    for record in &planned {
        assert_eq!(
            record.data["reasons"],
            serde_json::json!(["execution_not_granted"])
        );
    }
}

/// C16: without the grant, or with the unit's `act` false, the unit lists, records `planned`
/// naming the key that stops it (`act.execute`, `unit.disk-reclaim.act`), and deletes
/// nothing.
#[test]
fn act_false_records_planned() {
    for (extra, key) in [
        ("[unit.disk-reclaim]\nact = true\n", "act.execute"),
        (
            "[unit.disk-reclaim]\nact = false\n\
             [act]\nexecute = { \"unit:disk-reclaim\" = [\"reclaim_dir\"] }\n",
            "unit.disk-reclaim.act",
        ),
    ] {
        let d = Disk::with_host(LINES, extra);
        let [a, b, c] = three(&d);
        let before: Vec<_> = [&a, &b, &c].map(|wt| plan_support::listing(wt)).into();
        d.fixture_reads(&[UNDER]);

        let pass = d.reconcile();
        assert_eq!(own_errors(&pass), Vec::<String>::new());
        let report = report(&pass);
        assert!(!report.targets.is_empty(), "{report:#?}");
        assert!(report.targets.iter().all(|t| t.status.is_none()));
        let after: Vec<_> = [&a, &b, &c].map(|wt| plan_support::listing(wt)).into();
        assert_eq!(after, before, "{key}");
        assert_eq!(d.records(RecordKind::Act), Vec::new(), "{key}");
        let planned = d.records(RecordKind::Planned);
        assert_eq!(planned.len(), report.targets.len(), "{key}");
        for record in &planned {
            assert_eq!(
                record.data["reasons"],
                serde_json::json!(["execution_not_granted"])
            );
            let keys = record.data["provenance"]["config"].as_array().unwrap();
            assert!(keys.iter().any(|k| k["key"] == key), "{key}: {record:#?}");
        }
    }
}

/// A pass whose volume reads above the recovery line still finishes, at step 2, a committed
/// execution of the unit that died, and executes nothing more.
#[test]
fn a_pass_above_the_recovery_line_finishes_a_committed_execution() {
    let d = granted("");
    let [a, b, c] = three(&d);
    let plan =
        d.s.plan(cardinal::contract::PlanKind::ReclaimDir, &a.join("target"));
    let scope = cardinal_test_support::Scope::new();
    let points = support::testpoints::TestPoints::new(&scope);
    let unit = cardinal::act::ActOptions {
        caller: cardinal::contract::Caller::Unit("disk-reclaim".into()),
        ..live_support::owner()
    };
    let mut command = d.s.act_command(&plan.id, &unit);
    points.apply(&mut command, &[testpoint::ACT_COMMITTED], None);
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    points.wait_reached(testpoint::ACT_COMMITTED, child.id(), live_support::WAIT);
    child.kill();
    let trash = d.s.state.trash_entry(&plan.id);
    assert!(trash.join("t").is_dir());
    d.fixture_reads(&[Some(60 * MIB)]);

    let pass = d.reconcile();
    assert_eq!(own_errors(&pass), Vec::<String>::new());
    assert_eq!(
        pass.recovered,
        [cardinal::contract::RecoveredExecution {
            plan_id: plan.id.clone(),
            status: ActStatus::Executed,
        }]
    );
    assert!(!trash.exists());
    assert!(pass.disk_reports.is_empty(), "{pass:#?}");
    assert!(b.join("target/b.bin").exists());
    assert!(c.join("target/c.bin").exists());
}
