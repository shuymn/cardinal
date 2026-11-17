//! The admission path at run time (C17, C18, `docs/architecture.md` section 13.5): `cardinal
//! run` neither uses the network nor calls or waits for the arbiter. It runs with recording
//! fakes for `git`, `gh`, and `claude` first in `PATH` and a recording listener in the proxy
//! variables, through a recovery pass, a budget wait, and an admission; no fake runs, no
//! connection arrives, and the inbox is unchanged. On Linux the runner also holds no socket at
//! `admission.reserved` and `run.after_spawn`.

#![cfg(unix)]

mod runner_support;
mod support;

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::process::Command;

use cardinal::contract::{DerivedState, RecordKind};
use cardinal::testpoint::{ADMISSION_RESERVED, RUN_AFTER_SPAWN};
use runner_support::{Env, WAIT, fake_reservation, spec};
use support::helper::{self, ChildGuard, Mode};

#[test]
fn helper() {
    helper::run_if_requested();
}

struct Isolation {
    env: Env,
    listener: TcpListener,
    proxy: String,
    path: String,
}

impl Isolation {
    fn new() -> Self {
        let env = Env::new(
            "[budget]\ncpu = 1\n[admission]\ndefault_resources = { cpu = 1 }\n\
             wait_fraction = 0.5\nmax_wait_ms = 20000\n",
        );
        let fakes = env.file("fakes");
        std::fs::create_dir(&fakes).unwrap();
        let calls = env.file("calls");
        for name in ["git", "gh", "claude", "curl"] {
            let fake = fakes.join(name);
            std::fs::write(
                &fake,
                format!("#!/bin/sh\necho {name} \"$@\" >> '{}'\n", calls.display()),
            )
            .unwrap();
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        let path = format!(
            "{}:{}",
            fakes.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Self {
            env,
            listener,
            proxy,
            path,
        }
    }

    fn isolate(&self, command: &mut Command) {
        command.env("PATH", &self.path);
        for var in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            command.env(var, &self.proxy);
        }
    }

    fn assert_untouched(&self, inbox_before: &[String]) {
        assert_eq!(
            self.env.lines("calls"),
            Vec::<String>::new(),
            "no fake was invoked"
        );
        match self.listener.accept() {
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("a connection arrived: {other:?}"),
        }
        assert_eq!(inbox(&self.env), inbox_before, "the inbox is unchanged");
    }
}

fn inbox(env: &Env) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(env.state().inbox_dir())
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// The sockets among `pid`'s open descriptors (Linux).
#[cfg(target_os = "linux")]
fn sockets(pid: u32) -> Vec<String> {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .map(|target| target.to_string_lossy().into_owned())
        .filter(|target| target.starts_with("socket:"))
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn sockets(_pid: u32) -> Vec<String> {
    Vec::new()
}

/// Leaves a stale reservation (its live lock free, its group empty) for the recovery pass.
fn leave_stale(env: &Env) {
    let state = env.state();
    let job = cardinal::state::new_id();
    std::fs::write(state.live_lock(&job), b"").unwrap();
    let reservation = fake_reservation(&job, 1, runner_support::unused_pgid(), None);
    std::fs::write(
        state.reservation(&job),
        serde_json::to_vec(&reservation).unwrap(),
    )
    .unwrap();
}

#[test]
fn no_network() {
    let isolation = Isolation::new();
    let env = &isolation.env;
    leave_stale(env);
    let inbox_before = inbox(env);
    // A full budget: the run waits, then starts over budget at the cap.
    let _full = env.hold_budget(1);
    let mut command = env.run_at(
        Some(&spec(1, 600)),
        &["true"],
        &[ADMISSION_RESERVED, RUN_AFTER_SPAWN],
        None,
    );
    isolation.isolate(&mut command);
    let mut runner = ChildGuard::new(command.spawn().unwrap());
    for point in [ADMISSION_RESERVED, RUN_AFTER_SPAWN] {
        env.points.wait_reached(point, runner.id(), WAIT);
        assert_eq!(sockets(runner.id()), Vec::<String>::new(), "{point}");
        env.points.release(point);
    }
    assert_eq!(runner.child().wait().unwrap().code(), Some(0));
    let finished = env.records_of(RecordKind::JobFinished);
    assert_eq!(finished[0].data["admission"], "overflow");
    assert_eq!(
        env.records_of(RecordKind::JobRecovered).len(),
        1,
        "the recovery pass ran"
    );
    isolation.assert_untouched(&inbox_before);
}

/// A recovery-needed reservation that recovery may not stop (its group is unverified) is what
/// the arbiter handles later; admission neither raises it to the inbox nor waits for anyone.
#[test]
fn no_arbiter() {
    let isolation = Isolation::new();
    let env = &isolation.env;
    // A live group with no recorded child: recovery-needed and never signalled.
    let mut group = helper::command(&Mode::Sleep);
    env.isolate(&mut group);
    group.process_group(0);
    let mut group = ChildGuard::new(group.spawn().unwrap());
    let pgid = group.id();
    let start_s = support::helper::wait_for("the group leader's start time", WAIT, || {
        cardinal::os::start_time_s(pgid)
    });
    let state = env.state();
    let job = cardinal::state::new_id();
    let mut reservation = fake_reservation(&job, 1, pgid, None);
    reservation.runner.start_s = start_s;
    reservation.group = cardinal::contract::GroupIdentity::Pgid {
        pgid,
        leader_start_s: start_s,
    };
    std::fs::write(
        state.reservation(&job),
        serde_json::to_vec(&reservation).unwrap(),
    )
    .unwrap();
    let inbox_before = inbox(env);

    let mut command = env.run(Some(&spec(1, 600)), &["true"]);
    isolation.isolate(&mut command);
    let out = runner_support::output(&mut command);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty() && out.stderr.is_empty());

    let views = cardinal::admission::list_reservations(&state).unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].derived, DerivedState::RecoveryNeeded);
    assert!(!views[0].verified);
    assert!(
        group.child().try_wait().unwrap().is_none(),
        "an unverified group is not signalled"
    );
    isolation.assert_untouched(&inbox_before);
}
