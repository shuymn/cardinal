//! Mount identity (`docs/architecture.md` section 9.2): what tells a mount point from a plain
//! directory when both are on the same device, as a bind mount is.
//!
//! The identity is the mount ID (`statx`, Linux 5.8 and later) on Linux and the path the file
//! system is mounted on (`statfs`) on macOS. Two entries are on the same mount when their
//! identities are equal. An error means the mount cannot be told, and the callers fail
//! closed; a device comparison alone never stands in for it.

use std::ffi::OsStr;
use std::os::fd::AsFd;
use std::path::Path;

/// What identifies a mount; compare with `==`.
pub type MountId = Vec<u8>;

/// The mount of the open directory `dir`.
#[cfg(target_os = "linux")]
pub fn of_dir(dir: impl AsFd) -> Result<MountId, String> {
    statx_mount(dir, c"", rustix::fs::AtFlags::EMPTY_PATH)
}

/// The mount of the open directory `dir`.
#[cfg(target_os = "macos")]
pub fn of_dir(dir: impl AsFd) -> Result<MountId, String> {
    let st = rustix::fs::fstatfs(dir).map_err(|err| format!("fstatfs: {err}"))?;
    Ok(st
        .f_mntonname
        .iter()
        .map(|c| u8::from_ne_bytes(c.to_ne_bytes()))
        .take_while(|&b| b != 0)
        .collect())
}

/// The mount of the open directory `dir`.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn of_dir(_dir: impl AsFd) -> Result<MountId, String> {
    Err("mount identity is not available on this platform".to_owned())
}

/// The mount of the entry `name` in the open directory `dir` that is not a directory,
/// without following a final symbolic link. Only Linux mounts on files; elsewhere such an
/// entry is on its directory's mount.
pub fn of_entry_at(dir: impl AsFd, name: &OsStr) -> Result<MountId, String> {
    #[cfg(target_os = "linux")]
    {
        statx_mount(dir, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = name;
        of_dir(dir)
    }
}

/// The mount of the entry at `path`, without following a final symbolic link.
pub fn of_path(path: &Path) -> Result<MountId, String> {
    #[cfg(target_os = "linux")]
    {
        statx_mount(rustix::fs::CWD, path, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
    }
    #[cfg(not(target_os = "linux"))]
    {
        use rustix::fs::{FileType, Mode, OFlags};
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let read = |path: &Path, flags: OFlags| {
            rustix::fs::open(path, flags, Mode::empty())
                .map_err(|err| format!("{}: {err}", path.display()))
                .and_then(of_dir)
        };
        let st = rustix::fs::lstat(path).map_err(|err| format!("{}: {err}", path.display()))?;
        if FileType::from_raw_mode(st.st_mode) == FileType::Directory {
            read(path, flags | OFlags::NOFOLLOW)
        } else {
            read(path.parent().unwrap_or(path), flags)
        }
    }
}

/// The type of the file system the open directory `dir` is on, which `local_volume` and the
/// freed-bytes bound read (`docs/architecture.md` sections 8.5 and 9.2): `f_fstypename` on
/// macOS (`apfs`, `hfs`, `nfs`, `smbfs`, …), and on Linux the name of `f_type`'s magic number
/// (`ext2/ext3/ext4`, which share one, `xfs`, `btrfs`, `tmpfs`, `nfs`, `fuse`, …, or the number
/// in hexadecimal when it has no name here).
#[cfg(target_os = "macos")]
pub fn fs_type(dir: impl AsFd) -> Result<String, String> {
    let st = rustix::fs::fstatfs(dir).map_err(|err| format!("fstatfs: {err}"))?;
    let name: Vec<u8> = st
        .f_fstypename
        .iter()
        .map(|c| u8::from_ne_bytes(c.to_ne_bytes()))
        .take_while(|&b| b != 0)
        .collect();
    Ok(String::from_utf8_lossy(&name).into_owned())
}

/// The type of the file system the open directory `dir` is on (see the macOS version).
#[cfg(target_os = "linux")]
#[allow(
    clippy::useless_conversion,
    clippy::unnecessary_fallible_conversions,
    reason = "the type of `f_type` differs between platforms"
)]
pub fn fs_type(dir: impl AsFd) -> Result<String, String> {
    let st = rustix::fs::fstatfs(dir).map_err(|err| format!("fstatfs: {err}"))?;
    // A 32-bit `f_type` may come sign-extended: the magic numbers are 32 bits.
    let magic = i64::try_from(st.f_type).map_err(|err| format!("f_type: {err}"))? & 0xffff_ffff;
    Ok(fs_type_name(magic).map_or_else(|| format!("{magic:#x}"), str::to_owned))
}

/// The type of the file system the open directory `dir` is on.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn fs_type(_dir: impl AsFd) -> Result<String, String> {
    Err("the file system type is not read on this platform".to_owned())
}

/// The name of a Linux file system's magic number (`statfs(2)`, `<linux/magic.h>`).
#[cfg(target_os = "linux")]
fn fs_type_name(magic: i64) -> Option<&'static str> {
    Some(match magic {
        0xEF53 => "ext2/ext3/ext4",
        0x5846_5342 => "xfs",
        0x9123_683E => "btrfs",
        0x0102_1994 => "tmpfs",
        0x8584_58F6 => "ramfs",
        0x6969 => "nfs",
        0x6573_5546 => "fuse",
        0x517B => "smb",
        0xFF53_4D42 => "cifs",
        0xFE53_4D42 => "smb2",
        0x0102_1997 => "v9fs",
        0x794C_7630 => "overlayfs",
        0x2FC1_2FC1 => "zfs",
        0xCA45_1A4E => "bcachefs",
        0xF2F5_2010 => "f2fs",
        0x7371_7368 => "squashfs",
        0x4D44 => "msdos",
        0x5346_544E => "ntfs",
        _ => return None,
    })
}

#[cfg(target_os = "linux")]
fn statx_mount<P: rustix::path::Arg>(
    dir: impl AsFd,
    path: P,
    flags: rustix::fs::AtFlags,
) -> Result<MountId, String> {
    use rustix::fs::{AtFlags, StatxFlags};
    let st = rustix::fs::statx(dir, path, flags | AtFlags::NO_AUTOMOUNT, StatxFlags::MNT_ID)
        .map_err(|err| format!("statx: {err}"))?;
    if StatxFlags::from_bits_retain(st.stx_mask).contains(StatxFlags::MNT_ID) {
        Ok(st.stx_mnt_id.to_ne_bytes().to_vec())
    } else {
        Err("statx reports no mount ID (Linux before 5.8)".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    #[test]
    fn a_mount_point_has_another_identity_than_its_parent() {
        let mount = Path::new(if cfg!(target_os = "linux") {
            "/proc"
        } else {
            "/dev"
        });
        let parent = super::of_path(Path::new("/"));
        let own = super::of_path(mount);
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert_ne!(parent.unwrap(), own.unwrap());
        } else {
            assert!(parent.is_err() && own.is_err(), "fails closed");
        }
    }

    /// The type of the volume the tests run on is one of the local types, and another mount's
    /// can differ from it.
    #[test]
    fn the_file_system_type_names_the_volume() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = std::fs::File::open(tmp.path()).unwrap();
        let ty = super::fs_type(&dir);
        if cfg!(target_os = "macos") {
            assert!(["apfs", "hfs"].contains(&ty.as_deref().unwrap()), "{ty:?}");
            let dev = std::fs::File::open("/dev").unwrap();
            assert_eq!(super::fs_type(&dev).unwrap(), "devfs");
        } else if cfg!(target_os = "linux") {
            assert!(ty.is_ok(), "{ty:?}");
            let proc = std::fs::File::open("/proc").unwrap();
            assert_eq!(
                super::fs_type(&proc).unwrap(),
                "0x9fa0",
                "procfs has no name here"
            );
        } else {
            assert!(ty.is_err(), "not read on this platform");
        }
    }

    #[test]
    fn entries_of_one_directory_share_its_mount() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("dir")).unwrap();
        std::fs::write(tmp.path().join("file"), b"x").unwrap();
        let dir = super::of_path(tmp.path());
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            let dir = dir.unwrap();
            assert_eq!(super::of_path(&tmp.path().join("dir")).unwrap(), dir);
            assert_eq!(super::of_path(&tmp.path().join("file")).unwrap(), dir);
            let fd = std::fs::File::open(tmp.path()).unwrap();
            assert_eq!(super::of_dir(&fd).unwrap(), dir);
            assert_eq!(super::of_entry_at(&fd, "file".as_ref()).unwrap(), dir);
        }
    }
}
