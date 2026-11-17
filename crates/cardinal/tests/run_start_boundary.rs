//! The start boundary (C5, `docs/architecture.md` section 5.3): before the spawn returns a
//! child, every failure the runner can observe runs the stage exactly once; a runner killed
//! before the spawn runs it zero times with a visible non-zero status; after the spawn, the
//! stage is never run again.

mod runner_support;
mod support;

#[cfg(unix)]
use cardinal_test_support::process::{GroupGuard, PidGuard, wait_for_file};
use runner_support::Env;
#[cfg(unix)]
use runner_support::{WAIT, output, wait_gone};

#[test]
fn helper() {
    support::helper::run_if_requested();
}

/// A stage that appends one line to `counter` in the work dir and exits 7.
#[cfg(unix)]
fn counting_stage(env: &Env) -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        format!("echo run >> '{}'; exit 7", env.file("counter").display()),
    ]
}

#[cfg(unix)]
fn args(stage: &[String]) -> Vec<&str> {
    stage.iter().map(String::as_str).collect()
}

#[cfg(unix)]
fn fail_open_reasons(env: &Env) -> Vec<String> {
    env.records_of(cardinal::contract::RecordKind::JobFailOpen)
        .iter()
        .map(|r| r.data["reason"].as_str().unwrap().to_owned())
        .collect()
}

#[cfg(unix)]
#[test]
fn injected_failure_before_starting_runs_the_stage_once() {
    let env = Env::with_budget(4);
    let stage = counting_stage(&env);
    let out = output(&mut env.run_at(
        None,
        &args(&stage),
        &[],
        Some(cardinal::testpoint::RUN_BEFORE_STARTING),
    ));
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(env.lines("counter"), ["run"]);
    assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
    assert_eq!(fail_open_reasons(&env), ["injected"]);
    assert_eq!(env.reservations(), [], "the reservation is dropped");
    assert_eq!(env.live_locks(), Vec::<String>::new());
}

/// Signal handlers that cannot be set up make the run fail open before admission: the stage
/// runs once, in the caller's group, where it gets terminal signals itself.
#[cfg(unix)]
#[test]
fn signal_setup_failure_runs_the_stage_once_in_the_callers_group() {
    let env = Env::with_budget(4);
    let stage = counting_stage(&env);
    let out = output(&mut env.run_at(
        None,
        &args(&stage),
        &[],
        Some(cardinal::testpoint::RUN_WATCHING),
    ));
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(env.lines("counter"), ["run"]);
    assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
    assert_eq!(fail_open_reasons(&env), ["signal_setup"]);
    let started = env.records_of(cardinal::contract::RecordKind::JobStarted);
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].data["path"], "fail_open");
    assert_eq!(
        env.records_of(cardinal::contract::RecordKind::JobAdmitted),
        []
    );
    assert_eq!(env.reservations(), []);
    assert!(env.worktree_lock_free());
}

#[cfg(unix)]
#[test]
fn runner_killed_before_starting_runs_the_stage_zero_times() {
    let env = Env::with_budget(4);
    let stage = counting_stage(&env);
    let status_file = env.file("status");
    // The calling shell reports the runner's death as the stage's status.
    let runner = env.run_at(
        None,
        &args(&stage),
        &[cardinal::testpoint::RUN_BEFORE_STARTING],
        None,
    );
    let after = format!("; echo $? > '{}'", status_file.display());
    let mut shell = env.in_shell(&runner, &after).spawn().unwrap();
    let runner = env.wait_any_reached(cardinal::testpoint::RUN_BEFORE_STARTING);
    PidGuard::new(runner).kill();
    assert_eq!(shell.wait().unwrap().code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(&status_file).unwrap().trim(),
        "137",
        "SIGKILL is visible to the calling shell"
    );
    env.points.release(cardinal::testpoint::RUN_BEFORE_STARTING);
    assert_eq!(env.lines("counter"), Vec::<String>::new(), "zero runs");
    let views = cardinal::admission::list_reservations(&env.state()).unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(
        views[0].derived,
        cardinal::contract::DerivedState::Stale,
        "the group was the runner alone"
    );
}

#[cfg(unix)]
#[test]
fn runner_killed_after_spawn_never_reruns_the_stage() {
    let env = Env::with_budget(4);
    let started = env.file("started");
    let stage = [
        "sh".to_owned(),
        "-c".to_owned(),
        format!(
            "echo run >> '{}'; : > '{}'; sleep 0.3",
            env.file("counter").display(),
            started.display()
        ),
    ];
    let mut runner = env
        .run_at(
            None,
            &args(&stage),
            &[cardinal::testpoint::RUN_AFTER_SPAWN],
            None,
        )
        .spawn()
        .unwrap();
    env.points
        .wait_reached(cardinal::testpoint::RUN_AFTER_SPAWN, runner.id(), WAIT);
    let group = GroupGuard::new(env.scope(), runner.id());
    let child = env.reservations()[0]
        .child
        .expect("running records the child");
    runner.kill_leader();
    wait_for_file(&started, WAIT);
    wait_gone(child.pid, child.start_s);
    // The group goes while the runner, its leader, is unreaped, so its ID is still the group's.
    drop(group);
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&runner.wait().unwrap()),
        Some(9)
    );
    assert_eq!(env.lines("counter"), ["run"], "one run, never repeated");
    let finished = env.records_of(cardinal::contract::RecordKind::JobFinished);
    assert_eq!(
        finished,
        [],
        "the record has job_started without job_finished"
    );
}

#[cfg(unix)]
#[test]
fn failed_spawn_fails_open_and_reports_like_a_shell() {
    let env = Env::with_budget(4);
    let out = output(&mut env.run(None, &["cardinal-test-no-such-command"]));
    assert_eq!(out.status.code(), Some(127));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "cardinal-test-no-such-command: No such file or directory\n"
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert_eq!(fail_open_reasons(&env), ["spawn_failed"]);
    assert_eq!(env.reservations(), []);

    let script = env.file("not-executable");
    std::fs::write(&script, "#!/bin/sh\necho ran\n").unwrap();
    let path = script.to_str().unwrap();
    let out = output(&mut env.run(None, &[path]));
    assert_eq!(out.status.code(), Some(126));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!("{path}: Permission denied\n")
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
}

#[cfg(unix)]
#[test]
fn admission_lock_past_its_deadline_fails_open() {
    let env = Env::new(
        "[budget]\ncpu = 4\n[admission]\ndefault_resources = { cpu = 1 }\nlock_timeout_ms = 200\n",
    );
    let held = cardinal::state::try_lock(
        &env.state().admission_lock(),
        cardinal::state::LockMode::Exclusive,
    )
    .unwrap()
    .unwrap();
    let stage = counting_stage(&env);
    let start = std::time::Instant::now();
    let out = output(&mut env.run(None, &args(&stage)));
    assert!(start.elapsed() >= std::time::Duration::from_millis(200));
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(env.lines("counter"), ["run"]);
    assert_eq!(fail_open_reasons(&env), ["admission_lock_timeout"]);
    drop(held);
    assert_eq!(env.reservations(), []);
    assert_eq!(env.live_locks(), Vec::<String>::new());
}

#[cfg(unix)]
#[test]
fn configuration_spec_and_state_errors_fail_open() {
    let env = Env::new("typo = 1\n");
    let stage = counting_stage(&env);
    let out = output(&mut env.run(None, &args(&stage)));
    assert_eq!(out.status.code(), Some(7));
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    assert_eq!(fail_open_reasons(&env), ["config_invalid"]);

    let env = Env::with_budget(4);
    let stage = counting_stage(&env);
    let mut bad_spec = vec!["run", "--spec-b64", "not*base64", "--"];
    bad_spec.extend(args(&stage));
    let mut bad_spec = env.command(&bad_spec);
    let out = output(&mut bad_spec);
    assert_eq!(out.status.code(), Some(7));
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    assert_eq!(fail_open_reasons(&env), ["spec_invalid"]);
    assert_eq!(env.lines("counter"), ["run"]);

    let not_a_dir = env.file("state-file");
    std::fs::write(&not_a_dir, b"").unwrap();
    let out = output(
        env.run(None, &args(&stage))
            .env("CARDINAL_STATE_DIR", &not_a_dir),
    );
    assert_eq!(out.status.code(), Some(7));
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    assert_eq!(env.lines("counter"), ["run", "run"]);
}

/// The runner reads `host.toml` only: a broken profile in `repos/` does not make it fail open.
#[cfg(unix)]
#[test]
fn a_broken_profile_does_not_fail_open() {
    let env = Env::with_budget(4);
    let repos = env.dirs.config().join("repos");
    std::fs::create_dir(&repos).unwrap();
    std::fs::write(repos.join("broken.toml"), "not toml [").unwrap();
    let stage = counting_stage(&env);
    let out = output(&mut env.run(None, &args(&stage)));
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(fail_open_reasons(&env), Vec::<String>::new());
    assert_eq!(
        env.records_of(cardinal::contract::RecordKind::JobAdmitted)
            .len(),
        1
    );
    assert_eq!(env.lines("counter"), ["run"]);
}

/// Phase 0 on Windows: no process groups, so `cardinal run` always fails open and returns the
/// stage's status.
#[cfg(windows)]
#[test]
fn windows_runs_fail_open() {
    let env = Env::with_budget(4);
    let out = runner_support::output(&mut env.run(None, &["cmd", "/C", "exit 7"]));
    assert_eq!(out.status.code(), Some(7));
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    let reasons: Vec<String> = env
        .records_of(cardinal::contract::RecordKind::JobFailOpen)
        .iter()
        .map(|r| r.data["reason"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(reasons, ["group_setup"]);
    assert_eq!(env.reservations(), []);
    assert!(env.worktree_lock_free());
}
