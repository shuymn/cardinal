//! Walking a target without following symbolic links (`docs/architecture.md` sections 9.2
//! and 9.5). `plan` reaches each directory by path and, on Unix, lists it through the
//! descriptor it identified it by; `act` takes its snapshot with directory descriptors opened
//! relative to their parents. Both produce the same [`Entry`] values, so both evaluate the
//! same checks.

use std::fs::Metadata;
use std::io;
use std::path::{Path, PathBuf};

use crate::contract::RootId;
use crate::observe::{Bound, Counted, Exclusive, Measure};
use crate::testpoint;

/// The type of an entry, as `lstat` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Fifo,
    Socket,
    /// A block or character device.
    Device,
    Other,
}

impl EntryKind {
    /// A FIFO, a socket, or a device: where processes meet, or what does not belong in build
    /// output, which `plain_target` refuses in a `reclaim_dir` target (section 9.2).
    #[must_use]
    pub fn is_special(self) -> bool {
        matches!(self, Self::Fifo | Self::Socket | Self::Device)
    }

    /// The type a test's fake names (`testpoint::fake_entry_type`).
    fn faked(name: &str) -> Option<Self> {
        match name {
            "fifo" => Some(Self::Fifo),
            "socket" => Some(Self::Socket),
            "device" => Some(Self::Device),
            _ => None,
        }
    }
}

/// One entry of a target as a walk or snapshot records it: its identity, size, modification
/// time, and allocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The path relative to the worktree root (empty for the worktree root itself).
    pub rel: PathBuf,
    pub kind: EntryKind,
    /// Device and inode on Unix. On Windows the device is 0, and the inode is a regular file's
    /// file index (0 for other entries).
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch.
    pub mtime_ns: i128,
    /// Bytes allocated on disk: blocks × 512 on Unix, the length on Windows.
    pub allocated: u64,
    /// The number of names the entry has, here and elsewhere (on Windows read for regular
    /// files, 1 for other entries).
    pub nlink: u64,
    /// What the measure of the target's file system reads of a regular file (section 8.5):
    /// its private size (APFS) or its counted data extents (ext2–4); `None` when not read.
    pub own: Option<u64>,
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
            special(file_type)
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
                own: None,
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
                own: None,
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
                Kind::Fifo => EntryKind::Fifo,
                Kind::Socket => EntryKind::Socket,
                Kind::Device => EntryKind::Device,
                Kind::Other => EntryKind::Other,
            },
            dev: st.dev,
            ino: st.ino,
            size: st.size,
            mtime_ns: st.mtime_ns,
            allocated: st.blocks.saturating_mul(512),
            nlink: st.nlink,
            own: None,
        }
    }

    /// This entry as a test's fake makes the walks read the entry at `path`
    /// (`testpoint::fake_entry_type`): a FIFO, a socket, or a device, which only root could
    /// make. Unchanged outside tests.
    #[must_use]
    pub fn faked(self, path: &Path) -> Self {
        match testpoint::fake_entry_type(path).and_then(|ty| EntryKind::faked(&ty)) {
            Some(kind) => Self { kind, ..self },
            None => self,
        }
    }
}

/// The type of an entry that is not a regular file, a directory, or a symbolic link.
#[cfg(unix)]
fn special(file_type: std::fs::FileType) -> EntryKind {
    use std::os::unix::fs::FileTypeExt;
    if file_type.is_fifo() {
        EntryKind::Fifo
    } else if file_type.is_socket() {
        EntryKind::Socket
    } else if file_type.is_block_device() || file_type.is_char_device() {
        EntryKind::Device
    } else {
        EntryKind::Other
    }
}

/// The type of an entry that is not a regular file, a directory, or a symbolic link.
#[cfg(not(unix))]
fn special(_file_type: std::fs::FileType) -> EntryKind {
    EntryKind::Other
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
    /// The type of the file system the target is on (`os::mount::fs_type`), which
    /// `local_volume` and the freed-bytes bound read (sections 8.5 and 9.2), or why it could
    /// not be read.
    pub fs_type: Result<String, String>,
    /// The reads of the bound's measure that failed, by worktree-relative path: a directory
    /// whose entries' private sizes, or a file whose extents, could not be read, and a
    /// directory the walk recorded and did not list, the target included, whose contents the
    /// bound would leave out.
    pub unmeasured: Vec<(PathBuf, String)>,
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
        self.sizes().allocated
    }

    /// The part of [`Walk::allocated`] held by files with names outside the target (a link
    /// count above the names the walk found), which removing the target does not free
    /// (section 8.5).
    #[must_use]
    pub fn aliased(&self) -> u64 {
        self.sizes().aliased
    }

    /// The bytes of the regular files with no name outside the target, each counted once:
    /// what removing the target frees of its files, its directories' blocks aside.
    #[must_use]
    pub fn own_file_bytes(&self) -> u64 {
        self.sizes().own_files
    }

    /// The lower bound of the bytes removing the target frees, by the measure of its file
    /// system (section 8.5): null when a read it needs failed, a directory was not listed, or
    /// the file system's sharing is not read.
    #[must_use]
    pub fn exclusive(&self) -> Exclusive {
        let mut bound = Bound::new(Measure::of(&self.fs_type));
        if let Some((path, err)) = self.unmeasured.first() {
            bound.fail(format!("{}: {err}", path.display()));
        }
        for entry in self.all() {
            let kind = match entry.kind {
                EntryKind::Dir => Counted::Dir,
                EntryKind::File => Counted::File,
                _ => Counted::Other,
            };
            bound.add(
                kind,
                (entry.dev, entry.ino),
                entry.nlink,
                entry.allocated,
                entry.own,
            );
        }
        bound.finish()
    }

    /// The sizes of the walk, files with several names counted once.
    fn sizes(&self) -> Sizes {
        // (device, inode) → (link count, names found, bytes allocated, a regular file)
        let mut linked: std::collections::HashMap<(u64, u64), (u64, u64, u64, bool)> =
            std::collections::HashMap::new();
        let mut sizes = Sizes::default();
        for entry in self.all() {
            let file = entry.kind == EntryKind::File;
            if entry.kind == EntryKind::Dir || entry.nlink <= 1 {
                sizes.allocated = sizes.allocated.saturating_add(entry.allocated);
                if file {
                    sizes.own_files = sizes.own_files.saturating_add(entry.allocated);
                }
                continue;
            }
            let slot = linked.entry((entry.dev, entry.ino)).or_insert((
                entry.nlink,
                0,
                entry.allocated,
                file,
            ));
            if slot.1 == 0 {
                sizes.allocated = sizes.allocated.saturating_add(entry.allocated);
            }
            slot.1 += 1;
        }
        for &(nlink, names, bytes, file) in linked.values() {
            if nlink > names {
                sizes.aliased = sizes.aliased.saturating_add(bytes);
            } else if file {
                sizes.own_files = sizes.own_files.saturating_add(bytes);
            }
        }
        sizes
    }
}

/// What [`Walk::sizes`] adds up.
#[derive(Clone, Copy, Debug, Default)]
struct Sizes {
    allocated: u64,
    aliased: u64,
    own_files: u64,
}

/// An entry read from `path` without following a final symbolic link. On Windows a regular
/// file's file index and link count are read too (`os::file_links`), which `std` metadata
/// leaves out, so a file with several names is told apart as on Unix.
fn lstat(rel: PathBuf, path: &Path) -> io::Result<Entry> {
    let entry = Entry::from_metadata(rel, &std::fs::symlink_metadata(path)?);
    #[cfg(windows)]
    if entry.kind == EntryKind::File {
        let (ino, nlink) = crate::os::file_links(path)?;
        return Ok(Entry {
            ino,
            nlink,
            ..entry
        });
    }
    Ok(entry)
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
    walk_listed(worktree, target).map(|(walk, _)| walk)
}

/// A walk of a worktree root that the walks of directories inside it can be taken from
/// ([`Tree::walk_of`]), so bulk `plan` does not walk them again (section 9.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tree {
    /// The walk of the worktree root.
    pub walk: Walk,
    /// The directories the walk recorded and did not list: on another device or mount, or
    /// replaced or removed before it listed them.
    unlisted: std::collections::HashSet<PathBuf>,
}

impl Tree {
    /// Walks the worktree root `worktree` as [`walk`] does, keeping what [`Tree::walk_of`]
    /// needs.
    pub fn walk(worktree: &Path) -> io::Result<Self> {
        let (walk, unlisted) = walk_listed(worktree, worktree)?;
        Ok(Self {
            walk,
            unlisted: unlisted.into_iter().collect(),
        })
    }

    /// The walk of the directory `rel` (worktree-relative) below the root `worktree`, as
    /// [`walk`] of it records it, taken from this walk: `None` unless this walk listed the root
    /// and `rel`, and the path `rel` holds that directory still.
    ///
    /// A directory this walk listed is on the root's device and mount, and so is every
    /// directory above it, none of them a symbolic link; [`walk`] of it finds it so (a
    /// [`Mount::Same`] target with no link in its path), lists it as this walk did, and
    /// records below it what this walk recorded, parents before children in name order, which
    /// is the run of entries after it here. The checks [`walk`] runs at its target are run
    /// again and must find it listable, with the entry recorded, so a target replaced or
    /// removed since, one below a directory replaced by a symbolic link, one that can no longer
    /// be read, or one now under a mount is walked on its own, as [`walk`] then finds it. Below
    /// it, facts read once are the ones a second walk would read again a moment later: the same
    /// entries, unless the tree changed in between, as it can during any walk. Windows reads
    /// no device and inode of a directory ([`Entry::dev`]), so there nothing is taken.
    #[must_use]
    pub fn walk_of(&self, worktree: &Path, rel: &Path) -> Option<Walk> {
        let root = &self.walk;
        let listed_root = root.root.rel.as_os_str().is_empty()
            && root.root.kind == EntryKind::Dir
            && root.mount == Mount::Same
            && root.link_in_path.is_none();
        if cfg!(windows)
            || !listed_root
            || rel.as_os_str().is_empty()
            || self.unlisted.contains(rel)
        {
            return None;
        }
        let start = root.entries.iter().position(|e| e.rel == rel)?;
        let target = &root.entries[start];
        if target.kind != EntryKind::Dir {
            return None;
        }
        // A walk of the target would list it: the checks it runs at the target pass now,
        // finding the entry recorded here. The descriptor they opened closes unread.
        let checked = check_root(worktree, &worktree.join(rel)).ok()?;
        if checked.listing.is_none() || checked.root != *target {
            return None;
        }
        let entries = root.entries[start + 1..]
            .iter()
            .take_while(|e| e.rel.starts_with(rel))
            .cloned()
            .collect();
        let below = |path: &PathBuf| path.starts_with(rel);
        let boundaries = root
            .boundaries
            .iter()
            .filter(|b| below(b))
            .cloned()
            .collect();
        let unmeasured = root
            .unmeasured
            .iter()
            .filter(|(path, _)| below(path))
            .cloned()
            .collect();
        Some(Walk {
            root: target.clone(),
            mount: Mount::Same,
            link_in_path: None,
            entries,
            boundaries,
            // The target is on the root's mount, unless a test's fake names it.
            fs_type: testpoint::fake_fs_type(&worktree.join(rel))
                .map_or_else(|| root.fs_type.clone(), Ok),
            unmeasured,
        })
    }
}

/// [`walk`], with the directories it recorded and did not list (see [`Tree`]).
fn walk_listed(worktree: &Path, target: &Path) -> io::Result<(Walk, Vec<PathBuf>)> {
    let Checked {
        mut root,
        mount,
        link_in_path,
        listing,
    } = check_root(worktree, target)?;
    let fs_type = fs_type(target, listing.as_ref().map(|(listing, _)| listing));
    let own = own(&Measure::of(&fs_type));
    let mut entries = Vec::new();
    let mut boundaries = Vec::new();
    let mut unlisted = Vec::new();
    let mut unmeasured = Vec::new();
    if root.kind == EntryKind::File {
        match own_of_file(target, &root, own) {
            Ok(read) => root.own = read,
            Err(err) => unmeasured.push((root.rel.clone(), err.to_string())),
        }
    }
    if root.kind == EntryKind::Dir && listing.is_none() {
        unmeasured.push((root.rel.clone(), "the target was not listed".to_owned()));
    }
    if let Some((listing, on_mount)) = listing {
        walk_dir(
            worktree,
            (listing, own),
            (&root.rel, root.dev, &on_mount),
            &mut entries,
            (&mut boundaries, &mut unlisted, &mut unmeasured),
        )?;
    }
    let walk = Walk {
        root,
        mount,
        link_in_path,
        entries,
        boundaries,
        fs_type,
        unmeasured,
    };
    Ok((walk, unlisted))
}

/// What a walk reads of its target before it lists anything below it.
struct Checked {
    root: Entry,
    mount: Mount,
    link_in_path: Option<PathBuf>,
    /// The target opened to be listed, and its mount: `None` when nothing below it is read.
    listing: Option<(Listing, MountKey)>,
}

/// The checks a walk of `target` runs at the target, in its order: the directories between the
/// worktree root and the target for a symbolic link, the target's entry and its mount against
/// its parent's, and the target opened to be listed, which is listed only when it is a
/// directory, with no link in its path, on its parent's mount, and still the directory recorded
/// on that mount when opened.
fn check_root(worktree: &Path, target: &Path) -> io::Result<Checked> {
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
    let root = lstat(rel, target)?.faked(target);
    let root_mount = mount_id(target);
    let mut mount = Mount::between(parent_dev, root.dev, &mount_id(parent), &root_mount);
    let mut listing = None;
    // A target that is not on its parent's mount is refused, so nothing below it is read.
    let listable = root.kind == EntryKind::Dir && link_in_path.is_none() && mount == Mount::Same;
    if let (true, Ok(on_mount)) = (listable, root_mount) {
        testpoint::hit(testpoint::WALK_TARGET_CHECKED).map_err(io::Error::other)?;
        match Listing::open(target, &root, &on_mount)? {
            Some(opened) => listing = Some((opened, on_mount)),
            None => mount = Mount::Boundary,
        }
    }
    Ok(Checked {
        root,
        mount,
        link_in_path,
        listing,
    })
}

/// The type of the file system `target` is on: read from `listing`, the target opened to be
/// listed, or else from the directory that contains it; a test's fake first
/// (`testpoint::fake_fs_type`).
fn fs_type(target: &Path, listing: Option<&Listing>) -> Result<String, String> {
    if let Some(ty) = testpoint::fake_fs_type(target) {
        return Ok(ty);
    }
    #[cfg(unix)]
    {
        if let Some(listing) = listing {
            crate::os::mount::fs_type(&listing.dir)
        } else {
            let parent = target.parent().unwrap_or(target);
            std::fs::File::open(parent)
                .map_err(|err| format!("{}: {err}", parent.display()))
                .and_then(crate::os::mount::fs_type)
        }
    }
    #[cfg(not(unix))]
    {
        let _ = listing;
        Err("the file system type is not read on this platform".to_owned())
    }
}

/// What a listing reads of each regular file for `measure`.
#[cfg(unix)]
type Own = crate::os::dir::Own;
/// Windows reads nothing of a file for the bound.
#[cfg(not(unix))]
#[derive(Clone, Copy)]
struct Own;

#[cfg(unix)]
fn own(measure: &Measure) -> Own {
    measure.own()
}

#[cfg(not(unix))]
fn own(_measure: &Measure) -> Own {
    Own
}

/// What `own` reads of the regular file `target`, whose entry the walk recorded as `seen`,
/// through its parent directory.
#[cfg(unix)]
fn own_of_file(target: &Path, seen: &Entry, own: Own) -> io::Result<Option<u64>> {
    let (Some(parent), Some(name)) = (target.parent(), target.file_name()) else {
        return Ok(None);
    };
    let parent = std::fs::File::open(parent)?;
    crate::os::dir::own_at(&parent, name, (seen.dev, seen.ino), own)
}

#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the same signature as the Unix version"
)]
fn own_of_file(_target: &Path, _seen: &Entry, _own: Own) -> io::Result<Option<u64>> {
    Ok(None)
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
/// goes into `boundaries` too, and every directory recorded and not listed into `unlisted` and
/// `unmeasured`.
fn walk_dir(
    worktree: &Path,
    (listing, own): (Listing, Own),
    (rel, dev, on_mount): (&Path, u64, &MountKey),
    out: &mut Vec<Entry>,
    (boundaries, unlisted, unmeasured): (
        &mut Vec<PathBuf>,
        &mut Vec<PathBuf>,
        &mut Vec<(PathBuf, String)>,
    ),
) -> io::Result<()> {
    let mut list = |listing: Listing, rel: &Path| -> io::Result<std::vec::IntoIter<Entry>> {
        let (entries, unread) = listing.entries(rel, own)?;
        unmeasured.extend(unread);
        let faked: Vec<Entry> = entries
            .into_iter()
            .map(|e| {
                let path = worktree.join(&e.rel);
                e.faked(&path)
            })
            .collect();
        Ok(faked.into_iter())
    };
    let mut not_listed = Vec::new();
    // The remaining entries of each directory being walked, innermost last.
    let mut pending = vec![list(listing, rel)?];
    while let Some(level) = pending.last_mut() {
        let Some(entry) = level.next() else {
            pending.pop();
            continue;
        };
        let mut listed = false;
        let mut why = "replaced or removed while the walk ran";
        if entry.kind == EntryKind::Dir && entry.dev != dev {
            boundaries.push(entry.rel.clone());
            why = "on another device";
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
                why = "on another mount";
            }
            if enter {
                testpoint::hit(testpoint::WALK_DIR_CHECKED).map_err(io::Error::other)?;
                let below = Listing::open(&path, &entry, on_mount)
                    .and_then(|below| below.map(|below| list(below, &entry.rel)).transpose());
                match below {
                    Ok(Some(below)) => {
                        pending.push(below);
                        listed = true;
                    }
                    // Replaced or removed since its entry was read: the entry stays as it was
                    // seen.
                    Ok(None) => {}
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err),
                }
            }
        }
        if entry.kind == EntryKind::Dir && !listed {
            unlisted.push(entry.rel.clone());
            not_listed.push((entry.rel.clone(), format!("{why}, not listed")));
        }
        out.push(entry);
    }
    unmeasured.extend(not_listed);
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

    /// The entries of the directory below the worktree-relative `rel`, in name order, each
    /// regular file with what `own` reads of it, and the reads of `own` that failed, by
    /// worktree-relative path; the directory is closed on return. An entry removed before it
    /// is read is left out.
    #[cfg(unix)]
    #[expect(clippy::type_complexity, reason = "two lists the walk keeps apart")]
    fn entries(self, rel: &Path, own: Own) -> io::Result<(Vec<Entry>, Vec<(PathBuf, String)>)> {
        let listed = crate::os::dir::list(&self.dir, own)?;
        let entries = listed
            .entries
            .into_iter()
            .map(|(name, st, read)| Entry {
                own: read,
                ..Entry::from_stat(rel.join(name), &st)
            })
            .collect();
        let unread = listed
            .unread
            .into_iter()
            .map(|(name, err)| (name.map_or_else(|| rel.to_path_buf(), |n| rel.join(n)), err))
            .collect();
        Ok((entries, unread))
    }

    /// The entries of the directory below the worktree-relative `rel`, in name order. An
    /// entry removed before it is read is left out.
    #[cfg(not(unix))]
    #[expect(clippy::type_complexity, reason = "two lists the walk keeps apart")]
    fn entries(self, rel: &Path, _own: Own) -> io::Result<(Vec<Entry>, Vec<(PathBuf, String)>)> {
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
            match lstat(rel.join(&name), &path) {
                Ok(entry) => entries.push(entry),
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }
        Ok((entries, Vec::new()))
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

    /// A file with a name outside the target is aliased and none of the target's own files;
    /// a file whose names are all inside counts once among them; directories count in
    /// neither.
    #[test]
    fn own_file_bytes_leave_out_aliased_files_and_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = dunce::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir_all(wt.join("t/sub")).unwrap();
        std::fs::create_dir_all(wt.join("only/pkg")).unwrap();
        let bytes = vec![7u8; 64 * 1024];
        for file in ["outside", "t/own", "t/twice"] {
            std::fs::write(wt.join(file), &bytes).unwrap();
        }
        std::fs::hard_link(wt.join("outside"), wt.join("t/linked")).unwrap();
        std::fs::hard_link(wt.join("t/twice"), wt.join("t/sub/twice")).unwrap();
        std::fs::hard_link(wt.join("outside"), wt.join("only/pkg/linked")).unwrap();
        let size = |rel: &str| {
            let walk = super::walk(&wt, &wt.join(rel)).unwrap();
            (walk.own_file_bytes(), walk.aliased(), walk.allocated())
        };
        let file = super::walk(&wt, &wt.join("outside"))
            .unwrap()
            .root
            .allocated;
        let (own, aliased, allocated) = size("t");
        assert_eq!((own, aliased), (2 * file, file));
        assert!(allocated >= 3 * file, "{allocated}");
        let (own, aliased, _) = size("only");
        assert_eq!((own, aliased), (0, file));
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
            // `own` is the bound's reading, which `lstat` does not give.
            let recorded = super::Entry {
                own: None,
                ..entry.clone()
            };
            assert_eq!(
                recorded,
                super::lstat(entry.rel.clone(), &wt.join(&entry.rel)).unwrap(),
                "recorded from lstat"
            );
        }
    }

    /// A directory's walk taken from its worktree's walk is the walk of that directory: the
    /// same root, entries, and boundaries. A target the worktree's walk did not list (a file, a
    /// symbolic link, a missing path, the root itself) is not taken.
    #[cfg(unix)]
    #[test]
    fn a_walk_taken_from_the_worktree_walk_is_the_walk_of_the_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = dunce::canonicalize(tmp.path()).unwrap().join("wt");
        for dir in [
            "node_modules/a/b",
            "node_modules/pkg/.git",
            "pkg/node_modules/dep",
            "pkg/node_modules-other",
        ] {
            std::fs::create_dir_all(wt.join(dir)).unwrap();
        }
        for file in [
            "node_modules/a/b/c.js",
            "node_modules/pkg/.git/HEAD",
            "pkg/node_modules/dep/index.js",
            "pkg/node_modules-other/f",
            ".DS_Store",
            "outside",
        ] {
            std::fs::write(wt.join(file), vec![7u8; 5000]).unwrap();
        }
        std::fs::hard_link(wt.join("outside"), wt.join("node_modules/a/linked")).unwrap();
        std::os::unix::fs::symlink("a/b/c.js", wt.join("node_modules/bin")).unwrap();
        std::os::unix::fs::symlink("node_modules", wt.join("linked")).unwrap();
        let tree = super::Tree::walk(&wt).unwrap();
        assert_eq!(tree.walk, super::walk(&wt, &wt).unwrap());
        for rel in [
            "node_modules",
            "node_modules/a",
            "node_modules/pkg/.git",
            "pkg/node_modules",
        ] {
            let alone = super::walk(&wt, &wt.join(rel)).unwrap();
            assert_eq!(
                tree.walk_of(&wt, rel.as_ref()).as_ref(),
                Some(&alone),
                "{rel}"
            );
        }
        for rel in [".DS_Store", "linked", "node_modules/bin", "missing", ""] {
            assert_eq!(tree.walk_of(&wt, rel.as_ref()), None, "{rel}");
        }
    }

    /// A directory the worktree's walk recorded and did not list (on another mount, or
    /// replaced before it was listed) gets a walk of its own.
    #[cfg(unix)]
    #[test]
    fn a_directory_the_worktree_walk_did_not_list_is_not_taken() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = dunce::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir_all(wt.join("node_modules/a")).unwrap();
        let mut tree = super::Tree::walk(&wt).unwrap();
        assert!(tree.walk_of(&wt, "node_modules".as_ref()).is_some());
        tree.unlisted.insert("node_modules".into());
        assert_eq!(tree.walk_of(&wt, "node_modules".as_ref()), None);
    }

    /// A directory replaced by another or by a symbolic link, or removed, since the worktree's
    /// walk listed it is not taken: what the path holds now is walked on its own.
    #[cfg(unix)]
    #[test]
    fn a_directory_changed_since_the_worktree_walk_is_not_taken() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = dunce::canonicalize(tmp.path()).unwrap();
        for dir in ["replaced/a", "linked/a", "removed/a", "kept/a"] {
            std::fs::create_dir_all(wt.join(dir)).unwrap();
        }
        let tree = super::Tree::walk(&wt).unwrap();
        // Renamed away first, so the new directory cannot reuse the old one's inode.
        std::fs::rename(wt.join("replaced"), wt.join("old-replaced")).unwrap();
        std::fs::create_dir_all(wt.join("replaced/a")).unwrap();
        std::fs::rename(wt.join("linked"), wt.join("old-linked")).unwrap();
        std::os::unix::fs::symlink("old-linked", wt.join("linked")).unwrap();
        std::fs::remove_dir_all(wt.join("removed")).unwrap();
        for rel in ["replaced", "linked", "removed"] {
            assert_eq!(tree.walk_of(&wt, rel.as_ref()), None, "{rel}");
        }
        let kept = super::walk(&wt, &wt.join("kept")).unwrap();
        assert_eq!(tree.walk_of(&wt, "kept".as_ref()), Some(kept));
    }

    /// A target that is still the directory recorded is not taken when a walk of it would
    /// now find it otherwise at the target: below a directory replaced by a symbolic link to
    /// it, or no longer readable.
    #[cfg(unix)]
    #[test]
    fn a_target_a_walk_would_now_find_otherwise_is_not_taken() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let wt = dunce::canonicalize(tmp.path()).unwrap();
        for dir in ["dir/cache/a", "locked/a"] {
            std::fs::create_dir_all(wt.join(dir)).unwrap();
        }
        let tree = super::Tree::walk(&wt).unwrap();
        std::fs::rename(wt.join("dir"), wt.join("saved")).unwrap();
        std::os::unix::fs::symlink("saved", wt.join("dir")).unwrap();
        let fresh = super::walk(&wt, &wt.join("dir/cache")).unwrap();
        assert_eq!(fresh.link_in_path, Some("dir".into()));
        assert_eq!(tree.walk_of(&wt, "dir/cache".as_ref()), None);
        let locked = wt.join("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o111)).unwrap();
        let taken = tree.walk_of(&wt, "locked".as_ref());
        let fresh = super::walk(&wt, &locked);
        let readable = std::fs::read_dir(&locked).is_ok();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if readable {
            // Root reads any directory, and so does its walk.
            assert_eq!(taken, fresh.ok());
        } else {
            // An unknown mount on macOS, which opens the target to read it; an error on Linux,
            // where the listing's open fails.
            assert!(fresh.is_err() || fresh.is_ok_and(|walk| walk.mount != Mount::Same));
            assert_eq!(taken, None);
        }
    }

    /// Windows reads no identity of a directory, so no walk is taken there.
    #[cfg(windows)]
    #[test]
    fn no_walk_is_taken_on_windows() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = dunce::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir_all(wt.join("node_modules/a")).unwrap();
        let tree = super::Tree::walk(&wt).unwrap();
        assert_eq!(tree.walk_of(&wt, "node_modules".as_ref()), None);
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
