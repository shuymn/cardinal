//! Snapshot and deletion through directory descriptors (`docs/architecture.md` section 9.5,
//! steps 4, 6, and 7).
//!
//! Every directory is opened relative to its parent with `O_DIRECTORY | O_NOFOLLOW`, starting
//! from the worktree root, so a directory replaced by a symbolic link is never entered, and a
//! directory replaced by another one is detected by its `(dev, ino)`. A directory on another
//! mount than the target (a bind mount on the same device included) is never entered, and a
//! target that is a mount point is refused. An entry is unlinked only when its metadata still
//! equals the snapshot; a directory is removed only once empty.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;

use crate::contract::RootId;
use crate::plan::{Entry, EntryKind, Mount, Walk};

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// An entry from `lstat`-style metadata.
#[allow(
    clippy::useless_conversion,
    clippy::unnecessary_fallible_conversions,
    reason = "the field types of `struct stat` differ between platforms"
)]
fn entry(rel: PathBuf, st: &Stat) -> Entry {
    let kind = match FileType::from_raw_mode(st.st_mode) {
        FileType::RegularFile => EntryKind::File,
        FileType::Directory => EntryKind::Dir,
        FileType::Symlink => EntryKind::Symlink,
        _ => EntryKind::Other,
    };
    Entry {
        rel,
        kind,
        dev: u64::try_from(st.st_dev).unwrap_or_default(),
        ino: u64::try_from(st.st_ino).unwrap_or_default(),
        size: u64::try_from(st.st_size).unwrap_or_default(),
        mtime_ns: i128::from(st.st_mtime) * 1_000_000_000 + i128::from(st.st_mtime_nsec),
        allocated: u64::try_from(st.st_blocks)
            .unwrap_or_default()
            .saturating_mul(512),
    }
}

fn lstat_at(dir: impl AsFd, name: &OsStr) -> io::Result<Stat> {
    Ok(rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?)
}

fn open_dir_at(dir: impl AsFd, name: &OsStr) -> io::Result<OwnedFd> {
    Ok(rustix::fs::openat(dir, name, DIR_FLAGS, Mode::empty())?)
}

/// What identifies the mount an open directory is on: the mount ID (`statx`, Linux 5.8 and
/// later) on Linux, the path the filesystem is mounted on (`statfs`) on macOS. An error means
/// the mount cannot be told, and the callers fail closed.
#[cfg(target_os = "linux")]
fn mount_of(fd: impl AsFd) -> Result<Vec<u8>, String> {
    statx_mount(fd, c"", AtFlags::EMPTY_PATH)
}

#[cfg(target_os = "linux")]
fn statx_mount<P: rustix::path::Arg>(
    dir: impl AsFd,
    path: P,
    flags: AtFlags,
) -> Result<Vec<u8>, String> {
    use rustix::fs::StatxFlags;
    let st = rustix::fs::statx(dir, path, flags | AtFlags::NO_AUTOMOUNT, StatxFlags::MNT_ID)
        .map_err(|err| format!("statx: {err}"))?;
    if StatxFlags::from_bits_retain(st.stx_mask).contains(StatxFlags::MNT_ID) {
        Ok(st.stx_mnt_id.to_ne_bytes().to_vec())
    } else {
        Err("statx reports no mount ID (Linux before 5.8)".to_owned())
    }
}

#[cfg(target_os = "macos")]
fn mount_of(fd: impl AsFd) -> Result<Vec<u8>, String> {
    let st = rustix::fs::fstatfs(fd).map_err(|err| format!("fstatfs: {err}"))?;
    Ok(st
        .f_mntonname
        .iter()
        .map(|c| u8::from_ne_bytes(c.to_ne_bytes()))
        .take_while(|&b| b != 0)
        .collect())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn mount_of(_fd: impl AsFd) -> Result<Vec<u8>, String> {
    Err("mount identity is not available on this platform".to_owned())
}

/// The mount of the entry `name` in `dir` that is not a directory. Only Linux mounts on
/// files; elsewhere such an entry is on its directory's mount.
fn file_mount_at(dir: &OwnedFd, name: &OsStr) -> Result<Vec<u8>, String> {
    #[cfg(target_os = "linux")]
    {
        statx_mount(dir, name, AtFlags::SYMLINK_NOFOLLOW)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = name;
        mount_of(dir)
    }
}

/// Names in an open directory, sorted, without `.` and `..`.
fn names(dir: impl AsFd) -> io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for entry in Dir::read_from(dir)? {
        let name = entry?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." {
            names.push(OsStr::from_bytes(&name).to_os_string());
        }
    }
    names.sort();
    Ok(names)
}

/// The target reached from the worktree root without following symbolic links.
#[derive(Debug)]
pub struct Target {
    /// The directory that contains the target.
    parent: OwnedFd,
    parent_dev: u64,
    /// The mount of `parent`.
    parent_mount: Result<Vec<u8>, String>,
    /// The target's name in `parent`.
    name: OsString,
    /// The target's path relative to the worktree (empty for the worktree itself).
    rel: PathBuf,
    /// The absolute target path, for reporting.
    path: PathBuf,
    /// The worktree root as opened.
    root: Entry,
    /// The first component on the way that is a symbolic link; nothing below it is reached.
    link_in_path: Option<PathBuf>,
}

impl Target {
    /// Opens the worktree's parent by path, then the worktree root and each directory down
    /// to the target's parent relative to its own parent, never following links.
    pub fn open(worktree: &Path, target: &Path) -> io::Result<Self> {
        let rel = target
            .strip_prefix(worktree)
            .map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "target outside the worktree")
            })?
            .to_path_buf();
        let (Some(wt_parent), Some(wt_name)) = (worktree.parent(), worktree.file_name()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the worktree has no parent",
            ));
        };
        let top = rustix::fs::open(
            wt_parent,
            DIR_FLAGS.difference(OFlags::NOFOLLOW),
            Mode::empty(),
        )?;
        let root = entry(PathBuf::new(), &lstat_at(&top, wt_name)?);
        let names: Vec<&OsStr> = rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(name) => Some(name),
                _ => None,
            })
            .collect();
        let Some((last, dirs)) = names.split_last() else {
            let parent_dev = entry(PathBuf::new(), &rustix::fs::fstat(&top)?).dev;
            let parent_mount = mount_of(&top);
            return Ok(Self {
                parent: top,
                parent_dev,
                parent_mount,
                name: wt_name.to_os_string(),
                rel,
                path: target.to_path_buf(),
                root,
                link_in_path: None,
            });
        };
        let mut current = open_dir_at(&top, wt_name)?;
        if !root.same_identity(&entry(PathBuf::new(), &rustix::fs::fstat(&current)?)) {
            return Err(io::Error::other(
                "the worktree root changed while it was opened",
            ));
        }
        let mut link_in_path = None;
        let mut walked = PathBuf::new();
        for name in dirs {
            walked.push(name);
            match open_dir_at(&current, name) {
                Ok(next) => current = next,
                Err(err) => {
                    if lstat_at(&current, name)
                        .is_ok_and(|st| FileType::from_raw_mode(st.st_mode) == FileType::Symlink)
                    {
                        link_in_path = Some(walked.clone());
                        break;
                    }
                    return Err(err);
                }
            }
        }
        let parent_dev = entry(PathBuf::new(), &rustix::fs::fstat(&current)?).dev;
        let parent_mount = mount_of(&current);
        Ok(Self {
            parent: current,
            parent_dev,
            parent_mount,
            name: last.to_os_string(),
            rel,
            path: target.to_path_buf(),
            root,
            link_in_path,
        })
    }

    /// The worktree root's identity as opened.
    #[must_use]
    pub fn root_id(&self) -> RootId {
        RootId {
            dev: self.root.dev,
            ino: self.root.ino,
        }
    }

    /// Records every entry of the target: `(dev, ino, type, size, mtime)`, without following
    /// symbolic links or entering another device or mount.
    pub fn snapshot(&self) -> io::Result<Walk> {
        if let Some(link) = &self.link_in_path {
            let mut root = self.root.clone();
            root.rel.clone_from(&self.rel);
            root.kind = EntryKind::Symlink;
            return Ok(Walk {
                root,
                mount: Mount::Unknown("not reached: a symbolic link is on the way".to_owned()),
                link_in_path: Some(link.clone()),
                entries: Vec::new(),
            });
        }
        let root = entry(self.rel.clone(), &lstat_at(&self.parent, &self.name)?);
        let mut entries = Vec::new();
        // A target that is not on its parent's mount is refused: nothing below is recorded.
        let mount = if root.dev != self.parent_dev {
            Mount::Boundary
        } else if root.kind == EntryKind::Dir {
            let fd = open_dir_at(&self.parent, &self.name)?;
            if !root.same_identity(&entry(self.rel.clone(), &rustix::fs::fstat(&fd)?)) {
                return Err(io::Error::other("the target changed while it was opened"));
            }
            let (mount, own) = self.target_mount(root.dev, Some(&fd));
            if let (Mount::Same, Ok(key)) = (&mount, &own) {
                snapshot_dir(&fd, &self.rel, root.dev, key, &mut entries)?;
            }
            mount
        } else {
            self.target_mount(root.dev, None).0
        };
        Ok(Walk {
            root,
            mount,
            link_in_path: None,
            entries,
        })
    }

    /// Whether the target, on device `dev`, is on its parent's mount, and its own mount:
    /// read from `dir` when the target is that open directory, by name otherwise.
    fn target_mount(&self, dev: u64, dir: Option<&OwnedFd>) -> (Mount, Result<Vec<u8>, String>) {
        let own = match dir {
            Some(fd) => mount_of(fd),
            None => file_mount_at(&self.parent, &self.name),
        };
        (
            Mount::between(self.parent_dev, dev, &self.parent_mount, &own),
            own,
        )
    }

    /// Deletes every entry of the target that still equals `walk`, bottom up. With
    /// `keep_git`, the root's `.git` and the root itself stay for [`Target::finish_worktree`].
    pub fn delete(&self, walk: &Walk, keep_git: bool) -> Deletion {
        let mut deletion = Deletion::default();
        // With a link on the way, `parent` is not the target's parent: touch nothing.
        if self.link_in_path.is_some() {
            deletion.skipped.push(self.path.clone());
            return deletion;
        }
        let index: HashMap<&Path, &Entry> = walk.all().map(|e| (e.rel.as_path(), e)).collect();
        let current = match lstat_at(&self.parent, &self.name) {
            Ok(st) => entry(self.rel.clone(), &st),
            Err(err) if err.kind() == io::ErrorKind::NotFound => return deletion,
            Err(err) => {
                deletion
                    .failed
                    .push(format!("{}: {err}", self.path.display()));
                return deletion;
            }
        };
        if walk.root.kind != EntryKind::Dir {
            if walk.root.unchanged(&current)
                && self.target_mount(current.dev, None).0 == Mount::Same
            {
                match rustix::fs::unlinkat(&self.parent, &self.name, AtFlags::empty()) {
                    Ok(()) => {
                        deletion.freed += walk.root.allocated;
                        deletion.root_removed = true;
                    }
                    Err(err) => deletion
                        .failed
                        .push(format!("{}: {err}", self.path.display())),
                }
            } else {
                deletion.skipped.push(self.path.clone());
            }
            return deletion;
        }
        let fd = match open_dir_at(&self.parent, &self.name) {
            Ok(fd) if walk.root.same_identity(&current) => fd,
            Ok(_) | Err(_) => {
                deletion.skipped.push(self.path.clone());
                return deletion;
            }
        };
        match rustix::fs::fstat(&fd) {
            Ok(st) if walk.root.same_identity(&entry(self.rel.clone(), &st)) => {}
            _ => {
                deletion.skipped.push(self.path.clone());
                return deletion;
            }
        }
        // Mounted on since the snapshot: not entered.
        let (Mount::Same, Ok(mount)) = self.target_mount(current.dev, Some(&fd)) else {
            deletion.skipped.push(self.path.clone());
            return deletion;
        };
        let context = Context {
            index: &index,
            dev: walk.root.dev,
            mount: &mount,
            base: &self.path,
            root_rel: &self.rel,
            keep_git,
        };
        context.delete_dir(&fd, &self.rel, &mut deletion);
        if !keep_git {
            match rustix::fs::unlinkat(&self.parent, &self.name, AtFlags::REMOVEDIR) {
                Ok(()) => {
                    deletion.freed += walk.root.allocated;
                    deletion.root_removed = true;
                }
                Err(Errno::NOTEMPTY | Errno::EXIST) => {}
                Err(err) => deletion
                    .failed
                    .push(format!("{}: {err}", self.path.display())),
            }
        }
        deletion
    }

    /// Step 7 for a worktree: if only `.git` is left in the root and it equals the snapshot,
    /// unlinks it and removes the root. `Ok(false)` when anything else is left.
    pub fn finish_worktree(&self, walk: &Walk, deletion: &mut Deletion) -> io::Result<bool> {
        let root = open_dir_at(&self.parent, &self.name)?;
        if !walk
            .root
            .same_identity(&entry(self.rel.clone(), &rustix::fs::fstat(&root)?))
        {
            return Ok(false);
        }
        if names(&root)? != [OsString::from(".git")] {
            return Ok(false);
        }
        let git_rel = self.rel.join(".git");
        let Some(snapshot) = walk.entries.iter().find(|e| e.rel == git_rel) else {
            return Ok(false);
        };
        let current = entry(git_rel, &lstat_at(&root, OsStr::new(".git"))?);
        if snapshot.kind == EntryKind::Dir || !snapshot.unchanged(&current) {
            deletion.skipped.push(self.path.join(".git"));
            return Ok(false);
        }
        rustix::fs::unlinkat(&root, ".git", AtFlags::empty())?;
        deletion.freed += snapshot.allocated;
        rustix::fs::unlinkat(&self.parent, &self.name, AtFlags::REMOVEDIR)?;
        deletion.freed += walk.root.allocated;
        deletion.root_removed = true;
        Ok(true)
    }
}

/// Records the entries below `fd`. A directory on another device or mount than `dev` and
/// `mount` is recorded but not entered; a mount that cannot be read fails the snapshot.
fn snapshot_dir(
    fd: &OwnedFd,
    rel: &Path,
    dev: u64,
    mount: &[u8],
    out: &mut Vec<Entry>,
) -> io::Result<()> {
    for name in names(fd)? {
        let st = match lstat_at(fd, &name) {
            Ok(st) => st,
            // Removed while walking: nothing to record.
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        let child = entry(rel.join(&name), &st);
        let enter = child.kind == EntryKind::Dir && child.dev == dev;
        let child_rel = child.rel.clone();
        let snapshot = child.clone();
        out.push(child);
        if enter {
            match open_dir_at(fd, &name) {
                Ok(child_fd) => {
                    let opened = entry(child_rel.clone(), &rustix::fs::fstat(&child_fd)?);
                    // Replaced between the stat and the open: the deletion sees the mismatch
                    // and keeps it, so nothing below needs recording.
                    if snapshot.same_identity(&opened)
                        && mount_of(&child_fd).map_err(io::Error::other)? == mount
                    {
                        snapshot_dir(&child_fd, &child_rel, dev, mount, out)?;
                    }
                }
                Err(err)
                    if matches!(
                        err.raw_os_error().map(Errno::from_raw_os_error),
                        Some(Errno::NOENT | Errno::LOOP | Errno::NOTDIR)
                    ) => {}
                Err(err) => return Err(err),
            }
        }
    }
    Ok(())
}

/// What a deletion did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Deletion {
    /// Bytes allocated by the entries removed.
    pub freed: u64,
    /// Entries left because they changed or appeared since the snapshot (absolute paths).
    pub skipped: Vec<PathBuf>,
    /// Entries that could not be removed, with the error.
    pub failed: Vec<String>,
    /// Whether the target itself was removed.
    pub root_removed: bool,
}

struct Context<'a> {
    index: &'a HashMap<&'a Path, &'a Entry>,
    /// The device and mount of the target; nothing on another one is entered.
    dev: u64,
    mount: &'a [u8],
    /// The absolute target path.
    base: &'a Path,
    /// The target relative to the worktree.
    root_rel: &'a Path,
    keep_git: bool,
}

impl Context<'_> {
    fn absolute(&self, rel: &Path) -> PathBuf {
        match rel.strip_prefix(self.root_rel) {
            Ok(below) if !below.as_os_str().is_empty() => self.base.join(below),
            _ => self.base.to_path_buf(),
        }
    }

    fn delete_dir(&self, fd: &OwnedFd, rel: &Path, out: &mut Deletion) {
        let names = match names(fd) {
            Ok(names) => names,
            Err(err) => {
                out.failed
                    .push(format!("{}: {err}", self.absolute(rel).display()));
                return;
            }
        };
        for name in names {
            let child_rel = rel.join(&name);
            if self.keep_git && rel == self.root_rel && name == ".git" {
                continue;
            }
            let path = self.absolute(&child_rel);
            let Some(snapshot) = self.index.get(child_rel.as_path()) else {
                out.skipped.push(path);
                continue;
            };
            let current = match lstat_at(fd, &name) {
                Ok(st) => entry(child_rel.clone(), &st),
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => {
                    out.failed.push(format!("{}: {err}", path.display()));
                    continue;
                }
            };
            if snapshot.kind == EntryKind::Dir {
                if !snapshot.same_identity(&current) || current.dev != self.dev {
                    out.skipped.push(path);
                    continue;
                }
                let Ok(child) = open_dir_at(fd, &name) else {
                    out.skipped.push(path);
                    continue;
                };
                match rustix::fs::fstat(&child) {
                    Ok(st) if snapshot.same_identity(&entry(child_rel.clone(), &st)) => {}
                    _ => {
                        out.skipped.push(path);
                        continue;
                    }
                }
                if mount_of(&child).as_deref() != Ok(self.mount) {
                    out.skipped.push(path);
                    continue;
                }
                self.delete_dir(&child, &child_rel, out);
                drop(child);
                match rustix::fs::unlinkat(fd, &name, AtFlags::REMOVEDIR) {
                    Ok(()) => out.freed += snapshot.allocated,
                    // Something below was kept; it is listed already.
                    Err(Errno::NOTEMPTY | Errno::EXIST | Errno::NOENT) => {}
                    Err(err) => out.failed.push(format!("{}: {err}", path.display())),
                }
            } else if snapshot.unchanged(&current) {
                match rustix::fs::unlinkat(fd, &name, AtFlags::empty()) {
                    Ok(()) => out.freed += snapshot.allocated,
                    Err(Errno::NOENT) => {}
                    Err(err) => out.failed.push(format!("{}: {err}", path.display())),
                }
            } else {
                out.skipped.push(path);
            }
        }
    }
}
