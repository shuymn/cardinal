//! Directories listed through the descriptor that identifies them (`docs/architecture.md`
//! section 9.2). `plan`'s walk opens a directory without following a final symbolic link,
//! checks the descriptor's identity and mount, and lists that same descriptor, reading each
//! entry's metadata relative to it, so what it lists is the directory it checked.
//! `stat` is also how `act`'s snapshot reads the metadata it records.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;

/// The type of an entry, as `lstat` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

/// What `lstat` reports of an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub kind: Kind,
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch.
    pub mtime_ns: i128,
    /// 512-byte blocks allocated.
    pub blocks: u64,
}

/// Opens the directory at `path` without following a final symbolic link (`O_DIRECTORY |
/// O_NOFOLLOW`); `None` when the entry there is not a directory, a symbolic link included.
pub fn open(path: &Path) -> io::Result<Option<File>> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match rustix::fs::open(path, flags, Mode::empty()) {
        Ok(fd) => Ok(Some(File::from(fd))),
        Err(Errno::LOOP | Errno::NOTDIR) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// The entries of the open directory `dir` in name order, without `.` and `..`: each name
/// with what `lstat` reports of it, read relative to `dir`. A directory removed while it is
/// read ends its listing with what was read, an entry removed before its `lstat` is left
/// out, and any other error fails the listing. `dir` is closed on return.
pub fn list(dir: File) -> io::Result<Vec<(OsString, Stat)>> {
    let mut stream = rustix::fs::Dir::new(OwnedFd::from(dir))?;
    let mut names = Vec::new();
    for entry in stream.by_ref() {
        match entry {
            Ok(entry) => {
                let name = entry.file_name().to_bytes();
                if name != b"." && name != b".." {
                    names.push(OsStr::from_bytes(name).to_os_string());
                }
            }
            Err(Errno::NOENT) => break,
            Err(err) => return Err(err.into()),
        }
    }
    names.sort();
    let fd = stream.fd()?;
    let mut entries = Vec::with_capacity(names.len());
    for name in names {
        match rustix::fs::statat(fd, &name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => entries.push((name, stat(&st))),
            Err(Errno::NOENT) => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(entries)
}

/// What `st`, read by `stat`, `lstat`, or `fstat`, reports of an entry.
#[allow(
    clippy::useless_conversion,
    clippy::unnecessary_fallible_conversions,
    reason = "the field types of `struct stat` differ between platforms"
)]
pub(crate) fn stat(st: &rustix::fs::Stat) -> Stat {
    let kind = match FileType::from_raw_mode(st.st_mode) {
        FileType::RegularFile => Kind::File,
        FileType::Directory => Kind::Dir,
        FileType::Symlink => Kind::Symlink,
        _ => Kind::Other,
    };
    Stat {
        kind,
        dev: u64::try_from(st.st_dev).unwrap_or_default(),
        ino: u64::try_from(st.st_ino).unwrap_or_default(),
        size: u64::try_from(st.st_size).unwrap_or_default(),
        mtime_ns: i128::from(st.st_mtime) * 1_000_000_000 + i128::from(st.st_mtime_nsec),
        blocks: u64::try_from(st.st_blocks).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn lists_through_the_descriptor_without_following_links() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dir");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("file"), b"12345").unwrap();
        std::os::unix::fs::symlink(tmp.path(), dir.join("link")).unwrap();
        std::os::unix::fs::symlink(&dir, tmp.path().join("to-dir")).unwrap();

        assert!(super::open(&tmp.path().join("to-dir")).unwrap().is_none());
        assert!(super::open(&dir.join("file")).unwrap().is_none());
        assert!(super::open(&dir.join("missing")).is_err());

        let opened = super::open(&dir).unwrap().unwrap();
        let entries = super::list(opened).unwrap();
        let names: Vec<_> = entries.iter().map(|(n, _)| n.to_str().unwrap()).collect();
        assert_eq!(names, ["file", "link", "sub"]);
        for (name, st) in &entries {
            let meta = std::fs::symlink_metadata(dir.join(name)).unwrap();
            assert_eq!(
                (st.dev, st.ino, st.size, st.blocks),
                (meta.dev(), meta.ino(), meta.size(), meta.blocks()),
                "{name:?}"
            );
        }
        let kinds: Vec<_> = entries.iter().map(|(_, st)| st.kind).collect();
        assert_eq!(
            kinds,
            [super::Kind::File, super::Kind::Symlink, super::Kind::Dir]
        );
    }

    /// A directory removed after it was opened lists as empty instead of failing the listing,
    /// although Linux reports `ENOENT` when it is read.
    #[test]
    fn a_directory_removed_while_open_lists_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dir");
        std::fs::create_dir(&dir).unwrap();
        let opened = super::open(&dir).unwrap().unwrap();
        std::fs::remove_dir(&dir).unwrap();
        assert_eq!(super::list(opened).unwrap(), []);
    }
}
