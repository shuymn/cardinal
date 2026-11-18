//! Ignored-data classification and disk usage (`docs/architecture.md` sections 8.3 and 8.5).
//!
//! Walks never follow symbolic links and never cross into a directory with its own `.git`
//! entry. A walk that cannot read part of an entry cannot prove it holds nothing protected,
//! so the entry is `unclassifiable`.

use std::path::{Path, PathBuf};

use crate::config::{DataList, data_patterns_cover};
use crate::contract::IgnoredClass;

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sizes {
    /// Bytes allocated: blocks × 512 on Unix, file lengths on Windows; a file with several
    /// names inside the entry counted once.
    pub allocated: u64,
    /// The part of `allocated` held by files with names outside the entry (a link count above
    /// the names the walk found), which deleting the entry does not free; `None` on Windows,
    /// where link counts are not read.
    pub aliased: Option<u64>,
}

/// The disk usage of `path` and everything below it, without following symbolic links and
/// without entering directories with their own `.git` entry below `path`. `None` if `path`
/// cannot be read.
#[must_use]
pub fn sizes(path: &Path) -> Option<Sizes> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    let mut usage = Usage::default();
    usage.add(&meta);
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
    Some(usage.sizes())
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
        if meta.is_dir() || meta.nlink() <= 1 {
            self.allocated = self.allocated.saturating_add(bytes);
            return;
        }
        let slot = self
            .linked
            .entry((meta.dev(), meta.ino()))
            .or_insert((meta.nlink(), 0, bytes));
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
    fn sizes(&self) -> Sizes {
        let aliased = self
            .linked
            .values()
            .filter(|(nlink, names, _)| nlink > names)
            .map(|(_, _, bytes)| bytes)
            .sum();
        Sizes {
            allocated: self.allocated,
            aliased: Some(aliased),
        }
    }

    #[cfg(not(unix))]
    fn sizes(&self) -> Sizes {
        Sizes {
            allocated: self.allocated,
            aliased: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{DataPatterns, allocated_bytes, classify};
    use crate::contract::IgnoredClass;

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
}
