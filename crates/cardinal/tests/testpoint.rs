//! Test points across processes (`docs/architecture.md` section 13.1): a paused process
//! waits for its release file, and an injected failure is returned at its point.

mod support;

use std::time::Duration;

use cardinal::testpoint::RUN_BEFORE_STARTING;
use cardinal_test_support::{Child, Scope};
use support::helper::{self, Mode};
use support::testpoints::TestPoints;

#[test]
fn helper() {
    helper::run_if_requested();
}

const WAIT: Duration = Duration::from_secs(10);

fn start(scope: &Scope, points: &TestPoints, pause: &[&str], fail: Option<&str>) -> Child {
    let mut command = helper::command(
        scope,
        &Mode::Testpoint {
            point: RUN_BEFORE_STARTING.into(),
        },
        scope.root(),
    );
    points.apply(&mut command, pause, fail);
    command.spawn().unwrap()
}

#[test]
fn paused_process_waits_for_its_release() {
    let scope = Scope::new();
    let points = TestPoints::new(&scope);
    let mut child = start(
        &scope,
        &points,
        &["admission.waiting", RUN_BEFORE_STARTING],
        None,
    );
    points.wait_reached(RUN_BEFORE_STARTING, child.id(), WAIT);
    std::thread::sleep(Duration::from_millis(100));
    assert!(child.try_wait().unwrap().is_none(), "still paused");
    points.release(RUN_BEFORE_STARTING);
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0));
}

#[test]
fn injected_failure_is_returned_at_its_point() {
    let scope = Scope::new();
    let points = TestPoints::new(&scope);
    let mut child = start(&scope, &points, &[], Some(RUN_BEFORE_STARTING));
    assert_eq!(child.wait().unwrap().code(), Some(3));
    assert!(
        !points.reached(RUN_BEFORE_STARTING, child.id()),
        "no pause requested"
    );
}

#[test]
fn other_points_pass_through() {
    let scope = Scope::new();
    let points = TestPoints::new(&scope);
    let mut child = start(&scope, &points, &["act.locked"], Some("act.checked"));
    assert_eq!(child.wait().unwrap().code(), Some(0));
}

#[test]
fn without_a_test_dir_nothing_happens() {
    let scope = Scope::new();
    let mut command = helper::command(
        &scope,
        &Mode::Testpoint {
            point: RUN_BEFORE_STARTING.into(),
        },
        scope.root(),
    );
    command
        .env("CARDINAL_TEST_PAUSE", RUN_BEFORE_STARTING)
        .env("CARDINAL_TEST_FAIL", RUN_BEFORE_STARTING);
    let mut child = command.spawn().unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(0));
}
