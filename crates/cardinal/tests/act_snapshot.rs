//! Snapshot inspection (A7, C13, C14; `docs/architecture.md` section 9.5): `act` inspects
//! the target before its checks, observes the worktree again after the snapshot, and on Linux
//! does not enter a bind mount below the target or accept a target that is one. A race pauses
//! `act` in a helper process at its test point, makes the change once the point is reached,
//! and releases it.
//!
//! Observation comes from the fake in `tests/fakes` until the integration gate; it reads
//! HEAD and branch from the worktree's git files, as git would.

mod fakes;
mod support;

#[cfg(target_os = "linux")]
use std::fmt::Write as _;
#[cfg(target_os = "linux")]
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

/// Runs `inner` inside a private mount namespace where each `(source, target)` of `binds` is
/// bind-mounted first. The mounts are undone after `inner`, and end with the namespace on
/// every other path (a failed mount, a panic, a kill).
#[cfg(target_os = "linux")]
fn with_binds(s: &Setup, flags: &[&str], binds: &[(&Path, &Path)], inner: &std::process::Command) {
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
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
    let mut command = std::process::Command::new("unshare");
    command
        .args(flags)
        .args(["sh", "-c", &script, "sh"])
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

/// Runs `act` of `plan` in a helper under `binds` (see [`with_binds`]).
#[cfg(target_os = "linux")]
fn act_with_binds(s: &Setup, flags: &[&str], plan: &str, binds: &[(&Path, &Path)]) -> ActResult {
    let out = s.fx.path(&format!("result-{plan}.json"));
    with_binds(s, flags, binds, &s.act_command(plan, &arbiter(), &out));
    fakes::read_result(&out)
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
    let request = serde_json::json!({
        "root": s.fx.root(), "worktree": worktree, "target": target, "out": out,
    });
    let mut walk = std::process::Command::new(std::env::current_exe().unwrap());
    walk.args(["helper", "--exact", "--nocapture", "--test-threads=1"])
        .env(WALK_ENV, request.to_string());
    with_binds(s, flags, binds, &walk);
    let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    let entries = doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_str().unwrap().to_owned())
        .collect();
    (doc["mount"].as_str().unwrap().to_owned(), entries)
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
