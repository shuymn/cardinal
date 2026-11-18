//! A small tmpfs volume (Linux), for tests of a volume that fills up
//! (`docs/architecture.md` section 13.3).
//!
//! The harness's bubblewrap mounts a 64 MiB tmpfs at one path under the per-run root for each
//! test process it confines, and names it in `CARDINAL_TEST_TMPFS`. bubblewrap mounts it in its
//! own user namespace, whose ID map lets the test create files there; a tmpfs mounted in the
//! namespace of [`Scope::mount_namespace`], which has no ID map, refuses every file it would
//! create (`EOVERFLOW`). Every test process runs in a bubblewrap of its own, so no other test
//! process sees its volume; within the process, one scope at a time holds it.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use super::{Scope, skip};

/// The variable naming the harness's tmpfs volume.
pub const TMPFS_VAR: &str = "CARDINAL_TEST_TMPFS";

/// Whether a scope of this process holds the volume.
static HELD: AtomicBool = AtomicBool::new(false);

/// A scope's hold on the volume, released when the scope has removed its root.
pub(super) struct Claim;

impl Drop for Claim {
    fn drop(&mut self) {
        HELD.store(false, Ordering::SeqCst);
    }
}

impl Scope {
    /// A scope whose root is on the harness's tmpfs volume, 64 MiB that this test process has
    /// to itself (named by `CARDINAL_TEST_TMPFS`): writes past its size fail with `ENOSPC`,
    /// and space returns at once on unlink.
    ///
    /// One scope at a time holds the volume: a call while another scope of this process holds
    /// it panics, so two parts of a test cannot fill the same volume by accident; whatever
    /// should share it goes in one scope. The volume must be an empty tmpfs mount when taken.
    ///
    /// Without the volume (the tests run unconfined) this returns `None` and reports the skip
    /// on stderr, and panics where `CI` is `true`.
    #[must_use]
    pub fn tmpfs() -> Option<Self> {
        let Some(volume) = std::env::var_os(TMPFS_VAR) else {
            skip(&format!(
                "no tmpfs volume: {TMPFS_VAR} is unset; scripts/test's bubblewrap mounts one"
            ));
            return None;
        };
        let volume = PathBuf::from(volume);
        assert!(
            HELD.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok(),
            "the tmpfs volume {} is held by another scope of this process",
            volume.display()
        );
        // Released on every path from here, a panic included.
        let claim = Claim;
        check_volume(&volume);
        let dir = tempfile::Builder::new()
            .prefix("cardinal-scope-")
            .tempdir_in(&volume)
            .expect("create the scope root on the tmpfs volume");
        let mut scope = Self::with_dir(dir);
        scope.volume = Some(claim);
        Some(scope)
    }
}

/// Panics unless `volume` is an absolute path to an empty tmpfs mount.
fn check_volume(volume: &Path) {
    let what = format!("the tmpfs volume {}", volume.display());
    assert!(volume.is_absolute(), "{what} is not an absolute path");
    let meta = std::fs::symlink_metadata(volume).unwrap_or_else(|err| panic!("{what}: {err}"));
    assert!(meta.is_dir(), "{what} is not a directory");
    let parent = volume.parent().unwrap_or(volume);
    let parent = std::fs::symlink_metadata(parent).unwrap_or_else(|err| panic!("{what}: {err}"));
    assert_ne!(meta.dev(), parent.dev(), "{what} is not a mount point");
    let stat = rustix::fs::statfs(volume).unwrap_or_else(|err| panic!("{what}: {err}"));
    assert_eq!(stat.f_type, libc::TMPFS_MAGIC, "{what} is not a tmpfs");
    let mut entries = std::fs::read_dir(volume).unwrap_or_else(|err| panic!("{what}: {err}"));
    assert!(entries.next().is_none(), "{what} is not empty");
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::PathBuf;

    use super::TMPFS_VAR;
    use crate::scope::{Scope, fill};

    fn volume() -> PathBuf {
        PathBuf::from(std::env::var_os(TMPFS_VAR).unwrap())
    }

    /// A scope on the volume is on 64 MiB of its own: a write past it fails with `ENOSPC`,
    /// an unlink returns the space at once, and the volume is empty again once the scope is
    /// dropped.
    #[test]
    fn a_tmpfs_scope_fills_up_and_frees_on_unlink() {
        let Some(scope) = Scope::tmpfs() else {
            return;
        };
        assert!(
            scope.root().starts_with(volume()),
            "{}",
            scope.root().display()
        );
        let (written, err) = fill(&scope.path("fill"));
        assert_eq!(
            err.expect("the volume fills").kind(),
            io::ErrorKind::StorageFull
        );
        assert!((32 << 20..=64 << 20).contains(&written), "{written}");
        std::fs::remove_file(scope.path("fill")).unwrap();
        let (again, err) = fill(&scope.path("again"));
        assert_eq!(err.unwrap().kind(), io::ErrorKind::StorageFull);
        assert_eq!(again, written, "the unlinked file's space is free");
        drop(scope);
        assert_eq!(std::fs::read_dir(volume()).unwrap().count(), 0);
    }

    /// While a scope holds the volume, taking it again panics; once that scope is dropped,
    /// the volume can be taken again.
    #[test]
    fn one_scope_at_a_time_holds_the_volume() {
        let Some(first) = Scope::tmpfs() else {
            return;
        };
        let Err(err) = catch_unwind(AssertUnwindSafe(Scope::tmpfs)) else {
            panic!("a second scope took the volume");
        };
        let message = err.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(message.contains("held by another scope"), "{message}");
        drop(first);
        let second = Scope::tmpfs().expect("the volume is free again");
        assert!(second.root().starts_with(volume()));
    }
}
