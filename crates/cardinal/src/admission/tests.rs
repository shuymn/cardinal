//! The recovery pass and admission passes against a fake process table.

use std::collections::BTreeMap;
use std::time::Duration;

use super::{Admitter, Pass, ProcessFacts, lock, recover, resources, write_reservation};
use crate::config::HostConfig;
use crate::contract::{
    AdmissionResult, DerivedState, GroupIdentity, GroupMember, Priority, ProcessIdentity,
    Reservation, ReservationSchema, ReservationState,
};
use crate::state::{self, LockMode, Probe, StateDir};

const BOOT: u64 = 1_000_000;

#[derive(Default)]
struct Fake {
    /// `pid -> (start_s, pgid)`
    processes: BTreeMap<u32, (u64, u32)>,
}

impl ProcessFacts for Fake {
    fn boot_s(&self) -> Option<u64> {
        Some(BOOT)
    }
    fn group_exists(&self, pgid: u32) -> bool {
        self.processes.values().any(|&(_, g)| g == pgid)
    }
    fn start_s(&self, pid: u32) -> Option<u64> {
        self.processes.get(&pid).map(|&(start, _)| start)
    }
    fn pgid(&self, pid: u32) -> Option<u32> {
        self.processes.get(&pid).map(|&(_, g)| g)
    }
    fn members(&self, pgid: u32) -> Vec<GroupMember> {
        self.processes
            .iter()
            .filter(|(_, (_, g))| *g == pgid)
            .map(|(&pid, &(start_s, _))| GroupMember {
                pid,
                start_s,
                name: format!("p{pid}"),
            })
            .collect()
    }
}

fn reservation(job: &str, pgid: u32, child: Option<(u32, u64)>, cpu: u64) -> Reservation {
    Reservation {
        schema: ReservationSchema,
        job_id: job.into(),
        state: ReservationState::Running,
        boot_s: BOOT,
        created_ms: 1,
        updated_ms: 1,
        runner: ProcessIdentity {
            pid: pgid,
            start_s: 10,
        },
        group: GroupIdentity::Pgid {
            pgid,
            leader_start_s: 10,
        },
        child: child.map(|(pid, start_s)| ProcessIdentity { pid, start_s }),
        stop_signal_ms: None,
        priority: Priority::Normal,
        resources: resources([("cpu", cpu)]),
        admission: AdmissionResult::WithinBudget,
        class: None,
        rule: None,
        worktree: None,
        session: None,
        cwd: "/w".into(),
        argv: vec!["x".into()],
    }
}

fn state() -> (tempfile::TempDir, StateDir) {
    let dir = tempfile::tempdir().unwrap();
    let state = StateDir::open(dir.path()).unwrap();
    (dir, state)
}

/// Writes `reservation` under the admission lock, as a runner does.
fn write(state: &StateDir, reservation: &Reservation) -> Result<(), super::AdmissionError> {
    let guard = lock(state, Duration::from_secs(1))?;
    write_reservation(&guard, state, reservation)
}

fn read(state: &StateDir, job: &str) -> Option<Reservation> {
    std::fs::read(state.reservation(job))
        .ok()
        .map(|bytes| serde_json::from_slice(&bytes).unwrap())
}

#[test]
fn stale_reservations_and_orphaned_live_locks_are_removed() {
    let (_dir, state) = state();
    // Runner 100 is gone and its group is empty.
    write(
        &state,
        &reservation("aaaa000000000001", 100, Some((101, 11)), 1),
    )
    .unwrap();
    std::fs::write(state.live_lock("aaaa000000000001"), b"").unwrap();
    // A runner that died before reserving left a free live lock.
    std::fs::write(state.live_lock("aaaa000000000002"), b"").unwrap();
    // A waiting runner holds its live lock without a reservation.
    let waiting = state::try_lock(&state.live_lock("aaaa000000000003"), LockMode::Exclusive)
        .unwrap()
        .unwrap();

    let guard = lock(&state, Duration::from_secs(1)).unwrap();
    let recovery = recover(&guard, &state, &Fake::default()).unwrap();
    assert_eq!(recovery.kept.len(), 0);
    assert_eq!(recovery.released.len(), 1);
    assert!(read(&state, "aaaa000000000001").is_none());
    assert!(!state.live_lock("aaaa000000000001").exists());
    assert!(!state.live_lock("aaaa000000000002").exists());
    assert!(
        state.live_lock("aaaa000000000003").exists(),
        "a held live lock is never collected"
    );
    drop(waiting);
}

/// A verified group is kept as it is: the pass writes nothing into its reservation and
/// releases it only once the group is empty.
#[test]
fn verified_groups_are_kept_untouched_until_empty() {
    let (_dir, state) = state();
    let job = "bbbb000000000001";
    write(&state, &reservation(job, 200, Some((201, 21)), 1)).unwrap();
    let written = std::fs::read(state.reservation(job)).unwrap();
    let facts = Fake {
        processes: [(201, (21, 200)), (202, (22, 200))].into_iter().collect(),
    };
    let guard = lock(&state, Duration::from_secs(1)).unwrap();

    for _ in 0..2 {
        let recovery = recover(&guard, &state, &facts).unwrap();
        assert_eq!(recovery.kept.len(), 1);
        assert_eq!(recovery.kept[0].1.state, DerivedState::RecoveryNeeded);
        assert!(recovery.kept[0].1.verified);
        assert!(recovery.released.is_empty());
        assert_eq!(std::fs::read(state.reservation(job)).unwrap(), written);
    }
    let recovery = recover(&guard, &state, &Fake::default()).unwrap();
    assert_eq!(recovery.released.len(), 1);
    assert!(
        read(&state, job).is_none(),
        "released once the group is empty"
    );
}

#[test]
fn unverified_groups_are_kept_and_never_signalled() {
    let (_dir, state) = state();
    // The child 301 exited; a descendant stays in group 300.
    write(
        &state,
        &reservation("cccc000000000001", 300, Some((301, 31)), 1),
    )
    .unwrap();
    // The runner died at `starting`: no child was recorded.
    write(&state, &reservation("cccc000000000002", 400, None, 1)).unwrap();
    let facts = Fake {
        processes: [(302, (32, 300)), (401, (41, 400))].into_iter().collect(),
    };
    let guard = lock(&state, Duration::from_secs(1)).unwrap();
    let recovery = recover(&guard, &state, &facts).unwrap();
    assert_eq!(recovery.kept.len(), 2);
    assert!(
        recovery
            .kept
            .iter()
            .all(|(_, d)| d.state == DerivedState::RecoveryNeeded && !d.verified)
    );
    assert!(recovery.released.is_empty());
}

#[test]
fn recovery_needed_reservations_count_against_the_budget() {
    let (_dir, state) = state();
    let budget = resources([("cpu", 2)]);
    // Group 500 is alive: its reservation counts although its runner is gone.
    write(&state, &reservation("dddd000000000001", 500, None, 2)).unwrap();
    let need = resources([("cpu", 1)]);
    let reserved_with = |facts: &Fake| {
        let guard = lock(&state, Duration::from_secs(1)).unwrap();
        let recovery = recover(&guard, &state, facts).unwrap();
        super::sum(recovery.kept.iter().map(|(r, _)| &r.resources))
    };
    let alive = Fake {
        processes: [(501, (51, 500))].into_iter().collect(),
    };
    assert!(!super::fits(&budget, &reserved_with(&alive), &need));
    // Once the group is empty, the next pass releases the reservation and the job fits.
    assert!(super::fits(
        &budget,
        &reserved_with(&Fake::default()),
        &need
    ));
    assert!(read(&state, "dddd000000000001").is_none());
}

#[test]
fn dropping_a_job_or_an_unadmitted_admitter_leaves_nothing() {
    let (_dir, state) = state();
    let mut host = HostConfig::defaults();
    host.budget = resources([("cpu", 1)]);
    let draft = reservation("eeee000000000001", 700, None, 2);
    {
        let mut admitter = Admitter::new(&state, &host, "eeee000000000001".into(), None);
        assert!(matches!(admitter.pass(&draft, false).unwrap(), Pass::Full));
        assert!(state.live_lock("eeee000000000001").exists());
        assert!(read(&state, "eeee000000000001").is_none(), "all or nothing");
    }
    assert!(!state.live_lock("eeee000000000001").exists());
    {
        let mut admitter = Admitter::new(&state, &host, "eeee000000000001".into(), None);
        let Pass::Admitted(job) = admitter.pass(&draft, true).unwrap() else {
            panic!("overflow always admits");
        };
        assert_eq!(job.reservation().admission, AdmissionResult::Overflow);
        drop(job);
    }
    assert!(read(&state, "eeee000000000001").is_none());
    assert!(!state.live_lock("eeee000000000001").exists());
}

/// Once `starting` is written the stage may be running: a job dropped then (an unwinding
/// panic) keeps its reservation and live lock file for recovery and only lets go of the lock.
#[test]
fn a_job_dropped_after_the_start_boundary_is_abandoned() {
    let (_dir, state) = state();
    let host = HostConfig::defaults();
    for running in [false, true] {
        let job_id = if running {
            "ffff000000000002"
        } else {
            "ffff000000000001"
        };
        let mut admitter = Admitter::new(&state, &host, job_id.into(), None);
        let Pass::Admitted(mut job) = admitter
            .pass(&reservation(job_id, 701, None, 1), true)
            .unwrap()
        else {
            panic!("overflow always admits");
        };
        drop(admitter);
        job.starting().unwrap();
        if running {
            job.running(ProcessIdentity {
                pid: 702,
                start_s: 11,
            })
            .unwrap();
        }
        drop(job);
        let kept = read(&state, job_id).expect("the reservation stays");
        assert_ne!(kept.state, ReservationState::Reserved, "{running}");
        assert!(state.live_lock(job_id).exists(), "{running}");
        assert!(
            state::try_lock(&state.live_lock(job_id), LockMode::Exclusive)
                .unwrap()
                .is_some(),
            "{running}: the live lock is no longer held"
        );
    }
}

/// A reserved job dropped, or an admitter that never admitted dropped, while another process
/// holds the admission lock deletes nothing: the reservation and the live lock file stay and
/// the live lock is let go, so the next recovery pass releases them.
#[test]
fn drops_without_the_admission_lock_delete_nothing() {
    let (_dir, state) = state();
    let mut host = HostConfig::defaults();
    host.budget = resources([("cpu", 1)]);
    host.admission.lock_timeout_ms = 0;
    let draft = reservation("eeee000000000002", 703, None, 1);
    let mut admitter = Admitter::new(&state, &host, "eeee000000000002".into(), None);
    let Pass::Admitted(job) = admitter.pass(&draft, false).unwrap() else {
        panic!("the budget is free");
    };
    drop(admitter);
    let mut waiting = Admitter::new(&state, &host, "eeee000000000003".into(), None);
    assert!(matches!(waiting.pass(&draft, false).unwrap(), Pass::Full));
    let before = std::fs::read_to_string(state.reservation("eeee000000000002")).unwrap();

    let held = lock(&state, Duration::from_secs(1)).unwrap();
    drop(job);
    drop(waiting);
    assert_eq!(
        std::fs::read_to_string(state.reservation("eeee000000000002")).ok(),
        Some(before),
        "the reserved job's reservation stays"
    );
    for job in ["eeee000000000002", "eeee000000000003"] {
        assert_eq!(
            state::probe(&state.live_lock(job)).unwrap(),
            Probe::Free,
            "{job}: the live lock file stays, free"
        );
    }
    drop(held);
    let recovery = recover(
        &lock(&state, Duration::from_secs(1)).unwrap(),
        &state,
        &Fake::default(),
    )
    .unwrap();
    assert_eq!(recovery.released.len(), 1);
    assert!(read(&state, "eeee000000000002").is_none());
    assert!(!state.live_lock("eeee000000000002").exists());
    assert!(!state.live_lock("eeee000000000003").exists());
}
