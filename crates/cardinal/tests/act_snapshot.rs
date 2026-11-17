//! Snapshot inspection (A7, C13, C14; `docs/architecture.md` sections 9.2 and 9.5): `act`
//! inspects the target before its checks, observes the worktree again after the snapshot, and
//! on Linux does not enter a bind mount below the target or accept a target that is one;
//! `plan`'s walk lists only the directories it recorded. A race pauses `act` or the walk in a
//! helper process at its test point, makes the change once the point is reached, and releases
//! it.
//!
//! Observation comes from the fake in `tests/fakes` until the integration gate; it reads
//! HEAD and branch from the worktree's git files, as git would.

mod fakes;
mod support;

#[cfg(target_os = "linux")]
use std::fmt::Write as _;
#[cfg(unix)]
use std::path::Path;
use std::time::Duration;

use cardinal::contract::{ActResult, ActStatus, IgnoredClass, PlanKind};
use fakes::{Setup, arbiter, write};
use support::helper::ChildGuard;
use support::testpoints::TestPoints;

#[test]
fn helper() {
    fakes::run_act_if_requested();
    walk_if_requested();
}

const WALK_ENV: &str = "CARDINAL_TEST_WALK";

/// Runs the `plan::walk` a helper process was started for, writes the mount and the entries
/// it found, and exits; returns at once otherwise.
fn walk_if_requested() {
    let Ok(request) = std::env::var(WALK_ENV) else {
        return;
    };
    let request: serde_json::Value = serde_json::from_str(&request).expect("a walk request");
    let path = |key: &str| std::path::PathBuf::from(request[key].as_str().expect(key));
    for key in ["worktree", "target", "out"] {
        fakes::assert_inside(&path("root"), &path(key), key);
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

/// Runs `act` of `plan` in a helper paused at `point`, calls `change` once the helper has
/// reached it, releases it, and returns its result.
fn act_with_change_at(s: &Setup, plan: &str, point: &str, change: impl FnOnce()) -> ActResult {
    let points = TestPoints::new();
    let out = points.dir().join("result.json");
    let mut command = s.act_command(plan, &arbiter(), &out);
    points.apply(&mut command, &[point], None);
    let mut child = ChildGuard::new(command.spawn().unwrap());
    points.wait_reached(point, child.id(), WAIT);
    change();
    points.release(point);
    assert!(child.child().wait().unwrap().success());
    fakes::read_result(&out)
}

/// A tracked file edited after the snapshot, before the checks: the checks see the worktree
/// dirty and nothing is touched.
#[test]
fn edit_before_checks_is_rejected() {
    let s = Setup::new();
    let wt = s.worktree("edited", &[("node_modules", IgnoredClass::Regenerable)]);
    write(&wt, "node_modules/pkg/index.js", "x");
    let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
    let result = act_with_change_at(&s, &plan.id, cardinal::testpoint::ACT_SNAPSHOTTED, || {
        write(&wt, "src/lib.rs", "// edited after the snapshot\n");
        // What `git status` now reports.
        s.fake
            .update(&wt, |w| w.facts.dirty = cardinal::contract::Tri::Yes);
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
        let wt = s.worktree("switched", &[("node_modules", IgnoredClass::Regenerable)]);
        write(&wt, "node_modules/pkg/index.js", "x");
        let target = match kind {
            PlanKind::ReclaimWorktree => wt.clone(),
            PlanKind::ReclaimDir => wt.join("node_modules"),
        };
        let plan = s.plan(kind, &target);
        s.fx.git(&wt, &["switch", "-q", "-c", &format!("other-{n}")]);
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

/// The flags that give `unshare` a private mount namespace where `mount --bind` works:
/// `-m` as root, `-r -m` (a user namespace) otherwise. Where neither does, `test` is skipped
/// with the reason reported, and fails instead where `CI` is `true`, so a CI run never passes
/// it without running it. The probe's mount ends with its namespace.
#[cfg(target_os = "linux")]
fn mount_namespace(s: &Setup, test: &str) -> Option<&'static [&'static str]> {
    let probe = s.fx.path("probe");
    std::fs::create_dir_all(&probe).unwrap();
    let mut tried = String::new();
    for flags in [&["-m"][..], &["-r", "-m"]] {
        let mut command = std::process::Command::new("unshare");
        command
            .args(flags)
            .args(["mount", "--bind"])
            .arg(&probe)
            .arg(&probe);
        let flags_shown = flags.join(" ");
        match fakes::confined(&s.fx, command, s.fx.root()).output() {
            Ok(probed) if probed.status.success() => return Some(flags),
            Ok(probed) => {
                let _ = writeln!(
                    tried,
                    "`unshare {flags_shown} mount --bind`: {}: {}",
                    probed.status,
                    String::from_utf8_lossy(&probed.stderr).trim()
                );
            }
            Err(err) => {
                let _ = writeln!(tried, "`unshare {flags_shown}`: {err}");
            }
        }
    }
    let reason = format!("{test}: no private mount namespace for a bind mount\n{tried}");
    assert!(
        std::env::var("CI").as_deref() != Ok("true"),
        "CI runs every bind-mount test: {reason}"
    );
    report(&format!("skipped {reason}"));
    None
}

/// Reports that `test` ran its assertions in a namespace made with `flags`.
#[cfg(target_os = "linux")]
fn ran_in_namespace(test: &str, flags: &[&str]) {
    report(&format!(
        "{test}: ran in a private mount namespace (unshare {})",
        flags.join(" ")
    ));
}

/// `path` quoted for `sh`.
#[cfg(target_os = "linux")]
fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// Runs `script` with `sh` inside a private mount namespace, with `inner`'s program and
/// arguments as its positional parameters and `inner`'s environment. Mounts made there end
/// with the namespace on every path (a failed mount, a panic, a kill).
#[cfg(target_os = "linux")]
fn in_namespace(s: &Setup, flags: &[&str], script: &str, inner: &std::process::Command) {
    let mut command = std::process::Command::new("unshare");
    command
        .args(flags)
        .args(["sh", "-c", script, "sh"])
        .arg(inner.get_program())
        .args(inner.get_args());
    for (key, value) in inner.get_envs() {
        if let Some(value) = value {
            command.env(key, value);
        }
    }
    let status = fakes::confined(&s.fx, command, s.fx.root())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "the helper in the namespace: {status}");
}

/// Runs `inner` inside a private mount namespace where each `(source, target)` of `binds` is
/// bind-mounted first. The mounts are undone after `inner`, and end with the namespace on
/// every other path.
#[cfg(target_os = "linux")]
fn with_binds(s: &Setup, flags: &[&str], binds: &[(&Path, &Path)], inner: &std::process::Command) {
    let mut mounts = String::new();
    let mut unmounts = String::new();
    for (source, target) in binds {
        let _ = writeln!(
            mounts,
            "mount --bind {} {} || exit 97",
            quote(source),
            quote(target)
        );
        unmounts.insert_str(0, &format!("umount {}\n", quote(target)));
    }
    let script = format!("{mounts}\"$@\"\nstatus=$?\n{unmounts}exit $status\n");
    in_namespace(s, flags, &script, inner);
}

/// Runs `act` of `plan` in a helper under `binds` (see [`with_binds`]).
#[cfg(target_os = "linux")]
fn act_with_binds(s: &Setup, flags: &[&str], plan: &str, binds: &[(&Path, &Path)]) -> ActResult {
    let out = s.fx.path(&format!("result-{plan}.json"));
    with_binds(s, flags, binds, &s.act_command(plan, &arbiter(), &out));
    fakes::read_result(&out)
}

/// A helper process (this test binary, see [`walk_if_requested`]) that runs `plan::walk` of
/// `target` and writes what it found to `out`, a fresh file.
#[cfg(unix)]
fn walk_command(s: &Setup, worktree: &Path, target: &Path, out: &Path) -> std::process::Command {
    match std::fs::remove_file(out) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => panic!("{err}"),
        _ => {}
    }
    let request = serde_json::json!({
        "root": s.fx.root(), "worktree": worktree, "target": target, "out": out,
    });
    let mut walk = std::process::Command::new(std::env::current_exe().unwrap());
    walk.args(["helper", "--exact", "--nocapture", "--test-threads=1"])
        .env(WALK_ENV, request.to_string());
    walk
}

/// The mount and the entries a walk helper wrote to `out`.
#[cfg(unix)]
fn read_walk(out: &Path) -> (String, Vec<String>) {
    let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(out).unwrap()).unwrap();
    let entries = doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_str().unwrap().to_owned())
        .collect();
    (doc["mount"].as_str().unwrap().to_owned(), entries)
}

/// Runs `plan::walk` of `target` in a helper under `binds`, and returns the mount and the
/// entries it found.
#[cfg(target_os = "linux")]
fn walk_with_binds(
    s: &Setup,
    flags: &[&str],
    worktree: &Path,
    target: &Path,
    binds: &[(&Path, &Path)],
) -> (String, Vec<String>) {
    let out = s.fx.path("walk.json");
    with_binds(s, flags, binds, &walk_command(s, worktree, target, &out));
    read_walk(&out)
}

/// Runs `plan::walk` of `target` in a helper paused at `point`, calls `change` once the
/// helper has reached it, releases it, and returns the mount and the entries it found.
#[cfg(unix)]
fn walk_with_change_at(
    s: &Setup,
    worktree: &Path,
    target: &Path,
    point: &str,
    change: impl FnOnce(),
) -> (String, Vec<String>) {
    let points = TestPoints::new();
    let out = s.fx.path("walk.json");
    let mut command = walk_command(s, worktree, target, &out);
    points.apply(&mut command, &[point], None);
    let mut child = ChildGuard::new(
        fakes::confined(&s.fx, command, s.fx.root())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    points.wait_reached(point, child.id(), WAIT);
    change();
    points.release(point);
    assert!(child.child().wait().unwrap().success());
    read_walk(&out)
}

/// Runs `plan::walk` of `target` in a helper inside a private mount namespace, paused at
/// `point`; once the helper has reached it, bind-mounts `source` over `over` in that
/// namespace and releases it. Returns the mount and the entries the walk found.
#[cfg(target_os = "linux")]
fn walk_with_bind_at(
    s: &Setup,
    flags: &[&str],
    (worktree, target): (&Path, &Path),
    point: &str,
    (source, over): (&Path, &Path),
) -> (String, Vec<String>) {
    use cardinal::testpoint::release_file;
    let points = TestPoints::new();
    let out = s.fx.path("walk.json");
    let mut walk = walk_command(s, worktree, target, &out);
    points.apply(&mut walk, &[point], None);
    // The helper's reached file is `<dir>/<point>.<pid>.reached`. It is waited for 20 s at
    // most; a paused helper aborts on its own after its pause limit.
    let script = format!(
        r#""$@" &
pid=$!
reached={dir}/'{point}'."$pid".reached
n=0
until [ -e "$reached" ]; do
    n=$((n + 1))
    if [ "$n" -gt 2000 ]; then kill "$pid"; exit 96; fi
    sleep 0.01
done
mount --bind {source} {over} || {{ kill "$pid"; exit 97; }}
: > {release}
wait "$pid"
status=$?
umount {over}
exit "$status"
"#,
        dir = quote(points.dir()),
        source = quote(source),
        over = quote(over),
        release = quote(&release_file(points.dir(), point)),
    );
    in_namespace(s, flags, &script, &walk);
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
    let Some(flags) = mount_namespace(&s, "bind_mounts_are_not_entered") else {
        return;
    };
    let owner = s.fx.path("owner");
    let big = "x".repeat(1 << 20);
    write(&owner, "data/keep", &big);
    let wt = s.worktree(
        "bound",
        &[
            ("target", IgnoredClass::Regenerable),
            ("node_modules", IgnoredClass::Regenerable),
        ],
    );
    write(&wt, "target/debug/app", "bin");
    write(&wt, "node_modules/pkg/index.js", "x");
    std::fs::create_dir_all(wt.join("node_modules/cache")).unwrap();
    let data = owner.join("data");
    let before = fakes::listing(&wt);

    let target = wt.join("target");
    let plan = s.plan(PlanKind::ReclaimDir, &target);
    let result = act_with_binds(&s, flags, &plan.id, &[(&data, &target)]);
    assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
    assert_eq!(result.reasons, ["plain_target"]);

    let cache = wt.join("node_modules/cache");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("node_modules"));
    let result = act_with_binds(&s, flags, &plan.id, &[(&data, &cache)]);
    assert_eq!(result.status, ActStatus::DryRun, "{result:#?}");
    assert!(
        result.freed_bytes.unwrap() < big.len() as u64,
        "the bound data is not counted: {result:#?}"
    );

    assert_eq!(std::fs::read_to_string(data.join("keep")).unwrap(), big);
    assert_eq!(fakes::listing(&wt), before, "nothing touched");

    let (mount, entries) = walk_with_binds(&s, flags, &wt, &target, &[(&data, &target)]);
    assert_eq!(mount, "Boundary");
    assert!(entries.is_empty(), "{entries:?}");
    let (mount, entries) =
        walk_with_binds(&s, flags, &wt, &wt.join("node_modules"), &[(&data, &cache)]);
    assert_eq!(mount, "Same");
    assert!(
        entries.contains(&"node_modules/cache".to_owned())
            && entries.contains(&"node_modules/pkg/index.js".to_owned())
            && !entries.iter().any(|e| e.starts_with("node_modules/cache/")),
        "the bind mount is listed, not entered: {entries:?}"
    );
    ran_in_namespace("bind_mounts_are_not_entered", flags);
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
    let Some(flags) = mount_namespace(&s, "bind_mounts_are_not_listed") else {
        return;
    };
    let data = s.fx.path("owner/data");
    write(&data, "keep", "x");
    let wt = s.worktree("bound", &[("node_modules", IgnoredClass::Regenerable)]);
    write(&wt, "node_modules/pkg/index.js", "x");
    let cache = wt.join("node_modules").join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let target = wt.join("node_modules");
    let plan = s.plan(PlanKind::ReclaimDir, &target);

    let watch = Watch::new(&data);
    let (mount, entries) = walk_with_binds(&s, flags, &wt, &target, &[(&data, &cache)]);
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

    let result = act_with_binds(&s, flags, &plan.id, &[(&data, &cache)]);
    assert_eq!(result.status, ActStatus::DryRun, "{result:#?}");
    assert!(!watch.seen().1, "the snapshot does not list the bind mount");
    ran_in_namespace("bind_mounts_are_not_listed", flags);
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

/// A directory removed after `plan`'s walk listed its parent, while the walk is below an
/// earlier one: the walk records it as it saw it and goes on.
#[cfg(unix)]
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
    let Some(flags) = mount_namespace(&s, test) else {
        return;
    };
    let mut failed = Vec::new();
    for (n, point) in WALK_POINTS.into_iter().enumerate() {
        let (wt, target, outside) = walk_race(&s, n);
        let over = replaced_at(point, &target);
        let found = walk_with_bind_at(&s, flags, (&wt, &target), point, (&outside, &over));
        if let Err(err) = replacement_left_out(point, &found) {
            failed.push(format!("{point}: {err}"));
        }
        assert!(outside.join("secret").exists());
    }
    assert!(failed.is_empty(), "{failed:#?}");
    ran_in_namespace(test, flags);
}
