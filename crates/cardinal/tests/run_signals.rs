//! Signals (A13, `docs/architecture.md` sections 5.5 and 5.6): a stage that dies by a signal
//! makes the runner die by the same signal, so the calling shell sees the same status and
//! message; signals sent to the runner reach the stage's group.

#![cfg(unix)]

mod runner_support;
mod support;

use std::os::unix::process::ExitStatusExt as _;
use std::process::Command;

use cardinal::testpoint::{ADMISSION_WAITING, RUN_BEFORE_STARTING};
use runner_support::{Env, Pty, WAIT, output};
use support::helper::{ChildGuard, Mode, wait_for_file};

#[test]
fn helper() {
    support::helper::run_if_requested();
}

/// `bash --norc --noprofile` (or `sh` without bash) in the worktree, isolated; it runs
/// `inner; echo …`, so it reports a signal death of `inner` instead of exec'ing it.
fn shell(env: &Env) -> Command {
    let bash = || {
        let mut bash = env.isolated("bash");
        bash.args(["--norc", "--noprofile"]);
        bash
    };
    if bash().args(["-c", ":"]).status().is_ok_and(|s| s.success()) {
        bash()
    } else {
        env.isolated("sh")
    }
}

#[test]
fn same_signal_death() {
    let env = Env::with_budget(4);
    let runner = env.run(None, &[]);
    let runner_path = runner.get_program().to_str().unwrap().to_owned();
    for (signal, name, expected) in [("TERM", "Terminated", 143), ("KILL", "Killed", 137)] {
        let stage = format!("sh -c 'kill -{signal} $$'");
        let direct = output(shell(&env).args(["-c", &format!("{stage}; echo status=$?")]));
        let through = output(
            shell(&env)
                .args([
                    "-c",
                    &format!("\"$0\" run -- {stage}; echo status=$?"),
                    &runner_path,
                ])
                .env("CARDINAL_STATE_DIR", env.dirs.state())
                .env("CARDINAL_CONFIG_DIR", env.dirs.config()),
        );
        assert_eq!(
            String::from_utf8_lossy(&direct.stdout),
            format!("status={expected}\n")
        );
        assert_eq!(through.stdout, direct.stdout, "{signal}");
        let direct_err = String::from_utf8_lossy(&direct.stderr);
        let through_err = String::from_utf8_lossy(&through.stderr);
        // The message names the command that died, which differs; the signal text matches.
        assert!(direct_err.contains(name), "{direct_err}");
        assert!(through_err.contains(name), "{through_err}");
    }

    // SIGPIPE, which the Rust runtime ignores in the runner, still ends it.
    let out = output(&mut env.run(None, &["sh", "-c", "kill -PIPE $$"]));
    assert_eq!(out.status.signal(), Some(13));
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
}

/// A signal the caller ignores stays ignored for the stage, as when the stage runs directly:
/// the runner installs no handler for it, on the admitted, fail-open, and TTY paths.
#[test]
fn ignored_signals_stay_ignored() {
    let env = Env::with_budget(4);
    let stage = ["sh", "-c", "kill -TERM $$; kill -HUP $$; printf survived"];
    let ignoring = |inner: &Command| {
        let mut shell = env.isolated("sh");
        shell
            .args(["-c", "trap '' TERM HUP; exec \"$@\"", "sh"])
            .arg(inner.get_program())
            .args(inner.get_args());
        for (key, value) in inner.get_envs() {
            match value {
                Some(value) => shell.env(key, value),
                None => shell.env_remove(key),
            };
        }
        shell
    };
    let mut direct = env.isolated(stage[0]);
    direct.args(&stage[1..]);
    let direct = output(&mut ignoring(&direct));
    assert_eq!(direct.status.code(), Some(0));
    assert_eq!(direct.stdout, b"survived");

    for path in ["admitted", "fail_open", "tty"] {
        let fail = (path == "fail_open").then_some(RUN_BEFORE_STARTING);
        let mut runner = ignoring(&env.run_at(None, &stage, &[], fail));
        let pty = Pty::open();
        if path == "tty" {
            runner.stdin(pty.stdin());
        }
        let out = output(&mut runner);
        assert_eq!(out.status.code(), Some(0), "{path}: {:?}", out.status);
        assert_eq!(out.stdout, direct.stdout, "{path}");
    }
    let paths: Vec<_> = env
        .records_of(cardinal::contract::RecordKind::JobStarted)
        .iter()
        .map(|r| r.data["path"].clone())
        .collect();
    assert_eq!(paths, ["admitted", "fail_open", "tty"]);
}

/// A caller that ignores SIGCHLD would have the kernel reap the runner's children (Linux),
/// leaving the runner nothing to wait for: the runner runs with SIGCHLD at its default, so it
/// learns the stage's status on every path, and the stage still starts with SIGCHLD ignored.
/// The stage reads its disposition with `sigaction`, because not every shell lists a signal
/// ignored at entry in `trap` output (bash 3.2 does not).
#[test]
fn an_ignored_sigchld_is_reset_for_the_runner_and_kept_for_the_stage() {
    let env = Env::with_budget(4);
    // Writes SIGCHLD's disposition to the file `out` and exits 3.
    let stage = |out: &str| support::helper::command(&Mode::ChildSignal { out: env.file(out) });
    let read = |out: &str| std::fs::read_to_string(env.file(out)).unwrap();
    let ignoring = |inner: &Command| {
        let mut shell = env.isolated("sh");
        shell
            .args(["-c", "trap '' CHLD; exec \"$@\"", "sh"])
            .arg(inner.get_program())
            .args(inner.get_args());
        for (key, value) in inner.get_envs() {
            match value {
                Some(value) => shell.env(key, value),
                None => shell.env_remove(key),
            };
        }
        shell
    };
    let mut plain = stage("plain");
    env.isolate(&mut plain);
    assert_eq!(output(&mut plain).status.code(), Some(3));
    assert_eq!(read("plain"), "default");
    let direct = output(&mut ignoring(&stage("direct")));
    assert_eq!(direct.status.code(), Some(3));
    assert_eq!(read("direct"), "ignored");

    for path in ["admitted", "fail_open", "tty"] {
        let fail = (path == "fail_open").then_some(RUN_BEFORE_STARTING);
        let probe = stage(path);
        let argv: Vec<&str> = std::iter::once(probe.get_program())
            .chain(probe.get_args())
            .map(|arg| arg.to_str().unwrap())
            .collect();
        let mut inner = env.run_at(None, &argv, &[], fail);
        inner.envs(
            probe
                .get_envs()
                .filter_map(|(key, value)| Some((key, value?))),
        );
        let mut runner = ignoring(&inner);
        let pty = Pty::open();
        if path == "tty" {
            runner.stdin(pty.stdin());
        }
        let out = output(&mut runner);
        assert_eq!(out.status.code(), Some(3), "{path}: {:?}", out.status);
        assert_eq!(out.stderr, b"", "{path}");
        assert_eq!(read(path), "ignored", "{path}");
    }
    let records = |kind| env.records_of(kind);
    assert_eq!(records(cardinal::contract::RecordKind::JobInfraFailure), []);
    let exits: Vec<_> = records(cardinal::contract::RecordKind::JobFinished)
        .iter()
        .map(|r| r.data["exit"].clone())
        .collect();
    assert_eq!(exits, [3, 3, 3]);
}

/// SIGTERM sent to the runner on the admitted path reaches the whole group; the runner does
/// not die by its own forwarded copy, and exits with the stage's status.
#[test]
fn signals_are_forwarded_to_the_group() {
    let env = Env::with_budget(4);
    let ready = env.file("ready");
    let got = env.file("got");
    let script = format!(
        "trap 'echo stage >> \"{got}\"; exit 5' TERM; \
         (trap 'echo grandchild >> \"{got}\"; exit 0' TERM; : > \"{ready}\"; \
          while :; do sleep 0.02; done) & \
         while :; do sleep 0.02; done",
        got = got.display(),
        ready = ready.display()
    );
    let mut runner = ChildGuard::new(env.run(None, &["sh", "-c", &script]).spawn().unwrap());
    let _group = runner_support::GroupGuard(runner.id());
    wait_for_file(&ready, WAIT);
    std::thread::sleep(std::time::Duration::from_millis(100));
    cardinal::os::unix::signal_process(runner.id(), cardinal::os::unix::SIGTERM).unwrap();
    let status = runner.child().wait().unwrap();
    assert_eq!(status.code(), Some(5), "{status:?}");
    support::helper::wait_for("both trap lines", WAIT, || {
        (env.lines("got").len() == 2).then_some(())
    });
    let mut lines = env.lines("got");
    lines.sort();
    assert_eq!(lines, ["grandchild", "stage"]);
}

/// A stopped stage is still supervised: a signal to the runner reaches its group, and the stage
/// gets it once continued. (macOS also reports a stopped child to a wait for exits.)
#[test]
fn signals_reach_a_stopped_stage() {
    let env = Env::with_budget(4);
    let pid_file = env.file("pid");
    let script = format!(
        "trap 'exit 7' TERM; echo $$ > '{}'; kill -STOP $$; sleep 1; exit 3",
        pid_file.display()
    );
    let mut runner = ChildGuard::new(env.run(None, &["sh", "-c", &script]).spawn().unwrap());
    let _group = runner_support::GroupGuard(runner.id());
    let stage: u32 = support::helper::wait_for("the stage PID", WAIT, || {
        std::fs::read_to_string(&pid_file).ok()?.trim().parse().ok()
    });
    support::helper::wait_for("the stage to stop", WAIT, || {
        let out = env
            .isolated("ps")
            .args(["-o", "stat=", "-p", &stage.to_string()])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout)
            .trim_start()
            .starts_with('T')
            .then_some(())
    });
    cardinal::os::unix::signal_process(runner.id(), cardinal::os::unix::SIGTERM).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    cardinal::os::unix::signal_process(stage, cardinal::os::unix::SIGCONT).unwrap();
    let status = runner.child().wait().unwrap();
    assert_eq!(status.code(), Some(7), "{status:?}");
}

/// On the fail-open path the stage shares the caller's group and gets terminal signals
/// itself; the runner ignores SIGINT, as `system()` does, and forwards SIGTERM.
#[test]
fn fail_open_ignores_sigint_and_forwards_sigterm() {
    let env = Env::with_budget(4);
    let ready = env.file("ready");
    let script = format!(
        "trap '' INT; trap 'exit 6' TERM; : > '{}'; while :; do sleep 0.02; done",
        ready.display()
    );
    let mut runner = ChildGuard::new(
        env.run_at(None, &["sh", "-c", &script], &[], Some(RUN_BEFORE_STARTING))
            .spawn()
            .unwrap(),
    );
    wait_for_file(&ready, WAIT);
    cardinal::os::unix::signal_process(runner.id(), cardinal::os::unix::SIGINT).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        runner.child().try_wait().unwrap().is_none(),
        "SIGINT does not end the runner"
    );
    cardinal::os::unix::signal_process(runner.id(), cardinal::os::unix::SIGTERM).unwrap();
    assert_eq!(runner.child().wait().unwrap().code(), Some(6));
}

/// A signal before the spawn cancels the run: the stage never starts, the runner leaves
/// nothing behind, and it dies by that signal.
#[test]
fn signal_before_the_spawn_cancels_the_run() {
    let env = Env::with_budget(1);
    let _full = env.hold_budget(1);
    let script = format!("echo run >> '{}'", env.file("counter").display());
    let mut runner = ChildGuard::new(
        env.run_at(None, &["sh", "-c", &script], &[ADMISSION_WAITING], None)
            .spawn()
            .unwrap(),
    );
    env.points
        .wait_reached(ADMISSION_WAITING, runner.id(), WAIT);
    cardinal::os::unix::signal_process(runner.id(), cardinal::os::unix::SIGTERM).unwrap();
    env.points.release(ADMISSION_WAITING);
    let status = runner.child().wait().unwrap();
    assert_eq!(status.signal(), Some(15), "{status:?}");
    assert_eq!(env.lines("counter"), Vec::<String>::new());
    assert_eq!(env.reservations().len(), 1, "only the held reservation");
    assert_eq!(env.live_locks().len(), 1, "the runner's live lock is gone");
    assert!(env.worktree_lock_free());
    let finished = env.records_of(cardinal::contract::RecordKind::JobFinished);
    assert_eq!(finished[0].data["cancelled"], "signal 15");
}
