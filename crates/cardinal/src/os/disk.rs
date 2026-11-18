//! Free space and the volume of a path (`docs/architecture.md` section 10.4).
//!
//! A path's volume is its mount, identified as `os::mount` identifies it: the path the file
//! system is mounted on (`statfs`) on macOS and the mount ID (`statx`) on Linux, whose
//! device, root, and mount point come from `/proc/self/mountinfo`. Free space is what new
//! data can use: on macOS the important-usage capacity of `CFURL` (purgeable space
//! included), falling back to the available capacity when that is missing or zero, and
//! `statfs` for [`Measure::Statfs`]; on Linux `statvfs` (`f_bavail × f_frsize`) for both.
//! Both resolve symbolic links first, so a path's identity and its readings are of the same
//! file system; [`entry_volume`] resolves all but the final component, giving the volume of
//! an entry itself, a link included. Windows reads neither: every read is unavailable, which
//! changes no disk state.

use std::path::{Path, PathBuf};

use super::{Measure, Space, Unavailable};

/// A volume: what identifies its mount, and the path it is mounted on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    /// Compare with `==`; the volume key of section 10.4 is the FNV-1a of these bytes.
    pub id: Vec<u8>,
    pub mount: PathBuf,
}

/// The volume holding `path`, symbolic links resolved.
pub fn volume(path: &Path) -> Result<Volume, Unavailable> {
    imp::volume(&resolved(path)?)
}

/// The volume holding the entry at `path` itself: the directory that holds it resolved, a
/// final symbolic link not followed, so a link, dangling or not, is on its directory's
/// volume.
pub fn entry_volume(path: &Path) -> Result<Volume, Unavailable> {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => imp::volume(&resolved(parent)?.join(name)),
        _ => volume(path),
    }
}

/// The free and total bytes of the volume holding `path`, symbolic links resolved.
pub fn space(path: &Path, measure: Measure) -> Result<Space, Unavailable> {
    imp::space(&resolved(path)?, measure)
}

/// `path` with every symbolic link resolved: `os::mount` identifies the mount of a final
/// link itself, while `statvfs` and `CFURL` read what it points to.
fn resolved(path: &Path) -> Result<PathBuf, Unavailable> {
    std::fs::canonicalize(path).map_err(|err| Unavailable(format!("{}: {err}", path.display())))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn unavailable(path: &Path, what: &str, err: impl std::fmt::Display) -> Unavailable {
    Unavailable(format!("{}: {what}: {err}", path.display()))
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::c_void;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr::{self, NonNull};

    use objc2_core_foundation::{
        CFNumber, CFRetained, CFString, CFType, CFURL,
        kCFURLVolumeAvailableCapacityForImportantUsageKey, kCFURLVolumeAvailableCapacityKey,
        kCFURLVolumeTotalCapacityKey,
    };

    use super::{Measure, Space, Unavailable, Volume, unavailable};

    pub fn volume(path: &Path) -> Result<Volume, Unavailable> {
        let id = crate::os::mount::of_path(path).map_err(|err| unavailable(path, "mount", err))?;
        let mount = PathBuf::from(std::ffi::OsStr::from_bytes(&id));
        Ok(Volume { id, mount })
    }

    pub fn space(path: &Path, measure: Measure) -> Result<Space, Unavailable> {
        match measure {
            Measure::Statfs => statfs(path),
            Measure::Usable => usable(path),
        }
    }

    fn statfs(path: &Path) -> Result<Space, Unavailable> {
        let st = rustix::fs::statfs(path).map_err(|err| unavailable(path, "statfs", err))?;
        let block = u64::from(st.f_bsize);
        Ok(Space {
            free_bytes: st.f_bavail.saturating_mul(block),
            total_bytes: st.f_blocks.saturating_mul(block),
        })
    }

    /// The important-usage capacity, else the available capacity, of the volume holding
    /// `path`, read through `CFURL` as `sysinfo` reads it.
    fn usable(path: &Path) -> Result<Space, Unavailable> {
        let url = CFURL::from_file_path(path)
            .ok_or_else(|| unavailable(path, "CFURL", "not a file URL"))?;
        // SAFETY: the keys are CoreFoundation's own constants.
        let (important, available, total) = unsafe {
            (
                kCFURLVolumeAvailableCapacityForImportantUsageKey,
                kCFURLVolumeAvailableCapacityKey,
                kCFURLVolumeTotalCapacityKey,
            )
        };
        let free = number(&url, important)
            .filter(|bytes| *bytes != 0)
            .or_else(|| number(&url, available))
            .ok_or_else(|| unavailable(path, "CFURL", "no available capacity"))?;
        let total =
            number(&url, total).ok_or_else(|| unavailable(path, "CFURL", "no total capacity"))?;
        Ok(Space {
            free_bytes: free,
            total_bytes: total,
        })
    }

    /// The resource property `key` of `url` as a non-negative number.
    fn number(url: &CFURL, key: Option<&CFString>) -> Option<u64> {
        let key = key?;
        let mut value: *const c_void = ptr::null();
        // SAFETY: `value` is a valid out pointer, and a null error pointer asks for no error.
        // A property copied out follows the Create rule: it is released below.
        let found = unsafe {
            url.resource_property_for_key(
                Some(key),
                (&raw mut value).cast::<c_void>(),
                ptr::null_mut(),
            )
        };
        let value = NonNull::new(value.cast_mut())?;
        // SAFETY: a +1 reference to a CoreFoundation object, which this takes over.
        let value: CFRetained<CFType> = unsafe { CFRetained::from_raw(value.cast()) };
        if !found {
            return None;
        }
        let number = value.downcast::<CFNumber>().ok()?;
        number.as_i64().and_then(|n| u64::try_from(n).ok())
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::path::{Path, PathBuf};

    use super::{Measure, Space, Unavailable, Volume, unavailable};

    /// The volume of `path`: its mount ID, then the device and root `/proc/self/mountinfo`
    /// lists for that mount, since the kernel reuses the ID of a mount that is gone and a
    /// new mount must not take the history of an old one.
    pub fn volume(path: &Path) -> Result<Volume, Unavailable> {
        let mut id =
            crate::os::mount::of_path(path).map_err(|err| unavailable(path, "mount", err))?;
        let wanted = id
            .as_slice()
            .try_into()
            .map(u64::from_ne_bytes)
            .map_err(|_| unavailable(path, "mount", "an unexpected mount ID"))?;
        let mountinfo = std::fs::read("/proc/self/mountinfo")
            .map_err(|err| unavailable(path, "/proc/self/mountinfo", err))?;
        let (device, root, mount) = mount_line(&mountinfo, wanted)
            .ok_or_else(|| unavailable(path, "/proc/self/mountinfo", "the mount is not listed"))?;
        for field in [device, root] {
            id.push(0);
            id.extend_from_slice(&field);
        }
        Ok(Volume { id, mount })
    }

    /// The device (`major:minor`), the root in its file system, and the mount point of the
    /// mount whose ID is `wanted`, from the lines of `/proc/self/mountinfo`: the mount ID
    /// first, then the parent's, the device, the root, and the mount point, with a space,
    /// tab, newline, or backslash written as its three-digit octal escape.
    pub(super) fn mount_line(mountinfo: &[u8], wanted: u64) -> Option<(Vec<u8>, Vec<u8>, PathBuf)> {
        use std::os::unix::ffi::OsStringExt;
        mountinfo.split(|&b| b == b'\n').find_map(|line| {
            let mut fields = line.split(|&b| b == b' ');
            let id: u64 = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
            if id != wanted {
                return None;
            }
            let device = fields.nth(1)?.to_vec();
            let root = unescape(fields.next()?);
            let point = fields.next()?;
            Some((
                device,
                root,
                PathBuf::from(std::ffi::OsString::from_vec(unescape(point))),
            ))
        })
    }

    fn unescape(field: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(field.len());
        let mut i = 0;
        while i < field.len() {
            let octal = field
                .get(i + 1..i + 4)
                .filter(|_| field[i] == b'\\')
                .and_then(|digits| std::str::from_utf8(digits).ok())
                .and_then(|digits| u8::from_str_radix(digits, 8).ok());
            if let Some(byte) = octal {
                out.push(byte);
                i += 4;
            } else {
                out.push(field[i]);
                i += 1;
            }
        }
        out
    }

    pub fn space(path: &Path, _measure: Measure) -> Result<Space, Unavailable> {
        let st = rustix::fs::statvfs(path).map_err(|err| unavailable(path, "statvfs", err))?;
        Ok(Space {
            free_bytes: st.f_bavail.saturating_mul(st.f_frsize),
            total_bytes: st.f_blocks.saturating_mul(st.f_frsize),
        })
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use std::path::Path;

    use super::{Measure, Space, Unavailable, Volume};

    pub fn volume(path: &Path) -> Result<Volume, Unavailable> {
        Err(Unavailable(format!(
            "{}: volumes are not read on this platform",
            path.display()
        )))
    }

    pub fn space(path: &Path, _measure: Measure) -> Result<Space, Unavailable> {
        Err(Unavailable(format!(
            "{}: free space is not read on this platform",
            path.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::{Measure, space, volume};

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_directory_has_space_and_a_volume() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("dir")).unwrap();
        for measure in [Measure::Usable, Measure::Statfs] {
            let space = space(tmp.path(), measure).unwrap();
            assert!(space.total_bytes > 0, "{measure:?}: {space:?}");
            assert!(
                space.free_bytes <= space.total_bytes,
                "{measure:?}: {space:?}"
            );
        }
        let own = volume(tmp.path()).unwrap();
        assert_eq!(volume(&tmp.path().join("dir")).unwrap(), own);
        let canonical = std::fs::canonicalize(tmp.path()).unwrap();
        assert!(own.mount.is_absolute(), "{own:?}");
        if cfg!(target_os = "linux") {
            assert!(canonical.starts_with(&own.mount), "{own:?}");
        }
        assert!(space(&tmp.path().join("missing"), Measure::Statfs).is_err());
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    #[test]
    fn nothing_is_read_on_this_platform() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(space(tmp.path(), Measure::Usable).is_err());
        assert!(volume(tmp.path()).is_err());
    }

    /// A symbolic link to a directory on another volume is that volume: its identity and its
    /// readings both (section 10.4), the link resolved as `statvfs` and `CFURL` resolve it.
    /// `CFURL` reads the volume of an unresolved link itself, so on macOS the `Usable`
    /// reading fails without the resolution; Linux's `statvfs` follows links either way. As an
    /// entry, the link is on its directory's volume, dangling or not.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_link_is_the_volume_it_points_to() {
        let scope = cardinal_test_support::Scope::new();
        #[cfg(target_os = "macos")]
        let target = scope.path("volume");
        #[cfg(target_os = "macos")]
        let Some(_image) = ({
            std::fs::create_dir(&target).unwrap();
            scope.attach_image(&target)
        }) else {
            return;
        };
        #[cfg(target_os = "linux")]
        let Some(other) = cardinal_test_support::Scope::tmpfs() else {
            return;
        };
        #[cfg(target_os = "linux")]
        let target = other.root().to_path_buf();
        let link = scope.path("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let there = volume(&target).unwrap();
        assert_ne!(volume(scope.root()).unwrap(), there, "two volumes");
        assert_eq!(volume(&link).unwrap(), there);
        let here = volume(scope.root()).unwrap();
        let dangling = scope.path("dangling");
        std::os::unix::fs::symlink(target.join("gone"), &dangling).unwrap();
        for entry in [&link, &dangling] {
            assert_eq!(
                super::entry_volume(entry).unwrap(),
                here,
                "{}",
                entry.display()
            );
        }
        for measure in [Measure::Statfs, Measure::Usable] {
            assert_eq!(
                space(&link, measure).unwrap().total_bytes,
                space(&target, measure).unwrap().total_bytes,
                "{measure:?}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mount_lines_are_unescaped() {
        let info = b"21 1 0:20 / /proc rw - proc proc rw\n\
            36 21 98:0 /sub\\040dir /mnt/with\\040space rw - ext4 /dev/x rw\n";
        assert_eq!(
            super::imp::mount_line(info, 36),
            Some((
                b"98:0".to_vec(),
                b"/sub dir".to_vec(),
                "/mnt/with space".into()
            ))
        );
        assert_eq!(
            super::imp::mount_line(info, 21),
            Some((b"0:20".to_vec(), b"/".to_vec(), "/proc".into()))
        );
        assert_eq!(super::imp::mount_line(info, 7), None);
    }

    /// The same mount ID on another device, or with another root, is another volume: a new
    /// mount that reuses the ID of one that is gone does not take its history.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_volume_is_more_than_its_mount_id() {
        let tmp = tempfile::tempdir().unwrap();
        let own = volume(tmp.path()).unwrap();
        let mount_id =
            crate::os::mount::of_path(&std::fs::canonicalize(tmp.path()).unwrap()).unwrap();
        let wanted = u64::from_ne_bytes(mount_id.as_slice().try_into().unwrap());
        let mountinfo = std::fs::read("/proc/self/mountinfo").unwrap();
        let (device, root, _) = super::imp::mount_line(&mountinfo, wanted).unwrap();
        assert!(device.contains(&b':') && root.starts_with(b"/"), "{own:?}");
        let expected = [mount_id.as_slice(), &[0u8], &device[..], &[0u8], &root[..]].concat();
        assert_eq!(own.id, expected);
    }
}
