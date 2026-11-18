//! Walking a target without following symbolic links (`docs/architecture.md` sections 9.2
//! and 9.5). `plan` reaches each directory by path and, on Unix, lists it through the
//! descriptor it identified it by; `act` takes its snapshot with directory descriptors opened
//! relative to their parents. Both produce the same [`Entry`] values, so both evaluate the
//! same checks.

use std::fs::Metadata;
use std::io;
use std::path::{Path, PathBuf};

use crate::contract::RootId;
use crate::testpoint;

/// The type of an entry, as `lstat` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// One entry of a target as a walk or snapshot records it: its identity, size, modification
/// time, and allocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The path relative to the worktree root (empty for the worktree root itself).
    pub rel: PathBuf,
    pub kind: EntryKind,
    /// Device and inode on Unix; 0 on Windows.
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch.
    pub mtime_ns: i128,
    /// Bytes allocated on disk: blocks × 512 on Unix, the length on Windows.
    pub allocated: u64,
    /// The number of names the entry has, here and elsewhere (1 on Windows).
    pub nlink: u64,
}

impl Entry {
    /// Whether `other` is this entry: same device, inode, and type.
    #[must_use]
    pub fn same_identity(&self, other: &Entry) -> bool {
        self.dev == other.dev && self.ino == other.ino && self.kind == other.kind
    }

    /// An entry from `std` metadata read without following a final symbolic link.
    #[must_use]
    pub fn from_metadata(rel: PathBuf, meta: &Metadata) -> Self {
        let file_type = meta.file_type();
        let kind = if file_type.is_symlink() {
            EntryKind::Symlink
        } else if file_type.is_dir() {
            EntryKind::Dir
        } else if file_type.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                rel,
                kind,
                dev: meta.dev(),
                ino: meta.ino(),
                size: meta.size(),
                mtime_ns: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
                allocated: meta.blocks().saturating_mul(512),
                nlink: meta.nlink(),
            }
        }
        #[cfg(not(unix))]
        {
            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| i128::try_from(d.as_nanos()).unwrap_or(i128::MAX));
            Self {
                rel,
                kind,
                dev: 0,
                ino: 0,
                size: meta.len(),
                mtime_ns,
                allocated: meta.len(),
                nlink: 1,
            }
        }
    }

    /// An entry from what `os::dir` read of it without following a symbolic link.
    #[cfg(unix)]
    pub(crate) fn from_stat(rel: PathBuf, st: &crate::os::dir::Stat) -> Self {
        use crate::os::dir::Kind;
        Self {
            rel,
            kind: match st.kind {
                Kind::File => EntryKind::File,
                Kind::Dir => EntryKind::Dir,
                Kind::Symlink => EntryKind::Symlink,
                Kind::Other => EntryKind::Other,
            },
            dev: st.dev,
            ino: st.ino,
            size: st.size,
            mtime_ns: st.mtime_ns,
            allocated: st.blocks.saturating_mul(512),
            nlink: st.nlink,
        }
    }
}

/// Whether a target is on the mount of the directory that contains it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mount {
    /// The same device and mount as its parent.
    Same,
    /// A mount point (a bind mount on the same device included), or on another device; for
    /// `plan`'s walk on Unix also a target that was no longer the directory it recorded, or no
    /// longer on its parent's mount, when the walk opened it to list it.
    Boundary,
    /// The mount could not be determined; the checks fail closed.
    Unknown(String),
}

impl Mount {
    /// Decides from the devices and the mount identities of a target and its parent. Another
    /// device is a boundary whatever the mounts say; on the same device, another mount is one
    /// too (a bind mount), and a mount that cannot be read is unknown.
    #[must_use]
    pub fn between<K: PartialEq>(
        parent_dev: u64,
        dev: u64,
        parent: &Result<K, String>,
        own: &Result<K, String>,
    ) -> Self {
        if parent_dev != dev {
            return Self::Boundary;
        }
        match (parent, own) {
            (Ok(parent), Ok(own)) if parent == own => Self::Same,
            (Ok(_), Ok(_)) => Self::Boundary,
            (Err(err), _) | (_, Err(err)) => Self::Unknown(err.clone()),
        }
    }
}

/// A target and everything below it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Walk {
    /// The target itself.
    pub root: Entry,
    /// Whether the target is on its parent's mount.
    pub mount: Mount,
    /// The first component between the worktree root and the target that is a symbolic
    /// link (worktree-relative), if any.
    pub link_in_path: Option<PathBuf>,
    /// Every entry below the root, parents before children. A directory on another device
    /// or mount than the root is recorded but not entered; nothing below a root that is not on
    /// its parent's mount is read.
    pub entries: Vec<Entry>,
    /// The directories below the root on another device or mount than the root, recorded and
    /// not entered, which fail `plain_target` (section 9.2).
    pub boundaries: Vec<PathBuf>,
}

impl Walk {
    /// The root, then every entry below it.
    pub fn all(&self) -> impl Iterator<Item = &Entry> {
        std::iter::once(&self.root).chain(&self.entries)
    }

    /// The bytes allocated by the root and everything below it, each file counted once
    /// however many of its names the walk found.
    #[must_use]
    pub fn allocated(&self) -> u64 {
        self.sizes().0
    }

    /// The part of [`Walk::allocated`] held by files with names outside the target (a link
    /// count above the names the walk found), which removing the target does not free
    /// (section 8.5); `None` on Windows, where link counts are not read.
    #[must_use]
    pub fn aliased(&self) -> Option<u64> {
        cfg!(unix).then(|| self.sizes().1)
    }

    /// Allocated and aliased bytes, files with several names counted once.
    fn sizes(&self) -> (u64, u64) {
        // (device, inode) → (link count, names found, bytes allocated)
        let mut linked: std::collections::HashMap<(u64, u64), (u64, u64, u64)> =
            std::collections::HashMap::new();
        let mut allocated = 0u64;
        for entry in self.all() {
            if entry.kind == EntryKind::Dir || entry.nlink <= 1 {
                allocated = allocated.saturating_add(entry.allocated);
                continue;
            }
            let slot =
                linked
                    .entry((entry.dev, entry.ino))
                    .or_insert((entry.nlink, 0, entry.allocated));
            if slot.1 == 0 {
                allocated = allocated.saturating_add(entry.allocated);
            }
            slot.1 += 1;
        }
        let aliased = linked
            .values()
            .filter(|(nlink, names, _)| nlink > names)
            .map(|(_, _, bytes)| bytes)
            .sum();
        (allocated, aliased)
    }
}

/// Walks `target`, which is `worktree` or a path inside it, without following symbolic links
/// or entering another device or mount. Plans use it; `act` uses its own descriptor-based
/// snapshot on Unix and this walk on Windows. Mounts are told by [`mount_id`], as `act` tells
/// them.
///
/// Each directory is reached by path and listed only once it is found to be what the walk
/// recorded: on Unix it is opened without following a final symbolic link, and listed through
/// that descriptor only while the descriptor has the `dev` and `ino` the walk recorded and the
/// target's mount; its entries are read relative to it. A target that fails this is reported
/// as [`Mount::Boundary`]; a directory below it is recorded as the walk saw it and not entered.
pub fn walk(worktree: &Path, target: &Path) -> io::Result<Walk> {
    let rel = target
        .strip_prefix(worktree)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "target outside the worktree"))?
        .to_path_buf();
    let mut link_in_path = None;
    let mut prefix = PathBuf::new();
    if let Some(parent) = rel.parent() {
        for component in parent.components() {
            prefix.push(component);
            if std::fs::symlink_metadata(worktree.join(&prefix))?.is_symlink() {
                link_in_path = Some(prefix.clone());
                break;
            }
        }
    }
    let parent = target.parent().unwrap_or(target);
    let parent_dev = Entry::from_metadata(PathBuf::new(), &std::fs::symlink_metadata(parent)?).dev;
    let root = Entry::from_metadata(rel, &std::fs::symlink_metadata(target)?);
    let root_mount = mount_id(target);
    let mut mount = Mount::between(parent_dev, root.dev, &mount_id(parent), &root_mount);
    let mut entries = Vec::new();
    let mut boundaries = Vec::new();
    // A target that is not on its parent's mount is refused, so nothing below it is read.
    let listable = root.kind == EntryKind::Dir && link_in_path.is_none() && mount == Mount::Same;
    if let (true, Ok(on_mount)) = (listable, &root_mount) {
        testpoint::hit(testpoint::WALK_TARGET_CHECKED).map_err(io::Error::other)?;
        match Listing::open(target, &root, on_mount)? {
            Some(listing) => {
                walk_dir(
                    worktree,
                    listing,
                    (&root.rel, root.dev, on_mount),
                    &mut entries,
                    &mut boundaries,
                )?;
            }
            None => mount = Mount::Boundary,
        }
    }
    Ok(Walk {
        root,
        mount,
        link_in_path,
        entries,
        boundaries,
    })
}

/// What identifies a mount.
#[cfg(unix)]
type MountKey = crate::os::mount::MountId;
/// Windows reads no mount identity (see [`mount_id`]): every directory the walk reaches is on
/// its parent's volume.
#[cfg(not(unix))]
#[derive(Debug, PartialEq, Eq)]
struct MountKey;

/// The mount of the entry at `path` (`os::mount`), not following a final symbolic link.
#[cfg(unix)]
fn mount_id(path: &Path) -> Result<MountKey, String> {
    crate::os::mount::of_path(path)
}

/// Windows has no mount to tell apart on one volume: a folder a volume is mounted on is a
/// reparse point, which `symlink_metadata` reports as a link, so the walk neither follows nor
/// enters it, and a target that is one fails `plain_target` as a link.
#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the same signature as the Unix version"
)]
fn mount_id(_path: &Path) -> Result<MountKey, String> {
    Ok(MountKey)
}

/// Records the entries below the directory `listing` of `worktree`, parents before children in
/// name order, without following symbolic links. A directory is listed only after its device
/// and mount are found to be `dev` and `on_mount`, by its path in `worktree` and then (Unix) on
/// the descriptor it is listed through: one on another device or mount, or replaced since its
/// entry was read, is recorded but never listed, and a mount that cannot be read fails the
/// walk. Each directory is listed in full and closed before anything below it is opened, so
/// one directory is open at a time. A directory found on another device or mount by its path
/// goes into `boundaries` too.
fn walk_dir(
    worktree: &Path,
    listing: Listing,
    (rel, dev, on_mount): (&Path, u64, &MountKey),
    out: &mut Vec<Entry>,
    boundaries: &mut Vec<PathBuf>,
) -> io::Result<()> {
    // The remaining entries of each directory being walked, innermost last.
    let mut pending = vec![listing.entries(rel)?.into_iter()];
    while let Some(level) = pending.last_mut() {
        let Some(entry) = level.next() else {
            pending.pop();
            continue;
        };
        if entry.kind == EntryKind::Dir && entry.dev != dev {
            boundaries.push(entry.rel.clone());
        }
        if entry.kind == EntryKind::Dir && entry.dev == dev {
            let path = worktree.join(&entry.rel);
            let enter = match mount_id(&path) {
                Ok(mount) => mount == *on_mount,
                // Removed since it was listed: the entry stays as it was seen.
                Err(_) if gone(&path) => false,
                Err(err) => return Err(io::Error::other(err)),
            };
            if !enter && !gone(&path) {
                boundaries.push(entry.rel.clone());
            }
            if enter {
                testpoint::hit(testpoint::WALK_DIR_CHECKED).map_err(io::Error::other)?;
                let listed = Listing::open(&path, &entry, on_mount)
                    .and_then(|below| below.map(|below| below.entries(&entry.rel)).transpose());
                match listed {
                    Ok(Some(below)) => pending.push(below.into_iter()),
                    // Replaced or removed since its entry was read: the entry stays as it was
                    // seen.
                    Ok(None) => {}
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err),
                }
            }
        }
        out.push(entry);
    }
    Ok(())
}

/// Whether nothing is at `path` any more.
fn gone(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_err_and(|err| err.kind() == io::ErrorKind::NotFound)
}

/// A directory the walk lists: on Unix, opened and found to be the directory the walk
/// recorded, on the target's mount.
struct Listing {
    #[cfg(unix)]
    dir: std::fs::File,
    #[cfg(not(unix))]
    path: PathBuf,
}

impl Listing {
    /// Opens the directory at `path`, recorded as `seen`, to list it; `None` when what is there
    /// is no longer `seen` (another type, a symbolic link, another directory) or not on
    /// `mount`. The check is on the descriptor the listing reads through.
    #[cfg(unix)]
    fn open(path: &Path, seen: &Entry, mount: &MountKey) -> io::Result<Option<Self>> {
        let Some(dir) = crate::os::dir::open(path)? else {
            return Ok(None);
        };
        let opened = Entry::from_metadata(seen.rel.clone(), &dir.metadata()?);
        if !seen.same_identity(&opened)
            || crate::os::mount::of_dir(&dir).map_err(io::Error::other)? != *mount
        {
            return Ok(None);
        }
        Ok(Some(Self { dir }))
    }

    /// Windows lists by path: a reparse point is recorded as a link and never listed, and
    /// there is no mount to tell apart (see [`mount_id`]).
    #[cfg(not(unix))]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the same signature as the Unix version"
    )]
    fn open(path: &Path, _seen: &Entry, _mount: &MountKey) -> io::Result<Option<Self>> {
        Ok(Some(Self {
            path: path.to_owned(),
        }))
    }

    /// The entries of the directory below the worktree-relative `rel`, in name order; the
    /// directory is closed on return. An entry removed before it is read is left out.
    #[cfg(unix)]
    fn entries(self, rel: &Path) -> io::Result<Vec<Entry>> {
        Ok(crate::os::dir::list(self.dir)?
            .into_iter()
            .map(|(name, st)| Entry::from_stat(rel.join(name), &st))
            .collect())
    }

    /// The entries of the directory below the worktree-relative `rel`, in name order. An
    /// entry removed before it is read is left out.
    #[cfg(not(unix))]
    fn entries(self, rel: &Path) -> io::Result<Vec<Entry>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&self.path)? {
            match entry {
                Ok(entry) => names.push(entry.file_name()),
                // The directory was removed while it was read: what was read stays.
                Err(err) if err.kind() == io::ErrorKind::NotFound => break,
                Err(err) => return Err(err),
            }
        }
        names.sort();
        let mut entries = Vec::with_capacity(names.len());
        for name in names {
            let path = self.path.join(&name);
            // `lstat` of the entry itself, not `DirEntry::metadata`: that is the copy of the
            // entry's attributes in its parent directory's index (`FindNextFileW`), which NTFS
            // updates lazily, so a directory's mtime there can predate an entry created below
            // it.
            match std::fs::symlink_metadata(&path) {
                Ok(meta) => entries.push(Entry::from_metadata(rel.join(name), &meta)),
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }
        Ok(entries)
    }
}

/// The identity of a worktree root: `dev` and `ino` on Unix, and its creation time where
/// the OS reports one.
pub fn root_identity(worktree: &Path) -> io::Result<(Option<RootId>, Option<u64>)> {
    let meta = std::fs::symlink_metadata(worktree)?;
    let created_ms = meta
        .created()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    #[cfg(unix)]
    let id = {
        use std::os::unix::fs::MetadataExt;
        Some(RootId {
            dev: meta.dev(),
            ino: meta.ino(),
        })
    };
    #[cfg(not(unix))]
    let id = None;
    Ok((id, created_ms))
}

#[cfg(test)]
mod tests {
    use super::{EntryKind, Mount};

    #[test]
    fn mount_boundary_is_told_by_device_and_mount() {
        let same: Result<u64, String> = Ok(7);
        let other: Result<u64, String> = Ok(8);
        let unknown: Result<u64, String> = Err("statx: no mount ID".into());
        assert_eq!(Mount::between(1, 1, &same, &same), Mount::Same);
        // A bind mount: the same device, another mount.
        assert_eq!(Mount::between(1, 1, &same, &other), Mount::Boundary);
        // Another device is a boundary even when the mounts cannot be read.
        assert_eq!(Mount::between(1, 2, &unknown, &unknown), Mount::Boundary);
        assert_eq!(
            Mount::between(1, 1, &same, &unknown),
            Mount::Unknown("statx: no mount ID".into())
        );
        assert_eq!(
            Mount::between(1, 1, &unknown, &same),
            Mount::Unknown("statx: no mount ID".into())
        );
    }

    #[test]
    fn entries_come_parents_first_in_name_order() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = dunce::canonicalize(tmp.path()).unwrap();
        for dir in ["t/b", "t/a/y"] {
            std::fs::create_dir_all(wt.join(dir)).unwrap();
        }
        for file in ["t/c", "t/a/z", "t/a/y/x"] {
            std::fs::write(wt.join(file), b"x").unwrap();
        }
        let walk = super::walk(&wt, &wt.join("t")).unwrap();
        let rels: Vec<_> = walk.entries.iter().map(|e| e.rel.clone()).collect();
        let expected: Vec<std::path::PathBuf> = ["t/a", "t/a/y", "t/a/y/x", "t/a/z", "t/b", "t/c"]
            .into_iter()
            .map(Into::into)
            .collect();
        assert_eq!(rels, expected);
        for entry in &walk.entries {
            let meta = std::fs::symlink_metadata(wt.join(&entry.rel)).unwrap();
            assert_eq!(
                *entry,
                super::Entry::from_metadata(entry.rel.clone(), &meta),
                "recorded from lstat"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_target_that_is_a_link_is_not_entered() {
        let tmp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir_all(root.join("wt")).unwrap();
        std::fs::create_dir_all(root.join("outside/dir")).unwrap();
        std::os::unix::fs::symlink(root.join("outside"), root.join("wt/target")).unwrap();
        let walk = super::walk(&root.join("wt"), &root.join("wt/target")).unwrap();
        assert_eq!(walk.root.kind, EntryKind::Symlink);
        assert!(walk.entries.is_empty(), "{:?}", walk.entries);
    }

    #[test]
    fn walks_without_following_links() {
        let tmp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();
        let wt = root.join("wt");
        std::fs::create_dir_all(wt.join("target/a")).unwrap();
        std::fs::write(wt.join("target/a/f"), b"12345").unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        std::fs::write(root.join("outside/secret"), b"x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("outside"), wt.join("target/link")).unwrap();

        let walk = super::walk(&wt, &wt.join("target")).unwrap();
        assert_eq!(walk.root.rel, std::path::Path::new("target"));
        assert_eq!(walk.root.kind, EntryKind::Dir);
        assert!(walk.link_in_path.is_none());
        let rels: Vec<_> = walk.entries.iter().map(|e| e.rel.clone()).collect();
        assert!(rels.contains(&"target/a/f".into()), "{rels:?}");
        assert!(
            !rels.iter().any(|r| r.ends_with("secret")),
            "links are not followed: {rels:?}"
        );
        assert!(walk.allocated() >= walk.entries.iter().map(|e| e.allocated).sum());

        #[cfg(unix)]
        {
            let through = super::walk(&wt, &wt.join("target/link/secret")).unwrap();
            assert_eq!(through.link_in_path, Some("target/link".into()));
        }
    }
}
