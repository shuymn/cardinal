//! Lock liveness against real processes (`docs/architecture.md` section 4.3): the OS drops a
//! lock when its holder dies, and a child of the holder never inherits it, so a surviving
//! child cannot keep a dead runner looking alive.

mod support;

use std::time::Duration;

use cardinal::os;
use cardinal::state::{LockMode, Probe, probe, try_lock};
use cardinal_test_support::Scope;
use cardinal_test_support::process::{PidGuard, wait_for, wait_for_file};
use support::helper::{self, Mode};

#[test]
fn helper() {
    helper::run_if_requested();
}

const WAIT: Duration = Duration::from_secs(10);

#[test]
fn lock_is_released_when_the_holder_dies_and_its_child_does_not_hold_it() {
    let scope = Scope::new();
    let lock = scope.path("live.lock");
    let ready = scope.path("ready");
    let pid_file = scope.path("pid");
    let mut holder = helper::spawn(
        &scope,
        &Mode::Spawn {
            ready: ready.clone(),
            pid_file: pid_file.clone(),
            lock: Some(lock.clone()),
        },
        scope.root(),
    );
    wait_for_file(&ready, WAIT);
    let child: u32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
    let _child = PidGuard::new(child);
    let child_start = wait_for("the holder's child", WAIT, || os::start_time_s(child));

    assert_eq!(probe(&lock).unwrap(), Probe::Held);
    assert!(try_lock(&lock, LockMode::Shared).unwrap().is_none());

    holder.kill();

    assert!(
        os::is_alive(child, child_start),
        "the holder's child outlives it"
    );
    assert_eq!(
        probe(&lock).unwrap(),
        Probe::Free,
        "the lock must be free once its holder died, while the holder's child lives on"
    );
    assert!(try_lock(&lock, LockMode::Exclusive).unwrap().is_some());
}
