//! TTY passthrough (A11, `docs/architecture.md` section 5.2): when stdin is a terminal, the
//! runner runs the stage without a process group or admission, holding the in-use lock, and
//! records `tty_passthrough`.

#![cfg(unix)]

mod runner_support;
mod support;

use std::io::Read as _;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use cardinal::contract::RecordKind;
use cardinal_test_support::process::wait_for;
use cardinal_test_support::pty::Pty;
use runner_support::{Env, WAIT, output};
use support::helper::Mode;

#[test]
fn helper() {
    support::helper::run_if_requested();
}

/// The argv of a helper that writes its process group ID to `out`.
fn group_probe(env: &Env, out: &Path) -> Vec<String> {
    let mode = Mode::Group {
        out: out.to_owned(),
    };
    let probe = support::helper::command(env.scope(), &mode, &env.worktree());
    std::iter::once(probe.get_program())
        .chain(probe.get_args())
        .map(|arg| arg.to_str().unwrap().to_owned())
        .collect()
}

fn read_pgid(path: &Path) -> u32 {
    std::fs::read_to_string(path)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

#[test]
fn passthrough() {
    let env = Env::with_budget(1);
    // A full budget: an admitted run would wait for the whole cap.
    let _full = env.hold_budget(1);
    let pgid_file = env.file("pgid");
    let mut stage = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        "\"$0\" \"$@\" >/dev/null; [ -t 0 ] && echo terminal".to_owned(),
    ];
    stage.extend(group_probe(&env, &pgid_file));
    let stage: Vec<&str> = stage.iter().map(String::as_str).collect();
    let pty = Pty::open();
    let start = Instant::now();
    let out = output(env.run(None, &stage).stdin(pty.stdin()));
    assert!(start.elapsed() < Duration::from_secs(5), "no budget wait");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"terminal\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");

    assert_eq!(
        read_pgid(&pgid_file),
        env.caller_group(),
        "the stage stays in the caller's process group"
    );

    assert_eq!(env.reservations().len(), 1, "only the held reservation");
    assert_eq!(env.records_of(RecordKind::JobAdmitted), []);
    assert_eq!(env.records_of(RecordKind::TtyPassthrough).len(), 1);
    let started = env.records_of(RecordKind::JobStarted);
    assert_eq!(started[0].data["path"], "tty");
}

/// Job control: a stage that stops itself stops the runner with the same signal, as the caller
/// would see a direct stage stop; continuing the runner continues the stage.
#[test]
fn a_stopped_stage_stops_the_runner() {
    use rustix::process::{Pid, WaitOptions, waitpid};

    let env = Env::with_budget(1);
    let pty = Pty::open();
    let mut runner = env
        .run(None, &["sh", "-c", "kill -STOP $$; printf resumed"])
        .stdin(pty.stdin())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = Pid::from_raw(runner.id().cast_signed()).unwrap();
    let stopped_by = wait_for("the runner to stop", WAIT, || {
        let (_, status) =
            waitpid(Some(pid), WaitOptions::UNTRACED | WaitOptions::NOHANG).unwrap()?;
        status.stopping_signal()
    });
    assert_eq!(stopped_by, cardinal::os::unix::SIGSTOP);
    cardinal::os::unix::signal_process(runner.id(), cardinal::os::unix::SIGCONT).unwrap();
    let mut stdout = String::new();
    runner
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    assert_eq!(stdout, "resumed");
    assert_eq!(runner.wait().unwrap().code(), Some(0));
}

/// The control: without a terminal, the same stage runs in the runner's own group.
#[test]
fn without_a_terminal_the_stage_runs_in_its_own_group() {
    let env = Env::with_budget(1);
    let pgid_file = env.file("pgid");
    let stage = group_probe(&env, &pgid_file);
    let stage: Vec<&str> = stage.iter().map(String::as_str).collect();
    let out = output(&mut env.run(None, &stage));
    assert_eq!(out.status.code(), Some(0));
    let stage_pgid = read_pgid(&pgid_file);
    assert_ne!(stage_pgid, env.caller_group());
    let started = env.records_of(RecordKind::JobStarted);
    assert_eq!(started[0].data["pgid"], stage_pgid);
}
