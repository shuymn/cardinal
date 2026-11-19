//! Unit X's execution tests (`docs/architecture.md` section 13.7): each refusal before the
//! move, alone, beside a control on which every check passes and the arbiter's `act`, which
//! holds no grant, reports `dry_run` with `execution_not_granted`, and the plan's and the dry
//! run's readings of the freed bytes.

#![cfg(unix)]

mod live_support;
mod observe_common;
mod plan_support;
mod support;

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use cardinal::contract::{ActResult, ActStatus, Check, CheckResult, Plan, PlanKind};
use live_support::{GRANT, Live, owner};
use plan_support::{RUN_LIMIT, Setup, arbiter, write_synced};

/// `cardinal plan reclaim-dir TARGET`, reading the fakes in `fakes` (`CARDINAL_TEST_DIR`)
/// when given.
fn plan(s: &Setup, target: &Path, fakes: Option<&Path>) -> Plan {
    let Some(dir) = fakes else {
        return s.plan(PlanKind::ReclaimDir, target);
    };
    let output = s
        .cardinal()
        .env("CARDINAL_TEST_DIR", dir)
        .args(["plan", "reclaim-dir"])
        .arg(target)
        .output_within(RUN_LIMIT)
        .expect("run cardinal plan");
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).expect("a plan")
}

/// `cardinal act` on `plan` as the arbiter, reading the fakes in `fakes` when given.
fn act(s: &Setup, plan: &Plan, fakes: Option<&Path>) -> ActResult {
    let mut command = s.act_command(&plan.id, &arbiter());
    if let Some(dir) = fakes {
        command.env("CARDINAL_TEST_DIR", dir);
    }
    let output = command.output_within(RUN_LIMIT).expect("run cardinal act");
    plan_support::act_result(&output).expect("an act result")
}

/// The checks of `checks` that did not pass.
fn failed(checks: &[Check]) -> Vec<&str> {
    checks
        .iter()
        .filter(|c| c.result != CheckResult::Pass)
        .map(|c| c.name.as_str())
        .collect()
}

/// The control: every check of `target` passes, and `act` reports `dry_run`.
fn assert_passes(s: &Setup, target: &Path, fakes: Option<&Path>) -> (Plan, ActResult) {
    let plan = plan(s, target, fakes);
    assert!(plan.invariants_ok, "{:#?}", plan.checks);
    let result = act(s, &plan, fakes);
    assert_eq!(result.status, ActStatus::DryRun, "{result:#?}");
    assert_eq!(result.reasons, ["execution_not_granted"], "{result:#?}");
    (plan, result)
}

/// The refusal: `target`'s plan fails `check` alone, and `act` refuses it for that reason
/// alone. Returns the check as `act` evaluated it.
fn assert_refused(s: &Setup, target: &Path, check: &str, fakes: Option<&Path>) -> Check {
    let plan = plan(s, target, fakes);
    assert_eq!(failed(&plan.checks), [check], "{:#?}", plan.checks);
    let result = act(s, &plan, fakes);
    assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
    assert_eq!(result.reasons, [check], "{result:#?}");
    result
        .checks
        .into_iter()
        .find(|c| c.name == check)
        .expect("the check")
}

/// Writes `line` to stderr directly, past the test harness's capture.
#[cfg(target_os = "linux")]
fn report(line: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "{line}");
}

/// Reports a test that does not run here, or fails it where `CI` is `true`, so CI never
/// passes a test it did not run.
#[cfg(target_os = "linux")]
fn skip(test: &str, reason: &str) {
    assert!(
        std::env::var("CI").as_deref() != Ok("true"),
        "CI runs {test}: {reason}"
    );
    report(&format!("skipped {test}: {reason}"));
}

/// The refusals of section 13.7 before the move, each alone, each after its control.
mod refusals {
    use std::os::unix::fs::FileTypeExt as _;

    use super::*;

    /// A `sleep` started outside the worktree with `stdin` open on `file`.
    fn hold(s: &Setup, file: &Path) -> cardinal_test_support::Child {
        let mut command = s.fx.command("sleep", s.fx.root());
        command
            .arg("60")
            .stdin(std::fs::File::open(file).expect("open the held file"));
        command.spawn().expect("start sleep")
    }

    /// A process holding a descriptor on a file of the target, with its working directory
    /// outside the worktree, refuses it by `not_in_use`, naming the process and the file.
    #[test]
    fn a_descriptor_on_a_file() {
        let s = Setup::new();
        let wt = s.worktree("held");
        let target = wt.join("target");
        write_synced(&wt, "target/debug/app", b"bin");
        assert_passes(&s, &target, None);

        let mut child = hold(&s, &target.join("debug/app"));
        let check = assert_refused(&s, &target, "not_in_use", None);
        let pid = child.id().to_string();
        assert!(
            check
                .detail
                .contains(&format!("{pid} (descriptor 0, target/debug/app)")),
            "{check:#?}"
        );
        child.kill();
    }

    /// A file of the target with a hard link outside it, held open through that link, does
    /// not count on Linux: removing the target leaves the file to its other name. macOS names
    /// a descriptor by the link looked up last, which the walk makes the one inside, so there
    /// it refuses the target.
    #[test]
    fn a_file_linked_outside_and_held_open_there_does_not_count() {
        let s = Setup::new();
        let wt = s.worktree("linked-held");
        let target = wt.join("target");
        let outside = s.fx.path("outside");
        write_synced(&outside, "shared.bin", b"shared");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::hard_link(outside.join("shared.bin"), target.join("shared.bin")).unwrap();

        let mut child = hold(&s, &outside.join("shared.bin"));
        if cfg!(target_os = "linux") {
            assert_passes(&s, &target, None);
        } else {
            assert_refused(&s, &target, "not_in_use", None);
        }
        child.kill();
    }

    /// A file of the target with a hard link outside it, held open through its name inside,
    /// refuses the target by `not_in_use`, naming the process, and on Linux that name: only a
    /// file held through a name outside is left to that name. macOS names a descriptor's file
    /// by the link looked up last (`F_GETPATH`), which the plan's and `act`'s own reads of the
    /// other link can change, so there the detail names the process alone.
    #[test]
    fn a_file_linked_outside_and_held_open_inside_counts() {
        let s = Setup::new();
        let wt = s.worktree("linked-held-inside");
        let target = wt.join("target");
        write_synced(&wt, "target/debug/held", b"held");
        let outside = s.fx.path("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::hard_link(target.join("debug/held"), outside.join("held")).unwrap();
        assert_passes(&s, &target, None);

        let mut child = hold(&s, &target.join("debug/held"));
        let check = assert_refused(&s, &target, "not_in_use", None);
        let pid = child.id().to_string();
        let named = if cfg!(target_os = "macos") {
            format!("{pid} (descriptor 0, ")
        } else {
            format!("{pid} (descriptor 0, target/debug/held)")
        };
        assert!(check.detail.contains(&named), "{check:#?}");
        child.kill();
    }

    /// A process running an executable from the target refuses it by `not_in_use`.
    #[test]
    fn an_executable() {
        let s = Setup::new();
        let wt = s.worktree("executable");
        let target = wt.join("target");
        std::fs::create_dir_all(target.join("bin")).unwrap();
        std::fs::copy("/bin/sleep", target.join("bin/sleep")).unwrap();
        if cfg!(target_os = "macos") {
            // A copy of a platform binary is killed at exec unless signed again.
            let status =
                s.fx.command("codesign", s.fx.root())
                    .args(["--force", "--sign", "-"])
                    .arg(target.join("bin/sleep"))
                    .status()
                    .expect("run codesign");
            assert!(status.success(), "codesign: {status}");
        }
        assert_passes(&s, &target, None);

        let mut command = s.fx.command(target.join("bin/sleep"), s.fx.root());
        let mut child = command.arg("60").spawn().expect("start the copied sleep");
        let check = assert_refused(&s, &target, "not_in_use", None);
        assert!(
            check
                .detail
                .contains(&format!("{} (executable, target/bin/sleep)", child.id())),
            "{check:#?}"
        );
        child.kill();
    }

    /// A process whose root directory is the target refuses it by `not_in_use` (Linux: a
    /// user namespace of the process's own lets it change its root without privileges). The
    /// process runs `sleep` from outside the worktree, the target holding only the dynamic
    /// loader and libraries it loads after the change.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_root_directory() {
        let s = Setup::new();
        let wt = s.worktree("rooted");
        let target = wt.join("target");
        let bin = s.fx.path("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::copy("/bin/sleep", bin.join("sleep")).unwrap();
        let ldd =
            s.fx.command("ldd", s.fx.root())
                .arg(bin.join("sleep"))
                .output()
                .expect("run ldd");
        assert!(ldd.status.success(), "{ldd:?}");
        for lib in libraries(&String::from_utf8_lossy(&ldd.stdout)) {
            let copy = target.join(lib.strip_prefix("/").unwrap());
            std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
            std::fs::copy(&lib, &copy).unwrap();
        }
        assert_passes(&s, &target, None);

        let mut command = s.fx.command("./sleep", &bin);
        command.arg("60");
        s.fx.scope().chroot(&mut command, &target);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                skip("a_root_directory", &format!("no user namespace: {err}"));
                return;
            }
        };
        let check = assert_refused(&s, &target, "not_in_use", None);
        assert!(
            check
                .detail
                .contains(&format!("{} (root directory, target)", child.id())),
            "{check:#?}"
        );
        child.kill();
        report("a_root_directory: ran in a user namespace");
    }

    /// The absolute paths `ldd` prints: each library it resolved and the dynamic loader.
    #[cfg(target_os = "linux")]
    fn libraries(ldd: &str) -> Vec<PathBuf> {
        ldd.lines()
            .filter_map(|line| {
                let path = match line.split_once("=>") {
                    Some((_, resolved)) => resolved.split_whitespace().next()?,
                    None => line.split_whitespace().next()?,
                };
                path.starts_with('/').then(|| PathBuf::from(path))
            })
            .collect()
    }

    /// A FIFO in the target refuses it by `plain_target`, naming it.
    #[test]
    fn a_fifo() {
        let s = Setup::new();
        let wt = s.worktree("fifo");
        let target = wt.join("target");
        write_synced(&wt, "target/debug/app", b"bin");
        assert_passes(&s, &target, None);

        let fifo = std::ffi::CString::new(target.join("debug/pipe").as_os_str().as_encoded_bytes())
            .unwrap();
        // SAFETY: a NUL-terminated path.
        let made = unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) };
        assert_eq!(made, 0, "mkfifo: {}", std::io::Error::last_os_error());
        let check = assert_refused(&s, &target, "plain_target", None);
        assert!(check.detail.contains("target/debug/pipe"), "{check:#?}");
    }

    /// A socket left in the target by a server that is gone refuses it by `plain_target`,
    /// naming it, so the owner can remove it.
    #[test]
    fn a_socket() {
        let s = Setup::new();
        let wt = s.worktree("socket");
        let target = wt.join("target");
        write_synced(&wt, "target/debug/app", b"bin");
        assert_passes(&s, &target, None);

        // Bound by a relative name from inside the target: a socket's path is short.
        let status =
            s.fx.command("perl", &target.join("debug"))
                .args([
                    "-MIO::Socket::UNIX",
                    "-e",
                    "IO::Socket::UNIX->new(Local => 'server.sock', Listen => 1) or die qq($!)",
                ])
                .status()
                .expect("run perl");
        assert!(status.success(), "perl: {status}");
        assert!(
            std::fs::symlink_metadata(target.join("debug/server.sock"))
                .unwrap()
                .file_type()
                .is_socket()
        );
        let check = assert_refused(&s, &target, "plain_target", None);
        assert!(
            check.detail.contains("target/debug/server.sock"),
            "{check:#?}"
        );
    }

    /// A device in the target refuses it by `plain_target`, naming it. Making a device needs
    /// root, so the walks read a regular file as one (the fake of section 13.7).
    #[test]
    fn a_device() {
        let s = Setup::new();
        let wt = s.worktree("device");
        let target = wt.join("target");
        write_synced(&wt, "target/debug/app", b"bin");
        write_synced(&wt, "target/dev0", b"");
        let fakes = s.fx.path("fakes");
        std::fs::create_dir_all(&fakes).unwrap();
        assert_passes(&s, &target, Some(&fakes));

        std::fs::write(
            fakes.join("entry-types"),
            format!("device\t{}\n", target.join("dev0").display()),
        )
        .unwrap();
        let check = assert_refused(&s, &target, "plain_target", Some(&fakes));
        assert!(check.detail.contains("target/dev0"), "{check:#?}");
    }

    /// A bare repository (`HEAD`, `objects/`, and `refs/`) at the target's root, and one
    /// below it, each refuse it by `no_nested_repository`.
    #[test]
    fn a_bare_repository_at_the_root_and_below() {
        let s = Setup::new();
        for (name, at) in [("bare-root", ""), ("bare-below", "vendor/repo.git/")] {
            let wt = s.worktree(name);
            let target = wt.join("target");
            write_synced(&wt, "target/debug/app", b"bin");
            write_synced(&wt, &format!("target/{at}HEAD"), b"ref: refs/heads/main\n");
            std::fs::create_dir_all(target.join(at).join("objects")).unwrap();
            assert_passes(&s, &target, None);

            std::fs::create_dir_all(target.join(at).join("refs")).unwrap();
            let check = assert_refused(&s, &target, "no_nested_repository", None);
            let named = Path::new("target").join(at);
            let named = named.to_str().unwrap().trim_end_matches('/');
            assert!(check.detail.contains(named), "{at}: {check:#?}");
        }
    }

    /// A target on a file system the descriptor scan cannot see into, `nfs` or `fuse` as
    /// the walks read it (the fake of section 13.7), refuses it by `local_volume`.
    #[test]
    fn an_injected_file_system_type() {
        let s = Setup::new();
        let wt = s.worktree("remote");
        let target = wt.join("target");
        write_synced(&wt, "target/debug/app", b"bin");
        let fakes = s.fx.path("fakes");
        std::fs::create_dir_all(&fakes).unwrap();
        assert_passes(&s, &target, Some(&fakes));

        for ty in ["nfs", "fuse"] {
            std::fs::write(
                fakes.join("fs-types"),
                format!("{ty}\t{}\n", target.display()),
            )
            .unwrap();
            let check = assert_refused(&s, &target, "local_volume", Some(&fakes));
            assert!(check.detail.contains(ty), "{check:#?}");
        }
    }
}

/// The plan's and the dry run's readings of the freed-bytes bound (section 13.7).
mod freed_bytes {
    use super::*;
    use cardinal::contract::ExclusiveMeasure;

    /// The bytes allocated to the file at `path`.
    fn allocated(path: &Path) -> u64 {
        std::fs::symlink_metadata(path).unwrap().blocks() * 512
    }

    /// Plans `target`, asserts its bound is `bytes` under `measure`, and that a dry run
    /// reports it as `freed_bytes`.
    fn assert_bound(s: &Setup, target: &Path, measure: ExclusiveMeasure, bytes: u64) -> Plan {
        let (plan, result) = assert_passes(s, target, None);
        assert_eq!(
            (plan.exclusive_measure, plan.exclusive_bytes),
            (Some(measure), Some(bytes)),
            "{plan:#?}"
        );
        assert_eq!(result.freed_bytes, Some(bytes), "{result:#?}");
        plan
    }

    /// A linked worktree at `rel` inside the fixture on a new branch `branch`, meeting every
    /// reclaim condition as [`Setup::worktree`] makes one.
    #[cfg(target_os = "macos")]
    pub(super) fn worktree_at(s: &Setup, rel: &str, branch: &str) -> PathBuf {
        let wt = s.fx.worktree_add(&s.main, rel, branch);
        s.fx.commit_file(&wt, "src/lib.rs", &format!("// {branch}\n"), "work");
        s.publish(&wt, branch);
        s.merge(&wt);
        wt
    }

    /// Clones `from` to `to` (`clonefile`).
    #[cfg(target_os = "macos")]
    fn clone(from: &Path, to: &Path) {
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        let from = std::ffi::CString::new(from.as_os_str().as_encoded_bytes()).unwrap();
        let to = std::ffi::CString::new(to.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: both are NUL-terminated paths.
        let cloned = unsafe { libc::clonefile(from.as_ptr(), to.as_ptr(), 0) };
        assert_eq!(cloned, 0, "clonefile: {}", std::io::Error::last_os_error());
    }

    /// On an APFS volume (`Scope::attach_image`), worktrees hold: a `node_modules` whose
    /// files are clones of a store on the volume outside the worktree, with a cache written
    /// into it in place, bound by the cache alone, under a tenth of its allocated bytes; a
    /// `target` of files written in place, bound by their blocks; a target with a file that
    /// has a hard link outside it, and one holding only two clones of one file, each bound by
    /// 0; and a target of 800 empty files and one 4 KiB file, bound by 4 KiB. With the store
    /// deleted first, a second such `node_modules` is bound by at least its files' allocated
    /// bytes.
    #[cfg(target_os = "macos")]
    #[test]
    fn on_an_apfs_volume() {
        let s = Setup::new();
        let volume = s.fx.path("volume");
        std::fs::create_dir_all(&volume).unwrap();
        let Some(mut image) = s.fx.scope().attach_image(&volume) else {
            return;
        };
        let store = volume.join("store");
        let mut files = Vec::new();
        for name in ["a", "b", "c"] {
            let file = store.join(format!("{name}.js"));
            write_synced(&store, &format!("{name}.js"), &vec![b'p'; 256 * 1024]);
            files.push(file);
        }

        let cloned = worktree_at(&s, "volume/cloned", "cloned");
        let mut cloned_bytes = 0;
        for file in &files {
            let name = file.file_stem().unwrap().to_str().unwrap();
            let copy = cloned.join(format!(
                "node_modules/.pnpm/{name}@1.0.0/node_modules/{name}/index.js"
            ));
            clone(file, &copy);
            cloned_bytes += allocated(&copy);
        }
        write_synced(
            &cloned,
            "node_modules/.cache/run.json",
            &vec![b'c'; 16 * 1024],
        );
        let cache = allocated(&cloned.join("node_modules/.cache/run.json"));
        let plan = assert_bound(
            &s,
            &cloned.join("node_modules"),
            ExclusiveMeasure::PrivateSize,
            cache,
        );
        assert_eq!(
            plan.aliased_bytes,
            Some(0),
            "clones have one link: {plan:#?}"
        );
        let total = plan.allocated_bytes.unwrap();
        assert!(total >= cloned_bytes + cache, "{plan:#?}");
        assert!(cache * 10 < total, "{plan:#?}");

        write_synced(&cloned, "target/out.bin", &vec![b't'; 64 * 1024]);
        write_synced(&cloned, "target/sub/out2.bin", &vec![b'u'; 32 * 1024]);
        let written = allocated(&cloned.join("target/out.bin"))
            + allocated(&cloned.join("target/sub/out2.bin"));
        assert_bound(
            &s,
            &cloned.join("target"),
            ExclusiveMeasure::PrivateSize,
            written,
        );

        let linked = worktree_at(&s, "volume/linked", "linked");
        std::fs::create_dir_all(linked.join("target")).unwrap();
        std::fs::hard_link(&files[0], linked.join("target/a.js")).unwrap();
        assert_bound(&s, &linked.join("target"), ExclusiveMeasure::PrivateSize, 0);

        write_synced(&linked, "node_modules/one.bin", &vec![b'1'; 1 << 20]);
        clone(
            &linked.join("node_modules/one.bin"),
            &linked.join("node_modules/two.bin"),
        );
        let plan = assert_bound(
            &s,
            &linked.join("node_modules"),
            ExclusiveMeasure::PrivateSize,
            0,
        );
        assert!(plan.allocated_bytes.unwrap() >= 2 << 20, "{plan:#?}");

        let many = worktree_at(&s, "volume/many", "many");
        for n in 0..800 {
            write_synced(&many, &format!("target/empty/{n}"), b"");
        }
        write_synced(&many, "target/small.bin", &[b's'; 4096]);
        assert_bound(
            &s,
            &many.join("target"),
            ExclusiveMeasure::PrivateSize,
            4096,
        );

        // A store of its own, whose files nothing else clones.
        let second = volume.join("second-store");
        let orphaned = worktree_at(&s, "volume/orphaned", "orphaned");
        let mut own = 0;
        for name in ["a", "b", "c"] {
            write_synced(&second, &format!("{name}.js"), &vec![b'q'; 256 * 1024]);
            let copy = orphaned.join(format!("node_modules/{name}/index.js"));
            clone(&second.join(format!("{name}.js")), &copy);
            own += allocated(&copy);
        }
        std::fs::remove_dir_all(&second).unwrap();
        let (plan, result) = assert_passes(&s, &orphaned.join("node_modules"), None);
        assert!(plan.exclusive_bytes.unwrap() >= own, "{plan:#?}");
        assert_eq!(result.freed_bytes, plan.exclusive_bytes, "{result:#?}");

        image.detach().expect("detach the image");
    }

    /// The image's free bytes after a `sync`.
    #[cfg(target_os = "macos")]
    fn free_after_sync(volume: &Path) -> u64 {
        // SAFETY: `sync` takes no arguments and cannot fail.
        unsafe { libc::sync() };
        let stat = rustix::fs::statvfs(volume).unwrap();
        stat.f_bavail * stat.f_frsize
    }

    /// Plans `target` with the state dir `state` on the image, reports it with `--dry-run`,
    /// and executes it, reading the image's free space at `act.committed` and at
    /// `act.finishing`, between which the execution only takes and deletes entries. Returns
    /// the plan, the dry run, the result, and the rise in free space.
    #[cfg(target_os = "macos")]
    fn measured(
        s: &Setup,
        state: &Path,
        volume: &Path,
        target: &Path,
    ) -> (Plan, ActResult, ActResult, u64) {
        use cardinal::testpoint::{ACT_COMMITTED, ACT_FINISHING};
        let output = s
            .cardinal()
            .env("CARDINAL_STATE_DIR", state)
            .args(["plan", "reclaim-dir"])
            .arg(target)
            .output_within(RUN_LIMIT)
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let plan: Plan = serde_json::from_slice(&output.stdout).unwrap();
        assert!(plan.invariants_ok, "{:#?}", plan.checks);
        let act = |dry_run: bool| {
            let options = cardinal::act::ActOptions { dry_run, ..owner() };
            let mut command = s.act_command(&plan.id, &options);
            command.env("CARDINAL_STATE_DIR", state);
            command
        };
        let output = act(true).output_within(RUN_LIMIT).unwrap();
        let dry = plan_support::act_result(&output).unwrap();
        assert_eq!(dry.reasons, ["dry_run_requested"], "{dry:#?}");
        let scope = cardinal_test_support::Scope::new();
        let points = support::testpoints::TestPoints::new(&scope);
        let mut command = act(false);
        points.apply(&mut command, &[ACT_COMMITTED, ACT_FINISHING], None);
        let child = command
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        points.wait_reached(ACT_COMMITTED, child.id(), live_support::WAIT);
        let committed = free_after_sync(volume);
        points.release(ACT_COMMITTED);
        points.wait_reached(ACT_FINISHING, child.id(), live_support::WAIT);
        let finishing = free_after_sync(volume);
        points.release(ACT_FINISHING);
        let result = plan_support::act_result(&child.wait_with_output().unwrap()).unwrap();
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
        (plan, dry, result, finishing.saturating_sub(committed))
    }

    /// The freed bytes of an execution (section 13.7), on an APFS volume with the state dir
    /// on it: the image's free space rises by at least the plan's `exclusive_bytes`, the dry
    /// run's `freed_bytes`, and the result's `freed_bytes`, for a target of files written in
    /// place, and for a target holding only two clones of one file by their size, which the
    /// result counts though the plan's bound is 0.
    #[cfg(target_os = "macos")]
    #[test]
    fn of_an_execution_on_an_apfs_volume() {
        let s = Setup::with_host(GRANT);
        let volume = s.fx.path("volume");
        std::fs::create_dir_all(&volume).unwrap();
        let Some(mut image) = s.fx.scope().attach_image(&volume) else {
            return;
        };
        let state = volume.join("state");

        let written = worktree_at(&s, "volume/written", "written");
        write_synced(&written, "target/a.bin", &vec![b'a'; 256 * 1024]);
        write_synced(&written, "target/sub/b.bin", &vec![b'b'; 128 * 1024]);
        let (plan, dry, result, rise) = measured(&s, &state, &volume, &written.join("target"));
        let bound = plan.exclusive_bytes.unwrap();
        assert!(bound >= 384 * 1024, "{plan:#?}");
        assert_eq!(dry.freed_bytes, Some(bound));
        let freed = result.freed_bytes.unwrap();
        assert!(
            rise >= bound && rise >= freed,
            "rise {rise}, bound {bound}, freed {freed}"
        );

        let clones = worktree_at(&s, "volume/clones", "clones");
        write_synced(&clones, "target/one.bin", &vec![b'1'; 1 << 20]);
        clone(
            &clones.join("target/one.bin"),
            &clones.join("target/two.bin"),
        );
        let (plan, dry, result, rise) = measured(&s, &state, &volume, &clones.join("target"));
        assert_eq!(plan.exclusive_bytes, Some(0), "{plan:#?}");
        assert_eq!(dry.freed_bytes, Some(0));
        let freed = result.freed_bytes.unwrap();
        assert!(freed >= 1 << 20, "{result:#?}");
        assert!(rise >= freed, "rise {rise}, freed {freed}");

        image.detach().expect("detach the image");
    }

    /// On ext4 (R on CI's runners): a sparse file with no data and a 2 KiB extended
    /// attribute that a file outside the target also has, which ext4 keeps in one shared
    /// block, reads 4,096 bytes in `st_blocks` and counts nothing in the bound, and a file of
    /// written data counts its extents.
    #[cfg(target_os = "linux")]
    #[test]
    fn ext4_extents() {
        let s = Setup::new();
        let magic = rustix::fs::statfs(s.fx.root()).unwrap().f_type;
        if magic != 0xEF53 {
            skip(
                "ext4_extents",
                &format!("R is not ext4 (f_type {magic:#x})"),
            );
            return;
        }
        let wt = s.worktree("extents");
        let target = wt.join("target");
        let value = vec![b'x'; 2048];
        let outside = s.fx.path("outside/attributed");
        write_synced(&s.fx.path("outside"), "attributed", b"");
        let sparse = target.join("sparse");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::File::create(&sparse)
            .unwrap()
            .set_len(1 << 20)
            .unwrap();
        for file in [&outside, &sparse] {
            rustix::fs::setxattr(
                file,
                "user.cardinal",
                &value,
                rustix::fs::XattrFlags::empty(),
            )
            .unwrap();
            std::fs::File::open(file).unwrap().sync_all().unwrap();
        }
        assert_eq!(allocated(&sparse), 4096, "one shared attribute block");
        write_synced(&wt, "target/data.bin", &vec![b'd'; 64 * 1024]);
        assert_bound(&s, &target, ExclusiveMeasure::Extents, 64 * 1024);
        report("ext4_extents: ran on ext4");
    }
}

/// The control of section 13.7: a granted owner's `act` executes a target on which every
/// check passes. The target is gone from the worktree, the trash entry with it, the result
/// is `executed` and durable, and nothing else in the worktree changed (A27).
#[test]
fn control_executes() {
    let x = Live::new("control", "target", &["debug/app", "debug/deps/lib.rlib"]);
    std::os::unix::fs::symlink("debug/app", x.target().join("app")).unwrap();
    let mut x = x;
    x.replan();
    let mut before = plan_support::listing(&x.wt);
    before.retain(|path, _| !path.starts_with("target"));
    let result = x.act();
    assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
    assert_eq!(result.reasons, Vec::<String>::new(), "{result:#?}");
    assert!(result.skipped_changed.is_empty(), "{result:#?}");
    assert!(result.trash.is_none(), "{result:#?}");
    assert!(!x.target().exists());
    assert!(!x.entry().exists());
    assert!(x.s.state.execution_result(&x.plan.id).exists());
    assert_eq!(plan_support::listing(&x.wt), before);
}

/// Once: a second `act` on an executed plan reports `already_executed` and changes nothing,
/// and a target made again at the path needs a new plan.
#[test]
fn once() {
    let x = Live::new("once", "target", &["debug/app"]);
    assert_eq!(x.act().status, ActStatus::Executed);
    write_synced(&x.wt, "target/debug/app", b"rebuilt");
    let result = x.act();
    assert_eq!(result.status, ActStatus::AlreadyExecuted, "{result:#?}");
    assert_eq!(
        std::fs::read(x.target().join("debug/app")).unwrap(),
        b"rebuilt"
    );
}

/// A25: a whole worktree's plan never executes, for the granted owner too: `dry_run` with
/// `execution_not_designed`, and nothing changes.
#[test]
fn worktree_plans_never_execute() {
    let s = Setup::with_host(GRANT);
    let wt = s.worktree("whole");
    write_synced(&wt, "target/debug/app", b"bin");
    let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
    assert!(plan.invariants_ok, "{:#?}", plan.checks);
    let before = plan_support::listing(&wt);
    let result = s.act(&plan.id, &owner()).unwrap();
    assert_eq!(result.status, ActStatus::DryRun, "{result:#?}");
    assert_eq!(result.reasons, ["execution_not_designed"], "{result:#?}");
    assert_eq!(plan_support::listing(&wt), before);
    assert!(!s.state.execution_claim(&plan.id).exists());
}

/// C15: a runner holding the worktree's in-use lock while its stage runs keeps a granted
/// owner's `act` out: the one try at the lock fails, and `act` reports `rejected`
/// (`not_in_use`) with nothing moved.
#[test]
fn held_lock_refuses() {
    let x = Live::new("held-lock", "target", &["debug/app"]);
    let before = x.files();
    let started = x.s.fx.path("started");
    let gate = x.s.fx.path("gate");
    let script = format!(
        "touch '{}'; i=0; while [ ! -e '{}' ] && [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done",
        started.display(),
        gate.display()
    );
    let mut runner = x.s.cardinal_in(&x.wt);
    runner.args(["run", "--", "sh", "-c", &script]);
    let mut runner = runner.spawn().unwrap();
    cardinal_test_support::process::wait_for("the stage", live_support::WAIT, || {
        started.exists().then_some(())
    });
    let result = x.act();
    std::fs::write(&gate, b"").unwrap();
    assert_eq!(runner.wait().unwrap().code(), Some(0));
    assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
    assert_eq!(result.reasons, ["not_in_use"], "{result:#?}");
    assert_eq!(
        result.error.as_deref(),
        Some("another process holds the worktree's in-use lock"),
        "{result:#?}"
    );
    assert_eq!(x.files(), before);
    assert!(!x.s.state.execution_claim(&x.plan.id).exists());
}

/// A25: who executes. Every case leaves the target and the state's trash as they were.
mod grants {
    use super::*;

    /// Runs `act` on `x`'s plan as `options` and asserts `dry_run` for `reason`, nothing
    /// moved, and no claim.
    #[track_caller]
    fn assert_reports(x: &Live, options: &cardinal::act::ActOptions, reason: &str) {
        let before = x.files();
        let result = x.s.act(&x.plan.id, options).unwrap();
        assert_eq!(result.status, ActStatus::DryRun, "{result:#?}");
        assert_eq!(result.reasons, [reason], "{result:#?}");
        assert_eq!(x.files(), before);
        assert!(!x.s.state.execution_claim(&x.plan.id).exists());
        assert!(!x.entry().exists());
    }

    /// Without `[act] execute`, the owner reports `execution_not_granted`.
    #[test]
    fn the_owner_needs_a_grant() {
        let x = Live::with_host("", "ungranted", "target", &["debug/app"], |_, _| {});
        assert_reports(&x, &owner(), "execution_not_granted");
    }

    /// A grant to another caller does not reach the owner.
    #[test]
    fn a_grant_names_its_caller() {
        let host = "[unit.disk-reclaim]\nact = true\n[act]\nexecute = { \"unit:disk-reclaim\" = [\"reclaim_dir\"] }\n";
        let x = Live::with_host(host, "other", "target", &["debug/app"], |_, _| {});
        assert_reports(&x, &owner(), "execution_not_granted");
    }

    /// The granted owner's `--dry-run` reports `dry_run_requested`.
    #[test]
    fn a_dry_run_is_requested() {
        let x = Live::new("dry", "target", &["debug/app"]);
        let options = cardinal::act::ActOptions {
            dry_run: true,
            ..owner()
        };
        assert_reports(&x, &options, "dry_run_requested");
    }

    /// The arbiter never executes, with the owner granted.
    #[test]
    fn the_arbiter_never_executes() {
        let x = Live::new("arbiter", "target", &["debug/app"]);
        assert_reports(&x, &arbiter(), "execution_not_granted");
    }

    /// `unit:disk-reclaim` executes only with its grant and its `act` true: granted with
    /// `act` false, it reports `execution_not_granted` and records `planned` (C16), and with
    /// both its `act` executes, as the pass runs it (section 9.7).
    #[test]
    fn a_unit_needs_its_grant_and_its_gate() {
        let unit = cardinal::act::ActOptions {
            caller: cardinal::contract::Caller::Unit("disk-reclaim".into()),
            ..owner()
        };
        let grant = "[act]\nexecute = { \"unit:disk-reclaim\" = [\"reclaim_dir\"] }\n";
        let gated = Live::with_host(
            &format!("[unit.disk-reclaim]\nact = false\n{grant}"),
            "gated",
            "target",
            &["debug/app"],
            |_, _| {},
        );
        assert_reports(&gated, &unit, "execution_not_granted");

        let x = Live::with_host(
            &format!("[unit.disk-reclaim]\nact = true\n{grant}"),
            "unit",
            "target",
            &["debug/app"],
            |_, _| {},
        );
        let result = x.s.act(&x.plan.id, &unit).unwrap();
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
        assert_eq!(result.caller, "unit:disk-reclaim");
        assert!(!x.target().exists());
        assert!(!x.entry().exists());
    }
}

/// Report-only targets (A27, section 13.7): each alone reports `dry_run` with its rule's
/// reason while the control executes.
mod eligibility {
    use super::*;

    /// Runs the owner's `act` on `x` and asserts `dry_run` with `reason`, nothing moved, no
    /// claim, and the detail naming `what`.
    #[track_caller]
    fn assert_report_only(x: &Live, reason: &str, what: &str) {
        let before = x.files();
        let result = x.act();
        assert_eq!(result.status, ActStatus::DryRun, "{result:#?}");
        assert_eq!(result.reasons, [reason], "{result:#?}");
        let error = result.error.as_deref().unwrap_or_default();
        assert!(error.contains(what), "{what}: {result:#?}");
        assert_eq!(x.files(), before);
        assert!(!x.s.state.execution_claim(&x.plan.id).exists());
        assert!(!x.entry().exists());
    }

    /// A worktree whose only skip-worktree entry is a `.gitignore` removed from the worktree,
    /// with `core.sparseCheckout` false: `sparse_checkout`.
    #[test]
    fn a_skip_worktree_entry() {
        let x = Live::with_host(
            GRANT,
            "skip",
            "pkg/node_modules",
            &["a/index.js"],
            |s, wt| {
                s.fx.commit_file(wt, "pkg/.gitignore", "*.log\n", "pkg ignores");
                s.fx.skip_worktree(wt, "pkg/.gitignore");
                std::fs::remove_file(wt.join("pkg/.gitignore")).unwrap();
            },
        );
        assert_report_only(&x, "sparse_checkout", "pkg/.gitignore");
    }

    /// `core.sparseCheckout` set with no skip-worktree or sparse-directory entry and no
    /// `info/sparse-checkout` file executes.
    #[test]
    fn sparse_checkout_set_alone_executes() {
        let x = Live::with_host(GRANT, "sparse-set", "target", &["debug/app"], |s, wt| {
            s.fx.config_core(wt, "core.sparseCheckout", "true");
        });
        assert_eq!(x.act().status, ActStatus::Executed);
    }

    /// A `sharedindex.*` file that `git update-index --no-split-index` left behind, with the
    /// index whole again: no split index, so the plan passes and the owner's `act` executes,
    /// leaving the leftover file as it was.
    #[test]
    fn a_shared_index_left_behind_executes() {
        let x = Live::with_host(GRANT, "unsplit", "target", &["debug/app"], |s, wt| {
            s.fx.split_index(wt);
            s.fx.no_split_index(wt);
        });
        let git_dir = x.git_dir();
        let shared: Vec<_> = std::fs::read_dir(&git_dir)
            .unwrap()
            .map(|e| e.unwrap())
            .filter(|e| {
                e.file_name()
                    .as_encoded_bytes()
                    .starts_with(b"sharedindex.")
            })
            .map(|e| (e.path(), e.metadata().unwrap().modified().unwrap()))
            .collect();
        assert_eq!(shared.len(), 1, "{shared:?}");
        assert_eq!(x.act().status, ActStatus::Executed);
        assert_eq!(
            std::fs::metadata(&shared[0].0).unwrap().modified().unwrap(),
            shared[0].1
        );
    }

    /// A profile for `github.com/o/other` that protects `target/draft`.
    const OTHER_PROTECTS_DRAFT: &str =
        "[match]\nremotes = [\"github.com/o/other\"]\n[data]\nprotected = [\"target/draft\"]\n";

    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// `origin` named only by a legacy `remotes/origin` file, the URL of a profile that
    /// protects `target/draft`, made unreadable: git exits 2 as for no `origin`, with the
    /// failed read on stderr. `observed` fails naming `origin`, and the owner's `act` leaves
    /// `target/draft` where it is.
    #[test]
    fn an_unreadable_legacy_origin_is_unknown() {
        let s = Setup::with_host(GRANT);
        s.profile("other", OTHER_PROTECTS_DRAFT);
        let wt = s.worktree("legacy");
        write_synced(&wt, "target/draft/x", b"draft");
        let config = s.main.join(".git/config");
        let text = std::fs::read_to_string(&config).unwrap();
        let start = text
            .find("[remote \"origin\"]")
            .expect("origin in the config");
        let end = text[start + 1..]
            .find("\n[")
            .map_or(text.len(), |at| start + at + 2);
        std::fs::write(&config, format!("{}{}", &text[..start], &text[end..])).unwrap();
        let legacy = s.main.join(".git/remotes/origin");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "URL: https://github.com/o/other.git\n").unwrap();
        set_mode(&legacy, 0o000);
        let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
        let result = s.act(&plan.id, &owner()).unwrap();
        set_mode(&legacy, 0o644);
        let observed = plan.checks.iter().find(|c| c.name == "observed").unwrap();
        assert_eq!(observed.result, CheckResult::Fail, "{:#?}", plan.checks);
        let unknown = observed
            .detail
            .strip_prefix("unknown: ")
            .unwrap_or_default();
        assert!(unknown.contains("origin"), "{observed:#?}");
        assert_ne!(result.status, ActStatus::Executed, "{result:#?}");
        assert!(wt.join("target/draft/x").exists());
    }

    /// An `[observe] managed` root that is a symbolic link to the worktree, in a directory
    /// that cannot be searched: the root does not resolve, so `not_managed` is unknown and
    /// the owner's `act` moves nothing.
    #[test]
    fn an_unresolvable_managed_root_is_unknown() {
        let mut s = Setup::with_host("");
        let wt = s.worktree("unresolved");
        write_synced(&wt, "target/debug/app", b"app");
        let shut = s.fx.path("shut");
        std::fs::create_dir(&shut).unwrap();
        let root = shut.join("link");
        std::os::unix::fs::symlink(&wt, &root).unwrap();
        s.scene.observe_config(&format!(
            "managed = [{{ app = \"t3code\", root = {:?} }}]\n{GRANT}",
            root.display().to_string()
        ));
        set_mode(&shut, 0o000);
        let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
        let result = s.act(&plan.id, &owner()).unwrap();
        set_mode(&shut, 0o755);
        let check = plan
            .checks
            .iter()
            .find(|c| c.name == "not_managed")
            .unwrap();
        assert_eq!(check.result, CheckResult::Unknown, "{:#?}", plan.checks);
        assert_ne!(result.status, ActStatus::Executed, "{result:#?}");
        assert!(wt.join("target/debug/app").exists());
    }

    /// `core.excludesFile` set to `target/global-ignore`, holding the rule:
    /// `not_directory_excluded`, an ignore input in the target.
    #[test]
    fn an_excludes_file_in_the_target() {
        let x = Live::with_host(GRANT, "excludes-in", "target", &["debug/app"], |s, wt| {
            write_synced(wt, "target/global-ignore", b"target/\n");
            s.fx.config_core(wt, "core.excludesFile", "target/global-ignore");
        });
        assert_report_only(&x, "not_directory_excluded", "target/global-ignore");
    }

    /// The same excludes file, with the root `.gitignore` holding the rule `check-ignore -v`
    /// names: still `not_directory_excluded`.
    #[test]
    fn an_excludes_file_in_the_target_beside_the_rule() {
        let x = Live::with_host(
            GRANT,
            "excludes-beside",
            "target",
            &["debug/app"],
            |s, wt| {
                write_synced(wt, "target/global-ignore", b"*.swp\n");
                s.fx.config_core(wt, "core.excludesFile", "target/global-ignore");
            },
        );
        assert_report_only(&x, "not_directory_excluded", "target/global-ignore");
    }

    /// A target on another mount than the state dir (an APFS image on macOS):
    /// `not_on_state_mount`; and a mount over `<state>/trash/`, with the target on the state
    /// dir's volume, too.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_target_off_the_state_mount() {
        let s = Setup::with_host(GRANT);
        let volume = s.fx.path("volume");
        std::fs::create_dir_all(&volume).unwrap();
        let Some(mut image) = s.fx.scope().attach_image(&volume) else {
            return;
        };
        let wt = freed_bytes::worktree_at(&s, "volume/other", "other");
        write_synced(&wt, "target/debug/app", b"bin");
        let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
        assert!(plan.invariants_ok, "{:#?}", plan.checks);
        let before = plan_support::listing(&wt.join("target"));
        let result = s.act(&plan.id, &owner()).unwrap();
        assert_eq!(result.status, ActStatus::DryRun, "{result:#?}");
        assert_eq!(result.reasons, ["not_on_state_mount"], "{result:#?}");
        assert_eq!(plan_support::listing(&wt.join("target")), before);
        image.detach().expect("detach the image");

        let x = Live::new("trash-mounted", "target", &["debug/app"]);
        let trash = x.s.state.trash_dir();
        std::fs::create_dir_all(&trash).unwrap();
        let Some(mut image) = x.s.fx.scope().attach_image(&trash) else {
            return;
        };
        assert_report_only(&x, "not_on_state_mount", "<state>/trash/");
        image.detach().expect("detach the image");
    }

    /// On macOS, a tracked `Target/x` with `core.ignoreCase` false, which APFS resolves to
    /// the target: `tracked_overlap`.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_tracked_alias_of_the_target() {
        let x = Live::with_host(GRANT, "alias", "target", &["x"], |s, wt| {
            s.fx.config_core(wt, "core.ignoreCase", "false");
            // The directory is made as `target`; the index names it `Target`.
            write_synced(wt, "target/x", b"x");
            s.fx.add(wt, "Target/x");
            s.fx.commit(wt, "track Target/x");
        });
        if !x.wt.join("TARGET").exists() {
            eprintln!("skipped a_tracked_alias_of_the_target: the volume is case-sensitive");
            return;
        }
        assert_report_only(&x, "tracked_overlap", "Target/x");
    }

    /// On macOS, a tracked path with the target's name decomposed (NFD), which APFS resolves
    /// to the target: `tracked_overlap`.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_tracked_decomposed_name() {
        let composed = "caf\u{e9}";
        let decomposed = "cafe\u{301}";
        let x = Live::with_host(GRANT, "nfd", composed, &["x"], |s, wt| {
            s.fx.config_core(wt, "core.precomposeUnicode", "false");
            s.profile(
                "generic",
                &plan_support::PROFILE.replace(
                    "regenerable = [",
                    &format!("regenerable = [\"{composed}\", "),
                ),
            );
            let gitignore = std::fs::read_to_string(wt.join(".gitignore")).unwrap();
            s.fx.commit_file(
                wt,
                ".gitignore",
                &format!("{gitignore}/{composed}/\n"),
                "ignore the target",
            );
            // The directory is made composed; the index names it decomposed.
            write_synced(wt, &format!("{composed}/x"), b"x");
            s.fx.add_force(wt, &format!("{decomposed}/x"));
            s.fx.commit(wt, "track the decomposed name");
        });
        assert_report_only(&x, "tracked_overlap", "x");
    }
}
