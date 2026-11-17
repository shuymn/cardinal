//! Walking a target without following symbolic links (`docs/architecture.md` sections 9.2
//! and 9.5). `plan` walks by path; `act` takes its snapshot with directory descriptors and
//! produces the same [`Entry`] values, so both evaluate the same checks.

use std::collections::HashSet;
use std::fs::Metadata;
use std::io;
use std::path::{Path, PathBuf};

use crate::contract::RootId;

/// The type of an entry, as `lstat` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// One entry of a target: its identity and the metadata a deletion compares.
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
}

impl Entry {
    /// Whether `other` is this entry: same device, inode, and type.
    #[must_use]
    pub fn same_identity(&self, other: &Entry) -> bool {
        self.dev == other.dev && self.ino == other.ino && self.kind == other.kind
    }

    /// Whether `other` is this entry, unchanged: same identity, size, and mtime.
    #[must_use]
    pub fn unchanged(&self, other: &Entry) -> bool {
        self.same_identity(other) && self.size == other.size && self.mtime_ns == other.mtime_ns
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
            }
        }
    }
}

/// Whether a target is on the mount of the directory that contains it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mount {
    /// The same device and mount as its parent.
    Same,
    /// A mount point (a bind mount on the same device included), or on another device.
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
    /// or mount than the root is listed but not entered; nothing below a root that is not on
    /// its parent's mount is read.
    pub entries: Vec<Entry>,
}

impl Walk {
    /// The root, then every entry below it.
    pub fn all(&self) -> impl Iterator<Item = &Entry> {
        std::iter::once(&self.root).chain(&self.entries)
    }

    /// The bytes allocated by the root and everything below it.
    #[must_use]
    pub fn allocated(&self) -> u64 {
        self.all().map(|e| e.allocated).sum()
    }
}

/// Walks `target`, which is `worktree` or a path inside it, by path without following
/// symbolic links or entering another device or mount. Plans use it; `act` uses its own
/// descriptor-based snapshot on Unix and this walk on Windows, where it only dry-runs.
/// Mounts are read from `/proc/self/mountinfo` on Linux; elsewhere a mount point is told by
/// its device, which is what `plan` reports and `act` checks again with `statfs`.
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
    let mount_points = mount_points();
    let mount = match &mount_points {
        _ if root.dev != parent_dev => Mount::Boundary,
        Ok(points) if points.contains(target) => Mount::Boundary,
        Ok(_) => Mount::Same,
        Err(err) => Mount::Unknown(err.clone()),
    };
    let mut entries = Vec::new();
    // A target that is not on its parent's mount is refused, so nothing below it is read.
    if let (EntryKind::Dir, None, Mount::Same, Ok(points)) =
        (root.kind, &link_in_path, &mount, &mount_points)
    {
        walk_dir(target, &root.rel, root.dev, points, &mut entries)?;
    }
    Ok(Walk {
        root,
        mount,
        link_in_path,
        entries,
    })
}

/// The mount points of this process, as absolute paths: from `/proc/self/mountinfo` on
/// Linux, none elsewhere.
#[cfg_attr(
    not(target_os = "linux"),
    expect(
        clippy::unnecessary_wraps,
        reason = "only Linux can fail to read its mounts"
    )
)]
fn mount_points() -> Result<HashSet<PathBuf>, String> {
    #[cfg(target_os = "linux")]
    {
        let path = "/proc/self/mountinfo";
        std::fs::read_to_string(path)
            .map(|text| parse_mountinfo(&text))
            .map_err(|err| format!("{path}: {err}"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(HashSet::new())
    }
}

/// The mount points listed in `mountinfo` text (the fifth field, octal escapes decoded).
#[cfg_attr(
    not(any(target_os = "linux", test)),
    expect(dead_code, reason = "only Linux reads mountinfo")
)]
fn parse_mountinfo(text: &str) -> HashSet<PathBuf> {
    text.lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(|field| {
            let bytes = field.as_bytes();
            let mut out = Vec::with_capacity(bytes.len());
            let mut i = 0;
            while i < bytes.len() {
                let escape = bytes.get(i + 1..i + 4).filter(|_| bytes[i] == b'\\');
                let octal = |d: &[u8]| u8::from_str_radix(std::str::from_utf8(d).ok()?, 8).ok();
                if let Some(byte) = escape.and_then(octal) {
                    out.push(byte);
                    i += 4;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            PathBuf::from(String::from_utf8_lossy(&out).into_owned())
        })
        .collect()
}

fn walk_dir(
    dir: &Path,
    rel: &Path,
    dev: u64,
    mount_points: &HashSet<PathBuf>,
    out: &mut Vec<Entry>,
) -> io::Result<()> {
    let mut names: Vec<_> = std::fs::read_dir(dir)?
        .map(|entry| entry.map(|e| e.file_name()))
        .collect::<io::Result<_>>()?;
    names.sort();
    for name in names {
        let path = dir.join(&name);
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            // Removed while walking: nothing to record.
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        let entry = Entry::from_metadata(rel.join(&name), &meta);
        let descend =
            entry.kind == EntryKind::Dir && entry.dev == dev && !mount_points.contains(&path);
        let child_rel = entry.rel.clone();
        out.push(entry);
        if descend {
            walk_dir(&path, &child_rel, dev, mount_points, out)?;
        }
    }
    Ok(())
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
    fn mountinfo_lists_mount_points() {
        let text = "22 1 0:21 / /proc rw,nosuid - proc proc rw\n\
                    95 29 8:1 /home/u/data /work/a\\040b/target rw - ext4 /dev/sda1 rw\n";
        let points = super::parse_mountinfo(text);
        assert!(points.contains(std::path::Path::new("/proc")));
        assert!(
            points.contains(std::path::Path::new("/work/a b/target")),
            "{points:?}"
        );
        assert_eq!(points.len(), 2);
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
