//! A cardinal process that is terminated while a child it runs through `os::child` is alive
//! kills the child's process group (Unix) or job (Windows) first, and on Unix still ends by
//! the signal (`docs/architecture.md` section 8.1).

mod observe_common;
mod support;

#[cfg(unix)]
use std::path::Path;
use std::process::{Child, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use observe_common::Scene;

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
fn wait_or_kill(child: &mut Child) -> ExitStatus {
    if let Some(status) = poll(Duration::from_secs(20), || child.try_wait().unwrap()) {
        return status;
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("cardinal did not end after the signal");
}

/// Kills the git wrapper's process group if a failing test left it running.
#[cfg(unix)]
struct StopGroup(Option<(u32, u64)>);

#[cfg(unix)]
impl Drop for StopGroup {
    fn drop(&mut self) {
        if let Some((pid, start_s)) = self.0
            && cardinal::os::is_alive(pid, start_s)
        {
            cardinal::os::kill_process_group(pid);
        }
    }
}

/// Whether this process ignores `sig`; cardinal inherits that and leaves the signal ignored.
#[cfg(unix)]
fn ignored(sig: i32) -> bool {
    let mut current = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
    // SAFETY: without a new action, `sigaction` only writes the current one into `current`,
    // which is valid for writes.
    let read = unsafe { libc::sigaction(sig, std::ptr::null(), current.as_mut_ptr()) };
    // SAFETY: `current` is zero-initialized, a valid `sigaction`, and filled on success.
    read == 0 && unsafe { current.assume_init() }.sa_sigaction == libc::SIG_IGN
}

/// The git wrapper: starts `sleep` in the background, records its own PID and the sleep's,
/// and waits.
#[cfg(unix)]
fn sleeping_git(scene: &mut Scene) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let pids = scene.fx.path("pids");
    let wrapper = scene.fx.path("git-wrapper");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nsleep 30 &\necho $$ $! > '{0}.tmp'\nmv '{0}.tmp' '{0}'\nwait\n",
            pids.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
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

    use rustix::process::{Pid, Signal};

    for sig in [Signal::TERM, Signal::INT, Signal::HUP] {
        if ignored(sig.as_raw()) {
            eprintln!("skipping signal {}: this process ignores it", sig.as_raw());
            continue;
        }
        let mut scene = Scene::new();
        let pids_file = sleeping_git(&mut scene);
        let mut cardinal = start_inventory(&scene);
        let pids = read_pids(&pids_file);
        assert_eq!(pids.len(), 2, "{pids:?}");
        let wrapper = pids[0];
        let _stop = StopGroup(cardinal::os::start_time_s(wrapper).map(|s| (wrapper, s)));

        rustix::process::kill_process(Pid::from_child(&cardinal), sig).unwrap();
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

/// On Windows a terminated cardinal runs no code; its children go with it because cardinal
/// keeps itself in a job that is killed when its last handle closes.
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
    let ping = poll(Duration::from_secs(30), || {
        let table = ProcessTable::snapshot(detail).ok()?;
        table
            .iter()
            .find(|p| p.name.to_ascii_lowercase().starts_with("ping") && p.cmd.contains(&target))
            .map(|p| (p.pid, p.start_s))
    });
    let Some((pid, start_s)) = ping else {
        let _ = cardinal.kill();
        let _ = cardinal.wait();
        panic!("the git wrapper's ping started");
    };

    cardinal.kill().unwrap();
    wait_or_kill(&mut cardinal);
    assert!(
        poll(Duration::from_secs(5), || (!cardinal::os::is_alive(
            pid, start_s
        ))
        .then_some(()))
        .is_some(),
        "ping {pid} outlived cardinal"
    );
}
