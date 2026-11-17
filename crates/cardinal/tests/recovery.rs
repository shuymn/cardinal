//! Recovery after a runner crash (C8, A4, `docs/architecture.md` sections 4.5 and 13.2): a
//! dead runner's reservation and in-use state count until its process group is empty or the
//! boot changed, and recovery never signals a group, verified or not.
//!
//! Real process groups cover what a system produces on demand; a fake process table covers a
//! reused PGID and a child that left its descendants behind.

mod runner_support;
mod support;

use cardinal::admission::{Derived, ProcessFacts, derive};
use cardinal::contract::{DerivedState, GroupMember, ProcessIdentity, Reservation};
use cardinal::state::Probe;
use runner_support::fake_reservation;

#[test]
fn helper() {
    support::helper::run_if_requested();
}

/// A fake process table of `(pid, start_s, pgid)`.
struct Table(Vec<(u32, u64, u32)>);

impl ProcessFacts for Table {
    fn boot_s(&self) -> Option<u64> {
        cardinal::os::boot_time_s().ok()
    }
    fn group_exists(&self, pgid: u32) -> bool {
        self.0.iter().any(|&(_, _, g)| g == pgid)
    }
    fn start_s(&self, pid: u32) -> Option<u64> {
        self.0.iter().find(|p| p.0 == pid).map(|p| p.1)
    }
    fn pgid(&self, pid: u32) -> Option<u32> {
        self.0.iter().find(|p| p.0 == pid).map(|p| p.2)
    }
    fn members(&self, pgid: u32) -> Vec<GroupMember> {
        self.0
            .iter()
            .filter(|p| p.2 == pgid)
            .map(|&(pid, start_s, _)| GroupMember {
                pid,
                start_s,
                name: String::new(),
            })
            .collect()
    }
}

/// A crashed job: runner 100 (start 1) led group 100; its child was 101 (start 2).
fn crashed() -> Reservation {
    fake_reservation(
        "0000000000000001",
        1,
        100,
        Some(ProcessIdentity {
            pid: 101,
            start_s: 2,
        }),
    )
}

fn derived(table: &Table) -> Derived {
    derive(&crashed(), Some(Probe::Free), table)
}

#[test]
fn reused_pgid_with_a_new_leader_is_released() {
    // PID 100 leads a group again with another start time: the job's group emptied first.
    let table = Table(vec![(100, 50, 100), (120, 51, 100)]);
    assert_eq!(derived(&table).state, DerivedState::Stale);
}

#[test]
fn reused_pgid_whose_leader_exited_is_kept_and_never_signalled() {
    // Group 100 has a member, but neither the leader nor the recorded child: the ID may have
    // been reused by an unrelated group whose leader exited.
    let table = Table(vec![(130, 60, 100)]);
    assert_eq!(
        derived(&table),
        Derived {
            state: DerivedState::RecoveryNeeded,
            verified: false
        }
    );
}

#[test]
fn child_that_left_descendants_is_kept_and_never_signalled() {
    let table = Table(vec![(102, 3, 100)]);
    assert_eq!(
        derived(&table),
        Derived {
            state: DerivedState::RecoveryNeeded,
            verified: false
        }
    );
    // The recorded child alive in its group verifies it; alive elsewhere does not.
    let verified = Table(vec![(101, 2, 100), (102, 3, 100)]);
    assert!(derived(&verified).verified);
    let moved = Table(vec![(101, 2, 999), (102, 3, 100)]);
    assert!(!derived(&moved).verified);
    let reused_child_pid = Table(vec![(101, 9, 100)]);
    assert!(!derived(&reused_child_pid).verified);
}

#[test]
fn liveness_comes_first_and_an_unprobed_lock_counts_as_held() {
    let table = Table(vec![]);
    for probe in [Some(Probe::Held), None] {
        assert_eq!(derive(&crashed(), probe, &table).state, DerivedState::Live);
    }
    assert_eq!(
        derive(&crashed(), Some(Probe::Missing), &table).state,
        DerivedState::Stale
    );
}

#[test]
fn previous_boot_is_stale_even_with_a_live_group() {
    let mut reservation = crashed();
    reservation.boot_s -= 1000;
    let table = Table(vec![(100, 1, 100), (101, 2, 100)]);
    assert_eq!(
        derive(&reservation, Some(Probe::Free), &table).state,
        DerivedState::Stale
    );
    reservation.boot_s += 998;
    assert_eq!(
        derive(&reservation, Some(Probe::Free), &table).state,
        DerivedState::RecoveryNeeded,
        "a 2 s boot-time difference is the same boot"
    );
}

#[cfg(unix)]
mod real {
    use std::time::{Duration, Instant};

    use cardinal::admission::{in_use, list_reservations, recovery_pass};
    use cardinal::config::HostConfig;
    use cardinal::contract::{DerivedState, ProcessIdentity, RecordKind};
    use cardinal::testpoint::{RUN_AFTER_SPAWN, RUN_BEFORE_STARTING};

    use cardinal_test_support::process::{GroupGuard, wait_for, wait_for_file};

    use crate::runner_support::{Env, WAIT, fake_reservation, output, wait_gone};
    use crate::support::helper::{self, Mode};

    fn host() -> HostConfig {
        HostConfig::defaults()
    }

    /// Section 13.2: a runner killed while its stage runs leaves a verified recovery-needed
    /// reservation that keeps its worktree in use and its budget taken. Recovery passes leave
    /// the group running; once the group is empty, the next admission releases the
    /// reservation and admits the next job.
    #[test]
    fn crashed_runner_group_is_kept_until_empty() {
        let env = Env::with_budget(1);
        let mut runner = env
            .run_at(
                None,
                &["sh", "-c", "sleep 30 & sleep 30 & wait"],
                &[RUN_AFTER_SPAWN],
                None,
            )
            .spawn()
            .unwrap();
        env.points.wait_reached(RUN_AFTER_SPAWN, runner.id(), WAIT);
        let pgid = runner.id();
        let group = GroupGuard(pgid);
        let job = env.reservations()[0].job_id.clone();
        wait_for("both sleeps in the group", WAIT, || {
            let members = list_reservations(&env.state()).ok()?[0].members.len();
            (members >= 4).then_some(())
        });
        runner.kill();

        let views = list_reservations(&env.state()).unwrap();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].derived, DerivedState::RecoveryNeeded);
        assert!(views[0].verified);
        assert_eq!(views[0].members.len(), 3, "sh and two sleeps");
        let busy = in_use(&env.state(), &env.worktree_key()).unwrap();
        assert!(busy.value);
        assert_eq!(busy.jobs, std::slice::from_ref(&job));
        assert!(
            env.worktree_lock_free(),
            "the OS dropped the dead runner's shared lock; the reservation keeps it in use"
        );

        let members: Vec<(u32, u64)> = views[0]
            .members
            .iter()
            .map(|m| (m.pid, m.start_s))
            .collect();
        for _ in 0..2 {
            let report = recovery_pass(&env.state(), &host()).unwrap();
            assert_eq!(report.released, Vec::<String>::new());
            assert_eq!(report.reservations[0].derived, DerivedState::RecoveryNeeded);
        }
        std::thread::sleep(Duration::from_millis(200));
        for &(pid, start_s) in &members {
            assert!(
                cardinal::os::is_alive(pid, start_s),
                "{pid} was not signalled"
            );
        }
        assert_eq!(env.records_of(RecordKind::JobRecovered), []);

        drop(group);
        let out = output(&mut env.run(None, &["sh", "-c", "echo next"]));
        assert_eq!(out.status.code(), Some(0));
        assert_eq!(out.stdout, b"next\n");
        let recovered: Vec<(String, String)> = env
            .records_of(RecordKind::JobRecovered)
            .iter()
            .map(|r| {
                (
                    r.job_id.clone().unwrap(),
                    r.data["action"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(recovered, [(job, "released".into())]);
        let next = env.records_of(RecordKind::JobFinished);
        assert_eq!(next[0].data["admission"], "within_budget");
        assert_eq!(env.reservations(), []);
        assert!(!in_use(&env.state(), &env.worktree_key()).unwrap().value);
    }

    /// A runner killed before the spawn led a group of itself alone: its reservation is stale.
    #[test]
    fn runner_killed_before_starting_leaves_a_stale_reservation() {
        let env = Env::with_budget(1);
        let mut runner = env
            .run_at(None, &["true"], &[RUN_BEFORE_STARTING], None)
            .spawn()
            .unwrap();
        env.points
            .wait_reached(RUN_BEFORE_STARTING, runner.id(), WAIT);
        runner.kill();
        let job = env.reservations()[0].job_id.clone();
        assert_eq!(
            list_reservations(&env.state()).unwrap()[0].derived,
            DerivedState::Stale
        );
        let report = recovery_pass(&env.state(), &host()).unwrap();
        assert_eq!(report.released, [job]);
        assert_eq!(env.reservations(), []);
        assert_eq!(env.live_locks(), Vec::<String>::new());
    }

    /// The child exited and left a descendant in the group: the group is not verified, it is
    /// never signalled, and the reservation counts until the group is empty.
    #[test]
    fn unverified_group_not_signalled() {
        let env = Env::with_budget(1);
        let pids = env.file("pids");
        let script = format!("sleep 30 & echo $! > '{}'; sleep 0.3", pids.display());
        let mut runner = env
            .run_at(None, &["sh", "-c", &script], &[RUN_AFTER_SPAWN], None)
            .spawn()
            .unwrap();
        env.points.wait_reached(RUN_AFTER_SPAWN, runner.id(), WAIT);
        let pgid = runner.id();
        let group = GroupGuard(pgid);
        let child = env.reservations()[0].child.unwrap();
        runner.kill();
        wait_gone(child.pid, child.start_s);
        wait_for_file(&pids, WAIT);
        let descendant: u32 = wait_for("the descendant's PID", WAIT, || {
            std::fs::read_to_string(&pids).ok()?.trim().parse().ok()
        });
        let descendant_start = cardinal::os::start_time_s(descendant).unwrap();

        for _ in 0..2 {
            let report = recovery_pass(&env.state(), &host()).unwrap();
            assert_eq!(report.released, Vec::<String>::new());
            assert_eq!(report.reservations.len(), 1);
            assert_eq!(report.reservations[0].derived, DerivedState::RecoveryNeeded);
            assert!(!report.reservations[0].verified);
            assert_eq!(
                report.reservations[0]
                    .members
                    .iter()
                    .map(|m| m.pid)
                    .collect::<Vec<_>>(),
                [descendant]
            );
        }
        assert!(cardinal::os::is_alive(descendant, descendant_start));

        drop(group);
        wait_gone(descendant, descendant_start);
        let report = recovery_pass(&env.state(), &host()).unwrap();
        assert_eq!(report.released.len(), 1, "released once the group is empty");
        assert_eq!(env.reservations(), []);
    }

    /// A stage whose direct child exits while a descendant stays in its group: the runner
    /// returns the child's status at once, and keeps the reservation, and so the budget and the
    /// worktree, until the descendant has ended. Nothing signals the descendant.
    #[test]
    fn descendant_keeps_the_reservation_after_the_child_exits() {
        let env = Env::with_budget(1);
        let pids = env.file("pids");
        let script = format!(
            "sleep 30 </dev/null >/dev/null 2>&1 & echo $! > '{}'",
            pids.display()
        );
        let start = Instant::now();
        let out = output(&mut env.run(None, &["sh", "-c", &script]));
        assert_eq!(out.status.code(), Some(0));
        assert!(start.elapsed() < Duration::from_secs(5), "returned at once");
        let pgid: u32 = env.records_of(RecordKind::JobStarted)[0].data["pgid"]
            .as_u64()
            .unwrap()
            .try_into()
            .unwrap();
        let group = GroupGuard(pgid);
        let descendant: u32 = std::fs::read_to_string(&pids)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let descendant_start = cardinal::os::start_time_s(descendant).unwrap();
        let job = env.records_of(RecordKind::JobFinished)[0]
            .job_id
            .clone()
            .unwrap();

        let report = recovery_pass(&env.state(), &host()).unwrap();
        assert_eq!(report.released, Vec::<String>::new());
        assert_eq!(report.reservations.len(), 1);
        assert_eq!(report.reservations[0].reservation.job_id, job);
        assert_eq!(report.reservations[0].derived, DerivedState::RecoveryNeeded);
        assert!(in_use(&env.state(), &env.worktree_key()).unwrap().value);
        assert!(cardinal::os::is_alive(descendant, descendant_start));

        drop(group);
        wait_gone(descendant, descendant_start);
        let report = recovery_pass(&env.state(), &host()).unwrap();
        assert_eq!(report.released, [job]);
        assert_eq!(env.reservations(), []);
    }

    /// A reservation from an earlier boot is released without signalling its group, even
    /// when a live group with the recorded ID and child exists now.
    #[test]
    fn previous_boot_released() {
        let env = Env::with_budget(1);
        let mut process = helper::spawn(env.scope(), &Mode::Sleep, &env.worktree());
        let pid = process.id();
        let start_s = wait_for("the helper's start time", WAIT, || {
            cardinal::os::start_time_s(pid)
        });
        let mut reservation = fake_reservation(
            "00000000000000b0",
            1,
            pid,
            Some(ProcessIdentity { pid, start_s }),
        );
        reservation.runner.start_s = start_s;
        reservation.group = cardinal::contract::GroupIdentity::Pgid {
            pgid: pid,
            leader_start_s: start_s,
        };
        reservation.boot_s -= 1000;
        let state = env.state();
        std::fs::write(
            state.reservation(&reservation.job_id),
            serde_json::to_vec(&reservation).unwrap(),
        )
        .unwrap();

        let report = recovery_pass(&state, &host()).unwrap();
        assert_eq!(report.released, [reservation.job_id.clone()]);
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            process.try_wait().unwrap().is_none(),
            "the group was never signalled"
        );
        assert!(cardinal::os::is_alive(pid, start_s));
    }
}
