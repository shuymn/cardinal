//! Admission under contention (C2, `docs/architecture.md` sections 4.4 and 13.1): decided
//! inside the admission lock from live and recovery-needed reservations against the budgets,
//! all resources or none. Every contended test waits for the second runner to reach its
//! contended point before releasing the first.

#![cfg(unix)]

mod runner_support;
mod support;

use cardinal::contract::{AdmissionResult, RecordKind};
use cardinal::testpoint::{ADMISSION_RESERVED, ADMISSION_WAITING};
use runner_support::{Env, WAIT, output, spec};

#[test]
fn helper() {
    support::helper::run_if_requested();
}

/// A stage that logs its start and end to `log` with a pause between them.
fn logging_stage(env: &Env, name: &str) -> [String; 3] {
    let log = env.file("log");
    [
        "sh".into(),
        "-c".into(),
        format!(
            "echo {name}-start >> '{log}'; sleep 0.2; echo {name}-end >> '{log}'",
            log = log.display()
        ),
    ]
}

fn args(stage: &[String]) -> Vec<&str> {
    stage.iter().map(String::as_str).collect()
}

/// Budget of one job: the second runner waits until the first one's reservation is gone.
#[test]
fn second_job_waits_until_the_first_is_released() {
    let env = Env::with_budget(1);
    let first_stage = logging_stage(&env, "a");
    let second_stage = logging_stage(&env, "b");
    let mut first = env
        .run_at(None, &args(&first_stage), &[ADMISSION_RESERVED], None)
        .spawn()
        .unwrap();
    env.points
        .wait_reached(ADMISSION_RESERVED, first.id(), WAIT);
    let mut second = env
        .run_at(None, &args(&second_stage), &[ADMISSION_WAITING], None)
        .spawn()
        .unwrap();
    env.points
        .wait_reached(ADMISSION_WAITING, second.id(), WAIT);
    let reserved: Vec<u32> = env.reservations().iter().map(|r| r.runner.pid).collect();
    assert_eq!(reserved, [first.id()], "the waiting job reserves nothing");

    env.points.release(ADMISSION_RESERVED);
    assert_eq!(first.wait().unwrap().code(), Some(0));
    env.points.release(ADMISSION_WAITING);
    assert_eq!(second.wait().unwrap().code(), Some(0));

    assert_eq!(env.lines("log"), ["a-start", "a-end", "b-start", "b-end"]);
    let finished = env.records_of(RecordKind::JobFinished);
    assert_eq!(finished.len(), 2);
    assert!(
        finished
            .iter()
            .all(|r| r.data["admission"] == "within_budget")
    );
    assert_eq!(env.reservations(), []);
    assert_eq!(env.live_locks(), Vec::<String>::new());
}

/// A job gets all of its resources or none: one full resource makes it wait although the other
/// one fits, and it reserves nothing while it waits.
#[test]
fn all_resources_or_none() {
    let env = Env::new(
        "[budget]\ncpu = 4\nmemory_mb = 100\n[admission]\ndefault_resources = { cpu = 1 }\n\
         max_wait_ms = 20000\n",
    );
    let mut full = env.hold_budget(1);
    {
        // Fill memory with the held reservation.
        let state = env.state();
        let path = state.reservation(&full.job);
        let mut reservation: cardinal::contract::Reservation =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        reservation.resources = cardinal::admission::resources([("cpu", 1), ("memory_mb", 100)]);
        std::fs::write(&path, serde_json::to_vec(&reservation).unwrap()).unwrap();
    }
    let mut need = spec(1, 60_000);
    need.resources = cardinal::admission::resources([("cpu", 1), ("memory_mb", 1)]);
    let mut runner = env
        .run_at(Some(&need), &["true"], &[ADMISSION_WAITING], None)
        .spawn()
        .unwrap();
    env.points
        .wait_reached(ADMISSION_WAITING, runner.id(), WAIT);
    assert_eq!(
        env.reservations().len(),
        1,
        "only the held reservation exists while the job waits"
    );
    full.release();
    env.points.release(ADMISSION_WAITING);
    assert_eq!(runner.wait().unwrap().code(), Some(0));
    let admitted = env.records_of(RecordKind::JobAdmitted);
    assert_eq!(admitted.len(), 1);
    assert_eq!(admitted[0].data["admission"], "within_budget");
    assert_eq!(
        admitted[0].data["resources"],
        serde_json::json!({"cpu": 1, "memory_mb": 1})
    );
    assert_eq!(
        env.reservations(),
        [],
        "the stale held reservation was released"
    );
}

/// Runners started together never run more jobs at once than the budget allows.
#[test]
fn concurrent_runners_stay_within_the_budget() {
    let env = Env::with_budget(2);
    let running = env.file("running");
    std::fs::create_dir(&running).unwrap();
    let script = format!(
        "mkdir '{dir}'/$$; ls '{dir}' | wc -l >> '{max}'; sleep 0.3; rmdir '{dir}'/$$",
        dir = running.display(),
        max = env.file("seen").display()
    );
    let mut runners: Vec<cardinal_test_support::Child> = (0..5)
        .map(|_| env.run(None, &["sh", "-c", &script]).spawn().unwrap())
        .collect();
    for runner in &mut runners {
        assert_eq!(runner.wait().unwrap().code(), Some(0));
    }
    let seen: Vec<u32> = env
        .lines("seen")
        .iter()
        .map(|l| l.trim().parse().unwrap())
        .collect();
    assert_eq!(seen.len(), 5);
    assert!(seen.iter().all(|&n| n <= 2), "{seen:?}");
    let overflow = env
        .records_of(RecordKind::JobFinished)
        .iter()
        .filter(|r| r.data["admission"] == "overflow")
        .count();
    assert_eq!(overflow, 0);
}

/// Without a spec, `admission.default_resources` applies.
#[test]
fn default_resources_apply_without_a_spec() {
    let env =
        Env::new("[budget]\ncpu = 8\n[admission]\ndefault_resources = { cpu = 3, gpu = 1 }\n");
    let mut runner = env
        .run_at(None, &["true"], &[ADMISSION_RESERVED], None)
        .spawn()
        .unwrap();
    env.points
        .wait_reached(ADMISSION_RESERVED, runner.id(), WAIT);
    let reservation = &env.reservations()[0];
    assert_eq!(
        reservation.resources,
        cardinal::admission::resources([("cpu", 3), ("gpu", 1)])
    );
    assert_eq!(reservation.admission, AdmissionResult::WithinBudget);
    env.points.release(ADMISSION_RESERVED);
    assert_eq!(runner.wait().unwrap().code(), Some(0));
    let out = output(&mut env.run(Some(&spec(2, 1000)), &["true"]));
    assert_eq!(out.status.code(), Some(0));
}
