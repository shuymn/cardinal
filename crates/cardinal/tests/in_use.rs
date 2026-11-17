//! The worktree in-use lock (C15, A9, `docs/architecture.md` section 4.6): the runner holds it
//! shared from acceptance until the stage exits, on every path where it can take it, so an
//! exclusive `try_lock` (what `act` does) fails meanwhile; when it cannot be taken within the
//! wait cap, the stage runs anyway and `in_use_unprotected` is recorded.

#![cfg(unix)]

mod runner_support;
mod support;

use std::process::Stdio;

use cardinal::admission::in_use;
use cardinal::contract::RecordKind;
use cardinal::testpoint::{
    ADMISSION_RESERVED, ADMISSION_WAITING, INUSE_WAITING, RUN_AFTER_SPAWN, RUN_BEFORE_STARTING,
};
use cardinal_test_support::process::wait_for_file;
use cardinal_test_support::pty::Pty;
use runner_support::{Env, WAIT, spec};

#[test]
fn helper() {
    support::helper::run_if_requested();
}

/// A stage that marks it started, then waits for `go` before it exits.
fn gated_stage(env: &Env) -> [String; 3] {
    [
        "sh".into(),
        "-c".into(),
        format!(
            ": > '{started}'; while [ ! -e '{go}' ]; do sleep 0.02; done",
            started = env.file("started").display(),
            go = env.file("go").display()
        ),
    ]
}

fn args(stage: &[String]) -> Vec<&str> {
    stage.iter().map(String::as_str).collect()
}

#[test]
fn waiting_and_running_jobs_block_act() {
    let env = Env::with_budget(1);
    let mut full = env.hold_budget(1);
    let mut runner = env
        .run_at(None, &["true"], &[ADMISSION_WAITING, RUN_AFTER_SPAWN], None)
        .spawn()
        .unwrap();
    env.points
        .wait_reached(ADMISSION_WAITING, runner.id(), WAIT);
    assert!(
        !env.worktree_lock_free(),
        "a waiting job holds the worktree"
    );
    full.release();
    env.points.release(ADMISSION_WAITING);

    env.points.wait_reached(RUN_AFTER_SPAWN, runner.id(), WAIT);
    assert!(
        !env.worktree_lock_free(),
        "a running job holds the worktree"
    );
    let busy = in_use(
        &env.state(),
        &cardinal::config::HostConfig::defaults(),
        &env.worktree_key(),
    )
    .unwrap();
    assert!(busy.value);
    let job = env
        .reservations()
        .into_iter()
        .find(|r| r.runner.pid == runner.id())
        .unwrap();
    assert_eq!(busy.jobs, std::slice::from_ref(&job.job_id));
    let worktree = job.worktree.expect("the reservation names the worktree");
    assert_eq!(worktree.key, env.worktree_key());
    assert_eq!(worktree.path, env.worktree().to_str().unwrap());
    assert!(worktree.protected);

    env.points.release(RUN_AFTER_SPAWN);
    assert_eq!(runner.wait().unwrap().code(), Some(0));
    assert!(env.worktree_lock_free());
    assert!(
        !in_use(
            &env.state(),
            &cardinal::config::HostConfig::defaults(),
            &env.worktree_key()
        )
        .unwrap()
        .value
    );
}

/// While `act` holds the worktree exclusively, the stage does not start; it starts once the
/// lock is free.
#[test]
fn act_lock_delays_the_stage() {
    let env = Env::with_budget(1);
    let act = env.hold_worktree_exclusive();
    let counter = env.file("counter");
    let script = format!("echo run >> '{}'", counter.display());
    let mut runner = env
        .run_at(None, &["sh", "-c", &script], &[INUSE_WAITING], None)
        .spawn()
        .unwrap();
    env.points.wait_reached(INUSE_WAITING, runner.id(), WAIT);
    env.points.release(INUSE_WAITING);
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(env.lines("counter"), Vec::<String>::new());
    drop(act);
    assert_eq!(runner.wait().unwrap().code(), Some(0));
    assert_eq!(env.lines("counter"), ["run"]);
    assert_eq!(env.records_of(RecordKind::InUseUnprotected), []);
}

#[test]
fn unprotected_run_recorded() {
    let env = Env::new(
        "[budget]\ncpu = 4\n[admission]\ndefault_resources = { cpu = 1 }\nwait_fraction = 0.5\n",
    );
    let _act = env.hold_worktree_exclusive();
    let counter = env.file("counter");
    let script = format!("echo run >> '{}'", counter.display());
    let start = std::time::Instant::now();
    let mut runner = env
        .run_at(
            Some(&spec(1, 400)),
            &["sh", "-c", &script],
            &[ADMISSION_RESERVED],
            None,
        )
        .spawn()
        .unwrap();
    env.points
        .wait_reached(ADMISSION_RESERVED, runner.id(), WAIT);
    assert!(start.elapsed() >= std::time::Duration::from_millis(200));
    let worktree = env.reservations()[0].worktree.clone().unwrap();
    assert!(!worktree.protected);
    env.points.release(ADMISSION_RESERVED);
    assert_eq!(runner.wait().unwrap().code(), Some(0));
    assert_eq!(env.lines("counter"), ["run"]);
    let unprotected = env.records_of(RecordKind::InUseUnprotected);
    assert_eq!(unprotected.len(), 1);
    assert_eq!(unprotected[0].data["key"], env.worktree_key().as_str());
}

/// Fail-open (an injected failure, an invalid configuration) and TTY passthrough keep the
/// in-use lock until the stage exits.
#[test]
fn held_on_fail_open_and_tty() {
    for path in ["fail_open", "config_invalid", "tty"] {
        let env = if path == "config_invalid" {
            Env::new("typo = 1\n")
        } else {
            Env::with_budget(1)
        };
        let stage = gated_stage(&env);
        let mut command = if path == "fail_open" {
            env.run_at(None, &args(&stage), &[], Some(RUN_BEFORE_STARTING))
        } else {
            env.run(None, &args(&stage))
        };
        let pty = Pty::open();
        if path == "tty" {
            command.stdin(pty.stdin());
        } else {
            command.stdin(Stdio::null());
        }
        let mut runner = command.spawn().unwrap();
        wait_for_file(&env.file("started"), WAIT);
        assert!(!env.worktree_lock_free(), "{path}");
        std::fs::write(env.file("go"), b"").unwrap();
        assert_eq!(runner.wait().unwrap().code(), Some(0), "{path}");
        assert!(env.worktree_lock_free(), "{path}");
        let kind = if path == "tty" {
            RecordKind::TtyPassthrough
        } else {
            RecordKind::JobFailOpen
        };
        assert_eq!(env.records_of(kind).len(), 1, "{path}");
    }
}

/// A stage outside any worktree takes no in-use lock.
#[test]
fn outside_a_worktree_nothing_is_locked() {
    let env = Env::with_budget(1);
    let mut command = env.command_in(env.scope().root(), &["run", "--", "true"]);
    env.points.apply(&mut command, &[ADMISSION_RESERVED], None);
    let mut runner = command.spawn().unwrap();
    env.points
        .wait_reached(ADMISSION_RESERVED, runner.id(), WAIT);
    assert_eq!(env.reservations()[0].worktree, None);
    env.points.release(ADMISSION_RESERVED);
    assert_eq!(runner.wait().unwrap().code(), Some(0));
    assert_eq!(
        std::fs::read_dir(env.state().worktrees_dir())
            .unwrap()
            .count(),
        0
    );
}
