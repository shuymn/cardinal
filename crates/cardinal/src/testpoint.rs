//! Pause points and failure injection for tests (`docs/architecture.md` section 13.1).
//!
//! Active only when `CARDINAL_TEST_DIR` is set; otherwise [`hit`] costs one cached
//! environment read.
//!
//! - `CARDINAL_TEST_PAUSE=<point>[,<point>]`: at a listed point the process creates
//!   `<dir>/<point>.<pid>.reached` and polls for `<dir>/<point>.release`. After 30 s it aborts,
//!   so a broken test fails instead of hanging.
//! - `CARDINAL_TEST_FAIL=<point>`: the process returns an injected error at that point.
//! - `CARDINAL_TEST_PANIC=<point>[,<point>]`: the process panics at a listed point.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Reservation committed, admission lock released.
pub const ADMISSION_RESERVED: &str = "admission.reserved";
/// Budget full, about to sleep.
pub const ADMISSION_WAITING: &str = "admission.waiting";
/// Waiting for the worktree in-use lock.
pub const INUSE_WAITING: &str = "inuse.waiting";
/// The runner is about to set up its signal handlers (Unix); an injected failure stands for
/// handlers that cannot be set up.
pub const RUN_WATCHING: &str = "run.watching";
/// Immediately before the `starting` record and the spawn.
pub const RUN_BEFORE_STARTING: &str = "run.before_starting";
/// The admitted path is about to spawn the stage, after its last cancellation check.
pub const RUN_SPAWNING: &str = "run.spawning";
/// The spawn returned a child, before anything about the child is read.
pub const RUN_SPAWNED: &str = "run.spawned";
/// The child has been spawned.
pub const RUN_AFTER_SPAWN: &str = "run.after_spawn";
/// The stage's child has been reaped and its status not read; an injected failure stands for
/// a status the wait could not get.
pub const RUN_REAPED: &str = "run.reaped";
/// `act` holds the exclusive in-use lock.
pub const ACT_LOCKED: &str = "act.locked";
/// `act` recorded its snapshot and has not run its checks.
pub const ACT_SNAPSHOTTED: &str = "act.snapshotted";
/// `act` checks passed and nothing is deleted yet.
pub const ACT_CHECKED: &str = "act.checked";
/// The hook classified the command.
pub const HOOK_CLASSIFIED: &str = "hook.classified";
/// A record appender holds the shared records lock and the open current file, and has not
/// written its line.
pub const RECORD_OPENED: &str = "record.opened";
/// The inbox refresh observed reservations, processes, and sessions, and has not raised or
/// cleared anything.
pub const INBOX_OBSERVED: &str = "inbox.observed";

/// Every pause point.
pub const POINTS: &[&str] = &[
    ADMISSION_RESERVED,
    ADMISSION_WAITING,
    INUSE_WAITING,
    RUN_WATCHING,
    RUN_BEFORE_STARTING,
    RUN_SPAWNING,
    RUN_SPAWNED,
    RUN_AFTER_SPAWN,
    RUN_REAPED,
    ACT_LOCKED,
    ACT_SNAPSHOTTED,
    ACT_CHECKED,
    HOOK_CLASSIFIED,
    RECORD_OPENED,
    INBOX_OBSERVED,
];

/// How long a paused process waits for its release file before aborting.
pub const PAUSE_LIMIT: Duration = Duration::from_secs(30);

/// A failure injected with `CARDINAL_TEST_FAIL`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("injected failure at {point}")]
pub struct Injected {
    pub point: &'static str,
}

#[derive(Debug)]
struct Settings {
    dir: PathBuf,
    pause: Vec<String>,
    fail: Option<String>,
    panic: Vec<String>,
}

fn settings() -> Option<&'static Settings> {
    static SETTINGS: OnceLock<Option<Settings>> = OnceLock::new();
    SETTINGS
        .get_or_init(|| {
            let dir = std::env::var_os("CARDINAL_TEST_DIR").filter(|d| !d.is_empty())?;
            let var = |name| std::env::var(name).unwrap_or_default();
            let list = |name| {
                var(name)
                    .split(',')
                    .filter(|p| !p.is_empty())
                    .map(str::to_owned)
                    .collect()
            };
            Some(Settings {
                dir: PathBuf::from(dir),
                pause: list("CARDINAL_TEST_PAUSE"),
                fail: Some(var("CARDINAL_TEST_FAIL")).filter(|p| !p.is_empty()),
                panic: list("CARDINAL_TEST_PANIC"),
            })
        })
        .as_ref()
}

/// Marks that the process reached `point`: pauses there, returns an injected failure, or
/// panics when the test environment asks for it. A no-op outside tests.
pub fn hit(point: &'static str) -> Result<(), Injected> {
    let Some(settings) = settings() else {
        return Ok(());
    };
    if settings.pause.iter().any(|p| p == point) {
        pause(&settings.dir, point);
    }
    assert!(
        !settings.panic.iter().any(|p| p == point),
        "injected panic at {point}"
    );
    if settings.fail.as_deref() == Some(point) {
        return Err(Injected { point });
    }
    Ok(())
}

/// `<dir>/<point>.<pid>.reached`
#[must_use]
pub fn reached_file(dir: &Path, point: &str, pid: u32) -> PathBuf {
    dir.join(format!("{point}.{pid}.reached"))
}

/// `<dir>/<point>.release`
#[must_use]
pub fn release_file(dir: &Path, point: &str) -> PathBuf {
    dir.join(format!("{point}.release"))
}

fn pause(dir: &Path, point: &str) {
    if std::fs::write(reached_file(dir, point, std::process::id()), b"").is_err() {
        std::process::abort();
    }
    let release = release_file(dir, point);
    let start = Instant::now();
    while !release.exists() {
        if start.elapsed() > PAUSE_LIMIT {
            std::process::abort();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
