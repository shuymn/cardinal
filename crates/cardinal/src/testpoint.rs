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
/// A pass took a successful reading of a volume and has made the first of its two writes, the
/// disk state and the readings history, and not the second (section 10.4).
pub const RECONCILE_DISK_WRITTEN: &str = "reconcile.disk_written";
/// A `reconcile` pass kept its full observation and has raised no item.
pub const RECONCILE_OBSERVED: &str = "reconcile.observed";
/// The arbiter launcher holds `arbiter/.lock` and has started no command.
pub const ARBITER_LOCKED: &str = "arbiter.locked";
/// The launcher recorded the pass and its keys in `arbiter/last.json` and assigned no item.
pub const ARBITER_RECORDED: &str = "arbiter.recorded";
/// The launcher, holding `inbox/.lock`, read the items of the pass and decided its outcome from
/// their answers, and has written it into none of them.
pub const ARBITER_ITEMS_WRITING: &str = "arbiter.items_writing";
/// The launcher wrote the pass's outcome into every item of the pass and, still holding
/// `inbox/.lock`, not yet its end into `arbiter/last.json`.
pub const ARBITER_OUTCOMES: &str = "arbiter.outcomes";
/// An `--assigned` answer is about to wait for `inbox/.lock`.
pub const ARBITER_ANSWERING: &str = "arbiter.answering";
/// `plan`'s walk found the target a directory on its parent's mount, and has not opened it
/// to list it.
pub const WALK_TARGET_CHECKED: &str = "walk.target_checked";
/// `plan`'s walk read the mount of a directory below the target by its path, and has not
/// opened it to list it.
pub const WALK_DIR_CHECKED: &str = "walk.dir_checked";
/// `plan`'s walk read the names in a directory it opened (the target's included) and has
/// `lstat`ed none of them.
pub const WALK_DIR_LISTED: &str = "walk.dir_listed";
/// A read of the quiet period's signals read the names in an ignored entry and has `lstat`ed
/// none of them (section 9.8).
pub const QUIET_DIR_LISTED: &str = "quiet.dir_listed";
/// Observation listed a worktree's filter drivers and has not run `status`.
pub const OBSERVE_FILTERS_LISTED: &str = "observe.filters_listed";
/// The hook classified the command.
pub const HOOK_CLASSIFIED: &str = "hook.classified";
/// A session hook found the worktrees of its `cwd` and has not looked for its agent or taken
/// a lease lock (section 9.8).
pub const HOOK_LEASE_AGENT: &str = "hook.lease_agent";
/// `session-end` updated its leases and has not decided whether its snapshot fits its budget.
pub const HOOK_SNAPSHOT_ADMISSION: &str = "hook.snapshot_admission";
/// The inbox refresh holds a lease file's lock and has not checked the file again or read it.
pub const INBOX_LEASE_LOCKED: &str = "inbox.lease_locked";
/// A record appender holds the shared records lock and the open current file, and has not
/// written its line.
pub const RECORD_OPENED: &str = "record.opened";
/// The inbox refresh ran its recovery pass and is about to list the processes; an injected
/// failure stands for a process table that cannot be read.
pub const INBOX_LISTING: &str = "inbox.listing";
/// The inbox refresh observed reservations, and processes and sessions unless the listing
/// failed, and has not raised or cleared anything.
pub const INBOX_OBSERVED: &str = "inbox.observed";
/// A refresh found `inbox/.refresh.lock` held and is about to wait for it.
pub const INBOX_REFRESH_WAITING: &str = "inbox.refresh_waiting";
/// `act` or recovery opened an execution's `act.lock` and has not tried it.
pub const ACT_LOCK_OPENED: &str = "act.lock_opened";
/// `act` holds the plan's `act.lock` and the worktree's in-use lock and has taken no
/// snapshot.
pub const ACT_EXCLUDED: &str = "act.excluded";
/// `act`'s snapshot, or step 5's walk of the moved target, read the names in a directory of the
/// target and has `lstat`ed none of them.
pub const ACT_DIR_LISTED: &str = "act.dir_listed";
/// The claim is written and synced under a temporary name and not renamed into place.
pub const ACT_PUBLISHING: &str = "act.publishing";
/// The claim is published and the trash entry created; nothing moved.
pub const ACT_CLAIMED: &str = "act.claimed";
/// The target is moved and synced; nothing checked after the move.
pub const ACT_MOVED: &str = "act.moved";
/// Every check after the move passed; the commit's checks have not run.
pub const ACT_CHECKED: &str = "act.checked";
/// Step 8's scan of the working directories returned; the deadline of the reads after the
/// move not checked after it, nothing published.
pub const ACT_CWD_READ: &str = "act.cwd_read";
/// `committed` is renamed into place and not synced, the in-use lock held; nothing taken.
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
    RECONCILE_DISK_WRITTEN,
    RECONCILE_OBSERVED,
    ARBITER_LOCKED,
    ARBITER_RECORDED,
    ARBITER_ITEMS_WRITING,
    ARBITER_OUTCOMES,
    ARBITER_ANSWERING,
    WALK_TARGET_CHECKED,
    WALK_DIR_CHECKED,
    WALK_DIR_LISTED,
    QUIET_DIR_LISTED,
    OBSERVE_FILTERS_LISTED,
    HOOK_CLASSIFIED,
    HOOK_LEASE_AGENT,
    HOOK_SNAPSHOT_ADMISSION,
    INBOX_LEASE_LOCKED,
    RECORD_OPENED,
    INBOX_LISTING,
    INBOX_OBSERVED,
    INBOX_REFRESH_WAITING,
    ACT_LOCK_OPENED,
    ACT_EXCLUDED,
    ACT_DIR_LISTED,
    ACT_PUBLISHING,
    ACT_CLAIMED,
    ACT_MOVED,
    ACT_CHECKED,
    ACT_CWD_READ,
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

/// The file system type a test makes the walks read for `path` (`docs/architecture.md`
/// section 13.7): with `CARDINAL_TEST_DIR` set, `<dir>/fs-types` holds lines
/// `<type>\t<prefix>`, and a path at or below a prefix reads that type, the longest prefix
/// winning. `None` outside tests and for any other path.
#[must_use]
pub fn fake_fs_type(path: &Path) -> Option<String> {
    static LINES: OnceLock<Vec<(String, PathBuf)>> = OnceLock::new();
    LINES
        .get_or_init(|| fake_lines("fs-types"))
        .iter()
        .filter(|(_, prefix)| path.starts_with(prefix))
        .max_by_key(|(_, prefix)| prefix.components().count())
        .map(|(ty, _)| ty.clone())
}

/// The type a test makes the walks read for the entry at `path` (section 13.7), since making
/// a device needs root: with `CARDINAL_TEST_DIR` set, `<dir>/entry-types` holds lines
/// `<type>\t<path>` (`fifo`, `socket`, or `device`). `None` outside tests and for any other
/// path.
#[must_use]
pub fn fake_entry_type(path: &Path) -> Option<String> {
    static LINES: OnceLock<Vec<(String, PathBuf)>> = OnceLock::new();
    LINES
        .get_or_init(|| fake_lines("entry-types"))
        .iter()
        .find(|(_, at)| at == path)
        .map(|(ty, _)| ty.clone())
}

/// How far a test has moved the deleter's clock ahead (section 13.7), so the next batch of
/// deletions is due without waiting: with `CARDINAL_TEST_DIR` set, `<dir>/clock` holds a
/// number of milliseconds, read at each call, which the test raises while the deleter is
/// paused. Zero outside tests and without the file.
#[must_use]
pub fn fake_clock_ahead() -> Duration {
    let Some(settings) = settings() else {
        return Duration::ZERO;
    };
    std::fs::read_to_string(settings.dir.join("clock"))
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .map_or(Duration::ZERO, Duration::from_millis)
}

/// A write of the kind `kind` that a test makes fail for lack of space (section 13.7): with
/// `CARDINAL_TEST_DIR` set, `<dir>/write-failures` holds lines `<kind>\t<error>\t<times>`, the
/// error `ENOSPC` or `EDQUOT` and the times `once` or `always`. An error when this call is
/// one the file makes fail, counted per process; `Ok` outside tests and for any other kind.
pub fn fake_write(kind: &str) -> std::io::Result<()> {
    use std::collections::HashSet;
    use std::io::{Error, ErrorKind};
    use std::sync::Mutex;

    static FAILED_ONCE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let Some(settings) = settings() else {
        return Ok(());
    };
    let text = std::fs::read_to_string(settings.dir.join("write-failures")).unwrap_or_default();
    for line in text.lines() {
        let mut fields = line.split('\t');
        let (Some(name), Some(error), Some(times)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if name != kind {
            continue;
        }
        if times == "once" {
            let failed = FAILED_ONCE.get_or_init(Mutex::default);
            let first = failed
                .lock()
                .is_ok_and(|mut failed| failed.insert(kind.to_owned()));
            if !first {
                continue;
            }
        }
        let error_kind = match error {
            "EDQUOT" => ErrorKind::QuotaExceeded,
            _ => ErrorKind::StorageFull,
        };
        return Err(Error::new(
            error_kind,
            format!("injected {error} at the write of {kind}"),
        ));
    }
    Ok(())
}

/// The `<value>\t<path>` lines of the fake file `name` in the test directory, which a test
/// writes before it starts the process that reads them; none outside tests.
fn fake_lines(name: &str) -> Vec<(String, PathBuf)> {
    let text = settings()
        .and_then(|settings| std::fs::read_to_string(settings.dir.join(name)).ok())
        .unwrap_or_default();
    text.lines()
        .filter_map(|line| {
            let (value, path) = line.split_once('\t')?;
            Some((value.to_owned(), PathBuf::from(path)))
        })
        .collect()
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

    /// The points `docs/architecture.md` section 13.1 lists: each code span of the paragraph
    /// that starts with "Points: ", outside the parenthesized descriptions and before the bold
    /// sentence that follows the list.
    fn documented() -> BTreeSet<String> {
        use pulldown_cmark::{Event, Parser, Tag, TagEnd};

        let doc = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/architecture.md"),
        )
        .unwrap();
        let (mut points, mut at_start, mut in_points, mut depth) =
            (BTreeSet::new(), false, false, 0_u32);
        for event in Parser::new(&doc) {
            match event {
                Event::Start(Tag::Paragraph) => at_start = true,
                Event::Text(text) if at_start => {
                    at_start = false;
                    in_points = text.starts_with("Points: ");
                }
                Event::Text(text) if in_points => {
                    for c in text.chars() {
                        match c {
                            '(' => depth += 1,
                            ')' => depth = depth.saturating_sub(1),
                            _ => {}
                        }
                    }
                }
                Event::Code(code) if in_points && depth == 0 => {
                    points.insert(code.into_string());
                }
                Event::Start(Tag::Strong) | Event::End(TagEnd::Paragraph) if in_points => {
                    return points;
                }
                _ => at_start = false,
            }
        }
        panic!("no paragraph starts with \"Points: \"");
    }

    #[test]
    fn points_are_the_documented_ones() {
        let declared: BTreeSet<String> = POINTS.iter().map(|p| (*p).to_owned()).collect();
        assert_eq!(declared.len(), POINTS.len(), "a point is listed twice");
        assert_eq!(declared, documented());
    }
}
