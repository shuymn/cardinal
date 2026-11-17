//! Failures after the start boundary (C5, `docs/architecture.md` section 5.6): the stage is
//! never run again; the runner reports an infrastructure failure, returns the stage's own
//! status when it knows it, and otherwise exits 125 with one line on stderr.

mod runner_support;
mod support;

#[cfg(unix)]
use cardinal::contract::RecordKind;
#[cfg(unix)]
use runner_support::{Env, output};

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

/// When the runner cannot learn the stage's status, it exits 125 with one line on stderr and
/// does not rerun the stage. A parent that ignores SIGCHLD makes Linux reap the runner's child
/// itself, so the runner's wait fails; macOS still reports the status, so this runs on Linux.
#[cfg(target_os = "linux")]
#[test]
fn unknown_status_exits_125_with_one_line() {
    let env = Env::with_budget(4);
    let stage = stage(&env, 3);
    let args: Vec<&str> = stage.iter().map(String::as_str).collect();
    let runner = env.run(None, &args);
    let mut ignoring = std::process::Command::new("perl");
    ignoring
        .args(["-e", "$SIG{CHLD} = 'IGNORE'; exec @ARGV or die"])
        .arg(runner.get_program())
        .args(runner.get_args())
        .current_dir(env.worktree());
    for (key, value) in runner.get_envs() {
        match value {
            Some(value) => ignoring.env(key, value),
            None => ignoring.env_remove(key),
        };
    }
    let out = output(&mut ignoring);
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
