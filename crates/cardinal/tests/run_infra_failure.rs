//! Failures after the start boundary (C5, `docs/architecture.md` section 5.6): the stage is
//! never run again; the runner reports an infrastructure failure, returns the stage's own
//! status when it knows it, and otherwise exits 125 with one line on stderr.

mod runner_support;
mod support;

#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use cardinal::contract::{RecordKind, ReservationState};
#[cfg(unix)]
use cardinal::state::LockMode;
#[cfg(unix)]
use cardinal::testpoint::{RECORD_OPENED, RUN_AFTER_SPAWN, RUN_REAPED, RUN_SPAWNED, RUN_SPAWNING};
#[cfg(unix)]
use runner_support::{Env, GroupGuard, WAIT, output};
#[cfg(unix)]
use support::helper::{ChildGuard, wait_for};

#[test]
fn helper() {
    support::helper::run_if_requested();
}

#[cfg(unix)]
fn stage(env: &Env, exit: u8) -> [String; 3] {
    [
        "sh".into(),
        "-c".into(),
        format!(
            "echo run >> '{}'; printf out; printf err >&2; exit {exit}",
            env.file("counter").display()
        ),
    ]
}

#[cfg(unix)]
fn infra_reasons(env: &Env) -> Vec<String> {
    env.records_of(RecordKind::JobInfraFailure)
        .iter()
        .map(|r| r.data["reason"].as_str().unwrap().to_owned())
        .collect()
}

/// A runner error after the spawn is recorded; the stage's status is still known and returned,
/// and nothing is added to its output.
#[cfg(unix)]
#[test]
fn failure_after_spawn_keeps_the_stage_status_and_never_reruns() {
    let env = Env::with_budget(4);
    let stage = stage(&env, 7);
    let args: Vec<&str> = stage.iter().map(String::as_str).collect();
    let out = output(&mut env.run_at(None, &args, &[], Some(cardinal::testpoint::RUN_AFTER_SPAWN)));
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(out.stdout, b"out");
    assert_eq!(
        out.stderr, b"err",
        "no infrastructure line: the status is known"
    );
    assert_eq!(env.lines("counter"), ["run"]);
    assert_eq!(infra_reasons(&env), ["injected failure at run.after_spawn"]);
    assert_eq!(env.records_of(RecordKind::JobFailOpen), []);
    assert_eq!(env.reservations(), []);
}

/// A panic before the spawn ends in fail-open: the stage runs once, with nothing added to its
/// output (section 5.3).
#[cfg(unix)]
#[test]
fn panic_before_the_spawn_runs_the_stage_once() {
    let env = Env::with_budget(4);
    let stage = stage(&env, 7);
    let args: Vec<&str> = stage.iter().map(String::as_str).collect();
    let out = output(
        env.run_at(None, &args, &[], None)
            .env("CARDINAL_TEST_PANIC", RUN_SPAWNING),
    );
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(out.stdout, b"out");
    assert_eq!(out.stderr, b"err");
    assert_eq!(env.lines("counter"), ["run"]);
    let fail_open = env.records_of(RecordKind::JobFailOpen);
    assert_eq!(fail_open.len(), 1);
    assert_eq!(fail_open[0].data["reason"], "panic");
    let detail = fail_open[0].data["detail"].as_str().unwrap();
    assert!(
        detail.contains("injected panic at run.spawning"),
        "{detail}"
    );
    assert_eq!(env.reservations(), []);
}

/// A panic once the spawn returned, before anything about the child is read or later: the
/// stage is not run again, and the runner waits for it and returns its status.
#[cfg(unix)]
#[test]
fn panic_after_the_spawn_returns_the_stage_status() {
    for point in [RUN_SPAWNED, RUN_AFTER_SPAWN] {
        let env = Env::with_budget(4);
        let stage = stage(&env, 7);
        let args: Vec<&str> = stage.iter().map(String::as_str).collect();
        let out = output(
            env.run_at(None, &args, &[], None)
                .env("CARDINAL_TEST_PANIC", point),
        );
        assert_eq!(out.status.code(), Some(7), "{point}");
        assert_eq!(out.stdout, b"out", "{point}");
        assert_eq!(out.stderr, b"err", "{point}: the status is known");
        assert_eq!(env.lines("counter"), ["run"], "{point}");
        assert_eq!(env.records_of(RecordKind::JobFailOpen), [], "{point}");
        let reasons = infra_reasons(&env);
        assert_eq!(reasons.len(), 1, "{point}: {reasons:?}");
        assert!(
            reasons[0].contains(&format!("injected panic at {point}")),
            "{reasons:?}"
        );
        assert_eq!(env.records_of(RecordKind::JobFinished)[0].data["exit"], 7);
        assert_eq!(env.reservations(), [], "{point}");
    }
}

/// A panic in a record append once the stage has started, and the same panic again while the
/// runner recovers from it: the reservation stays while the stage runs, the runner waits for
/// the stage and returns its status, and only the records are lost (section 4.7).
#[cfg(unix)]
#[test]
fn a_panic_while_recording_after_the_spawn_keeps_the_reservation() {
    let env = Env::with_budget(4);
    let go = env.file("go");
    let script = format!(
        "echo run >> '{}'; while [ ! -e '{}' ]; do sleep 0.05; done; exit 7",
        env.file("counter").display(),
        go.display()
    );
    // Every append before the spawn gives up on the held lock, so the first append that opens
    // the records file is the first one after the spawn.
    let records = cardinal::state::lock(
        &env.state().records_lock(),
        LockMode::Exclusive,
        Instant::now() + WAIT,
    )
    .unwrap();
    let mut command = env.run_at(None, &["sh", "-c", &script], &[RUN_SPAWNED], None);
    command.env("CARDINAL_TEST_PANIC", RECORD_OPENED);
    let mut runner = ChildGuard::new(command.spawn().unwrap());
    let _group = GroupGuard(runner.id());
    env.wait_any_reached(RUN_SPAWNED);
    drop(records);
    env.points.release(RUN_SPAWNED);

    wait_for(
        "the reservation at running, or the runner's exit",
        WAIT,
        || {
            let running = env
                .reservations()
                .first()
                .is_some_and(|r| r.state == ReservationState::Running);
            (running || runner.child().try_wait().unwrap().is_some()).then_some(())
        },
    );
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        runner.child().try_wait().unwrap(),
        None,
        "the runner waits for the stage"
    );
    assert_eq!(env.reservations().len(), 1, "kept while the stage runs");

    std::fs::write(&go, b"").unwrap();
    let status = wait_for("the runner to exit", WAIT, || {
        runner.child().try_wait().unwrap()
    });
    assert_eq!(status.code(), Some(7));
    assert_eq!(env.lines("counter"), ["run"]);
    assert_eq!(env.reservations(), []);
    assert_eq!(env.records(), [], "every append panicked");
}

/// A recorder that panics on every append loses the records, never the run: the stage runs
/// once on its own path and its status is returned.
#[cfg(unix)]
#[test]
fn a_recorder_that_always_panics_loses_only_records() {
    let env = Env::with_budget(4);
    let stage = stage(&env, 7);
    let args: Vec<&str> = stage.iter().map(String::as_str).collect();
    let out = output(
        env.run_at(None, &args, &[], None)
            .env("CARDINAL_TEST_PANIC", RECORD_OPENED),
    );
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(out.stdout, b"out");
    assert_eq!(out.stderr, b"err");
    assert_eq!(env.lines("counter"), ["run"]);
    assert_eq!(env.records(), []);
    assert_eq!(env.reservations(), []);
}

/// A panic before the spawn, then another in the fail-open that follows once it has started
/// the stage: recovery is entered again, waits for the stage, and returns its status.
#[cfg(unix)]
#[test]
fn a_panic_in_recovery_after_its_spawn_waits_for_the_stage() {
    let env = Env::with_budget(4);
    let stage = stage(&env, 7);
    let args: Vec<&str> = stage.iter().map(String::as_str).collect();
    let out = output(env.run_at(None, &args, &[], None).env(
        "CARDINAL_TEST_PANIC",
        format!("{RUN_SPAWNING},{RUN_SPAWNED}"),
    ));
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(out.stdout, b"out");
    assert_eq!(out.stderr, b"err", "the status is known");
    assert_eq!(env.lines("counter"), ["run"]);
    let fail_open = env.records_of(RecordKind::JobFailOpen);
    assert_eq!(fail_open.len(), 1);
    assert!(
        fail_open[0].data["detail"]
            .as_str()
            .unwrap()
            .contains("injected panic at run.spawning")
    );
    let reasons = infra_reasons(&env);
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    assert!(
        reasons[0].contains("injected panic at run.spawned"),
        "{reasons:?}"
    );
    assert_eq!(env.reservations(), []);
}

/// When the runner cannot learn the stage's status, it exits 125 with one line on stderr and
/// does not rerun the stage. The injected failure stands for a wait that reaped the child and
/// got no status.
#[cfg(unix)]
#[test]
fn unknown_status_exits_125_with_one_line() {
    let env = Env::with_budget(4);
    let stage = stage(&env, 3);
    let args: Vec<&str> = stage.iter().map(String::as_str).collect();
    let out = output(&mut env.run_at(None, &args, &[], Some(RUN_REAPED)));
    assert_eq!(out.status.code(), Some(125));
    assert_eq!(out.stdout, b"out");
    let stderr = String::from_utf8(out.stderr).unwrap();
    let (stage_err, line) = stderr.split_at(3);
    assert_eq!(stage_err, "err");
    assert!(
        line.starts_with("[cardinal] Infrastructure failure after the command started: ")
            && line.ends_with(". The command was not rerun.\n")
            && line.lines().count() == 1,
        "{line:?}"
    );
    assert_eq!(env.lines("counter"), ["run"]);
    assert_eq!(infra_reasons(&env).len(), 1);
    assert_eq!(env.reservations(), []);
}
