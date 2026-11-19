//! The child primitives of `cardinal::os::unix` (`docs/architecture.md` section 5.5): a wait
//! that does not reap tells a stopped child from an ended one on every platform, takes a stop
//! report only when asked to, and leaves the child to be reaped; a group `kill` still reports
//! while no live member is left counts as empty once `kill` stops reporting it.

#![cfg(unix)]

mod runner_support;
mod support;

use std::time::{Duration, Instant};

use cardinal::os::unix::{
    ChildState, EMPTYING, SIGCONT, SIGSTOP, Until, group_emptied, group_exists, signal_process,
    take_stop, wait_unreaped,
};
use runner_support::{Env, WAIT};

#[test]
fn a_stopped_child_is_not_ended_and_stays_unreaped() {
    let env = Env::with_budget(1);
    let mut child = env
        .in_worktree("sh")
        .args(["-c", "kill -STOP $$; exit 3"])
        .spawn()
        .unwrap();
    let pid = child.id();
    assert_eq!(
        wait_unreaped(pid, Until::EndedOrStopped).unwrap(),
        ChildState::Stopped(SIGSTOP)
    );
    assert_eq!(
        wait_unreaped(pid, Until::Now).unwrap(),
        ChildState::Stopped(SIGSTOP),
        "the report stays"
    );

    // macOS reports the pending stop to a wait for exits too; it must not end the wait.
    let waiter = std::thread::spawn(move || wait_unreaped(pid, Until::Ended).unwrap());
    std::thread::sleep(Duration::from_millis(300));
    assert!(!waiter.is_finished(), "a stopped child has not ended");

    assert_eq!(take_stop(pid).unwrap(), Some(SIGSTOP));
    assert_eq!(take_stop(pid).unwrap(), None, "a stop is reported once");
    assert_eq!(wait_unreaped(pid, Until::Now).unwrap(), ChildState::Running);

    signal_process(pid, SIGCONT).unwrap();
    assert_eq!(waiter.join().unwrap(), ChildState::Ended);
    assert_eq!(
        wait_unreaped(pid, Until::Now).unwrap(),
        ChildState::Ended,
        "still unreaped"
    );
    assert_eq!(child.wait().unwrap().code(), Some(3));
}

/// A group whose only member has exited and is not reaped yet: `kill(-pgid, 0)` still reports
/// it (`EPERM` on macOS), as macOS does for a moment after a runner reaped its stage, while the
/// process table lists no live member. Asked once, the group is not empty; asked until a
/// deadline the reap falls well within, it is empty once `kill` stops reporting it. The reap
/// comes only after `group_emptied` found the group reported, and the deadline, not the
/// reaper's timing, bounds the wait.
#[test]
fn a_group_without_a_live_member_is_empty_once_kill_says_so() {
    let env = Env::with_budget(1);
    let mut child = env
        .in_worktree("sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    let pgid = child.id();
    assert_eq!(
        wait_unreaped(pgid, Until::Ended).unwrap(),
        ChildState::Ended
    );
    assert!(
        group_exists(pgid),
        "kill still reports the group of a zombie"
    );
    assert!(
        !group_emptied(pgid, Instant::now()),
        "asked once while the zombie is unreaped"
    );
    let reaper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        child.wait().unwrap()
    });
    assert!(
        group_emptied(pgid, Instant::now() + WAIT),
        "empty once the zombie is reaped"
    );
    assert!(reaper.join().unwrap().success());
}

/// The wait is bounded and never releases a group `kill` still reports: a zombie nobody reaps
/// keeps its group non-empty until the deadline, and a live member ends the wait at once.
#[test]
fn a_group_kill_still_reports_is_not_empty() {
    let env = Env::with_budget(1);
    let mut zombie = env
        .in_worktree("sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    assert_eq!(
        wait_unreaped(zombie.id(), Until::Ended).unwrap(),
        ChildState::Ended
    );
    assert!(
        !group_emptied(zombie.id(), Instant::now() + EMPTYING),
        "an unreaped zombie keeps its group"
    );
    assert!(zombie.wait().unwrap().success());

    let mut live = env.in_worktree("sleep").arg("30").spawn().unwrap();
    let asked = Instant::now();
    assert!(
        !group_emptied(live.id(), asked + WAIT),
        "a live member keeps its group"
    );
    assert!(
        asked.elapsed() < WAIT / 2,
        "without waiting for the deadline"
    );
    live.kill();
}
