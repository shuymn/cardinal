//! Snapshot inspection through directory descriptors (`docs/architecture.md` section 9.5).
//!
//! Every directory is opened relative to its parent with `O_DIRECTORY | O_NOFOLLOW`, starting
//! from the worktree root, so a symbolic link is never followed, and a directory replaced while
//! it is opened is detected by its `(dev, ino)`. A directory on another mount than the target
//! (a bind mount on the same device included) is recorded but not entered, and a target that
//! is a mount point is reported as one. Nothing here writes.
//!
//! The snapshot keeps a descriptor open for every directory from the target down to the one
//! it is reading, so directories nested deeper than the open-file limit allows fail it with
//! `EMFILE`; the walk checks are then `unknown` and `act` rejects the plan.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;

use crate::contract::RootId;
use crate::observe::Measure;
use crate::os::dir::{self, Own};
use crate::os::mount::{self, MountId};
use crate::plan::{Entry, EntryKind, Mount, Walk};
use crate::testpoint;

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// An entry from `lstat`-style metadata.
fn entry(rel: PathBuf, st: &Stat) -> Entry {
    Entry::from_stat(rel, &dir::stat(st))
}

fn lstat_at(dir: impl AsFd, name: &OsStr) -> io::Result<Stat> {
    Ok(rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?)
}

fn open_dir_at(dir: impl AsFd, name: &OsStr) -> io::Result<OwnedFd> {
    Ok(rustix::fs::openat(dir, name, DIR_FLAGS, Mode::empty())?)
}

/// The target reached from the worktree root without following symbolic links.
#[derive(Debug)]
pub struct Target {
    /// The directory that contains the target.
    parent: OwnedFd,
    parent_dev: u64,
    /// The mount of `parent`.
    parent_mount: Result<MountId, String>,
    /// The target's name in `parent`.
    name: OsString,
    /// The target's path relative to the worktree (empty for the worktree itself).
    rel: PathBuf,
    /// The worktree root's path, which the tests' fakes name paths below.
    worktree: PathBuf,
    /// The worktree root as opened.
    root: Entry,
    /// The directories from the worktree root's child down to `parent`, each by name with the
    /// `dev` and `ino` it was opened with: the chain an execution binds itself to (section 9.7).
    chain: Vec<(OsString, u64, u64)>,
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
            let parent_mount = mount::of_dir(&top);
            return Ok(Self {
                parent: top,
                parent_dev,
                parent_mount,
                name: wt_name.to_os_string(),
                rel,
                worktree: worktree.to_path_buf(),
                root,
                chain: Vec::new(),
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
        let mut chain = Vec::new();
        for name in dirs {
            walked.push(name);
            match open_dir_at(&current, name) {
                Ok(next) => {
                    let st = dir::stat_fd(&next)?;
                    chain.push((name.to_os_string(), st.dev, st.ino));
                    current = next;
                }
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
        let parent_mount = mount::of_dir(&current);
        Ok(Self {
            parent: current,
            parent_dev,
            parent_mount,
            name: last.to_os_string(),
            rel,
            worktree: worktree.to_path_buf(),
            root,
            chain,
            link_in_path,
        })
    }

    /// The directory that contains the target, opened through the chain.
    #[must_use]
    pub fn parent(&self) -> &OwnedFd {
        &self.parent
    }

    /// The target's name in its parent.
    #[must_use]
    pub fn name(&self) -> &OsStr {
        &self.name
    }

    /// The target's path relative to the worktree.
    #[must_use]
    pub fn rel(&self) -> &Path {
        &self.rel
    }

    /// The directories from the worktree root's child down to the target's parent, each by
    /// name with its `dev` and `ino` as opened.
    #[must_use]
    pub fn chain(&self) -> &[(OsString, u64, u64)] {
        &self.chain
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
    /// symbolic links or entering another device or mount, each regular file with what the
    /// measure of its file system reads of it (section 8.5).
    pub fn snapshot(&self) -> io::Result<Walk> {
        if let Some(link) = &self.link_in_path {
            let mut root = self.root.clone();
            root.rel.clone_from(&self.rel);
            root.kind = EntryKind::Symlink;
            let not_reached = "not reached: a symbolic link is on the way".to_owned();
            return Ok(Walk {
                root,
                mount: Mount::Unknown(not_reached.clone()),
                link_in_path: Some(link.clone()),
                entries: Vec::new(),
                boundaries: Vec::new(),
                fs_type: Err(not_reached),
                unmeasured: Vec::new(),
                unlisted: Vec::new(),
                vanished: Vec::new(),
            });
        }
        let path = self.worktree.join(&self.rel);
        let mut root = entry(self.rel.clone(), &lstat_at(&self.parent, &self.name)?).faked(&path);
        let mut recorded = Recorded::default();
        let fs_type;
        // A target that is not on its parent's mount is refused: nothing below is recorded.
        let mount = if root.dev != self.parent_dev {
            fs_type = self.fs_type(&path, None);
            Mount::Boundary
        } else if root.kind == EntryKind::Dir {
            let fd = open_dir_at(&self.parent, &self.name)?;
            if !root.same_identity(&entry(self.rel.clone(), &rustix::fs::fstat(&fd)?)) {
                return Err(io::Error::other("the target changed while it was opened"));
            }
            fs_type = self.fs_type(&path, Some(&fd));
            let own = Measure::of(&fs_type).own();
            let (mount, own_mount) = self.target_mount(root.dev, Some(&fd));
            if let (Mount::Same, Ok(key)) = (&mount, &own_mount) {
                let read = (own, self.worktree.as_path());
                let at = dir::At::entry(self.parent.as_fd(), &self.name);
                snapshot_dir((&fd, at), &self.rel, (root.dev, key), read, &mut recorded)?;
            }
            mount
        } else {
            fs_type = self.fs_type(&path, None);
            if root.kind == EntryKind::File {
                let own = Measure::of(&fs_type).own();
                match dir::own_at(&self.parent, &self.name, (root.dev, root.ino), own) {
                    Ok(read) => root.own = read,
                    Err(err) => recorded
                        .unmeasured
                        .push((self.rel.clone(), err.to_string())),
                }
            }
            self.target_mount(root.dev, None).0
        };
        Ok(Walk {
            root,
            mount,
            link_in_path: None,
            entries: recorded.entries,
            boundaries: recorded.boundaries,
            fs_type,
            unmeasured: recorded.unmeasured,
            // Step 5's comparison (section 9.7) refuses either, as it refuses them in the
            // moved tree.
            unlisted: recorded.unlisted,
            vanished: recorded.vanished,
        })
    }

    /// The type of the file system the target at `path` is on: read from `dir`, the target
    /// opened, or else from its parent; a test's fake first (`testpoint::fake_fs_type`).
    fn fs_type(&self, path: &Path, dir: Option<&OwnedFd>) -> Result<String, String> {
        if let Some(ty) = testpoint::fake_fs_type(path) {
            return Ok(ty);
        }
        match dir {
            Some(fd) => mount::fs_type(fd),
            None => mount::fs_type(&self.parent),
        }
    }

    /// Whether the target, on device `dev`, is on its parent's mount, and its own mount:
    /// read from `dir` when the target is that open directory, by name otherwise.
    fn target_mount(&self, dev: u64, dir: Option<&OwnedFd>) -> (Mount, Result<MountId, String>) {
        let own = match dir {
            Some(fd) => mount::of_dir(fd),
            None => mount::of_entry_at(&self.parent, &self.name),
        };
        (
            Mount::between(self.parent_dev, dev, &self.parent_mount, &own),
            own,
        )
    }
}

/// The entries below the open directory `fd`, a moved target named `name` in `parent`, walked
/// as the snapshot walks (section 9.7, step 5): by paths relative to it, without following
/// links or entering a directory on another device or mount than `dev` and `on_mount`, each
/// entry read as the tests' fakes below `at` make it read; and the paths it did not read
/// whole: the directories not entered or not listed, and what was removed while it read them.
pub fn walk_moved(
    (fd, parent, name): (&OwnedFd, BorrowedFd<'_>, &OsStr),
    (dev, on_mount): (u64, &[u8]),
    at: &Path,
) -> io::Result<(Vec<Entry>, Vec<PathBuf>)> {
    let mut recorded = Recorded::default();
    snapshot_dir(
        (fd, dir::At::entry(parent, name)),
        Path::new(""),
        (dev, on_mount),
        (Own::Nothing, at),
        &mut recorded,
    )?;
    let mut not_read = recorded.boundaries;
    not_read.extend(recorded.unlisted);
    not_read.extend(recorded.vanished);
    Ok((recorded.entries, not_read))
}

/// What [`snapshot_dir`] records: the entries, the directories not entered, the reads of the
/// bound's measure that failed, the directories recorded and not listed (those not entered
/// included), and what was removed while it read them.
#[derive(Default)]
struct Recorded {
    entries: Vec<Entry>,
    boundaries: Vec<PathBuf>,
    unmeasured: Vec<(PathBuf, String)>,
    unlisted: Vec<PathBuf>,
    vanished: Vec<PathBuf>,
}

/// Records the entries below `fd`, named by `at`, each regular file with what `own` reads of
/// it, and each entry as the tests' fakes below `worktree` make it read. A directory on another
/// device or mount than `dev` and `on_mount` is recorded but not entered, and goes into
/// `boundaries` and `unlisted`; one replaced before it was opened is recorded as it was seen and
/// goes into `unlisted`; one removed before it was opened, an entry removed before its `lstat`,
/// and a directory removed or replaced while it was listed go into `vanished`. A mount that
/// cannot be read fails the snapshot.
fn snapshot_dir(
    (fd, at): (&OwnedFd, dir::At<'_>),
    rel: &Path,
    (dev, on_mount): (u64, &[u8]),
    (own, worktree): (Own, &Path),
    recorded: &mut Recorded,
) -> io::Result<()> {
    let listed = dir::list_with(&std::fs::File::from(fd.try_clone()?), own, at, &mut || {
        testpoint::hit(testpoint::ACT_DIR_LISTED).map_err(io::Error::other)
    })?;
    let path_of = |name: Option<OsString>| name.map_or_else(|| rel.to_path_buf(), |n| rel.join(n));
    recorded.unmeasured.extend(
        listed
            .unread
            .into_iter()
            .map(|(name, err)| (path_of(name), err)),
    );
    recorded
        .vanished
        .extend(listed.vanished.into_iter().map(path_of));
    for (name, st, read) in listed.entries {
        let child_rel = rel.join(&name);
        let child = Entry {
            own: read,
            ..Entry::from_stat(child_rel.clone(), &st)
        }
        .faked(&worktree.join(&child_rel));
        if child.kind == EntryKind::Dir && child.dev != dev {
            recorded.boundaries.push(child.rel.clone());
            recorded.unlisted.push(child.rel.clone());
        }
        let enter = child.kind == EntryKind::Dir && child.dev == dev;
        let index = recorded.entries.len();
        recorded.entries.push(child);
        if enter {
            let child_rel = recorded.entries[index].rel.clone();
            match open_dir_at(fd, &name) {
                Ok(child_fd) => {
                    let opened = entry(PathBuf::new(), &rustix::fs::fstat(&child_fd)?);
                    // Replaced between the stat and the open: the entry stays as the stat
                    // saw it, and nothing below the replacement is recorded.
                    if !recorded.entries[index].same_identity(&opened) {
                        recorded.unlisted.push(child_rel);
                    } else if mount::of_dir(&child_fd).map_err(io::Error::other)? == on_mount {
                        let read = (own, worktree);
                        let at = dir::At::entry(fd.as_fd(), &name);
                        snapshot_dir((&child_fd, at), &child_rel, (dev, on_mount), read, recorded)?;
                    } else {
                        recorded.boundaries.push(child_rel.clone());
                        recorded.unlisted.push(child_rel);
                    }
                }
                Err(err) => match err.raw_os_error().map(Errno::from_raw_os_error) {
                    Some(Errno::NOENT) => recorded.vanished.push(child_rel),
                    Some(Errno::LOOP | Errno::NOTDIR) => recorded.unlisted.push(child_rel),
                    _ => return Err(err),
                },
            }
        }
    }
    Ok(())
}
