//! Disk states (A16, A17, `docs/architecture.md` sections 10.4 and 13.6), driven through
//! `cardinal reconcile` by the readings fake, with the lines at 1, 2, and 3 MiB (critical,
//! warning, recovery) and the whole fixture one volume. A pass reads a volume once at step 4
//! and, under pressure there, once more at step 6; each read takes the fake's next value.

mod disk_support;
mod observe_common;
mod plan_support;
mod support;

use cardinal::contract::{DiskLevel, InboxStatus, RecordKind};
use cardinal::reconcile::TIMER_INTERVAL_MS;
use disk_support::{Disk, MIB, carried_report, own_errors, report};

const LINES: (u64, u64, u64) = (1, 2, 3);

/// `tenths` tenths of a MiB.
const fn mib(tenths: u64) -> u64 {
    tenths * MIB / 10
}

/// Each pass's state of the one volume.
fn levels(d: &Disk, passes: usize) -> Vec<DiskLevel> {
    (0..passes)
        .map(|_| {
            let pass = d.reconcile();
            assert_eq!(own_errors(&pass), Vec::<String>::new());
            d.state().state
        })
        .collect()
}

/// Pressure starts below the warning line and ends only above the recovery line: a reading
/// back above the warning line, or on the recovery line, stays in `warning`. `since_ms` moves
/// only on a transition, and 12 readings are kept.
#[test]
fn hysteresis() {
    let d = Disk::new(LINES);
    let recovery = 3 * MIB;
    d.fixture_reads(&[
        Some(mib(25)), // ok
        Some(mib(15)), // warning
        Some(mib(15)),
        Some(mib(25)), // above the warning line
        Some(mib(25)),
        Some(recovery),
        Some(recovery),
        Some(recovery + 1), // above the recovery line
    ]);
    let mut since = Vec::new();
    let mut seen = Vec::new();
    for _ in 0..5 {
        seen.extend(levels(&d, 1));
        since.push(d.state().since_ms);
    }
    assert_eq!(
        seen,
        [
            DiskLevel::Ok,
            DiskLevel::Warning,
            DiskLevel::Warning,
            DiskLevel::Warning,
            DiskLevel::Ok
        ]
    );
    assert!(since[0] < since[1], "{since:?}");
    assert_eq!(since[1], since[2]);
    assert_eq!(since[2], since[3]);
    assert!(since[3] < since[4], "{since:?}");
    let transitions: Vec<(String, String)> = d
        .records(RecordKind::DiskState)
        .iter()
        .map(|r| (r.data["from"].to_string(), r.data["to"].to_string()))
        .collect();
    assert_eq!(
        transitions,
        [
            ("\"unknown\"".into(), "\"ok\"".into()),
            ("\"ok\"".into(), "\"warning\"".into()),
            ("\"warning\"".into(), "\"ok\"".into()),
        ]
    );
    assert_eq!(d.state().readings.len(), 8);
    // Passes above every line read once each.
    levels(&d, 5);
    let state = d.state();
    assert_eq!(state.readings.len(), 12, "{state:#?}");
    assert_eq!(state.readings[11].free_bytes, Some(recovery + 1));
}

/// One pass after another, each earlier one a timer interval and a few seconds before.
fn levels_an_interval_apart(d: &Disk, passes: usize) -> Vec<DiskLevel> {
    (0..passes)
        .map(|pass| {
            if pass > 0 {
                d.age(TIMER_INTERVAL_MS + 4_000);
            }
            levels(d, 1)[0]
        })
        .collect()
}

/// From `ok`, readings an interval apart that fall fast enough to reach the critical line
/// within two timer intervals enter `warning` above the warning line; the same fall within a
/// pass's time, or the same reading without the fall, stays `ok`.
#[test]
fn prediction() {
    let control = Disk::new(LINES);
    control.fixture_reads(&[Some(mib(29)), Some(mib(22))]);
    let close = Disk::new(LINES);
    close.fixture_reads(&[Some(mib(29)), Some(mib(22))]);
    let steady = Disk::new(LINES);
    steady.fixture_reads(&[Some(mib(22))]);
    assert_eq!(
        levels_an_interval_apart(&steady, 2),
        [DiskLevel::Ok, DiskLevel::Ok]
    );
    assert_eq!(levels(&close, 2), [DiskLevel::Ok, DiskLevel::Ok]);
    // 0.7 MiB in an interval: 1.2 MiB above the critical line is less than two away.
    assert_eq!(
        levels_an_interval_apart(&control, 2),
        [DiskLevel::Ok, DiskLevel::Warning]
    );
    let records = control.records(RecordKind::DiskState);
    let entered = records.last().unwrap();
    assert_eq!(entered.data["to"], "warning");
    assert_eq!(entered.data["predicted"], true);
    assert_eq!(entered.data["predicted_only"], true);
    assert_eq!(entered.data["free_bytes"], mib(22));
    assert!(control.state().predicted_only);
}

/// A volume in `warning` only by prediction returns to `ok` at a reading above the warning
/// line once the fall stops, kept across passes in its disk state; one whose episode has been
/// below the warning line, even once after entering by prediction, stays in `warning` up to
/// the recovery line.
#[test]
fn prediction_only_warning_clears() {
    let predicted = Disk::new(LINES);
    // Pass 2 reads twice: step 4 enters `warning`, step 6 reads the same.
    predicted.fixture_reads(&[Some(mib(29)), Some(mib(22)), Some(mib(22)), Some(mib(25))]);
    assert_eq!(
        levels_an_interval_apart(&predicted, 3),
        [DiskLevel::Ok, DiskLevel::Warning, DiskLevel::Ok]
    );
    assert!(!predicted.state().predicted_only);
    assert_eq!(predicted.disk_statuses().0, Some(InboxStatus::Resolved));

    let dipped = Disk::new(LINES);
    dipped.fixture_reads(&[
        Some(mib(29)),
        Some(mib(22)),
        Some(mib(19)), // below the warning line at step 6
        Some(mib(25)),
        Some(mib(25)),
        Some(mib(25)),
    ]);
    assert_eq!(
        levels_an_interval_apart(&dipped, 3),
        [DiskLevel::Ok, DiskLevel::Warning, DiskLevel::Warning]
    );
    assert!(!dipped.state().predicted_only);
    assert_eq!(dipped.disk_statuses().0, Some(InboxStatus::Open));
}

/// A first reading below the critical line is `critical` at once, and `disk-reclaim` reports
/// on the volume in the same pass.
#[test]
fn first_reading_below_critical() {
    let d = Disk::new(LINES);
    d.fixture_reads(&[Some(mib(5))]);
    let pass = d.reconcile();
    assert_eq!(own_errors(&pass), Vec::<String>::new());
    assert_eq!(d.state().state, DiskLevel::Critical);
    assert_eq!(pass.disk[0].state, DiskLevel::Critical);
    // At step 4's reading, not by way of `warning` at step 6's.
    let transitions: Vec<(String, String)> = d
        .records(RecordKind::DiskState)
        .iter()
        .map(|r| (r.data["from"].to_string(), r.data["to"].to_string()))
        .collect();
    assert_eq!(transitions, [("\"unknown\"".into(), "\"critical\"".into())]);
    assert!(pass.observed);
    assert_eq!(report(&pass).reading.free_bytes, mib(5));
}

/// `critical` ends only above the critical line, into `warning`, and pressure only above the
/// recovery line; a reading on the critical line stays `critical`. Its items follow: cardinal
/// ends `disk_critical` at the first pass above the critical line, while `disk_low` stays
/// until the recovery line.
#[test]
fn critical_ends_above_the_critical_line() {
    let d = Disk::new(LINES);
    let critical = MIB;
    d.fixture_reads(&[
        Some(mib(5)),
        Some(mib(5)),
        Some(critical),
        Some(critical),
        Some(critical + 1),
        Some(critical + 1),
        Some(3 * MIB + 1),
    ]);
    let passes: Vec<_> = (0..4)
        .map(|_| (levels(&d, 1)[0], d.disk_statuses()))
        .collect();
    let (open, resolved) = (Some(InboxStatus::Open), Some(InboxStatus::Resolved));
    assert_eq!(
        passes,
        [
            (DiskLevel::Critical, (open, open)),
            (DiskLevel::Critical, (open, open)),
            (DiskLevel::Warning, (open, resolved)),
            (DiskLevel::Ok, (resolved, resolved)),
        ]
    );
}

/// A16: a failed reading changes no state. When step 6's reading fails, step 4's is the
/// pass's last successful one, and the report uses it, as does the `disk_low` item; a pass
/// whose readings all fail keeps the state and its time, reports nothing on the volume,
/// leaves the item as it was, and records the failure. The readings keep the failed read
/// without sizes.
#[test]
fn failed_final_reading_keeps_the_state() {
    let d = Disk::new(LINES);
    d.fixture_reads(&[Some(mib(15)), None]);
    let first = d.reconcile();
    let state = d.state();
    assert_eq!(state.state, DiskLevel::Warning);
    assert_eq!(report(&first).reading.free_bytes, mib(15));
    assert_eq!(report(&first).reading.at_ms, state.readings[0].at_ms);
    let items = d.disk_items();
    assert_eq!(items.len(), 1, "{items:#?}");
    assert_eq!(items[0].status, InboxStatus::Open);
    assert_eq!(carried_report(&items[0]).as_ref(), Some(report(&first)));
    assert_eq!(state.readings[1].free_bytes, None);
    assert!(
        own_errors(&first)
            .iter()
            .any(|e| e.contains("the readings fake fails this read")),
        "{first:#?}"
    );

    let second = d.reconcile();
    let after = d.state();
    assert_eq!(after.state, DiskLevel::Warning);
    assert_eq!(after.since_ms, state.since_ms);
    assert!(second.disk_reports.is_empty(), "{second:#?}");
    assert_eq!(second.disk[0].state, DiskLevel::Warning);
    assert_eq!(after.readings.len(), 4);
    assert_eq!(d.disk_items(), items);
    assert!(after.readings[2..].iter().all(|r| r.free_bytes.is_none()));
    let failures: Vec<_> = d
        .records(RecordKind::DiskState)
        .into_iter()
        .filter(|r| r.data.contains_key("error"))
        .collect();
    assert_eq!(failures.len(), 3, "{failures:#?}");
    assert!(failures.iter().all(|r| r.data["to"] == "warning"));
}
