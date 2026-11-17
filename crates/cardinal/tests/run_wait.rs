//! The budget wait and the phase 0 overflow (A1, C6, `docs/architecture.md` section 5.4): a
//! wait that reaches the cap starts the stage once over budget, and waiting adds no output.

#![cfg(unix)]

mod runner_support;
mod support;

use std::time::{Duration, Instant};

use cardinal::contract::{AdmissionResult, RecordKind};
use cardinal::testpoint::ADMISSION_RESERVED;
use runner_support::{Env, WAIT, output, spec};
use support::helper::ChildGuard;

#[test]
fn helper() {
    support::helper::run_if_requested();
}

/// A budget of one CPU and a wait cap of `timeout_ms / 2`.
fn env() -> Env {
    Env::new(
        "[budget]\ncpu = 1\n[admission]\ndefault_resources = { cpu = 1 }\n\
         max_wait_ms = 20000\nwait_fraction = 0.5\n",
    )
}

#[test]
fn overflow_runs_once() {
    let env = env();
    let _full = env.hold_budget(1);
    let counter = env.file("counter");
    let script = format!("echo run >> '{}'", counter.display());
    let start = Instant::now();
    let mut runner = ChildGuard::new(
        env.run_at(
            Some(&spec(1, 600)),
            &["sh", "-c", &script],
            &[ADMISSION_RESERVED],
            None,
        )
        .spawn()
        .unwrap(),
    );
    env.points
        .wait_reached(ADMISSION_RESERVED, runner.id(), WAIT);
    let waited = start.elapsed();
    assert!(waited >= Duration::from_millis(300), "{waited:?}");
    let ours: Vec<_> = env
        .reservations()
        .into_iter()
        .filter(|r| r.runner.pid == runner.id())
        .collect();
    assert_eq!(ours.len(), 1);
    assert_eq!(ours[0].admission, AdmissionResult::Overflow);
    env.points.release(ADMISSION_RESERVED);
    assert_eq!(runner.child().wait().unwrap().code(), Some(0));
    assert_eq!(env.lines("counter"), ["run"]);

    let finished = env.records_of(RecordKind::JobFinished);
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].data["admission"], "overflow");
    assert!(finished[0].data["waited_ms"].as_u64().unwrap() >= 300);
    assert_eq!(finished[0].data["measurements"][0]["kind"], "wall_time");
}

/// The stage's stdout, stderr, and status are the same run directly, through the runner
/// within budget, and through the runner after a wait that ends over budget.
#[test]
fn no_output_added() {
    let env = env();
    let stage = [
        "sh",
        "-c",
        "printf 'out\\n'; printf 'err\\n' >&2; printf 'tail'; exit 3",
    ];
    let mut direct = env.isolated(stage[0]);
    direct.args(&stage[1..]);
    let direct = output(&mut direct);
    assert_eq!(direct.status.code(), Some(3));

    let within = output(&mut env.run(Some(&spec(1, 600)), &stage));
    let full = env.hold_budget(1);
    let waited = output(&mut env.run(Some(&spec(1, 600)), &stage));
    drop(full);

    for (name, out) in [("within budget", &within), ("after a wait", &waited)] {
        assert_eq!(out.status.code(), direct.status.code(), "{name}");
        assert_eq!(out.stdout, direct.stdout, "{name}");
        assert_eq!(out.stderr, direct.stderr, "{name}");
    }
    let admissions: Vec<_> = env
        .records_of(RecordKind::JobFinished)
        .iter()
        .map(|r| r.data["admission"].clone())
        .collect();
    assert_eq!(admissions, ["within_budget", "overflow"]);
}
