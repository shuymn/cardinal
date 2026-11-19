//! Mount identity (`docs/architecture.md` section 9.2): what tells a mount point from a plain
//! directory when both are on the same device, as a bind mount is.
//!
//! The identity is the mount ID (`statx`, Linux 5.8 and later) on Linux and the path the file
//! system is mounted on (`statfs`) on macOS. Two entries are on the same mount when their
//! identities are equal. An error means the mount cannot be told, and the callers fail
//! closed; a device comparison alone never stands in for it.

use std::ffi::OsStr;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

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

/// One line of `/proc/self/mountinfo` (`proc(5)`): the mount's ID (field 1), the device of
/// its file system (field 3), the path in that file system it shows (its root, field 4), and
/// where it is mounted (field 5), each path with the kernel's octal escapes undone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountLine {
    pub id: u64,
    pub dev: (u32, u32),
    pub root: PathBuf,
    pub mount_point: PathBuf,
}

/// Parses `/proc/self/mountinfo`. Each row must hold every field `proc(5)` gives it: the
/// mount and parent IDs, the device, the root, the mount point, the options, any optional
/// fields, the `-` separator, and the file system type, source, and super options. A row cut
/// short, or with a field that does not parse, is an error, so a listing that is not complete
/// decides nothing.
pub fn parse_mountinfo(text: &[u8]) -> Result<Vec<MountLine>, String> {
    let bad = |line: &[u8]| format!("mountinfo line {:?}", String::from_utf8_lossy(line));
    let mut lines = Vec::new();
    // A last line without its line feed may be cut short.
    let Some(body) = text.strip_suffix(b"\n") else {
        return Err("mountinfo does not end in a line feed".into());
    };
    if body.is_empty() {
        return Err("mountinfo lists no mount".into());
    }
    for line in body.split(|&b| b == b'\n') {
        let fields: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
        let [id, parent, dev, root, mount_point, options, ..] = fields[..] else {
            return Err(bad(line));
        };
        // The separator follows the optional fields; three fields follow it.
        let complete = fields[6..]
            .iter()
            .position(|f| *f == b"-")
            .is_some_and(|at| fields.len() == 6 + at + 4)
            && fields.iter().all(|f| !f.is_empty())
            && !options.is_empty();
        let number = |field: &[u8]| std::str::from_utf8(field).ok()?.parse::<u64>().ok();
        if !complete || number(parent).is_none() {
            return Err(bad(line));
        }
        let id = number(id).ok_or_else(|| bad(line))?;
        let dev = std::str::from_utf8(dev)
            .ok()
            .and_then(|dev| dev.split_once(':'))
            .and_then(|(major, minor)| Some((major.parse().ok()?, minor.parse().ok()?)))
            .ok_or_else(|| bad(line))?;
        lines.push(MountLine {
            id,
            dev,
            root: unescape(root).ok_or_else(|| bad(line))?,
            mount_point: unescape(mount_point).ok_or_else(|| bad(line))?,
        });
    }
    Ok(lines)
}

/// A mountinfo path with its `\ooo` escapes undone (space, tab, newline, backslash).
fn unescape(field: &[u8]) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut out = Vec::with_capacity(field.len());
    let mut at = 0;
    while at < field.len() {
        if field[at] == b'\\' {
            let digits = field.get(at + 1..at + 4)?;
            let value = std::str::from_utf8(digits).ok()?;
            out.push(u8::from_str_radix(value, 8).ok()?);
            at += 4;
        } else {
            out.push(field[at]);
            at += 1;
        }
    }
    (out.first() == Some(&b'/')).then(|| PathBuf::from(OsStr::from_bytes(&out)))
}

/// The mount points of the mounts in `lines` that show the target, or a directory in it,
/// elsewhere (section 9.7, `bound_elsewhere`): the target is on the mount `mount_id` of the
/// file system on `dev`, at `path` as this process sees it; a mount of that file system whose
/// root is at or below the target's path in the file system keeps the moved tree reachable at
/// its mount point. An error when the target's own mount is not listed or does not contain
/// `path`.
pub fn binds_of(
    lines: &[MountLine],
    (dev, mount_id): ((u32, u32), u64),
    path: &Path,
) -> Result<Vec<PathBuf>, String> {
    let own = lines
        .iter()
        .find(|line| line.id == mount_id)
        .ok_or_else(|| format!("mountinfo does not list the mount {mount_id}"))?;
    let rel = path.strip_prefix(&own.mount_point).map_err(|_| {
        format!(
            "{} is not below its mount point {}",
            path.display(),
            own.mount_point.display()
        )
    })?;
    let in_fs = own.root.join(rel);
    Ok(lines
        .iter()
        .filter(|line| line.dev == dev && line.root.starts_with(&in_fs))
        .map(|line| line.mount_point.clone())
        .collect())
}

/// The mount points that show the entry `name` of the open directory `dir`, or a directory in
/// it, elsewhere ([`binds_of`]), read from `/proc/self/mountinfo` (Linux). macOS has no bind
/// mount that a user without root can make, so there it is none.
#[cfg(target_os = "linux")]
pub fn bound_elsewhere(dir: impl AsFd, name: &OsStr) -> Result<Vec<PathBuf>, String> {
    use rustix::fs::{AtFlags, StatxFlags};
    let st = rustix::fs::statx(
        &dir,
        name,
        AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT,
        StatxFlags::MNT_ID,
    )
    .map_err(|err| format!("statx: {err}"))?;
    if !StatxFlags::from_bits_retain(st.stx_mask).contains(StatxFlags::MNT_ID) {
        return Err("statx reports no mount ID (Linux before 5.8)".to_owned());
    }
    let parent =
        super::dir::current_path(&dir).map_err(|err| format!("the parent's path: {err}"))?;
    let text = std::fs::read("/proc/self/mountinfo")
        .map_err(|err| format!("/proc/self/mountinfo: {err}"))?;
    binds_of(
        &parse_mountinfo(&text)?,
        ((st.stx_dev_major, st.stx_dev_minor), st.stx_mnt_id),
        &parent.join(name),
    )
}

/// See the Linux version: macOS has no bind mount that a user without root can make.
#[cfg(not(target_os = "linux"))]
pub fn bound_elsewhere(_dir: impl AsFd, _name: &OsStr) -> Result<Vec<PathBuf>, String> {
    Ok(Vec::new())
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
    use std::path::{Path, PathBuf};

    /// A mount of the target's file system whose root is at or below the target, or below a
    /// directory in it, binds it elsewhere; mounts of other file systems, and the target's own
    /// mount, do not. Escaped paths are read as the kernel wrote them.
    #[test]
    fn mountinfo_binds() {
        let text = b"22 1 8:1 / / rw - ext4 /dev/sda1 rw\n\
              30 22 8:1 /home/u/w/target /mnt/bound rw - ext4 /dev/sda1 rw\n\
              31 22 8:1 /home/u/w/target/debug /mnt/deep\\040dir rw - ext4 /dev/sda1 rw\n\
              32 22 8:1 /home/u/w/targets /mnt/sibling rw - ext4 /dev/sda1 rw\n\
              33 22 8:2 /home/u/w/target /mnt/other-fs rw - ext4 /dev/sda2 rw\n\
              40 22 8:1 /home /home rw - ext4 /dev/sda1 rw\n";
        let lines = super::parse_mountinfo(text).unwrap();
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[2].mount_point, PathBuf::from("/mnt/deep dir"));
        let target = Path::new("/home/u/w/target");
        assert_eq!(
            super::binds_of(&lines, ((8, 1), 40), target).unwrap(),
            [PathBuf::from("/mnt/bound"), PathBuf::from("/mnt/deep dir")]
        );
        assert_eq!(
            super::binds_of(&lines, ((8, 1), 40), Path::new("/home/u/w/other")).unwrap(),
            Vec::<PathBuf>::new()
        );
        assert!(super::binds_of(&lines, ((8, 1), 99), target).is_err());
        assert!(super::binds_of(&lines, ((8, 1), 30), Path::new("/elsewhere")).is_err());
        for bad in [
            &b""[..],
            b"22 1 8:1 /\n",
            b"x 1 8:1 / / rw\n",
            b"22 1 8 / / rw\n",
            b"22 1 8:1 rel / rw\n",
            b"22 1 8:1 /\\04 / rw\n",
            // A row cut short after the mount point, or before its last fields.
            b"22 1 8:1 / /\n",
            b"22 1 8:1 / / rw\n",
            b"22 1 8:1 / / rw -\n",
            b"22 1 8:1 / / rw - ext4 /dev/sda1\n",
            b"22 x 8:1 / / rw - ext4 /dev/sda1 rw\n",
            b"22 1 8:1 / / rw - ext4 /dev/sda1 rw extra\n",
            // A last row without its line feed, which a read cut short also gives.
            b"22 1 8:1 / / rw - ext4 /dev/sda1 rw",
            b"22 1 8:1 / / rw - ext4 /dev/sda1 rw\n23 22 8:2 / /a rw - ext4 /dev/sda2 rw",
        ] {
            assert!(super::parse_mountinfo(bad).is_err(), "{bad:?}");
        }
    }

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
