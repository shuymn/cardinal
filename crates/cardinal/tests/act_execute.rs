//! Unit X's execution tests (`docs/architecture.md` section 13.7) as far as unit X1a reaches:
//! each refusal before the move, alone, beside a control on which every check passes and
//! `act` reports `dry_run`, and the plan's and the dry run's readings of the freed bytes.
//! Nothing executes: a plan whose checks pass reports `execution_not_designed`.

#![cfg(unix)]

mod observe_common;
mod plan_support;
mod support;

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use cardinal::contract::{ActResult, ActStatus, Check, CheckResult, Plan, PlanKind};
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
    assert_eq!(result.reasons, ["execution_not_designed"], "{result:#?}");
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
    /// refuses the target by `not_in_use`, naming the process and that name: only a file held
    /// through a name outside is left to that name.
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
        assert!(
            check
                .detail
                .contains(&format!("{pid} (descriptor 0, target/debug/held)")),
            "{check:#?}"
        );
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
    fn worktree_at(s: &Setup, rel: &str, branch: &str) -> PathBuf {
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
