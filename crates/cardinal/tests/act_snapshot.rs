//! Snapshot inspection (A7, C13, C14; `docs/architecture.md` sections 9.2 and 9.5): `act`
//! inspects the target before its checks, observes the worktree again after the snapshot, and
//! on Linux does not enter a bind mount below the target or accept a target that is one;
//! `plan`'s walk lists only the directories it recorded. A race pauses `act` (the `cardinal`
//! binary, observing the fixture through git and the fake `gh` of `tests/plan_support`) or
//! the walk (a helper process) at its test point, makes the change once the point is
//! reached, and releases it. On Linux the bind mounts are made in a mount namespace of the
//! process's own (`Scope::mount_namespace` and `Scope::bind_mount`).

mod observe_common;
mod plan_support;
mod support;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use cardinal::contract::{ActResult, ActStatus, PlanKind};
use cardinal_test_support::{Command, Scope};
use plan_support::{Setup, act_result, arbiter, write};
use support::helper::Mode;
use support::testpoints::TestPoints;

#[test]
fn helper() {
    walk_if_requested();
}

/// The custom helper mode that runs `plan::walk`.
const WALK_MODE: &str = "walk";

/// Runs the `plan::walk` a helper process was started for, writes the mount and the entries
/// it found, and exits; returns at once otherwise.
fn walk_if_requested() {
    let Some(request) = support::helper::custom_request(WALK_MODE) else {
        return;
    };
    let request: serde_json::Value = serde_json::from_str(&request).expect("a walk request");
    let path = |key: &str| std::path::PathBuf::from(request[key].as_str().expect(key));
    for key in ["worktree", "target", "out"] {
        plan_support::assert_inside(&path("root"), &path(key), key);
    }
    let walk = cardinal::plan::walk(&path("worktree"), &path("target")).expect("walk");
    let doc = serde_json::json!({
        "mount": format!("{:?}", walk.mount),
        "entries": walk.entries.iter().map(|e| e.rel.display().to_string()).collect::<Vec<_>>(),
    });
    std::fs::write(path("out"), doc.to_string()).expect("write the walk");
    std::process::exit(0);
}

const WAIT: Duration = Duration::from_secs(20);

/// Runs `act` of `plan` paused at `point`, calls `change` once it has reached it, releases
/// it, and returns its result.
fn act_with_change_at(s: &Setup, plan: &str, point: &str, change: impl FnOnce()) -> ActResult {
    // A test-point directory of its own, so no release file of an earlier race is left.
    let scope = Scope::new();
    let points = TestPoints::new(&scope);
    let mut command = s.act_command(plan, &arbiter());
    points.apply(&mut command, &[point], None);
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    points.wait_reached(point, child.id(), WAIT);
    change();
    points.release(point);
    act_result(&child.wait_with_output().unwrap()).expect("an act result")
}

/// A tracked file edited after the snapshot, before the checks: the checks, which observe
/// the worktree through git again, see it dirty, and nothing is touched.
#[test]
fn edit_before_checks_is_rejected() {
    let s = Setup::new();
    let wt = s.worktree("edited");
    write(&wt, "node_modules/pkg/index.js", "x");
    let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
    let result = act_with_change_at(&s, &plan.id, cardinal::testpoint::ACT_SNAPSHOTTED, || {
        write(&wt, "src/lib.rs", "// edited after the snapshot\n");
    });
    assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
    assert_eq!(result.reasons, ["clean"]);
    assert!(
        wt.join("node_modules/pkg/index.js").exists(),
        "nothing touched"
    );
    assert!(wt.join(".git").exists());
}

/// The worktree switched to another branch with the same HEAD after the plan: `act` observes
/// it again and expires the plan, for a worktree plan and for a directory inside it.
#[test]
fn branch_switch_expires_the_plan() {
    for (n, kind) in [PlanKind::ReclaimWorktree, PlanKind::ReclaimDir]
        .into_iter()
        .enumerate()
    {
        let s = Setup::new();
        let wt = s.worktree("switched");
        write(&wt, "node_modules/pkg/index.js", "x");
        let target = match kind {
            PlanKind::ReclaimWorktree => wt.clone(),
            PlanKind::ReclaimDir => wt.join("node_modules"),
        };
        let plan = s.plan(kind, &target);
        let other = format!("other-{n}");
        s.fx.branch(&wt, &other);
        s.fx.switch(&wt, &other);
        let result = s.act(&plan.id, &arbiter()).unwrap();
        assert_eq!(result.status, ActStatus::Expired, "{kind:?}: {result:#?}");
        assert_eq!(result.reasons, ["identity_changed:branch"], "{kind:?}");
        assert!(wt.join("node_modules/pkg/index.js").exists(), "{kind:?}");
    }
}

/// Writes `line` to this process's stderr directly, past the test harness's capture, so it
/// shows for a passing test too.
#[cfg(target_os = "linux")]
fn report(line: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "{line}");
}

/// Whether a child can bind-mount in a mount namespace of its own here. Where it cannot,
/// `test` is skipped with the reason reported, and fails instead where `CI` is `true`, so a CI
/// run never passes it without running it. The probe's mount ends with its namespace.
#[cfg(target_os = "linux")]
fn mount_namespace(s: &Setup, test: &str) -> bool {
    let probe = s.fx.path("probe");
    std::fs::create_dir_all(&probe).unwrap();
    let mut command = s.fx.command("true", s.fx.root());
    s.fx.scope()
        .mount_namespace(&mut command, &[(&probe, &probe)]);
    let error = match command.status() {
        Ok(status) if status.success() => return true,
        Ok(status) => status.to_string(),
        Err(err) => err.to_string(),
    };
    let reason = format!("{test}: no private mount namespace for a bind mount: {error}");
    assert!(
        std::env::var("CI").as_deref() != Ok("true"),
        "CI runs every bind-mount test: {reason}"
    );
    report(&format!("skipped {reason}"));
    false
}

/// Reports that `test` ran its assertions in a private mount namespace.
#[cfg(target_os = "linux")]
fn ran_in_namespace(test: &str) {
    report(&format!("{test}: ran in a private mount namespace"));
}

/// Runs `inner` in a mount namespace of its own where each `(source, target)` of `binds` is
/// bind-mounted first, and returns its output. The mounts end with the namespace.
#[cfg(target_os = "linux")]
fn with_binds(s: &Setup, binds: &[(&Path, &Path)], mut inner: Command) -> std::process::Output {
    s.fx.scope().mount_namespace(&mut inner, binds);
    inner.output().unwrap()
}

/// Runs `act` of `plan` under `binds` (see [`with_binds`]).
#[cfg(target_os = "linux")]
fn act_with_binds(s: &Setup, plan: &str, binds: &[(&Path, &Path)]) -> ActResult {
    let output = with_binds(s, binds, s.act_command(plan, &arbiter()));
    act_result(&output).expect("an act result")
}

/// A helper process (this test binary, see [`walk_if_requested`]) that runs `plan::walk` of
/// `target` and writes what it found to `out`, a fresh file.
fn walk_command(s: &Setup, worktree: &Path, target: &Path, out: &Path) -> Command {
    match std::fs::remove_file(out) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => panic!("{err}"),
        _ => {}
    }
    let request = serde_json::json!({
        "root": s.fx.root(), "worktree": worktree, "target": target, "out": out,
    });
    let mode = Mode::Custom {
        name: WALK_MODE,
        arg: request.to_string(),
    };
    support::helper::command(s.fx.scope(), &mode, s.fx.root())
}

/// The mount and the entries a walk helper wrote to `out`, with `/` between path components
/// on every platform.
fn read_walk(out: &Path) -> (String, Vec<String>) {
    let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(out).unwrap()).unwrap();
    let entries = doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_str().unwrap().replace('\\', "/"))
        .collect();
    (doc["mount"].as_str().unwrap().to_owned(), entries)
}

/// Runs `plan::walk` of `target` in a helper under `binds`, and returns the mount and the
/// entries it found.
#[cfg(target_os = "linux")]
fn walk_with_binds(
    s: &Setup,
    worktree: &Path,
    target: &Path,
    binds: &[(&Path, &Path)],
) -> (String, Vec<String>) {
    let out = s.fx.path("walk.json");
    let output = with_binds(s, binds, walk_command(s, worktree, target, &out));
    assert!(
        output.status.success(),
        "the walk in the namespace: {output:?}"
    );
    read_walk(&out)
}

/// Runs `plan::walk` of `target` in a helper paused at `point`, calls `change` once the
/// helper has reached it, releases it, and returns the mount and the entries it found.
fn walk_with_change_at(
    s: &Setup,
    worktree: &Path,
    target: &Path,
    point: &str,
    change: impl FnOnce(),
) -> (String, Vec<String>) {
    let scope = Scope::new();
    let points = TestPoints::new(&scope);
    let out = s.fx.path("walk.json");
    let mut command = walk_command(s, worktree, target, &out);
    points.apply(&mut command, &[point], None);
    let mut child = command.spawn().unwrap();
    points.wait_reached(point, child.id(), WAIT);
    change();
    points.release(point);
    assert!(child.wait().unwrap().success());
    read_walk(&out)
}

/// Runs `plan::walk` of `target` in a helper in a mount namespace of its own, paused at
/// `point`; once the helper has reached it, bind-mounts `source` over `over` in that
/// namespace and releases it. Returns the mount and the entries the walk found.
#[cfg(target_os = "linux")]
fn walk_with_bind_at(
    s: &Setup,
    (worktree, target): (&Path, &Path),
    point: &str,
    (source, over): (&Path, &Path),
) -> (String, Vec<String>) {
    let scope = Scope::new();
    let points = TestPoints::new(&scope);
    let out = s.fx.path("walk.json");
    let mut walk = walk_command(s, worktree, target, &out);
    points.apply(&mut walk, &[point], None);
    s.fx.scope().mount_namespace(&mut walk, &[]);
    let mut child = walk.spawn().unwrap();
    points.wait_reached(point, child.id(), WAIT);
    s.fx.scope().bind_mount(&child, source, over).unwrap();
    points.release(point);
    assert!(child.wait().unwrap().success());
    read_walk(&out)
}

/// A7 on Linux: a bind mount on the same filesystem is a mount point. `act` rejects a target
/// that is one, and the snapshot does not enter one below its target: what it would free
/// leaves out the bound data. The bound data survives both. `plan`'s walk by path tells the
/// same mounts.
#[cfg(target_os = "linux")]
#[test]
fn bind_mounts_are_not_entered() {
    let s = Setup::new();
    if !mount_namespace(&s, "bind_mounts_are_not_entered") {
        return;
    }
    let owner = s.fx.path("owner");
    let big = "x".repeat(1 << 20);
    write(&owner, "data/keep", &big);
    let wt = s.worktree("bound");
    write(&wt, "target/debug/app", "bin");
    write(&wt, "node_modules/pkg/index.js", "x");
    std::fs::create_dir_all(wt.join("node_modules/cache")).unwrap();
    let data = owner.join("data");
    let before = plan_support::listing(&wt);

    let target = wt.join("target");
    let plan = s.plan(PlanKind::ReclaimDir, &target);
    let result = act_with_binds(&s, &plan.id, &[(&data, &target)]);
    assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
    assert_eq!(result.reasons, ["plain_target"]);

    let cache = wt.join("node_modules/cache");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"));
    let result = act_with_binds(&s, &plan.id, &[(&data, &cache)]);
    assert_eq!(result.status, ActStatus::DryRun, "{result:#?}");
    assert!(
        result.freed_bytes.unwrap() < big.len() as u64,
        "the bound data is not counted: {result:#?}"
    );

    assert_eq!(std::fs::read_to_string(data.join("keep")).unwrap(), big);
    assert_eq!(plan_support::listing(&wt), before, "nothing touched");

    let (mount, entries) = walk_with_binds(&s, &wt, &target, &[(&data, &target)]);
    assert_eq!(mount, "Boundary");
    assert!(entries.is_empty(), "{entries:?}");
    let (mount, entries) = walk_with_binds(&s, &wt, &wt.join("node_modules"), &[(&data, &cache)]);
    assert_eq!(mount, "Same");
    assert!(
        entries.contains(&"node_modules/cache".to_owned())
            && entries.contains(&"node_modules/pkg/index.js".to_owned())
            && !entries.iter().any(|e| e.starts_with("node_modules/cache/")),
        "the bind mount is listed, not entered: {entries:?}"
    );
    ran_in_namespace("bind_mounts_are_not_entered");
}

/// Whether a directory was opened and whether its entries were read since [`Watch::new`],
/// from inotify, which reports both however the directory is reached (through a bind mount
/// in another mount namespace included).
#[cfg(target_os = "linux")]
struct Watch {
    inotify: std::os::fd::OwnedFd,
}

#[cfg(target_os = "linux")]
impl Watch {
    fn new(dir: &Path) -> Self {
        use rustix::fs::inotify::{self, CreateFlags, WatchFlags};
        let inotify = inotify::init(CreateFlags::NONBLOCK | CreateFlags::CLOEXEC).unwrap();
        inotify::add_watch(&inotify, dir, WatchFlags::OPEN | WatchFlags::ACCESS).unwrap();
        Self { inotify }
    }

    /// `(opened, listed)` for the directory itself, from the events queued so far.
    fn seen(&self) -> (bool, bool) {
        use rustix::fs::inotify::{ReadFlags, Reader};
        let mut buf = [std::mem::MaybeUninit::uninit(); 4096];
        let mut events = Reader::new(&self.inotify, &mut buf);
        let (mut opened, mut listed) = (false, false);
        loop {
            match events.next() {
                // An event on an entry inside carries its name; one on the directory does not.
                Ok(event) if event.file_name().is_none() => {
                    opened |= event.events().contains(ReadFlags::OPEN);
                    listed |= event.events().contains(ReadFlags::ACCESS);
                }
                Ok(_) => {}
                Err(rustix::io::Errno::AGAIN) => return (opened, listed),
                Err(err) => panic!("read the inotify events: {err}"),
            }
        }
    }
}

/// A7 on Linux: a bind mount below the target is told by its mount before it is read. `plan`'s
/// walk neither opens nor lists it; `act`'s snapshot opens it to read its mount and does not
/// list it.
#[cfg(target_os = "linux")]
#[test]
fn bind_mounts_are_not_listed() {
    let s = Setup::new();
    if !mount_namespace(&s, "bind_mounts_are_not_listed") {
        return;
    }
    let data = s.fx.path("owner/data");
    write(&data, "keep", "x");
    let wt = s.worktree("bound");
    write(&wt, "node_modules/pkg/index.js", "x");
    let cache = wt.join("node_modules").join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let target = wt.join("node_modules");
    let plan = s.plan(PlanKind::ReclaimDir, &target);

    let watch = Watch::new(&data);
    let (mount, entries) = walk_with_binds(&s, &wt, &target, &[(&data, &cache)]);
    assert_eq!(mount, "Same");
    assert!(
        entries.contains(&"node_modules/cache".to_owned()),
        "{entries:?}"
    );
    assert_eq!(
        watch.seen(),
        (false, false),
        "the walk neither opens nor lists the bind mount"
    );

    let result = act_with_binds(&s, &plan.id, &[(&data, &cache)]);
    assert_eq!(result.status, ActStatus::DryRun, "{result:#?}");
    assert!(!watch.seen().1, "the snapshot does not list the bind mount");
    ran_in_namespace("bind_mounts_are_not_listed");
}

/// A target, and a directory below it, set up for a walk race: `walk-<n>/node_modules` holds
/// `pkg/index.js`, and `outside-<n>`, which is outside the worktree, holds `secret`. Returns
/// the worktree, the target, and the outside directory.
#[cfg(unix)]
fn walk_race(s: &Setup, n: usize) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let wt = s.fx.path(&format!("walk-{n}"));
    write(&wt, "node_modules/pkg/index.js", "x");
    let outside = s.fx.path(&format!("outside-{n}"));
    write(&outside, "secret", "x");
    let target = wt.join("node_modules");
    (wt, target, outside)
}

/// What a walk race at `point` replaces: the target itself at `walk.target_checked`, its only
/// subdirectory `pkg` at `walk.dir_checked`.
#[cfg(unix)]
fn replaced_at(point: &str, target: &Path) -> std::path::PathBuf {
    if point == cardinal::testpoint::WALK_TARGET_CHECKED {
        target.to_path_buf()
    } else {
        target.join("pkg")
    }
}

/// Whether `plan`'s walk found what it must after a race at `point`: a replaced target is a
/// boundary with nothing below it read, and a replaced directory below the target is recorded
/// as the walk saw it and not listed. Either way nothing of the replacement is in the walk.
#[cfg(unix)]
fn replacement_left_out(point: &str, found: &(String, Vec<String>)) -> Result<(), String> {
    let expected = if point == cardinal::testpoint::WALK_TARGET_CHECKED {
        ("Boundary", &[][..])
    } else {
        ("Same", &["node_modules/pkg"][..])
    };
    if found.0 == expected.0 && found.1 == expected.1 {
        Ok(())
    } else {
        Err(format!("{found:?}, expected {expected:?}"))
    }
}

/// The walk points at which a race replaces a directory: the target after its mount check,
/// and a directory below it after its mount lookup, both before the directory is opened.
#[cfg(unix)]
const WALK_POINTS: [&str; 2] = [
    cardinal::testpoint::WALK_TARGET_CHECKED,
    cardinal::testpoint::WALK_DIR_CHECKED,
];

/// A7: a directory replaced after `plan`'s walk recorded it and checked its mount, before
/// the walk lists it, by a symbolic link to a directory outside the worktree or by that
/// directory moved in: the walk lists neither.
#[cfg(unix)]
#[test]
fn walk_lists_only_the_directories_it_recorded() {
    let s = Setup::new();
    let cases = WALK_POINTS
        .into_iter()
        .flat_map(|point| [(point, "a symbolic link"), (point, "another directory")]);
    let mut failed = Vec::new();
    for (n, (point, replacement)) in cases.enumerate() {
        let (wt, target, outside) = walk_race(&s, n);
        let replaced = replaced_at(point, &target);
        let found = walk_with_change_at(&s, &wt, &target, point, || {
            let mut aside = replaced.clone().into_os_string();
            aside.push(".old");
            std::fs::rename(&replaced, aside).unwrap();
            if replacement == "a symbolic link" {
                std::os::unix::fs::symlink(&outside, &replaced).unwrap();
            } else {
                std::fs::rename(&outside, &replaced).unwrap();
            }
        });
        if let Err(err) = replacement_left_out(point, &found) {
            failed.push(format!("{point}, {replacement}: {err}"));
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

/// A directory removed after `plan`'s walk checked it and before the walk lists it: the walk
/// records it as it saw it and goes on to the next one.
#[test]
fn walk_records_a_directory_removed_before_it_is_listed() {
    let s = Setup::new();
    let wt = s.fx.path("removed-first");
    write(&wt, "node_modules/a/index.js", "x");
    write(&wt, "node_modules/b/index.js", "x");
    let target = wt.join("node_modules");
    // The walk pauses at its first directory below the target, `a`, before it opens it.
    let (mount, entries) = walk_with_change_at(
        &s,
        &wt,
        &target,
        cardinal::testpoint::WALK_DIR_CHECKED,
        || std::fs::remove_dir_all(target.join("a")).unwrap(),
    );
    assert_eq!(mount, "Same");
    assert_eq!(
        entries,
        [
            "node_modules/a",
            "node_modules/b",
            "node_modules/b/index.js"
        ]
    );
}

/// A directory removed after `plan`'s walk listed its parent, while the walk is below an
/// earlier one: the walk records it as it saw it and goes on.
#[test]
fn walk_records_a_directory_removed_after_it_was_listed() {
    let s = Setup::new();
    let wt = s.fx.path("removed");
    write(&wt, "node_modules/a/index.js", "x");
    write(&wt, "node_modules/b/index.js", "x");
    let target = wt.join("node_modules");
    // The walk pauses at its first directory below the target, `a`, before it opens it.
    let (mount, entries) = walk_with_change_at(
        &s,
        &wt,
        &target,
        cardinal::testpoint::WALK_DIR_CHECKED,
        || std::fs::remove_dir_all(target.join("b")).unwrap(),
    );
    assert_eq!(mount, "Same");
    assert_eq!(
        entries,
        [
            "node_modules/a",
            "node_modules/a/index.js",
            "node_modules/b"
        ]
    );
}

/// A7 on Linux: a directory outside the worktree bind-mounted over the target, or over a
/// directory below it, after `plan`'s walk checked its mount and before the walk lists it:
/// the walk does not list the mount.
#[cfg(target_os = "linux")]
#[test]
fn walk_does_not_list_a_mount_made_after_its_check() {
    let s = Setup::new();
    let test = "walk_does_not_list_a_mount_made_after_its_check";
    if !mount_namespace(&s, test) {
        return;
    }
    let mut failed = Vec::new();
    for (n, point) in WALK_POINTS.into_iter().enumerate() {
        let (wt, target, outside) = walk_race(&s, n);
        let over = replaced_at(point, &target);
        let found = walk_with_bind_at(&s, (&wt, &target), point, (&outside, &over));
        if let Err(err) = replacement_left_out(point, &found) {
            failed.push(format!("{point}: {err}"));
        }
        assert!(outside.join("secret").exists());
    }
    assert!(failed.is_empty(), "{failed:#?}");
    ran_in_namespace(test);
}
