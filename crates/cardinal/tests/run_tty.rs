//! TTY passthrough (A11, `docs/architecture.md` section 5.2): when stdin is a terminal, the
//! runner runs the stage without a process group or admission, holding the in-use lock, and
//! records `tty_passthrough`.

#![cfg(unix)]

mod runner_support;
mod support;

use std::time::{Duration, Instant};

use cardinal::contract::RecordKind;
use runner_support::{Env, Pty, output};

#[test]
fn helper() {
    support::helper::run_if_requested();
}

#[test]
fn passthrough() {
    let env = Env::with_budget(1);
    // A full budget: an admitted run would wait for the whole cap.
    let _full = env.hold_budget(1);
    let pgid_file = env.file("pgid");
    let script = format!(
        "ps -o pgid= -p $$ > '{}'; [ -t 0 ] && echo terminal",
        pgid_file.display()
    );
    let pty = Pty::open();
    let start = Instant::now();
    let out = output(env.run(None, &["sh", "-c", &script]).stdin(pty.stdin()));
    assert!(start.elapsed() < Duration::from_secs(5), "no budget wait");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"terminal\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");

    let own = cardinal::os::unix::pgid_of(std::process::id()).unwrap();
    let stage_pgid: u32 = std::fs::read_to_string(&pgid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        stage_pgid, own,
        "the stage stays in the caller's process group"
    );

    assert_eq!(env.reservations().len(), 1, "only the held reservation");
    assert_eq!(env.records_of(RecordKind::JobAdmitted), []);
    assert_eq!(env.records_of(RecordKind::TtyPassthrough).len(), 1);
    let started = env.records_of(RecordKind::JobStarted);
    assert_eq!(started[0].data["path"], "tty");
}

/// The control: without a terminal, the same stage runs in the runner's own group.
#[test]
fn without_a_terminal_the_stage_runs_in_its_own_group() {
    let env = Env::with_budget(1);
    let pgid_file = env.file("pgid");
    let script = format!("ps -o pgid= -p $$ > '{}'", pgid_file.display());
    let out = output(&mut env.run(None, &["sh", "-c", &script]));
    assert_eq!(out.status.code(), Some(0));
    let own = cardinal::os::unix::pgid_of(std::process::id()).unwrap();
    let stage_pgid: u32 = std::fs::read_to_string(&pgid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_ne!(stage_pgid, own);
    let started = env.records_of(RecordKind::JobStarted);
    assert_eq!(started[0].data["pgid"], stage_pgid);
}
