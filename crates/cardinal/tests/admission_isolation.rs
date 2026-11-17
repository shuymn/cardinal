//! The admission path at run time (C17, C18, `docs/architecture.md` section 13.5): `cardinal
//! run` neither uses the network nor calls or waits for the arbiter. It runs with recording
//! fakes for `git`, `gh`, and `claude` first in `PATH` and a recording listener in the proxy
//! variables, through a recovery pass, a budget wait, and an admission; no fake runs, no
//! connection arrives, and the inbox is unchanged. The runner also holds no socket at
//! `admission.reserved` and `run.after_spawn`.

#![cfg(unix)]

mod runner_support;
mod support;

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt as _;

use cardinal::contract::{DerivedState, RecordKind};
use cardinal::testpoint::{ADMISSION_RESERVED, RUN_AFTER_SPAWN};
use cardinal_test_support::Command;
use cardinal_test_support::process::wait_for;
use runner_support::{Env, WAIT, fake_reservation, spec};
use support::helper::{self, Mode};

#[test]
fn helper() {
    helper::run_if_requested();
}

struct Isolation {
    env: Env,
    /// The recording listener the proxy variables name, where the tests' confinement gives
    /// them a loopback of their own (Linux). The macOS sandbox denies the test every TCP
    /// socket, and the runner too, so a listener could neither bind nor see a connection
    /// there; the variables then name a closed port.
    listener: Option<TcpListener>,
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
        let listener = (!cfg!(target_os = "macos")).then(|| {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            listener
        });
        let address = listener.as_ref().map_or_else(
            || "127.0.0.1:9".to_owned(),
            |l| l.local_addr().unwrap().to_string(),
        );
        let proxy = format!("http://{address}");
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
        if let Some(listener) = &self.listener {
            match listener.accept() {
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                other => panic!("a connection arrived: {other:?}"),
            }
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

/// The sockets among `pid`'s open descriptors (macOS).
#[cfg(target_os = "macos")]
fn sockets(pid: u32) -> Vec<String> {
    let pid = i32::try_from(pid).unwrap();
    let entry = std::mem::size_of::<libc::proc_fdinfo>();
    // SAFETY: without a buffer, `proc_pidinfo` only returns the size the list needs.
    let needed =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    assert!(
        needed > 0,
        "proc_pidinfo: {}",
        std::io::Error::last_os_error()
    );
    let empty = libc::proc_fdinfo {
        proc_fd: 0,
        proc_fdtype: 0,
    };
    // Room for descriptors opened since.
    let mut fds = vec![empty; usize::try_from(needed).unwrap() / entry + 16];
    let size = i32::try_from(fds.len() * entry).unwrap();
    // SAFETY: `fds` is valid for writes of `size` bytes.
    let filled =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), size) };
    assert!(
        filled > 0,
        "proc_pidinfo: {}",
        std::io::Error::last_os_error()
    );
    fds.truncate(usize::try_from(filled).unwrap() / entry);
    let socket = u32::try_from(libc::PROX_FDTYPE_SOCKET).unwrap();
    fds.iter()
        .filter(|fd| fd.proc_fdtype == socket)
        .map(|fd| format!("socket fd {}", fd.proc_fd))
        .collect()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
    let mut runner = command.spawn().unwrap();
    for point in [ADMISSION_RESERVED, RUN_AFTER_SPAWN] {
        env.points.wait_reached(point, runner.id(), WAIT);
        assert_eq!(sockets(runner.id()), Vec::<String>::new(), "{point}");
        env.points.release(point);
    }
    assert_eq!(runner.wait().unwrap().code(), Some(0));
    let finished = env.records_of(RecordKind::JobFinished);
    assert_eq!(finished[0].data["admission"], "overflow");
    assert_eq!(
        env.records_of(RecordKind::JobRecovered).len(),
        1,
        "the recovery pass ran"
    );
    isolation.assert_untouched(&inbox_before);
}

/// A recovery-needed reservation, which recovery never stops, is what the arbiter handles
/// later; admission neither raises it to the inbox nor waits for anyone.
#[test]
fn no_arbiter() {
    let isolation = Isolation::new();
    let env = &isolation.env;
    // A live group with no recorded child: recovery-needed and never signalled.
    let mut group = helper::spawn(env.scope(), &Mode::Sleep, &env.worktree());
    let pgid = group.id();
    let start_s = wait_for("the group leader's start time", WAIT, || {
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

    let views =
        cardinal::admission::list_reservations(&state, &cardinal::config::HostConfig::defaults())
            .unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].derived, DerivedState::RecoveryNeeded);
    assert!(!views[0].verified);
    assert!(
        group.try_wait().unwrap().is_none(),
        "an unverified group is not signalled"
    );
    isolation.assert_untouched(&inbox_before);
}
