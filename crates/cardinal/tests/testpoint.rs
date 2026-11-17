//! Test points across processes (`docs/architecture.md` section 13.1): a paused process
//! waits for its release file, and an injected failure is returned at its point.

mod support;

use std::time::Duration;

use cardinal::testpoint::RUN_BEFORE_STARTING;
use support::helper::{self, ChildGuard, Mode};
use support::testpoints::TestPoints;

#[test]
fn helper() {
    helper::run_if_requested();
}

const WAIT: Duration = Duration::from_secs(10);

fn start(points: &TestPoints, pause: &[&str], fail: Option<&str>) -> ChildGuard {
    let mut command = helper::command(&Mode::Testpoint {
        point: RUN_BEFORE_STARTING.into(),
    });
    points.apply(&mut command, pause, fail);
    ChildGuard::new(command.spawn().unwrap())
}

#[test]
fn paused_process_waits_for_its_release() {
    let points = TestPoints::new();
    let mut child = start(&points, &["admission.waiting", RUN_BEFORE_STARTING], None);
    points.wait_reached(RUN_BEFORE_STARTING, child.id(), WAIT);
    std::thread::sleep(Duration::from_millis(100));
    assert!(child.child().try_wait().unwrap().is_none(), "still paused");
    points.release(RUN_BEFORE_STARTING);
    let status = child.child().wait().unwrap();
    assert_eq!(status.code(), Some(0));
}

#[test]
fn injected_failure_is_returned_at_its_point() {
    let points = TestPoints::new();
    let mut child = start(&points, &[], Some(RUN_BEFORE_STARTING));
    assert_eq!(child.child().wait().unwrap().code(), Some(3));
    assert!(
        !points.reached(RUN_BEFORE_STARTING, child.id()),
        "no pause requested"
    );
}

#[test]
fn other_points_pass_through() {
    let points = TestPoints::new();
    let mut child = start(&points, &["act.locked"], Some("act.checked"));
    assert_eq!(child.child().wait().unwrap().code(), Some(0));
}

#[test]
fn without_a_test_dir_nothing_happens() {
    let mut command = helper::command(&Mode::Testpoint {
        point: RUN_BEFORE_STARTING.into(),
    });
    command
        .env_remove("CARDINAL_TEST_DIR")
        .env("CARDINAL_TEST_PAUSE", RUN_BEFORE_STARTING)
        .env("CARDINAL_TEST_FAIL", RUN_BEFORE_STARTING);
    let mut child = ChildGuard::new(command.spawn().unwrap());
    assert_eq!(child.child().wait().unwrap().code(), Some(0));
}
