//! The child primitives of `cardinal::os::unix` (`docs/architecture.md` section 5.5): a wait
//! that does not reap tells a stopped child from an ended one on every platform, takes a stop
//! report only when asked to, and leaves the child to be reaped.

#![cfg(unix)]

mod runner_support;
mod support;

use std::time::Duration;

use cardinal::os::unix::{
    ChildState, SIGCONT, SIGSTOP, Until, signal_process, take_stop, wait_unreaped,
};
use runner_support::Env;
use support::helper::ChildGuard;

#[test]
fn a_stopped_child_is_not_ended_and_stays_unreaped() {
    let env = Env::with_budget(1);
    let mut child = ChildGuard::new(
        env.isolated("sh")
            .args(["-c", "kill -STOP $$; exit 3"])
            .spawn()
            .unwrap(),
    );
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
    assert_eq!(child.child().wait().unwrap().code(), Some(3));
}
