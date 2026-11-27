//! Directories listed through the descriptor that identifies them (`docs/architecture.md`
//! sections 8.5 and 9.2). `plan`'s walk opens a directory without following a final symbolic
//! link, checks the descriptor's identity and mount, and lists that same descriptor, reading
//! each entry's metadata relative to it, so what it lists is the directory it checked.
//! `stat` is also how `act`'s snapshot reads the metadata it records.
//!
//! A listing also reads what the freed-bytes bound of section 8.5 needs of each regular file
//! where the file system tells it ([`Own`]): on macOS the directory is listed with
//! `getattrlistbulk`, which returns each entry's private size (`ATTR_CMNEXT_PRIVATESIZE`), and
//! on Linux each regular file's data extents are read with `FIEMAP`. A read that fails is
//! reported with the listing, which goes on without it.
//!
//! An execution's deleter (section 9.7) works through the same descriptors: it opens each
//! level relative to its parent ([`open_at`]), moves entries with a rename that never replaces
//! ([`rename_noreplace`]), and reads where a directory it holds is now ([`current_path`]).

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, RenameFlags};
use rustix::io::Errno;

/// The type of an entry, as `lstat` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Fifo,
    Socket,
    /// A block or character device.
    Device,
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
    /// The number of names the entry has, here and elsewhere.
    pub nlink: u64,
}

/// What a listing reads of each regular file beyond `lstat`: the reading a measure of the
/// freed-bytes bound needs (section 8.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Own {
    Nothing,
    /// The private size, `getattrlistbulk`'s `ATTR_CMNEXT_PRIVATESIZE` (macOS).
    PrivateSize,
    /// The bytes of the data extents `FIEMAP` reports that count ([`counted_extents`], Linux).
    Extents,
}

/// What a listing read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listed {
    /// The entries in name order, without `.` and `..`: each name with what `lstat` reports
    /// of it and, for a regular file, what [`Own`] asked of it when its read returned that.
    pub entries: Vec<(OsString, Stat, Option<u64>)>,
    /// The reads of [`Own`] that failed: the directory's (`None`), which leaves every entry
    /// without its reading, or one entry's, by name.
    pub unread: Vec<(Option<OsString>, String)>,
    /// What was removed while the listing read it: an entry gone before its `lstat` (or its
    /// [`Own`] read), by name, or the directory itself (`None`), whose listing ended there.
    pub vanished: Vec<Option<OsString>>,
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

/// Opens the directory `name` in the open directory `dir` as std's `remove_dir_all` opens
/// each level: relative to `dir`, with `O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC`. `None` when
/// the entry there is not a directory, a symbolic link included.
pub fn open_at(dir: impl AsFd, name: &OsStr) -> io::Result<Option<File>> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match rustix::fs::openat(dir, name, flags, Mode::empty()) {
        Ok(fd) => Ok(Some(File::from(fd))),
        Err(Errno::LOOP | Errno::NOTDIR) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// What `lstat` reports of the entry `name` in the open directory `dir`, not following a
/// final symbolic link.
pub fn stat_at(dir: impl AsFd, name: &OsStr) -> io::Result<Stat> {
    Ok(stat(&rustix::fs::statat(
        dir,
        name,
        AtFlags::SYMLINK_NOFOLLOW,
    )?))
}

/// What `fstat` reports of the open file `fd`.
pub fn stat_fd(fd: impl AsFd) -> io::Result<Stat> {
    Ok(stat(&rustix::fs::fstat(fd)?))
}

/// Renames `from` in the open directory `from_dir` to `to` in `to_dir` only when nothing is at
/// `to`: `renameatx_np` with `RENAME_EXCL` on macOS and `renameat2` with `RENAME_NOREPLACE`
/// on Linux (section 9.7, implementation notes), which fail with `EEXIST` when `to` is taken.
/// Each name is resolved in its own directory, wherever that directory is now.
pub fn rename_noreplace(
    from_dir: impl AsFd,
    from: &OsStr,
    to_dir: impl AsFd,
    to: &OsStr,
) -> io::Result<()> {
    Ok(rustix::fs::renameat_with(
        from_dir,
        from,
        to_dir,
        to,
        RenameFlags::NOREPLACE,
    )?)
}

/// Removes the entry `name` from the open directory `dir`: a directory only while it is empty
/// (`unlinkat` with `AT_REMOVEDIR`), anything else by its name.
pub fn remove_at(dir: impl AsFd, name: &OsStr, directory: bool) -> io::Result<()> {
    let flags = if directory {
        AtFlags::REMOVEDIR
    } else {
        AtFlags::empty()
    };
    Ok(rustix::fs::unlinkat(dir, name, flags)?)
}

/// The path the OS names now for the open file `fd`: `F_GETPATH` on macOS, which names it
/// the same way whichever spelling opened it, and the link `/proc/self/fd/<n>` on Linux, which
/// ends in ` (deleted)` once the file is removed. Both follow a file moved since it was
/// opened (section 9.7, implementation notes).
pub fn current_path(fd: impl AsFd) -> io::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let path = rustix::fs::getpath(fd)?;
        Ok(PathBuf::from(OsStr::from_bytes(path.to_bytes())))
    }
    #[cfg(not(target_os = "macos"))]
    {
        use std::os::fd::AsRawFd as _;
        std::fs::read_link(format!("/proc/self/fd/{}", fd.as_fd().as_raw_fd()))
    }
}

/// Where a directory being listed is named: the entry `name` in the open directory `parent`,
/// or a path.
#[derive(Clone, Copy, Debug)]
pub struct At<'a> {
    parent: BorrowedFd<'a>,
    name: &'a Path,
}

impl<'a> At<'a> {
    /// The entry `name` in the open directory `parent`.
    #[must_use]
    pub fn entry(parent: BorrowedFd<'a>, name: &'a OsStr) -> Self {
        Self {
            parent,
            name: Path::new(name),
        }
    }

    /// The directory at `path`, absolute or relative to the working directory.
    #[must_use]
    pub fn path(path: &'a Path) -> Self {
        Self {
            parent: rustix::fs::CWD,
            name: path,
        }
    }
}

/// The entries of the open directory `dir`, named by `at`, in name order, without `.` and
/// `..`: each name with what `lstat` reports of it, read relative to `dir`, and what `own` asks
/// of a regular file. A directory removed while it is read ends its listing with what was
/// read, an entry removed before its `lstat` is left out, each in [`Listed::vanished`], and any
/// other error of the listing or an `lstat` fails it; a read of `own` that fails is in
/// [`Listed::unread`]. Once listed, `at` is read again by `lstat` and must still name `dir`:
/// a directory removed or replaced after it was opened, which macOS lists as empty or as the
/// old one without an error, is in [`Listed::vanished`] (`None`) too. The listing reads `dir`
/// from its current offset and leaves it at the end.
pub fn list(dir: &File, own: Own, at: At<'_>) -> io::Result<Listed> {
    list_with(dir, own, at, &mut || Ok(()))
}

/// [`list`], `names_read` run once the names are read and before the first `lstat`.
pub fn list_with(
    dir: &File,
    own: Own,
    at: At<'_>,
    names_read: &mut dyn FnMut() -> io::Result<()>,
) -> io::Result<Listed> {
    let mut listed = Listed::default();
    #[cfg(target_os = "macos")]
    let bulk = match own {
        Own::PrivateSize => match bulk::read(dir) {
            Ok(found) => Some(found),
            Err(err) => {
                listed
                    .unread
                    .push((None, format!("getattrlistbulk: {err}")));
                // Listed again by name from the start.
                rustix::fs::seek(dir, rustix::fs::SeekFrom::Start(0))?;
                None
            }
        },
        Own::Nothing | Own::Extents => None,
    };
    #[cfg(target_os = "macos")]
    let names = match &bulk {
        Some(found) => {
            let mut names: Vec<OsString> = found.keys().cloned().collect();
            names.sort();
            names
        }
        None => names(dir, &mut listed.vanished)?,
    };
    #[cfg(not(target_os = "macos"))]
    let names = names(dir, &mut listed.vanished)?;
    names_read()?;
    for name in names {
        let st = match rustix::fs::statat(dir, &name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => stat(&st),
            Err(Errno::NOENT) => {
                listed.vanished.push(Some(name));
                continue;
            }
            Err(err) => return Err(err.into()),
        };
        let mut read = None;
        if st.kind == Kind::File {
            #[cfg(target_os = "macos")]
            if let Some(found) = &bulk {
                match found.get(&name).map(|entry| entry.reading(st.ino)) {
                    Some(Ok(private)) => read = private,
                    Some(Err(err)) => listed.unread.push((Some(name.clone()), err)),
                    None => {}
                }
            }
            #[cfg(target_os = "linux")]
            if own == Own::Extents {
                match extents_at(dir, &name, (st.dev, st.ino)) {
                    Ok(bytes) => read = Some(bytes),
                    // Removed since its `lstat`: left out, as an entry removed before it.
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {
                        listed.vanished.push(Some(name));
                        continue;
                    }
                    Err(err) => listed
                        .unread
                        .push((Some(name.clone()), format!("FIEMAP: {err}"))),
                }
            }
        }
        listed.entries.push((name, st, read));
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let _ = own;
    if !listed.vanished.contains(&None) && !still_named(dir, at)? {
        listed.vanished.push(None);
    }
    Ok(listed)
}

/// Whether `at` names the open directory `dir` still: the same `dev` and `ino` by `lstat`.
/// Nothing there is `false`; any other error of the `lstat` fails.
pub fn still_named(dir: &File, at: At<'_>) -> io::Result<bool> {
    let opened = stat_fd(dir)?;
    match rustix::fs::statat(at.parent, at.name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => {
            let named = stat(&st);
            Ok((named.dev, named.ino) == (opened.dev, opened.ino))
        }
        Err(Errno::NOENT | Errno::NOTDIR) => Ok(false),
        Err(err) => Err(err.into()),
    }
}

/// The names in the open directory `dir`, sorted, without `.` and `..`; read through a
/// duplicate of the descriptor, from its current offset. A directory removed while it is read
/// ends the listing with what was read, and `None` goes into `vanished`.
fn names(dir: &File, vanished: &mut Vec<Option<OsString>>) -> io::Result<Vec<OsString>> {
    let read = names_while(dir, &mut |_| true)?;
    if read.gone {
        vanished.push(None);
    }
    let mut names = read.names;
    names.sort();
    Ok(names)
}

/// What [`names_while`] read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Names {
    /// The names read, in the order the file system returned them, without `.` and `..`.
    pub names: Vec<OsString>,
    /// Whether the directory was removed while it was read, which ended the reading.
    pub gone: bool,
}

/// The names in the open directory `dir`, read as [`list`] reads them, through a duplicate of
/// the descriptor from its current offset, as long as `more`, given how many were read, allows
/// one more: a directory removed while it is read ends the reading with what was read.
pub fn names_while(dir: &File, more: &mut dyn FnMut(usize) -> bool) -> io::Result<Names> {
    let stream = rustix::fs::Dir::new(OwnedFd::from(dir.try_clone()?))?;
    let mut read = Names::default();
    for entry in stream {
        match entry {
            Ok(entry) => {
                let name = entry.file_name().to_bytes();
                if name == b"." || name == b".." {
                    continue;
                }
                if !more(read.names.len()) {
                    break;
                }
                read.names.push(OsStr::from_bytes(name).to_os_string());
            }
            Err(Errno::NOENT) => {
                read.gone = true;
                break;
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(read)
}

/// What the measure `own` reads of the regular file `name` in the open directory `dir`, which
/// `lstat` found to be the inode `seen` (`dev`, `ino`): its private size (macOS, `getattrlistat`, `None` when the file system
/// returns none) or its counted extents (Linux). `Ok(None)` for [`Own::Nothing`]. The walks
/// read the target this way when it is a file, which no listing of theirs reads.
pub fn own_at(dir: impl AsFd, name: &OsStr, seen: (u64, u64), own: Own) -> io::Result<Option<u64>> {
    match own {
        Own::Nothing => Ok(None),
        #[cfg(target_os = "macos")]
        Own::PrivateSize => bulk::read_at(dir, name)?
            .reading(seen.1)
            .map_err(io::Error::other),
        #[cfg(target_os = "linux")]
        Own::Extents => extents_at(dir, name, seen).map(Some),
        _ => {
            let _ = (dir, name, seen);
            Err(io::Error::other("not read on this platform"))
        }
    }
}

/// The private sizes of a directory's entries (`getattrlistbulk`, macOS).
#[cfg(target_os = "macos")]
mod bulk {
    use std::collections::HashMap;
    use std::ffi::{CString, OsStr, OsString};
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsFd, AsRawFd};
    use std::os::unix::ffi::OsStrExt;

    /// `ATTR_CMN_ERROR` of `<sys/attr.h>`, which `libc` lacks.
    const ATTR_CMN_ERROR: libc::attrgroup_t = 0x2000_0000;

    /// What `getattrlistbulk` returned of one entry.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Found {
        /// `ATTR_CMN_FILEID`, the inode number, when returned.
        pub ino: Option<u64>,
        /// `ATTR_CMNEXT_PRIVATESIZE`, when returned.
        pub private: Option<u64>,
        /// `ATTR_CMN_ERROR`, when the call could not read the entry's attributes.
        pub error: Option<u32>,
    }

    impl Found {
        /// The private size of the regular file `lstat` found to be inode `ino`: `None` when
        /// the file system returned none, and an error when the entry's attributes could not
        /// be read or belong to another inode (the name replaced between the reads).
        pub fn reading(&self, ino: u64) -> Result<Option<u64>, String> {
            if let Some(errno) = self.error {
                let err = io::Error::from_raw_os_error(i32::try_from(errno).unwrap_or(i32::MAX));
                return Err(format!("getattrlistbulk: {err}"));
            }
            match self.ino {
                Some(found) if found != ino => {
                    Err("getattrlistbulk: the entry was replaced while it was listed".to_owned())
                }
                _ => Ok(self.private),
            }
        }
    }

    /// The attributes requested: the returned set, the name (without which the call fails with
    /// `EINVAL`), the error, the inode number, and the private size, which
    /// `FSOPT_ATTR_CMN_EXTENDED` reads from the fork group.
    fn request(name: bool) -> libc::attrlist {
        let mut common = libc::ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_ERROR | libc::ATTR_CMN_FILEID;
        if name {
            common |= libc::ATTR_CMN_NAME;
        }
        libc::attrlist {
            bitmapcount: libc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: common,
            volattr: 0,
            dirattr: 0,
            fileattr: 0,
            forkattr: libc::ATTR_CMNEXT_PRIVATESIZE,
        }
    }

    /// Every entry of the open directory `dir` with what the call returned of it, read from
    /// the descriptor's current offset.
    pub fn read(dir: &File) -> io::Result<HashMap<OsString, Found>> {
        let mut request = request(true);
        // 8-byte aligned; each call fills it with as many entries as fit.
        let mut buf = vec![0_u64; 32 * 1024];
        let len = buf.len() * size_of::<u64>();
        let mut found = HashMap::new();
        loop {
            // SAFETY: `request` is a valid attrlist and `buf` is valid for writes of `len`
            // bytes, which is all the call writes.
            let count = unsafe {
                libc::getattrlistbulk(
                    dir.as_raw_fd(),
                    (&raw mut request).cast(),
                    buf.as_mut_ptr().cast(),
                    len,
                    u64::from(libc::FSOPT_ATTR_CMN_EXTENDED),
                )
            };
            let count = match usize::try_from(count) {
                Ok(0) => return Ok(found),
                Ok(count) => count,
                Err(_) => return Err(io::Error::last_os_error()),
            };
            // SAFETY: `buf` is initialized for `len` bytes; reading it as bytes is valid.
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), len) };
            let mut at = 0;
            for _ in 0..count {
                let (name, entry, length) = parse(bytes.get(at..).unwrap_or_default(), true)?;
                found.insert(name.unwrap_or_default(), entry);
                at += length;
            }
        }
    }

    /// What `getattrlistat` returns of the entry `name` of the open directory `dir`, not
    /// following a final symbolic link.
    pub fn read_at(dir: impl AsFd, name: &OsStr) -> io::Result<Found> {
        let path = CString::new(name.as_bytes()).map_err(io::Error::other)?;
        let mut request = request(false);
        let mut buf = [0_u64; 16];
        let len = buf.len() * size_of::<u64>();
        // SAFETY: `path` is NUL-terminated, `request` a valid attrlist, and `buf` valid for
        // writes of `len` bytes, which is all the call writes.
        let got = unsafe {
            libc::getattrlistat(
                dir.as_fd().as_raw_fd(),
                path.as_ptr(),
                (&raw mut request).cast(),
                buf.as_mut_ptr().cast(),
                len,
                libc::c_ulong::from(libc::FSOPT_NOFOLLOW | libc::FSOPT_ATTR_CMN_EXTENDED),
            )
        };
        if got != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `buf` is initialized for `len` bytes; reading it as bytes is valid.
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), len) };
        Ok(parse(bytes, false)?.1)
    }

    /// One entry as the calls pack it, at the start of `bytes`: its length, the returned set,
    /// then each returned attribute, 4-byte aligned: the error first (`getattrlistbulk(2)`),
    /// then the common attributes in bit order (the name's reference, the inode number), then
    /// the private size. Returns the name, what was read, and the entry's length. Every field
    /// and the name are read within that length, never from the entries after it.
    fn parse(bytes: &[u8], named: bool) -> io::Result<(Option<OsString>, Found, usize)> {
        let short = || io::Error::other("getattrlistbulk: an entry ends early");
        let length = bytes.get(..4).ok_or_else(short)?;
        let length = u32::from_ne_bytes(length.try_into().map_err(|_| short())?);
        let length = usize::try_from(length).map_err(io::Error::other)?;
        if length < 24 {
            return Err(short());
        }
        let bytes = bytes.get(..length).ok_or_else(short)?;
        let u32_at = |at: usize| -> io::Result<u32> {
            let field = bytes.get(at..at + 4).ok_or_else(short)?;
            Ok(u32::from_ne_bytes(field.try_into().map_err(|_| short())?))
        };
        let u64_at = |at: usize| -> io::Result<u64> {
            let field = bytes.get(at..at + 8).ok_or_else(short)?;
            Ok(u64::from_ne_bytes(field.try_into().map_err(|_| short())?))
        };
        let common = u32_at(4)?;
        let fork = u32_at(20)?;
        let mut at = 24;
        let mut found = Found::default();
        if common & ATTR_CMN_ERROR != 0 {
            found.error = Some(u32_at(at)?);
            at += 4;
        }
        let mut name = None;
        if named && common & libc::ATTR_CMN_NAME != 0 {
            let offset = i32::from_ne_bytes(u32_at(at)?.to_ne_bytes());
            let size = usize::try_from(u32_at(at + 4)?).map_err(io::Error::other)?;
            let start = at
                .checked_add_signed(isize::try_from(offset).map_err(io::Error::other)?)
                .ok_or_else(short)?;
            let raw = bytes.get(start..start + size).ok_or_else(short)?;
            let raw = raw.split(|&b| b == 0).next().unwrap_or_default();
            name = Some(OsStr::from_bytes(raw).to_os_string());
            at += 8;
        }
        if common & libc::ATTR_CMN_FILEID != 0 {
            found.ino = Some(u64_at(at)?);
            at += 8;
        }
        if fork & libc::ATTR_CMNEXT_PRIVATESIZE != 0 {
            // An `off_t`; a negative size is no reading.
            found.private = u64::try_from(i64::from_ne_bytes(u64_at(at)?.to_ne_bytes())).ok();
        }
        if named && name.is_none() {
            return Err(io::Error::other(
                "getattrlistbulk: an entry without its name",
            ));
        }
        Ok((name, found, length))
    }

    #[cfg(test)]
    mod tests {
        use super::parse;

        /// An entry as the calls pack it: its length, the returned set with `common` and
        /// `fork`, then `fields`.
        fn entry(length: u32, common: u32, fork: u32, fields: &[u8]) -> Vec<u8> {
            let mut out = length.to_ne_bytes().to_vec();
            for group in [common, 0, 0, 0, fork] {
                out.extend(group.to_ne_bytes());
            }
            out.extend(fields);
            out
        }

        /// A name reference: its offset from the reference and its size.
        fn name_at(offset: i32, size: u32) -> Vec<u8> {
            [offset.to_ne_bytes(), size.to_ne_bytes()].concat()
        }

        /// Fields and the name are read within the entry's declared length: a name or an
        /// inode number past it, where the next entry of the batch starts, ends the entry
        /// early rather than reading the next entry's bytes as this one's.
        #[test]
        fn an_entry_is_read_within_its_length() {
            let name = libc::ATTR_CMN_NAME;
            let mut whole = entry(40, name, 0, &name_at(8, 2));
            whole.extend(b"a\0\0\0\0\0\0\0");
            let (got, _, length) = parse(&whole, true).unwrap();
            assert_eq!((got.as_deref(), length), (Some("a".as_ref()), 40));

            let mut batch = entry(32, name, 0, &name_at(8, 2));
            batch.extend(entry(40, name, 0, &name_at(8, 2)));
            assert!(parse(&batch, true).is_err(), "a name past the entry");

            let mut batch = entry(24, libc::ATTR_CMN_FILEID, 0, &[]);
            batch.extend(entry(24, 0, 0, &[]));
            assert!(
                parse(&batch, false).is_err(),
                "an inode number past the entry"
            );

            assert!(
                parse(&entry(64, 0, 0, &[]), false).is_err(),
                "a length past the batch"
            );
        }
    }
}

/// The counted data extents of the regular file `name` in the open directory `dir` that
/// `lstat` found to be the inode `seen` (`dev`, `ino`; `FIEMAP`, section 8.5), read through a
/// descriptor opened read-only without following a symbolic link or blocking, which must
/// still be that file.
#[cfg(target_os = "linux")]
fn extents_at(dir: impl AsFd, name: &OsStr, seen: (u64, u64)) -> io::Result<u64> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY;
    let file = rustix::fs::openat(dir, name, flags | OFlags::CLOEXEC, Mode::empty())?;
    let opened = stat(&rustix::fs::fstat(&file)?);
    if opened.kind != Kind::File || (opened.dev, opened.ino) != seen {
        return Err(io::Error::other("the file was replaced while it was read"));
    }
    fiemap::read(&file).map(|extents| counted_extents(&extents))
}

/// One extent `FIEMAP` reports: its length and flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    pub length: u64,
    pub flags: u32,
}

/// `FIEMAP_EXTENT_*` flags of `<linux/fiemap.h>`.
pub mod extent {
    pub const LAST: u32 = 0x1;
    pub const UNKNOWN: u32 = 0x2;
    pub const DELALLOC: u32 = 0x4;
    pub const ENCODED: u32 = 0x8;
    pub const NOT_ALIGNED: u32 = 0x100;
    pub const DATA_INLINE: u32 = 0x200;
    pub const UNWRITTEN: u32 = 0x800;
    pub const SHARED: u32 = 0x2000;
}

/// The bytes of `extents` that the bound counts (section 8.5): every data extent but those
/// whose location is unknown (delayed allocation included), encoded, or not aligned (inline
/// data, which sits in the inode).
#[must_use]
pub fn counted_extents(extents: &[Extent]) -> u64 {
    let left_out = extent::UNKNOWN | extent::DELALLOC | extent::ENCODED | extent::NOT_ALIGNED;
    extents
        .iter()
        .filter(|e| e.flags & left_out == 0)
        .fold(0, |sum, e| sum.saturating_add(e.length))
}

/// `FS_IOC_FIEMAP` (Linux).
#[cfg(target_os = "linux")]
mod fiemap {
    use std::io;
    use std::os::fd::{AsFd, AsRawFd};

    use super::{Extent, extent};

    /// `struct fiemap_extent`.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct RawExtent {
        logical: u64,
        physical: u64,
        length: u64,
        reserved64: [u64; 2],
        flags: u32,
        reserved: [u32; 3],
    }

    /// How many extents one call asks for.
    const COUNT: usize = 64;

    /// `struct fiemap` with room for [`COUNT`] extents.
    #[repr(C)]
    struct Request {
        start: u64,
        length: u64,
        flags: u32,
        mapped: u32,
        count: u32,
        reserved: u32,
        extents: [RawExtent; COUNT],
    }

    /// `_IOWR('f', 11, struct fiemap)`.
    const FS_IOC_FIEMAP: u32 = 0xC020_660B;

    /// Every extent of the open file `file`, without `FIEMAP_FLAG_XATTR` (its data, not its
    /// extended attributes) or `FIEMAP_FLAG_SYNC` (nothing is written for the reading).
    pub fn read(file: impl AsFd) -> io::Result<Vec<Extent>> {
        let mut extents = Vec::new();
        let mut start = 0_u64;
        loop {
            let mut request = Request {
                start,
                length: u64::MAX - start,
                flags: 0,
                mapped: 0,
                count: u32::try_from(COUNT).unwrap_or(u32::MAX),
                reserved: 0,
                extents: [RawExtent::default(); COUNT],
            };
            #[allow(
                clippy::cast_possible_wrap,
                clippy::useless_conversion,
                reason = "the request number's type differs between C libraries"
            )]
            let op = FS_IOC_FIEMAP as libc::Ioctl;
            // SAFETY: `request` is a `struct fiemap` with room for `count` extents, which is
            // all the call writes.
            let got = unsafe { libc::ioctl(file.as_fd().as_raw_fd(), op, &raw mut request) };
            if got != 0 {
                return Err(io::Error::last_os_error());
            }
            let mapped = usize::try_from(request.mapped).map_or(0, |n| n.min(COUNT));
            let batch = &request.extents[..mapped];
            extents.extend(batch.iter().map(|e| Extent {
                length: e.length,
                flags: e.flags,
            }));
            match batch.last() {
                Some(last) if last.flags & extent::LAST == 0 => {
                    start = last.logical.saturating_add(last.length);
                }
                _ => return Ok(extents),
            }
        }
    }
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
        FileType::Fifo => Kind::Fifo,
        FileType::Socket => Kind::Socket,
        FileType::BlockDevice | FileType::CharacterDevice => Kind::Device,
        FileType::Unknown => Kind::Other,
    };
    Stat {
        kind,
        dev: u64::try_from(st.st_dev).unwrap_or_default(),
        ino: u64::try_from(st.st_ino).unwrap_or_default(),
        size: u64::try_from(st.st_size).unwrap_or_default(),
        mtime_ns: i128::from(st.st_mtime) * 1_000_000_000 + i128::from(st.st_mtime_nsec),
        blocks: u64::try_from(st.st_blocks).unwrap_or_default(),
        nlink: u64::try_from(st.st_nlink).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::{Extent, Kind, Own, counted_extents, extent};

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
        let listed = super::list(&opened, Own::Nothing, super::At::path(&dir)).unwrap();
        assert_eq!(listed.unread, []);
        let entries = listed.entries;
        let names: Vec<_> = entries.iter().map(|(n, ..)| n.to_str().unwrap()).collect();
        assert_eq!(names, ["file", "link", "sub"]);
        for (name, st, own) in &entries {
            let meta = std::fs::symlink_metadata(dir.join(name)).unwrap();
            assert_eq!(
                (st.dev, st.ino, st.size, st.blocks),
                (meta.dev(), meta.ino(), meta.size(), meta.blocks()),
                "{name:?}"
            );
            assert_eq!(*own, None, "nothing asked");
        }
        let kinds: Vec<_> = entries.iter().map(|(_, st, _)| st.kind).collect();
        assert_eq!(kinds, [Kind::File, Kind::Symlink, Kind::Dir]);
    }

    /// The no-replace rename moves an entry between open directories and fails with `EEXIST`
    /// when the name is taken, by a file or by an empty directory, leaving both as they were.
    #[test]
    fn a_rename_never_replaces() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(b.join("empty")).unwrap();
        std::fs::write(a.join("f"), b"moved").unwrap();
        std::fs::write(b.join("taken"), b"kept").unwrap();
        let (da, db) = (
            super::open(&a).unwrap().unwrap(),
            super::open(&b).unwrap().unwrap(),
        );
        for taken in ["taken", "empty"] {
            let err = super::rename_noreplace(&da, "f".as_ref(), &db, taken.as_ref()).unwrap_err();
            assert_eq!(err.raw_os_error(), Some(libc::EEXIST), "{taken}: {err}");
        }
        assert_eq!(std::fs::read(b.join("taken")).unwrap(), b"kept");
        assert!(b.join("empty").is_dir());
        super::rename_noreplace(&da, "f".as_ref(), &db, "new".as_ref()).unwrap();
        assert_eq!(std::fs::read(b.join("new")).unwrap(), b"moved");
        assert!(!a.join("f").exists());
    }

    /// A descriptor's current path follows its directory when it is moved, and `open_at`
    /// enters no symbolic link and no file.
    #[test]
    fn the_current_path_follows_a_moved_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir_all(root.join("one/sub")).unwrap();
        std::fs::create_dir_all(root.join("two")).unwrap();
        std::fs::write(root.join("one/file"), b"").unwrap();
        std::os::unix::fs::symlink(root.join("two"), root.join("one/link")).unwrap();
        let one = super::open(&root.join("one")).unwrap().unwrap();
        assert!(super::open_at(&one, "link".as_ref()).unwrap().is_none());
        assert!(super::open_at(&one, "file".as_ref()).unwrap().is_none());
        let sub = super::open_at(&one, "sub".as_ref()).unwrap().unwrap();
        assert_eq!(super::current_path(&sub).unwrap(), root.join("one/sub"));
        std::fs::rename(root.join("one"), root.join("two/moved")).unwrap();
        assert_eq!(
            super::current_path(&sub).unwrap(),
            root.join("two/moved/sub")
        );
        let st = super::stat_at(&one, "sub".as_ref()).unwrap();
        assert_eq!((st.dev, st.ino), {
            let own = super::stat_fd(&sub).unwrap();
            (own.dev, own.ino)
        });
    }

    /// A directory removed after it was opened lists as empty and as removed, instead of
    /// failing the listing, although only Linux reports `ENOENT` when it is read (macOS lists
    /// nothing without an error); one replaced at its name after it was opened lists as
    /// removed too, though its old entries are read. A directory left in place lists as it is,
    /// nothing removed (control).
    #[test]
    fn a_directory_removed_or_replaced_while_open_lists_as_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dir");
        std::fs::create_dir(&dir).unwrap();
        let at = super::At::path(&dir);
        for own in [Own::Nothing, Own::PrivateSize, Own::Extents] {
            let opened = super::open(&dir).unwrap().unwrap();
            let listed = super::list(&opened, own, at).unwrap();
            assert_eq!(listed.vanished, [], "{own:?}");
        }
        let opened = super::open(&dir).unwrap().unwrap();
        std::fs::remove_dir(&dir).unwrap();
        for own in [Own::Nothing, Own::PrivateSize, Own::Extents] {
            let listed = super::list(&opened, own, at).unwrap();
            assert_eq!(listed.entries, [], "{own:?}");
            assert_eq!(listed.vanished, [None], "{own:?}");
        }

        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("old"), b"").unwrap();
        for own in [Own::Nothing, Own::PrivateSize, Own::Extents] {
            let opened = super::open(&dir).unwrap().unwrap();
            std::fs::rename(&dir, tmp.path().join("aside")).unwrap();
            std::fs::create_dir(&dir).unwrap();
            let listed = super::list(&opened, own, at).unwrap();
            assert_eq!(listed.entries.len(), 1, "{own:?}");
            assert_eq!(listed.vanished, [None], "{own:?}");
            std::fs::remove_dir(&dir).unwrap();
            std::fs::rename(tmp.path().join("aside"), &dir).unwrap();
        }
    }

    /// FIFOs, sockets, and devices are told apart from other entries.
    #[test]
    fn special_files_have_their_kinds() {
        let tmp = tempfile::tempdir().unwrap();
        let fifo = tmp.path().join("fifo");
        let path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `path` is a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let _socket = std::os::unix::net::UnixListener::bind(tmp.path().join("socket")).unwrap();
        let opened = super::open(tmp.path()).unwrap().unwrap();
        let listed = super::list(&opened, Own::Nothing, super::At::path(tmp.path())).unwrap();
        let kinds: Vec<_> = listed.entries.iter().map(|(_, st, _)| st.kind).collect();
        assert_eq!(kinds, [Kind::Fifo, Kind::Socket]);
        let null = std::fs::File::open("/dev").unwrap();
        let dev =
            super::stat(&rustix::fs::statat(&null, "null", rustix::fs::AtFlags::empty()).unwrap());
        assert_eq!(dev.kind, Kind::Device);
    }

    /// `FIEMAP` extents count unless their location is unknown, delayed, encoded, or inline;
    /// unwritten (preallocated) and shared ones count.
    #[test]
    fn extents_leave_out_unknown_encoded_and_inline_data() {
        let e = |length, flags| Extent { length, flags };
        let extents = [
            e(4096, 0),
            e(8192, extent::UNWRITTEN),
            e(1 << 20, extent::UNKNOWN | extent::DELALLOC),
            e(1 << 21, extent::DELALLOC),
            e(1 << 22, extent::ENCODED),
            e(60, extent::NOT_ALIGNED | extent::DATA_INLINE | extent::LAST),
            e(16_384, extent::SHARED),
        ];
        assert_eq!(counted_extents(&extents), 4096 + 8192 + 16_384);
        assert_eq!(counted_extents(&[]), 0);
    }

    /// A listing that asks for private sizes reads, on APFS, what deleting each regular file
    /// would free: all of a file written in place, nothing of two clones of one file, and its
    /// inode number matches its `lstat`. Elsewhere it reads none and fails nothing.
    #[test]
    fn private_sizes_are_read_with_the_listing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dir");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("own"), vec![7_u8; 64 * 1024]).unwrap();
        std::fs::write(dir.join("empty"), b"").unwrap();
        let cloned = clone(&dir.join("own"), &dir.join("clone-a"))
            && clone(&dir.join("own"), &dir.join("clone-b"));
        let opened = super::open(&dir).unwrap().unwrap();
        let listed = super::list(&opened, Own::PrivateSize, super::At::path(&dir)).unwrap();
        let read = |name: &str| {
            listed
                .entries
                .iter()
                .find(|(n, ..)| n == name)
                .map(|(_, st, own)| (st.blocks * 512, *own))
                .unwrap()
        };
        let apfs = cfg!(target_os = "macos")
            && super::super::mount::fs_type(std::fs::File::open(&dir).unwrap()).unwrap() == "apfs";
        assert_eq!(listed.unread, []);
        assert_eq!(read("sub").1, None, "directories carry no reading");
        if apfs {
            assert!(cloned);
            // The original shares all its blocks with its clones now.
            assert_eq!(read("own").1, Some(0));
            assert_eq!(read("clone-a").1, Some(0));
            assert_eq!(read("empty").1, Some(0));
            std::fs::write(dir.join("fresh"), vec![1_u8; 32 * 1024]).unwrap();
            let opened = super::open(&dir).unwrap().unwrap();
            let again = super::list(&opened, Own::PrivateSize, super::At::path(&dir)).unwrap();
            let (allocated, private) = again
                .entries
                .iter()
                .find(|(n, ..)| n == "fresh")
                .map(|(_, st, own)| (st.blocks * 512, *own))
                .unwrap();
            assert_eq!(
                private,
                Some(allocated),
                "a file written in place is all its own"
            );
            let file = std::fs::File::open(&dir).unwrap();
            let fresh = &again.entries.iter().find(|(n, ..)| n == "fresh").unwrap().1;
            let single = super::own_at(
                &file,
                "fresh".as_ref(),
                (fresh.dev, fresh.ino),
                Own::PrivateSize,
            );
            assert_eq!(single.unwrap(), Some(allocated));
        } else {
            assert_eq!(read("own").1, None);
        }
    }

    /// A listing that asks for extents reads each regular file's data extents on Linux: a
    /// file's written blocks, none of a sparse file's hole.
    #[cfg(target_os = "linux")]
    #[test]
    fn extents_are_read_with_the_listing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("data"), vec![7_u8; 64 * 1024]).unwrap();
        let sparse = std::fs::File::create(dir.join("sparse")).unwrap();
        sparse.set_len(1 << 20).unwrap();
        sparse.sync_all().unwrap();
        std::fs::File::open(dir.join("data"))
            .unwrap()
            .sync_all()
            .unwrap();
        let opened = super::open(&dir).unwrap().unwrap();
        let listed = super::list(&opened, Own::Extents, super::At::path(&dir)).unwrap();
        let read = |name: &str| {
            listed
                .entries
                .iter()
                .find(|(n, ..)| n == name)
                .map(|(_, st, own)| (st.blocks * 512, *own))
                .unwrap()
        };
        if listed.unread.is_empty() {
            assert_eq!(read("data").1, Some(64 * 1024));
            assert_eq!(read("sparse").1, Some(0));
        } else {
            // A file system without FIEMAP (tmpfs before Linux 6.x) fails each read.
            assert_eq!(listed.unread.len(), 2, "{:?}", listed.unread);
            assert_eq!(read("data").1, None);
        }
    }

    /// Clones `from` to `to` (`clonefile`, macOS); false where it is not supported.
    fn clone(from: &std::path::Path, to: &std::path::Path) -> bool {
        #[cfg(target_os = "macos")]
        {
            let from = std::ffi::CString::new(from.as_os_str().as_encoded_bytes()).unwrap();
            let to = std::ffi::CString::new(to.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: both are NUL-terminated paths.
            unsafe { libc::clonefile(from.as_ptr(), to.as_ptr(), 0) == 0 }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (from, to);
            false
        }
    }
}
