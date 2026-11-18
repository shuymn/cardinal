//! Small APFS volumes from a disk image (macOS), for tests of a volume that fills up
//! (`docs/architecture.md` section 13.3).
//!
//! `scripts/test` creates one empty image under the per-run root before the tests start,
//! because `hdiutil create` fails inside the tests' sandbox while `hdiutil attach` of an
//! existing image works there. Each attachment clones that image into the scope's root and
//! attaches the clone with `-nobrowse`. The process that mounts it runs outside the sandbox,
//! so the guard detaches it. A guard that cannot leaves the clone and the scope's root to the
//! run's sweep, which detaches what is still attached under the run's root and fails the run.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use tempfile::TempDir;

use super::{Scope, report, skip};

/// The variable naming the empty image `scripts/test` creates.
pub const IMAGE_VAR: &str = "CARDINAL_TEST_IMAGE";

/// How long one `hdiutil attach` or `hdiutil detach` may take.
const HDIUTIL_DEADLINE: Duration = Duration::from_secs(60);

/// `CLONE_NOFOLLOW` of `<sys/clonefile.h>`: a source that is a symbolic link is cloned as the
/// link, not followed. The `libc` crate does not define it.
const CLONE_NOFOLLOW: u32 = 0x0001;

const HDIUTIL: &str = "/usr/bin/hdiutil";

impl Scope {
    /// Attaches a clone of the empty APFS image `scripts/test` created (256 MiB, named by
    /// `CARDINAL_TEST_IMAGE`) at `target`, an existing directory inside the root. Writes past
    /// its size fail with `ENOSPC`. The guard detaches it when it drops ([`Image`]).
    ///
    /// Without an image this returns `None` and reports the skip on stderr, and panics where
    /// `CI` is `true`, so a CI run never passes a test that needs it without running it.
    /// Panics if the image does not attach.
    #[must_use]
    pub fn attach_image(&self, target: &Path) -> Option<Image<'_>> {
        let Some(image) = std::env::var_os(IMAGE_VAR) else {
            skip(&format!(
                "no disk image: {IMAGE_VAR} is unset; scripts/test creates one"
            ));
            return None;
        };
        Some(
            self.attach(Path::new(&image), target)
                .unwrap_or_else(|err| {
                    panic!("attach the disk image at {}: {err}", target.display())
                }),
        )
    }

    /// Clones `image` into the root and attaches the clone at `target`, relative to this
    /// process's working directory. On any failure the clone is removed and nothing stays
    /// attached at `target`.
    pub(crate) fn attach(&self, image: &Path, target: &Path) -> io::Result<Image<'_>> {
        // One absolute path for the checks, `hdiutil`, and the cleanup: `hdiutil` runs in the
        // root, where a relative `target` would name another directory.
        let target = &std::path::absolute(target)?;
        let meta = std::fs::symlink_metadata(target)?;
        if !meta.is_dir() {
            return Err(io::Error::other(format!(
                "the mount point {} is not a directory",
                target.display()
            )));
        }
        let target = &std::fs::canonicalize(target)?;
        self.assert_inside(target, "image mount point");
        let parent_dev = meta.dev();
        if !std::fs::symlink_metadata(image)?.is_file() {
            return Err(io::Error::other(format!(
                "the image {} is not a regular file",
                image.display()
            )));
        }
        let clone_dir = tempfile::Builder::new()
            .prefix("image-")
            .tempdir_in(self.root())?;
        let clone = clone_dir.path().join("volume.dmg");
        clone_file(image, &clone)?;

        let attached = self
            .hdiutil(
                Path::new(HDIUTIL),
                &[
                    "attach".as_ref(),
                    "-nobrowse".as_ref(),
                    "-noautoopen".as_ref(),
                    "-noverify".as_ref(),
                    "-noautofsck".as_ref(),
                    "-mountpoint".as_ref(),
                    target.as_os_str(),
                    clone.as_os_str(),
                ],
            )
            .and_then(|stdout| {
                device(&stdout).ok_or_else(|| {
                    io::Error::other(format!("no device in hdiutil's output: {stdout}"))
                })
            });
        match attached {
            Ok(device) => Ok(Image {
                scope: self,
                hdiutil: PathBuf::from(HDIUTIL),
                device,
                target: target.clone(),
                parent_dev,
                detached: false,
                clone_dir,
            }),
            Err(err) => {
                // A deadline that ended `hdiutil` may have left the clone attached; only this
                // call mounts at `target`, so a volume there is the clone's.
                if is_mounted(target, parent_dev)
                    && let Err(detach) = self.hdiutil(
                        Path::new(HDIUTIL),
                        &["detach".as_ref(), "-force".as_ref(), target.as_os_str()],
                    )
                {
                    return Err(io::Error::other(format!(
                        "{err}; detaching what it left at {}: {detach}",
                        target.display()
                    )));
                }
                Err(err)
            }
        }
    }

    /// Runs `program`, `hdiutil`, with `args` under [`HDIUTIL_DEADLINE`]; its stdout, or an
    /// error with its status and stderr.
    fn hdiutil(&self, program: &Path, args: &[&std::ffi::OsStr]) -> io::Result<String> {
        let mut command = self.command(program, self.root());
        command.args(args);
        let output = command.output_within(HDIUTIL_DEADLINE)?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(io::Error::other(format!(
                "hdiutil {:?}: {}: {}",
                args.first(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }
}

/// An attached disk image; detached when dropped. A drop that cannot detach it does not
/// panic: it reports the failure on stderr and leaves the clone and the scope's root in
/// place, so nothing is removed through the mounted volume, for the run's sweep.
pub struct Image<'a> {
    scope: &'a Scope,
    /// `hdiutil`, which a test of a failed detach replaces.
    hdiutil: PathBuf,
    /// The image's whole device (`/dev/diskN`); detaching it ejects its APFS container too.
    device: String,
    target: PathBuf,
    /// The device of the mount point's directory before the attach.
    parent_dev: u64,
    detached: bool,
    // Removed after the detach (fields drop after `Drop::drop`), unless it failed.
    clone_dir: TempDir,
}

impl Image<'_> {
    /// Where the volume is mounted.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.target
    }

    /// Detaches the image unless an earlier call did, and checks that `target` is a plain
    /// directory again; only then is it done. After a failure the guard still holds the
    /// image: a later call, or the drop, tries again.
    pub fn detach(&mut self) -> io::Result<()> {
        if self.detached {
            return Ok(());
        }
        self.scope.hdiutil(
            &self.hdiutil,
            &["detach".as_ref(), "-force".as_ref(), self.device.as_ref()],
        )?;
        if is_mounted(&self.target, self.parent_dev) {
            return Err(io::Error::other(format!(
                "{} is still mounted after detaching {}",
                self.target.display(),
                self.device
            )));
        }
        self.detached = true;
        Ok(())
    }
}

impl Drop for Image<'_> {
    fn drop(&mut self) {
        if let Err(err) = self.detach() {
            self.clone_dir.disable_cleanup(true);
            self.scope.keep_root.store(true, Ordering::SeqCst);
            report(&format!(
                "detach the disk image at {}: {err}; left attached, with {}, for the run's sweep",
                self.target.display(),
                self.scope.root().display()
            ));
        }
    }
}

/// Clones `source` to `dest`, which must not exist, with `clonefile`: both share their blocks
/// until one is written. Fails across volumes.
fn clone_file(source: &Path, dest: &Path) -> io::Result<()> {
    let c = |path: &Path| CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other);
    let (source, dest) = (c(source)?, c(dest)?);
    // SAFETY: both pointers are NUL-terminated strings that outlive the call.
    if unsafe { libc::clonefile(source.as_ptr(), dest.as_ptr(), CLONE_NOFOLLOW) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The whole device `hdiutil attach` printed first (`/dev/diskN`).
fn device(stdout: &str) -> Option<String> {
    let first = stdout.split_whitespace().next()?;
    let number = first.strip_prefix("/dev/disk")?;
    (!number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())).then(|| first.to_owned())
}

/// Whether `target` is on another device than its directory was before the attach.
fn is_mounted(target: &Path, parent_dev: u64) -> bool {
    std::fs::symlink_metadata(target).is_ok_and(|meta| meta.dev() != parent_dev)
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::os::unix::fs::MetadataExt as _;
    use std::path::{Path, PathBuf};

    use super::{Scope, device};
    use crate::scope::fill;

    fn dev(path: &Path) -> u64 {
        std::fs::symlink_metadata(path).unwrap().dev()
    }

    /// A clone of the run's image is a volume of its own at the target: writes past its
    /// 256 MiB fail with `ENOSPC`, its space returns on unlink, and detaching leaves the
    /// target a plain directory again.
    #[test]
    fn an_attached_image_is_a_small_volume_until_detached() {
        let scope = Scope::new();
        let target = scope.path("volume");
        std::fs::create_dir(&target).unwrap();
        let Some(mut image) = scope.attach_image(&target) else {
            return;
        };
        assert_eq!(image.path(), target);
        assert_ne!(dev(&target), dev(scope.root()), "a volume of its own");
        let (written, err) = fill(&target.join("fill"));
        let err = err.expect("the volume fills");
        assert_eq!(err.kind(), io::ErrorKind::StorageFull, "{err}");
        assert!(written < 256 << 20, "{written}");
        std::fs::remove_file(target.join("fill")).unwrap();
        let (_, err) = fill(&target.join("again"));
        assert_eq!(err.unwrap().kind(), io::ErrorKind::StorageFull);
        image.detach().unwrap();
        assert_eq!(dev(&target), dev(scope.root()), "detached");
        assert!(!target.join("again").exists());
    }

    /// The guard detaches the image when it drops.
    #[test]
    fn dropping_the_guard_detaches_the_image() {
        let scope = Scope::new();
        let target = scope.path("volume");
        std::fs::create_dir(&target).unwrap();
        let Some(image) = scope.attach_image(&target) else {
            return;
        };
        std::fs::write(target.join("data"), b"x").unwrap();
        drop(image);
        assert_eq!(dev(&target), dev(scope.root()));
        assert!(!target.join("data").exists());
    }

    /// An attach that fails leaves nothing mounted and no clone in the root: a mount point
    /// that is not a directory, an image that is missing or a symbolic link, and a file
    /// `hdiutil` cannot attach.
    #[test]
    fn a_failed_attach_leaves_nothing_behind() {
        let scope = Scope::new();
        let target = scope.path("volume");
        std::fs::create_dir(&target).unwrap();
        let not_an_image = scope.path("not-an-image.dmg");
        std::fs::write(&not_an_image, b"not a disk image").unwrap();
        let link = scope.path("link.dmg");
        std::os::unix::fs::symlink(&not_an_image, &link).unwrap();
        let file_target = scope.path("file");
        std::fs::write(&file_target, b"").unwrap();
        let before = listing(scope.root());

        for (image, at) in [
            (&not_an_image, &file_target),
            (&scope.path("missing.dmg"), &target),
            (&link, &target),
            (&not_an_image, &target),
        ] {
            let err = scope.attach(image, at).err().expect("the attach fails");
            assert_eq!(dev(at), dev(scope.root()), "{err}");
            assert_eq!(listing(scope.root()), before, "{err}");
        }
    }

    /// A detach that fails keeps the guard and the image counted as attached, twice in a
    /// row, so dropping the guard tries again and detaches it.
    #[test]
    fn a_failed_detach_is_tried_again_on_drop() {
        let scope = Scope::new();
        let target = scope.path("volume");
        std::fs::create_dir(&target).unwrap();
        let Some(mut image) = scope.attach_image(&target) else {
            return;
        };
        image.hdiutil = PathBuf::from("/usr/bin/false");
        for attempt in 1..=2 {
            image.detach().expect_err("hdiutil fails");
            assert_ne!(
                dev(&target),
                dev(scope.root()),
                "still attached ({attempt})"
            );
        }
        image.hdiutil = PathBuf::from(super::HDIUTIL);
        drop(image);
        assert_eq!(dev(&target), dev(scope.root()), "detached on drop");
    }

    /// A guard that cannot detach its image, in `detach` and again in its drop, does not
    /// panic: the image stays attached, and its clone and the scope's root stay for the
    /// run's sweep, which this test then does itself.
    #[test]
    fn a_guard_that_cannot_detach_leaves_the_image_to_the_sweep() {
        let scope = Scope::new();
        let root = scope.root().to_owned();
        let target = scope.path("volume");
        std::fs::create_dir(&target).unwrap();
        let Some(mut image) = scope.attach_image(&target) else {
            return;
        };
        let device = image.device.clone();
        let clone = image.clone_dir.path().join("volume.dmg");
        image.hdiutil = PathBuf::from("/usr/bin/false");
        image.detach().expect_err("hdiutil fails");
        drop(image);
        drop(scope);
        assert_ne!(dev(&target), dev(&root), "still attached");
        assert!(clone.is_file(), "the clone stays");

        let sweep = Scope::new();
        sweep
            .hdiutil(
                Path::new(super::HDIUTIL),
                &["detach".as_ref(), "-force".as_ref(), device.as_ref()],
            )
            .unwrap();
        assert_eq!(dev(&target), dev(&root), "detached by the sweep");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A relative mount point is taken from this process's working directory, which need not
    /// be the root, where `hdiutil` runs: the volume is attached there, and detached from
    /// there.
    #[test]
    fn a_relative_mount_point_is_taken_from_the_working_directory() {
        let scope = Scope::new();
        let below = scope.path("below");
        std::fs::create_dir_all(below.join("volume")).unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(&below).unwrap();
        let image = scope.attach_image(Path::new("volume"));
        std::env::set_current_dir(previous).unwrap();
        let Some(mut image) = image else {
            return;
        };
        assert_eq!(image.path(), below.join("volume"));
        assert_ne!(dev(&below.join("volume")), dev(scope.root()));
        image.detach().unwrap();
        assert_eq!(dev(&below.join("volume")), dev(scope.root()));
    }

    /// The names directly in `dir`, sorted.
    fn listing(dir: &Path) -> Vec<std::ffi::OsString> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_whole_device_is_the_first_one_printed() {
        let out = "/dev/disk4          \t                               \t\n\
                   /dev/disk5          \tEF57347C-0000-11AA-AA11-0030654\t\n\
                   /dev/disk5s1        \t41504653-0000-11AA-AA11-0030654\t/tmp/x\n";
        assert_eq!(device(out).as_deref(), Some("/dev/disk4"));
        assert_eq!(device("/dev/disk4s1 x\n"), None);
        assert_eq!(device("/dev/disk\n"), None);
        assert_eq!(device(""), None);
    }
}
