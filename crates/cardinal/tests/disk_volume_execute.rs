//! Executing on a real small volume (C11, A31; `docs/architecture.md` sections 9.7 and 13.7,
//! "Full volumes" and "Under pressure"): the 256 MiB APFS image of `Scope::attach_image` on
//! macOS, the 64 MiB tmpfs of `Scope::tmpfs` on Linux, holding the state dir and a worktree
//! whose `target/` `disk-reclaim` may execute, with the lines and the reserve set as fractions
//! of the volume's size. The pass reads the volume as it is, with no readings fake.

#![cfg(any(target_os = "macos", target_os = "linux"))]

mod disk_support;
mod observe_common;
mod plan_support;
mod support;

use std::io::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use cardinal::contract::ActStatus;
use cardinal::os::Measure;
use cardinal::state::StateDir;
use disk_support::{Disk, MIB, fill};

#[test]
fn helper() {
    support::helper::run_if_requested();
}

/// `disk-reclaim` granted, its `act` true.
const GRANTED: &str = "[unit.disk-reclaim]\nact = true\n\
                       [act]\nexecute = { \"unit:disk-reclaim\" = [\"reclaim_dir\"] }\n";

/// The test volume at `$root`, attached until the end of the enclosing block; the test
/// returns where it cannot have one (reported as a skip).
macro_rules! volume {
    ($root:ident) => {
        #[cfg(target_os = "macos")]
        let scope = cardinal_test_support::Scope::new();
        #[cfg(target_os = "macos")]
        let $root = scope.path("volume");
        #[cfg(target_os = "macos")]
        let Some(_image) = ({
            std::fs::create_dir(&$root).unwrap();
            scope.attach_image(&$root)
        }) else {
            return;
        };
        #[cfg(target_os = "linux")]
        let Some(scope) = cardinal_test_support::Scope::tmpfs() else {
            return;
        };
        #[cfg(target_os = "linux")]
        let $root = scope.root().to_path_buf();
    };
}

/// The volume's mount and total size.
fn mount_and_total(root: &Path) -> (PathBuf, u64) {
    let mount = cardinal::os::volume(root).unwrap().mount;
    let total = cardinal::os::disk(root, Measure::Usable)
        .unwrap()
        .total_bytes;
    (mount, total)
}

fn free(root: &Path) -> u64 {
    cardinal::os::disk(root, Measure::Usable)
        .unwrap()
        .free_bytes
}

/// A disk setup whose state dir is on the volume at `root`.
fn with_state_on(root: &Path) -> Disk {
    let mut d = Disk::new((0, 1, 2));
    let state = root.join("state");
    d.s.scene.state_dir.clone_from(&state);
    d.s.state = StateDir::open(&state).unwrap();
    d
}

/// A linked worktree of `main` at `path` on a new branch `name`, made by git directly.
fn worktree_at(d: &Disk, path: &Path, name: &str) -> PathBuf {
    let out =
        d.s.fx
            .command("git", &d.s.main)
            .args(["worktree", "add", "-q", "-b", name, "--"])
            .arg(path)
            .output()
            .unwrap();
    assert!(out.status.success(), "{out:?}");
    path.to_path_buf()
}

/// C11: a pass writes a missing reserve only when the state dir's volume reads above its
/// recovery line plus the reserve's size. With the recovery line less than one reserve below
/// the free space, no reserve is written; with it more than two reserves below, the reserve is
/// written whole, of allocated blocks, and the volume's free space falls by about its size.
#[test]
fn reserve_written_above_the_line() {
    volume!(root);
    let (mount, total) = mount_and_total(&root);
    let reserve_mib = (total / 16 / MIB).max(1);
    let free_mib = free(&root) / MIB;
    let mut d = with_state_on(&root);

    // The recovery line less than one reserve below the free space.
    let close = free_mib - reserve_mib / 2;
    set_host(&mut d, &mount, (1, 2, close), reserve_mib);
    let pass = d.reconcile();
    assert!(!d.s.state.reserve_file().exists(), "{pass:#?}");

    // More than two reserves below.
    let far = free_mib - 3 * reserve_mib;
    set_host(&mut d, &mount, (1, 2, far), reserve_mib);
    let before = free(&root);
    let pass = d.reconcile();
    let reserve = std::fs::metadata(d.s.state.reserve_file())
        .unwrap_or_else(|err| panic!("{err}: {pass:#?}"));
    assert_eq!(reserve.len(), reserve_mib * MIB);
    assert!(reserve.blocks() * 512 >= reserve_mib * MIB, "{reserve:?}");
    let fell = before.saturating_sub(free(&root));
    assert!(
        fell >= reserve_mib * MIB * 9 / 10,
        "free space fell by {fell}, the reserve is {reserve_mib} MiB"
    );
}

/// Writes `host.toml` for the volume: the global lines below any free space, the volume's
/// `lines` in MiB `(critical, warning, recovery)`, a reserve of `reserve_mib`, and
/// `disk-reclaim` granted.
fn set_host(d: &mut Disk, mount: &Path, lines: (u64, u64, u64), reserve_mib: u64) {
    let (critical, warning, recovery) = lines;
    let main = d.s.main.display().to_string();
    d.s.scene.clear_observe_config();
    d.s.scene.observe_config(&format!(
        "repos = ['{main}']\n[disk]\ncritical_mib = 0\nwarning_mib = 1\nrecovery_mib = 2\n\
         reserve_mib = {reserve_mib}\nvolumes = [{{ mount = '{}', critical_mib = {critical}, \
         warning_mib = {warning}, recovery_mib = {recovery} }}]\n\
         [arbiter]\ncommand = ['true']\n{GRANTED}",
        mount.display()
    ));
}

/// Fills `root`'s volume until writes fail for lack of space: the file `name` grown until a
/// write fails, then small files `name.<n>` made until making one fails, and on APFS a
/// directory `name.d` that cannot be made, as an execution's records are. A tmpfs makes a
/// directory, and a file left empty, with no block until its inodes run out, which would leave
/// a pass blocks and no inode to write them to: there the first small file that fails ends it,
/// removed.
fn fill_up(root: &Path, name: &str) {
    let mut file = std::fs::File::create(root.join(name)).unwrap();
    let block = vec![9_u8; 1 << 20];
    for size in [1 << 20, 1 << 16, 1 << 12, 1 << 9] {
        while file
            .write_all(&block[..size])
            .and_then(|()| file.sync_all())
            .is_ok()
        {}
    }
    let mut n = 0;
    loop {
        let made = std::fs::File::create_new(root.join(format!("{name}.{n}")))
            .and_then(|mut f| f.write_all(&block[..4096]).and_then(|()| f.sync_all()));
        n += 1;
        match made {
            Ok(()) => {}
            Err(err) if cardinal::state::is_no_space(&err) => {
                if cfg!(target_os = "linux") {
                    let _ = std::fs::remove_file(root.join(format!("{name}.{}", n - 1)));
                    break;
                }
                if std::fs::create_dir(root.join(format!("{name}.d"))).is_err() {
                    break;
                }
                std::fs::remove_dir(root.join(format!("{name}.d"))).unwrap();
            }
            Err(err) => panic!("fill {name}: {err}"),
        }
    }
}

/// C11 and A31 on a full volume: with the state dir and a worktree on the volume and
/// `disk-reclaim` granted, a first pass writes the reserve; once a writer fills the volume to
/// `ENOSPC`, a pass still executes the worktree's `target/`, the reserve deleted at the first
/// write that fails, and nothing is left in the trash.
#[test]
fn full_state_through_reconcile() {
    volume!(root);
    let (mount, total) = mount_and_total(&root);
    let reserve_mib = (total / 16 / MIB).max(1);
    let mib = total / MIB;
    let mut d = with_state_on(&root);
    set_host(&mut d, &mount, (mib / 8, mib / 4, mib / 2), reserve_mib);
    let wt = worktree_at(&d, &root.join("full"), "full");
    fill(&wt, "target/debug/app", total / 32);
    fill(&wt, "target/debug/deps/lib", total / 64);

    let first = d.reconcile();
    assert!(d.s.state.reserve_file().exists(), "{first:#?}");
    assert!(first.disk_reports.is_empty(), "{first:#?}");

    fill_up(&root, "writer.bin");
    let pass = d.reconcile();
    let report = pass
        .disk_reports
        .first()
        .unwrap_or_else(|| panic!("{pass:#?}"));
    assert_eq!(
        report.targets.iter().map(|t| t.status).collect::<Vec<_>>(),
        [Some(ActStatus::Executed)],
        "{pass:#?}"
    );
    assert!(!wt.join("target").exists());
    assert!(!d.s.state.reserve_file().exists());
    let trash: Vec<_> = std::fs::read_dir(d.s.state.trash_dir()).unwrap().collect();
    assert!(trash.is_empty(), "{trash:?}");
    assert!(
        pass.errors.iter().any(|e| e.message.contains("No space")),
        "the volume was full for the pass: {pass:#?}"
    );
}
