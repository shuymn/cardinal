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
