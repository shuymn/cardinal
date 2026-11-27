//! Ignored-data classification and disk usage (`docs/architecture.md` sections 8.3 and 8.5).
//!
//! Walks never follow symbolic links and never cross into a directory with its own `.git`
//! entry. A walk that cannot read part of an entry cannot prove it holds nothing protected,
//! so the entry is `unclassifiable`.

use std::path::{Path, PathBuf};

use crate::config::{DataList, data_patterns_cover};
use crate::contract::{ExclusiveMeasure, IgnoredClass};

/// The `[data]` lists of the profiles that apply to a repository.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DataPatterns {
    pub regenerable: Vec<String>,
    pub protected: Vec<String>,
}

impl DataPatterns {
    /// Whether `rel` (worktree-relative) is, or is inside, a match of `list`.
    pub fn covers(&self, list: DataList, rel: &Path) -> bool {
        let patterns = match list {
            DataList::Regenerable => &self.regenerable,
            DataList::Protected => &self.protected,
        };
        data_patterns_cover(list, patterns, rel)
    }
}

/// Classifies the ignored entry `rel` (worktree-relative) of the worktree at `root`:
///
/// 1. `protected` if the entry, a directory it is inside, or anything below it matches a
///    protected pattern;
/// 2. `nested_repository` if a directory at or below it has its own `.git` entry or is a git
///    directory itself (`HEAD`, `objects/`, and `refs/`, as git recognizes a bare
///    repository);
/// 3. `regenerable` if the entry is, or is inside, a regenerable match;
/// 4. else `unclassifiable`, also when the walk could not read part of the entry.
#[must_use]
pub fn classify(root: &Path, rel: &Path, patterns: &DataPatterns) -> IgnoredClass {
    if patterns.covers(DataList::Protected, rel) {
        return IgnoredClass::Protected;
    }
    let walk = Walk::run(root, rel, patterns);
    if walk.protected {
        IgnoredClass::Protected
    } else if walk.nested {
        IgnoredClass::NestedRepository
    } else if walk.incomplete {
        IgnoredClass::Unclassifiable
    } else if patterns.covers(DataList::Regenerable, rel) {
        IgnoredClass::Regenerable
    } else {
        IgnoredClass::Unclassifiable
    }
}

#[derive(Default)]
struct Walk {
    protected: bool,
    nested: bool,
    incomplete: bool,
}

impl Walk {
    /// Walks everything below `root/rel`, stopping at the first protected entry.
    fn run(root: &Path, rel: &Path, patterns: &DataPatterns) -> Self {
        let mut walk = Self::default();
        let mut stack: Vec<PathBuf> = vec![rel.to_path_buf()];
        while let Some(dir_rel) = stack.pop() {
            let dir = root.join(&dir_rel);
            let Ok(meta) = std::fs::symlink_metadata(&dir) else {
                walk.incomplete = true;
                continue;
            };
            if !meta.is_dir() {
                continue;
            }
            if std::fs::symlink_metadata(dir.join(".git")).is_ok() || is_git_dir(&dir) {
                walk.nested = true;
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                walk.incomplete = true;
                continue;
            };
            for entry in entries {
                let Ok(entry) = entry else {
                    walk.incomplete = true;
                    continue;
                };
                let name = entry.file_name();
                if name.to_str().is_none() {
                    // A name that is not UTF-8 cannot be compared with the patterns.
                    walk.incomplete = true;
                }
                let child = dir_rel.join(&name);
                if patterns.covers(DataList::Protected, &child) {
                    walk.protected = true;
                    return walk;
                }
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(child);
                }
            }
        }
        walk
    }
}

/// Whether `dir` is a git directory: `HEAD`, `objects/`, and `refs/`, as git recognizes a
/// bare repository (section 8.3).
pub fn is_git_dir(dir: &Path) -> bool {
    let is = |name: &str, dir_wanted: bool| {
        std::fs::symlink_metadata(dir.join(name)).is_ok_and(|m| m.is_dir() == dir_wanted)
    };
    is("HEAD", false) && is("objects", true) && is("refs", true)
}

/// The disk usage of an entry (section 8.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sizes {
    /// Bytes allocated: blocks × 512 on Unix, file lengths on Windows; a file with several
    /// names inside the entry counted once.
    pub allocated: u64,
    /// The part of `allocated` held by files with names outside the entry (a link count above
    /// the names the walk found), which deleting the entry does not free; `None` on Windows,
    /// where link counts are not read.
    pub aliased: Option<u64>,
    /// A lower bound of what deleting the entry frees.
    pub exclusive: Exclusive,
}

/// How the exclusive bytes of the entries on one file system are read (section 8.5), by its
/// type ([`Measure::of`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Measure {
    /// APFS: each counted regular file's private size.
    PrivateSize,
    /// ext2, ext3, ext4: each counted regular file's data extents.
    Extents,
    /// HFS+, tmpfs: the allocated blocks of each counted file and of every directory.
    Links,
    /// Btrfs, XFS: no bound, their shared extents being unread.
    SharingUnread,
    /// Windows and every other type: no bound.
    NotMeasured,
    /// The type could not be read: no bound, and why.
    Unreadable(String),
}

impl Measure {
    /// The measure of the file system whose type `os::mount::fs_type` read as `fs_type`.
    #[must_use]
    pub fn of(fs_type: &Result<String, String>) -> Self {
        if cfg!(windows) {
            return Self::NotMeasured;
        }
        match fs_type.as_deref() {
            Ok("apfs") => Self::PrivateSize,
            Ok("ext2/ext3/ext4") => Self::Extents,
            Ok("hfs" | "tmpfs") => Self::Links,
            Ok("btrfs" | "xfs") => Self::SharingUnread,
            Ok(_) => Self::NotMeasured,
            Err(err) => Self::Unreadable(format!("the file system type: {err}")),
        }
    }

    /// What a listing reads of each regular file for this measure.
    #[cfg(unix)]
    #[must_use]
    pub fn own(&self) -> crate::os::dir::Own {
        use crate::os::dir::Own;
        match self {
            Self::PrivateSize => Own::PrivateSize,
            Self::Extents => Own::Extents,
            _ => Own::Nothing,
        }
    }
}

/// What an entry of a walk is, for the bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counted {
    Dir,
    /// A regular file.
    File,
    /// A symbolic link or a special file.
    Other,
}

/// The lower bound of the bytes deleting an entry frees, and how it was read (section 8.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exclusive {
    /// Null under a measure that does not read, or when a read it needs failed.
    pub bytes: Option<u64>,
    pub measure: ExclusiveMeasure,
    /// The read that failed: for `read_failed`, and under a measure that does not read, a
    /// directory the walk did not list, which the observation's errors report all the same.
    pub error: Option<String>,
}

/// Adds up [`Exclusive`] over the entries of a walk: a file counts once per `dev` and `ino`,
/// and only when the walk found as many of its names as its link count; a file with a name
/// outside counts nothing (section 8.5), and neither does a file whose names showed different
/// link counts, as when a name was added or removed while the walk ran.
#[derive(Clone, Debug)]
pub struct Bound {
    measure: Measure,
    /// The bytes of the directories counted at once.
    counted: u64,
    /// (device, inode) → what the walk found of each file.
    files: std::collections::HashMap<(u64, u64), Seen>,
    /// The first read the measure needed that failed.
    failed: Option<String>,
}

impl Bound {
    #[must_use]
    pub fn new(measure: Measure) -> Self {
        Self {
            measure,
            counted: 0,
            files: std::collections::HashMap::new(),
            failed: None,
        }
    }

    /// Adds an entry: what it is, its identity and link count, its allocated bytes, and what
    /// its listing read of it ([`Measure::own`]).
    pub fn add(
        &mut self,
        kind: Counted,
        (dev, ino): (u64, u64),
        nlink: u64,
        allocated: u64,
        own: Option<u64>,
    ) {
        let bytes = match (&self.measure, kind) {
            (Measure::Links, Counted::Dir) => {
                self.counted = self.counted.saturating_add(allocated);
                return;
            }
            (Measure::PrivateSize | Measure::Extents, Counted::File) => own,
            (Measure::Links, Counted::File | Counted::Other) => Some(allocated),
            _ => return,
        };
        let seen = self.files.entry((dev, ino)).or_insert(Seen {
            nlink,
            names: 0,
            bytes,
            unsteady: false,
        });
        seen.names += 1;
        seen.unsteady |= seen.nlink != nlink;
    }

    /// Records a read the measure needed that failed.
    pub fn fail(&mut self, error: impl Into<String>) {
        self.failed.get_or_insert_with(|| error.into());
    }

    /// The bound of the entries added.
    #[must_use]
    pub fn finish(self) -> Exclusive {
        let read = match self.measure {
            Measure::PrivateSize => ExclusiveMeasure::PrivateSize,
            Measure::Extents => ExclusiveMeasure::Extents,
            Measure::Links => ExclusiveMeasure::Links,
            // Nothing is read, while a read the walk needed that failed is still reported.
            Measure::SharingUnread => {
                return Exclusive {
                    error: self.failed,
                    ..Exclusive::none(ExclusiveMeasure::SharingUnread)
                };
            }
            Measure::NotMeasured => {
                return Exclusive {
                    error: self.failed,
                    ..Exclusive::none(ExclusiveMeasure::NotMeasured)
                };
            }
            Measure::Unreadable(err) => return Exclusive::failed(err),
        };
        if let Some(err) = self.failed {
            return Exclusive::failed(err);
        }
        let mut counted = self.counted;
        let mut lacking = false;
        for seen in self.files.values() {
            if !seen.unsteady && seen.names >= seen.nlink {
                match seen.bytes {
                    Some(bytes) => counted = counted.saturating_add(bytes),
                    None => lacking = true,
                }
            }
        }
        if lacking {
            // APFS returned no private size for a counted file: the link count cannot see
            // clones, so there is no bound.
            return Exclusive::none(ExclusiveMeasure::NotMeasured);
        }
        Exclusive {
            bytes: Some(counted),
            measure: read,
            error: None,
        }
    }
}

/// What a walk found of one file, for [`Bound`].
#[derive(Clone, Debug)]
struct Seen {
    /// The link count its first name showed.
    nlink: u64,
    /// The names found.
    names: u64,
    /// The measure's bytes.
    bytes: Option<u64>,
    /// A later name showed another link count.
    unsteady: bool,
}

impl Exclusive {
    fn none(measure: ExclusiveMeasure) -> Self {
        Self {
            bytes: None,
            measure,
            error: None,
        }
    }

    fn failed(error: String) -> Self {
        Self {
            bytes: None,
            measure: ExclusiveMeasure::ReadFailed,
            error: Some(error),
        }
    }
}

/// The disk usage of `path` and everything below it, without following symbolic links and
/// without entering directories with their own `.git` entry below `path`. `None` if `path`
/// cannot be read. The bound reads `path`'s file system by its measure, and is null, with the
/// error, when a directory below `path` cannot be opened or listed or is on another device, as
/// `plan`'s walk leaves it (section 8.5); the other sizes leave out what such a directory
/// holds.
#[must_use]
pub fn sizes(path: &Path) -> Option<Sizes> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    let mut usage = Usage::default();
    usage.add(&meta);
    #[cfg(unix)]
    let exclusive = unix_sizes(path, &meta, &mut usage);
    #[cfg(not(unix))]
    let exclusive = {
        if meta.is_dir() {
            let mut stack = vec![path.to_path_buf()];
            while let Some(dir) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    // Not followed: `DirEntry::metadata` reads a symbolic link itself.
                    let Ok(meta) = entry.metadata() else { continue };
                    if meta.is_dir() {
                        let child = entry.path();
                        if std::fs::symlink_metadata(child.join(".git")).is_ok() {
                            continue;
                        }
                        stack.push(child);
                    }
                    usage.add(&meta);
                }
            }
        }
        Bound::new(Measure::NotMeasured).finish()
    };
    Some(usage.sizes(exclusive))
}

/// [`sizes`] on Unix: lists each directory through `os::dir` with what the measure of the
/// file system reads, adding every entry to `usage`; returns the bound, which a directory
/// not read, or on another device, leaves null.
#[cfg(unix)]
fn unix_sizes(path: &Path, meta: &std::fs::Metadata, usage: &mut Usage) -> Exclusive {
    use std::os::unix::fs::MetadataExt;

    use crate::os::dir;

    let root_dev = meta.dev();
    let typed = if meta.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    let fs_type = std::fs::File::open(typed)
        .map_err(|err| format!("{}: {err}", typed.display()))
        .and_then(crate::os::mount::fs_type);
    let measure = Measure::of(&fs_type);
    let own = measure.own();
    let mut bound = Bound::new(measure);
    let kind = |kind: dir::Kind| match kind {
        dir::Kind::Dir => Counted::Dir,
        dir::Kind::File => Counted::File,
        _ => Counted::Other,
    };
    if meta.is_file() {
        let read = path
            .parent()
            .zip(path.file_name())
            .ok_or_else(|| std::io::Error::other("no parent"))
            .and_then(|(parent, name)| {
                let parent = std::fs::File::open(parent)?;
                dir::own_at(&parent, name, (meta.dev(), meta.ino()), own)
            });
        let read = read.unwrap_or_else(|err| {
            bound.fail(format!("{}: {err}", path.display()));
            None
        });
        bound.add(
            Counted::File,
            (meta.dev(), meta.ino()),
            meta.nlink(),
            meta.blocks() * 512,
            read,
        );
        return bound.finish();
    }
    let root_kind = if meta.is_dir() {
        Counted::Dir
    } else {
        Counted::Other
    };
    bound.add(
        root_kind,
        (meta.dev(), meta.ino()),
        meta.nlink(),
        meta.blocks() * 512,
        None,
    );
    if !meta.is_dir() {
        return bound.finish();
    }
    // Each directory to list, and whether it is on the root's device, whose measure it
    // reads.
    let mut stack = vec![(path.to_path_buf(), true)];
    while let Some((at, on_root)) = stack.pop() {
        if !on_root {
            bound.fail(format!("{}: on another device", at.display()));
        }
        let read_own = if on_root { own } else { dir::Own::Nothing };
        let listed = match list_dir(&at, read_own) {
            Ok(listed) => listed,
            Err(err) => {
                bound.fail(format!("{}: {err}", at.display()));
                continue;
            }
        };
        for (name, err) in listed.unread {
            let failed = name.map_or_else(|| at.clone(), |name| at.join(name));
            bound.fail(format!("{}: {err}", failed.display()));
        }
        for (name, st, read) in listed.entries {
            if st.kind == dir::Kind::Dir {
                let child = at.join(&name);
                if std::fs::symlink_metadata(child.join(".git")).is_ok() {
                    continue;
                }
                stack.push((child, st.dev == root_dev));
            }
            usage.add_stat(
                st.kind == dir::Kind::Dir,
                (st.dev, st.ino),
                st.nlink,
                st.blocks.saturating_mul(512),
            );
            if st.dev == root_dev {
                bound.add(
                    kind(st.kind),
                    (st.dev, st.ino),
                    st.nlink,
                    st.blocks.saturating_mul(512),
                    read,
                );
            }
        }
    }
    bound.finish()
}

/// Lists the directory at `path` through `os::dir`, reading `own` of each regular file.
#[cfg(unix)]
fn list_dir(path: &Path, own: crate::os::dir::Own) -> std::io::Result<crate::os::dir::Listed> {
    let opened = crate::os::dir::open(path)?
        .ok_or_else(|| std::io::Error::other("replaced while it was read"))?;
    crate::os::dir::list(&opened, own, crate::os::dir::At::path(path))
}

/// Bytes allocated to `path` and everything below it ([`sizes`]).
#[must_use]
pub fn allocated_bytes(path: &Path) -> Option<u64> {
    sizes(path).map(|s| s.allocated)
}

/// What [`sizes`] has added up so far.
#[derive(Default)]
struct Usage {
    allocated: u64,
    /// (device, inode) → (link count, names found, bytes allocated), for files with several
    /// names.
    #[cfg(unix)]
    linked: std::collections::HashMap<(u64, u64), (u64, u64, u64)>,
}

impl Usage {
    #[cfg(unix)]
    fn add(&mut self, meta: &std::fs::Metadata) {
        use std::os::unix::fs::MetadataExt;
        let bytes = meta.blocks().saturating_mul(512);
        self.add_stat(meta.is_dir(), (meta.dev(), meta.ino()), meta.nlink(), bytes);
    }

    #[cfg(unix)]
    fn add_stat(&mut self, dir: bool, id: (u64, u64), nlink: u64, bytes: u64) {
        if dir || nlink <= 1 {
            self.allocated = self.allocated.saturating_add(bytes);
            return;
        }
        let slot = self.linked.entry(id).or_insert((nlink, 0, bytes));
        if slot.1 == 0 {
            self.allocated = self.allocated.saturating_add(bytes);
        }
        slot.1 += 1;
    }

    #[cfg(not(unix))]
    fn add(&mut self, meta: &std::fs::Metadata) {
        if !meta.is_dir() {
            self.allocated = self.allocated.saturating_add(meta.len());
        }
    }

    #[cfg(unix)]
    fn sizes(&self, exclusive: Exclusive) -> Sizes {
        let aliased = self
            .linked
            .values()
            .filter(|(nlink, names, _)| nlink > names)
            .map(|(_, _, bytes)| bytes)
            .sum();
        Sizes {
            allocated: self.allocated,
            aliased: Some(aliased),
            exclusive,
        }
    }

    #[cfg(not(unix))]
    fn sizes(&self, exclusive: Exclusive) -> Sizes {
        Sizes {
            allocated: self.allocated,
            aliased: None,
            exclusive,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{DataPatterns, allocated_bytes, classify};
    use crate::contract::{ExclusiveMeasure, IgnoredClass};

    fn touch(root: &Path, rel: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"data").unwrap();
    }

    fn patterns(regenerable: &[&str], protected: &[&str]) -> DataPatterns {
        let owned = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect();
        DataPatterns {
            regenerable: owned(regenerable),
            protected: owned(protected),
        }
    }

    #[test]
    fn classes_follow_the_order_of_section_8_3() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "node_modules/a/index.js");
        touch(root, "target/a/b/fuzz/corpus/seed");
        touch(root, "target2/a/proptest-regressions/x.txt");
        touch(root, "target3/debug/x");
        touch(root, "fuzz/corpus/seed");
        touch(root, "fuzz/target/x");
        touch(root, "vendor/dep/.git/HEAD");
        touch(root, "cache/bare.git/HEAD");
        touch(root, "cache/bare.git/objects/info/x");
        touch(root, "cache/bare.git/refs/heads/x");
        touch(root, "notes.log");
        touch(root, "pkg/.DS_Store");
        let data = patterns(
            &[
                "node_modules",
                "target",
                "target2",
                "target3",
                "fuzz/target",
                ".DS_Store",
                "vendor",
                "cache",
            ],
            &["fuzz/corpus", "proptest-regressions"],
        );
        let class = |rel: &str| classify(root, Path::new(rel), &data);
        assert_eq!(class("node_modules"), IgnoredClass::Regenerable);
        assert_eq!(class("pkg/.DS_Store"), IgnoredClass::Regenerable);
        assert_eq!(class("fuzz/target"), IgnoredClass::Regenerable);
        assert_eq!(class("target3"), IgnoredClass::Regenerable);
        assert_eq!(
            class("target"),
            IgnoredClass::Protected,
            "a protected path deep inside"
        );
        assert_eq!(
            class("target2"),
            IgnoredClass::Protected,
            "a protected name deep inside"
        );
        assert_eq!(
            class("fuzz"),
            IgnoredClass::Protected,
            "a protected path below"
        );
        assert_eq!(
            class("fuzz/corpus/seed"),
            IgnoredClass::Protected,
            "inside a protected path"
        );
        assert_eq!(class("vendor"), IgnoredClass::NestedRepository);
        assert_eq!(
            class("cache"),
            IgnoredClass::NestedRepository,
            "a bare repository below"
        );
        assert_eq!(class("notes.log"), IgnoredClass::Unclassifiable);
        assert_eq!(class("missing"), IgnoredClass::Unclassifiable);
    }

    #[cfg(unix)]
    #[test]
    fn walks_do_not_follow_symbolic_links() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("w");
        touch(&root, "outside/proptest-regressions/x");
        // What a walk that followed the link would count: more than the link and the
        // directory holding it take on any file system, whatever its block size and however
        // it stores a long link target.
        let big = 1 << 20;
        std::fs::write(root.join("outside/big"), vec![1u8; big]).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::os::unix::fs::symlink(root.join("outside"), root.join("target/link")).unwrap();
        let class = classify(
            &root,
            Path::new("target"),
            &patterns(&["target"], &["proptest-regressions"]),
        );
        assert_eq!(class, IgnoredClass::Regenerable);
        let linked = allocated_bytes(&root.join("target")).unwrap();
        let outside = allocated_bytes(&root.join("outside")).unwrap();
        assert!(outside >= big as u64, "{outside} >= {big}");
        assert!(linked < big as u64, "{linked} < {big}");
    }

    #[test]
    fn allocated_bytes_counts_files() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "d/a");
        touch(tmp.path(), "d/b/c");
        assert!(allocated_bytes(&tmp.path().join("d")).unwrap() >= 8);
        assert_eq!(allocated_bytes(&tmp.path().join("missing")), None);
    }

    #[cfg(unix)]
    #[test]
    fn files_with_names_outside_are_aliased_and_counted_once() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("store")).unwrap();
        std::fs::create_dir_all(root.join("entry")).unwrap();
        std::fs::write(root.join("store/shared"), vec![1u8; 64 * 1024]).unwrap();
        std::fs::hard_link(root.join("store/shared"), root.join("entry/shared")).unwrap();
        std::fs::write(root.join("entry/own"), vec![2u8; 64 * 1024]).unwrap();
        std::fs::hard_link(root.join("entry/own"), root.join("entry/own2")).unwrap();
        let sizes = super::sizes(&root.join("entry")).unwrap();
        let file = |rel: &str| {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(root.join(rel)).unwrap().blocks() * 512
        };
        assert_eq!(sizes.aliased, Some(file("store/shared")), "{sizes:?}");
        let dir = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(root.join("entry")).unwrap().blocks() * 512
        };
        assert_eq!(
            sizes.allocated,
            dir + file("store/shared") + file("entry/own"),
            "two names inside count once"
        );
    }

    /// Each file system type has its measure (section 8.5): APFS its private sizes, ext2–4
    /// their extents, HFS+ and tmpfs their blocks with the link rule; Btrfs and XFS none, as
    /// their sharing is not read, and every other type none; a type that cannot be read none,
    /// with the error.
    #[test]
    fn each_file_system_type_has_its_measure() {
        use super::Measure;
        let of = |ty: &str| Measure::of(&Ok(ty.to_owned()));
        if cfg!(windows) {
            assert_eq!(of("apfs"), Measure::NotMeasured, "Windows measures nothing");
            return;
        }
        assert_eq!(of("apfs"), Measure::PrivateSize);
        assert_eq!(of("ext2/ext3/ext4"), Measure::Extents);
        assert_eq!(of("hfs"), Measure::Links);
        assert_eq!(of("tmpfs"), Measure::Links);
        assert_eq!(of("btrfs"), Measure::SharingUnread);
        assert_eq!(of("xfs"), Measure::SharingUnread);
        for other in ["nfs", "smbfs", "fuse", "msdos", "0x9fa0"] {
            assert_eq!(of(other), Measure::NotMeasured, "{other}");
        }
        let unread = Measure::of(&Err("fstatfs: EIO".to_owned()));
        assert!(
            matches!(unread, Measure::Unreadable(ref e) if e.contains("EIO")),
            "{unread:?}"
        );
        let finished = |measure: Measure| super::Bound::new(measure).finish();
        for (measure, read) in [
            (Measure::SharingUnread, ExclusiveMeasure::SharingUnread),
            (Measure::NotMeasured, ExclusiveMeasure::NotMeasured),
            (unread, ExclusiveMeasure::ReadFailed),
        ] {
            let bound = finished(measure);
            assert_eq!((bound.bytes, bound.measure), (None, read));
        }
    }

    /// A file counts once, and only with all its names inside; a file with a name outside
    /// counts nothing. Under `private_size` and `extents` only regular files count, by their
    /// reading; under `links` every file and directory by its blocks.
    #[test]
    fn the_bound_counts_a_file_once_with_all_its_names_inside() {
        use super::{Bound, Counted, Measure};
        let add_all = |bound: &mut Bound| {
            bound.add(Counted::Dir, (1, 1), 3, 4096, None);
            bound.add(Counted::File, (1, 2), 1, 8192, Some(100));
            // Two names inside, one outside: nothing.
            bound.add(Counted::File, (1, 3), 3, 8192, Some(1000));
            bound.add(Counted::File, (1, 3), 3, 8192, Some(1000));
            // Two names, both inside: once.
            bound.add(Counted::File, (1, 4), 2, 8192, Some(10_000));
            bound.add(Counted::File, (1, 4), 2, 8192, Some(10_000));
            bound.add(Counted::Other, (1, 5), 1, 512, None);
        };
        for measure in [Measure::PrivateSize, Measure::Extents] {
            let mut bound = Bound::new(measure.clone());
            add_all(&mut bound);
            let read = bound.finish();
            assert_eq!(read.bytes, Some(10_100), "{measure:?}");
            assert_eq!(read.error, None);
        }
        let mut bound = Bound::new(Measure::Links);
        add_all(&mut bound);
        let read = bound.finish();
        assert_eq!(
            (read.bytes, read.measure),
            (Some(4096 + 8192 + 8192 + 512), ExclusiveMeasure::Links)
        );
    }

    /// A file whose names showed different link counts counts nothing: a name added inside
    /// while the walk ran would otherwise reach the count its first name showed, and count a
    /// file that keeps a name outside.
    #[test]
    fn a_file_whose_link_count_changed_counts_nothing() {
        use super::{Bound, Counted, Measure};
        let mut bound = Bound::new(Measure::Extents);
        bound.add(Counted::File, (1, 2), 1, 8192, Some(100));
        // One name inside and one outside, then a second name added inside.
        bound.add(Counted::File, (1, 3), 2, 8192, Some(1000));
        bound.add(Counted::File, (1, 3), 3, 8192, Some(1000));
        // A single name, then a second one added inside.
        bound.add(Counted::File, (1, 4), 1, 8192, Some(10_000));
        bound.add(Counted::File, (1, 4), 2, 8192, Some(10_000));
        assert_eq!(bound.finish().bytes, Some(100));
    }

    /// A counted regular file without a reading leaves no bound (`not_measured`: APFS returned
    /// no private size), unless a name outside keeps it from counting; a read that failed
    /// leaves none either (`read_failed`), with the error.
    #[test]
    fn a_missing_or_failed_reading_leaves_no_bound() {
        use super::{Bound, Counted, Measure};
        let mut bound = Bound::new(Measure::PrivateSize);
        bound.add(Counted::File, (1, 2), 1, 8192, Some(100));
        bound.add(Counted::File, (1, 3), 2, 8192, None);
        assert_eq!(bound.clone().finish().bytes, Some(100), "linked outside");
        bound.add(Counted::File, (1, 4), 1, 8192, None);
        let read = bound.clone().finish();
        assert_eq!(
            (read.bytes, read.measure),
            (None, ExclusiveMeasure::NotMeasured)
        );
        bound.fail("/w/t: getattrlistbulk: EINVAL");
        let read = bound.finish();
        assert_eq!(
            (read.bytes, read.measure),
            (None, ExclusiveMeasure::ReadFailed)
        );
        assert_eq!(read.error.as_deref(), Some("/w/t: getattrlistbulk: EINVAL"));
    }

    /// A directory below the entry that cannot be listed leaves the bound null, with the
    /// error, rather than a number that leaves out what it holds.
    #[cfg(unix)]
    #[test]
    fn a_directory_not_listed_leaves_no_bound() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let entry = tmp.path().join("entry");
        for sub in ["a", "b"] {
            std::fs::create_dir_all(entry.join(sub)).unwrap();
            let file = entry.join(sub).join("own");
            std::fs::write(&file, vec![1_u8; 64 * 1024]).unwrap();
            std::fs::File::open(&file).unwrap().sync_all().unwrap();
        }
        let mode = |mode| {
            let perms = std::fs::Permissions::from_mode(mode);
            std::fs::set_permissions(entry.join("b"), perms).unwrap();
        };
        mode(0o000);
        let listable = std::fs::read_dir(entry.join("b")).is_ok();
        let exclusive = super::sizes(&entry).unwrap().exclusive;
        mode(0o755);
        if listable {
            // Root lists past permissions, and so does the walk.
            return;
        }
        assert_eq!(exclusive.bytes, None, "{exclusive:?}");
        if !matches!(
            exclusive.measure,
            ExclusiveMeasure::SharingUnread | ExclusiveMeasure::NotMeasured
        ) {
            assert_eq!(exclusive.measure, ExclusiveMeasure::ReadFailed);
        }
        let error = exclusive.error.unwrap_or_default();
        assert!(
            error.contains(&*entry.join("b").to_string_lossy()),
            "{error}"
        );
    }

    /// Under a measure that does not read, a read that failed, such as a directory the walk
    /// did not list, leaves the measure as it is and its error in the report.
    #[test]
    fn a_failed_read_is_kept_where_nothing_is_measured() {
        use super::{Bound, Measure};
        for (measure, read) in [
            (Measure::SharingUnread, ExclusiveMeasure::SharingUnread),
            (Measure::NotMeasured, ExclusiveMeasure::NotMeasured),
        ] {
            let mut bound = Bound::new(measure);
            bound.fail("/w/t/cache: on another mount, not listed");
            let finished = bound.finish();
            assert_eq!(
                (finished.bytes, finished.measure, finished.error.as_deref()),
                (None, read, Some("/w/t/cache: on another mount, not listed"))
            );
        }
    }

    /// `inventory --sizes` reads the bound as the plans do: on APFS a file written in place
    /// counts its private size, which is its allocation, and a file linked from outside counts
    /// nothing.
    #[cfg(unix)]
    #[test]
    fn sizes_read_the_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("entry/sub")).unwrap();
        std::fs::write(root.join("outside"), vec![1_u8; 64 * 1024]).unwrap();
        std::fs::hard_link(root.join("outside"), root.join("entry/linked")).unwrap();
        let own = root.join("entry/sub/own");
        std::fs::write(&own, vec![2_u8; 64 * 1024]).unwrap();
        std::fs::File::open(&own).unwrap().sync_all().unwrap();
        let sizes = super::sizes(&root.join("entry")).unwrap();
        let blocks = |path: &Path| {
            use std::os::unix::fs::MetadataExt;
            std::fs::symlink_metadata(path).unwrap().blocks() * 512
        };
        let exclusive = sizes.exclusive;
        match exclusive.measure {
            ExclusiveMeasure::PrivateSize | ExclusiveMeasure::Extents => {
                assert_eq!(exclusive.bytes, Some(blocks(&own)), "{exclusive:?}");
            }
            ExclusiveMeasure::Links => {
                let dirs = blocks(&root.join("entry")) + blocks(&root.join("entry/sub"));
                assert_eq!(exclusive.bytes, Some(dirs + blocks(&own)), "{exclusive:?}");
            }
            _ => assert_eq!(exclusive.bytes, None, "{exclusive:?}"),
        }
    }
}
