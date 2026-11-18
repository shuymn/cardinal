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
/// The status was read and `job_finished` appended, the runner's locks are dropped, and the
/// job-end disk check has not started.
pub const RUN_FINISHED: &str = "run.finished";
/// The job-end check found the volume below the warning line and is about to start
/// `cardinal reconcile`.
pub const RUN_RECONCILE_SPAWNING: &str = "run.reconcile_spawning";
/// `act` recorded its snapshot and has not run its checks.
pub const ACT_SNAPSHOTTED: &str = "act.snapshotted";
/// A `reconcile` pass kept its full observation and has raised no item.
pub const RECONCILE_OBSERVED: &str = "reconcile.observed";
/// The arbiter launcher holds `arbiter/.lock` and has started no command.
pub const ARBITER_LOCKED: &str = "arbiter.locked";
/// The launcher recorded the pass and its keys in `arbiter/last.json` and assigned no item.
pub const ARBITER_RECORDED: &str = "arbiter.recorded";
/// The launcher wrote the pass's outcome into every item of the pass, and not yet its end
/// into `arbiter/last.json`.
pub const ARBITER_OUTCOMES: &str = "arbiter.outcomes";
/// `plan`'s walk found the target a directory on its parent's mount, and has not opened it
/// to list it.
pub const WALK_TARGET_CHECKED: &str = "walk.target_checked";
/// `plan`'s walk read the mount of a directory below the target by its path, and has not
/// opened it to list it.
pub const WALK_DIR_CHECKED: &str = "walk.dir_checked";
/// Observation listed a worktree's filter drivers and has not run `status`.
pub const OBSERVE_FILTERS_LISTED: &str = "observe.filters_listed";
/// The hook classified the command.
pub const HOOK_CLASSIFIED: &str = "hook.classified";
/// A record appender holds the shared records lock and the open current file, and has not
/// written its line.
pub const RECORD_OPENED: &str = "record.opened";
/// The inbox refresh observed reservations, processes, and sessions, and has not raised or
/// cleared anything.
pub const INBOX_OBSERVED: &str = "inbox.observed";
/// A refresh found `inbox/.refresh.lock` held and is about to wait for it.
pub const INBOX_REFRESH_WAITING: &str = "inbox.refresh_waiting";
/// `act` or recovery opened an execution's `act.lock` and has not tried it.
pub const ACT_LOCK_OPENED: &str = "act.lock_opened";
/// `act` holds the plan's `act.lock` and the worktree's in-use lock and has taken no
/// snapshot.
pub const ACT_EXCLUDED: &str = "act.excluded";
/// The claim is written and synced under a temporary name and not renamed into place.
pub const ACT_PUBLISHING: &str = "act.publishing";
/// The claim is published and the trash entry created; nothing moved.
pub const ACT_CLAIMED: &str = "act.claimed";
/// The target is moved and synced; nothing checked after the move.
pub const ACT_MOVED: &str = "act.moved";
/// Every check after the move passed; the commit's checks have not run.
pub const ACT_CHECKED: &str = "act.checked";
/// `committed` is durable and the in-use lock released; nothing taken.
pub const ACT_COMMITTED: &str = "act.committed";
/// An entry is in the hold and not checked.
pub const ACT_TAKEN: &str = "act.taken";
/// An entry in the hold passed its check and is not deleted.
pub const ACT_HELD: &str = "act.held";
/// A restore renamed the target back and has not synced.
pub const ACT_RESTORED: &str = "act.restored";
/// `t` is removed after the deletion; `h` and the trash entry are left, and no result.
pub const ACT_FINISHING: &str = "act.finishing";
/// A recovery holds an execution's `act.lock` and has read nothing.
pub const RECOVER_LOCKED: &str = "recover.locked";
/// The snapshot opened the record files and took their lengths, released `records/.lock`, and
/// has read no line.
pub const SNAPSHOT_RECORDS_OPENED: &str = "snapshot.records_opened";
/// The snapshot read the reservations in its one pass, released the admission lock, and has
/// derived no state.
pub const SNAPSHOT_RESERVATIONS_READ: &str = "snapshot.reservations_read";

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
    RUN_FINISHED,
    RUN_RECONCILE_SPAWNING,
    ACT_SNAPSHOTTED,
    RECONCILE_OBSERVED,
    ARBITER_LOCKED,
    ARBITER_RECORDED,
    ARBITER_OUTCOMES,
    WALK_TARGET_CHECKED,
    WALK_DIR_CHECKED,
    OBSERVE_FILTERS_LISTED,
    HOOK_CLASSIFIED,
    RECORD_OPENED,
    INBOX_OBSERVED,
    INBOX_REFRESH_WAITING,
    ACT_LOCK_OPENED,
    ACT_EXCLUDED,
    ACT_PUBLISHING,
    ACT_CLAIMED,
    ACT_MOVED,
    ACT_CHECKED,
    ACT_COMMITTED,
    ACT_TAKEN,
    ACT_HELD,
    ACT_RESTORED,
    ACT_FINISHING,
    RECOVER_LOCKED,
    SNAPSHOT_RECORDS_OPENED,
    SNAPSHOT_RESERVATIONS_READ,
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use super::POINTS;

    /// The points `docs/architecture.md` section 13.1 lists: each backticked name of its
    /// "Points:" paragraph outside the parenthesized descriptions.
    fn documented() -> BTreeSet<String> {
        let doc = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/architecture.md"),
        )
        .unwrap();
        let start = doc.find("Points: ").expect("the Points paragraph");
        let paragraph = &doc[start..];
        let paragraph = &paragraph[..paragraph.find("**Every contended").unwrap()];
        let mut depth = 0_u32;
        let outside: String = paragraph
            .chars()
            .filter(|&c| {
                match c {
                    '(' => depth += 1,
                    ')' => depth = depth.saturating_sub(1),
                    _ => return depth == 0,
                }
                false
            })
            .collect();
        outside
            .split('`')
            .skip(1)
            .step_by(2)
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn points_are_the_documented_ones() {
        let declared: BTreeSet<String> = POINTS.iter().map(|p| (*p).to_owned()).collect();
        assert_eq!(declared.len(), POINTS.len(), "a point is listed twice");
        assert_eq!(declared, documented());
    }
}
