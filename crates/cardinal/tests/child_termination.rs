//! A cardinal process that is terminated while a child it runs through `os::child` is alive
//! takes the child's process group (Unix) or job (Windows) along, and on Unix still ends by
//! the signal; on Windows a process it started otherwise outlives it (`docs/architecture.md`
//! section 8.1).

mod observe_common;
mod support;

#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::{Duration, Instant};

use cardinal_test_support::Child;
use observe_common::Scene;

#[cfg(windows)]
#[test]
fn helper() {
    support::helper::run_if_requested();
}

/// Polls `check` every 20 ms until it returns `Some` or `limit` passes.
fn poll<T>(limit: Duration, mut check: impl FnMut() -> Option<T>) -> Option<T> {
    let start = Instant::now();
    loop {
        if let Some(value) = check() {
            return Some(value);
        }
        if start.elapsed() > limit {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `cardinal inventory --repo <repo>`, started with its output discarded.
fn start_inventory(scene: &Scene) -> Child {
    scene
        .cardinal()
        .args(["inventory", "--repo"])
        .arg(&scene.repo)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// Waits for `child` to end; kills it and fails the test if it does not within 20 s.
#[cfg(unix)]
fn wait_or_kill(child: &mut Child) -> ExitStatus {
    if let Some(status) = poll(Duration::from_secs(20), || child.try_wait().unwrap()) {
        return status;
    }
    child.kill();
    panic!("cardinal did not end after the signal");
}

/// The git wrapper: starts `sleep` in the background, records its own PID and the sleep's,
/// and waits.
#[cfg(unix)]
fn sleeping_git(scene: &mut Scene) -> std::path::PathBuf {
    let pids = scene.fx.path("pids");
    let wrapper = scene.fx.path("git-wrapper");
    scene.fx.scope().started_script(
        &wrapper,
        &format!(
            "sleep 30 &\necho $$ $! > '{0}.tmp'\nmv '{0}.tmp' '{0}'\nwait\n",
            pids.display()
        ),
    );
    scene.observe_config(&format!("git = '{}'", wrapper.display()));
    pids
}

/// The PIDs in the wrapper's `pids` file, once it exists.
#[cfg(unix)]
fn read_pids(file: &Path) -> Vec<u32> {
    poll(Duration::from_secs(20), || {
        let text = std::fs::read_to_string(file).ok()?;
        Some(
            text.split_whitespace()
                .map(|p| p.parse().unwrap())
                .collect(),
        )
    })
    .expect("the git wrapper started")
}

#[cfg(unix)]
#[test]
fn a_terminated_cardinal_kills_its_children_and_dies_by_the_signal() {
    use std::os::unix::process::ExitStatusExt;

    use cardinal_test_support::process::{GroupGuard, deliverable};
    use rustix::process::{Pid, Signal};

    let signals = [Signal::TERM, Signal::INT, Signal::HUP];
    deliverable(&signals.map(Signal::as_raw));
    for sig in signals {
        let mut scene = Scene::new();
        let pids_file = sleeping_git(&mut scene);
        let mut cardinal = start_inventory(&scene);
        let pids = read_pids(&pids_file);
        assert_eq!(pids.len(), 2, "{pids:?}");
        // The wrapper leads the group `os::child` made for it, which `sleep` shares; the guard
        // kills what a failing test leaves of it.
        let _group = GroupGuard::new(scene.fx.scope(), pids[0]);

        let id = i32::try_from(cardinal.id()).unwrap();
        // The child is unreaped until `wait_or_kill`, so its PID is still its own.
        rustix::process::kill_process(Pid::from_raw(id).unwrap(), sig).unwrap();
        let status = wait_or_kill(&mut cardinal);
        assert_eq!(status.signal(), Some(sig.as_raw()), "{status:?}");
        for pid in pids {
            assert!(
                poll(Duration::from_secs(5), || cardinal::os::start_time_s(pid)
                    .is_none()
                    .then_some(()))
                .is_some(),
                "signal {}: child {pid} outlived cardinal",
                sig.as_raw()
            );
        }
    }
}

/// On Windows a terminated cardinal runs no code; its children go with it because each runs
/// in a job that is killed when its only handle, which cardinal holds, closes. Cardinal alone
/// is killed: the job of the test's scope, which holds every process cardinal started, is
/// left running until the check.
#[cfg(windows)]
#[test]
fn a_terminated_cardinal_kills_its_children() {
    use cardinal::os::{Detail, ProcessTable};

    let mut scene = Scene::new();
    let id = std::process::id();
    let target = format!("127.{}.{}.39", (id >> 8) & 0xff, id & 0xff);
    let wrapper = scene.fx.path("git-wrapper.cmd");
    std::fs::write(&wrapper, format!("@ping -n 31 {target} >NUL\r\n")).unwrap();
    scene.observe_config(&format!("git = '{}'", wrapper.display()));
    let mut cardinal = start_inventory(&scene);

    let detail = Detail {
        cmd: true,
        cwd: false,
    };
    let (pid, start_s) = poll(Duration::from_secs(30), || {
        let table = ProcessTable::snapshot(detail).ok()?;
        table
            .iter()
            .find(|p| p.name.to_ascii_lowercase().starts_with("ping") && p.cmd.contains(&target))
            .map(|p| (p.pid, p.start_s))
    })
    .expect("the git wrapper's ping started");

    cardinal.kill_leader();
    assert!(
        poll(Duration::from_secs(5), || (!cardinal::os::is_alive(
            pid, start_s
        ))
        .then_some(()))
        .is_some(),
        "ping {pid} outlived cardinal"
    );
}

/// Only the children cardinal runs through `os::child` go with it: a process it starts
/// otherwise is in none of their jobs, so it outlives a terminated cardinal. A helper stands
/// in for cardinal: it runs a child through `os::child`, then starts a sleeper through a scope
/// of its own. The helper alone is killed; the job of the test's scope ends the sleeper when
/// the helper's `Child` is dropped.
#[cfg(windows)]
#[test]
fn a_process_started_outside_os_child_outlives_cardinal() {
    use support::helper::{self, Mode};

    let scene = Scene::new();
    let scope = scene.fx.scope();
    let out = scene.fx.path("sleeper.pid");
    let mut helper = helper::spawn(scope, &Mode::Outside { out: out.clone() }, scope.root());
    let pid: u32 = poll(Duration::from_secs(30), || {
        std::fs::read_to_string(&out).ok()?.trim().parse().ok()
    })
    .expect("the helper started its sleeper");
    let start_s = cardinal::os::start_time_s(pid).expect("the sleeper runs");

    helper.kill_leader();
    let survived = (0..50).all(|_| {
        std::thread::sleep(Duration::from_millis(20));
        cardinal::os::is_alive(pid, start_s)
    });
    assert!(survived, "sleeper {pid} was killed with the helper");
}
