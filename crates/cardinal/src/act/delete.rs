//! The deletion of a committed execution (`docs/architecture.md` section 9.7, "Deletion").
//!
//! The deleter follows the manifest, children before parents, `t` itself last. It opens each
//! directory one name at a time from its parent's descriptor, starting at the trash entry's,
//! as std's `remove_dir_all` opens each level (`O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC`), and
//! enters it only while it has its recorded `dev` and `ino`; a directory it cannot open, or
//! that is not the one recorded, is kept with everything below it. Unlike std, it never
//! unlinks whatever a name holds: each entry is **taken**, renamed without replacing into the
//! hold `h` under its manifest index; **checked** there, as the manifest's inode and type,
//! with the manifest's size and mtime for a file or link, taken from a directory still at its
//! manifest path (each directory from `t` down still named in its parent with its recorded
//! identity, from the trash entry's descriptor), with no repository marker in that directory
//! or above it in `t`, while the configuration last loaded matches the target with a
//! regenerable pattern and covers the entry, at that verified path, with no protected one;
//! and then **deleted** from the hold, a directory only while it is empty, or **put back**
//! under its name without replacing. An entry no longer at its name at its take is gone. A
//! directory holding a repository marker (`.git`, or `HEAD`, `objects`, and `refs` together)
//! is kept whole, and so is a directory that left its manifest path: the entry whose check
//! found either goes back, and nothing more is taken below it.
//!
//! Before each batch, when it starts and every [`BATCH`], the deleter asks its [`Guard`] to
//! scan the trash entry and to load the configuration again when one of its files changed; a
//! process found or a read that fails stops it, and what is left stays in `t`. A write it
//! needs that fails for lack of space spends the reserve and is tried once more (section 9.7,
//! "Space"); one that still fails stops it, and the claim is left to recovery. A directory that
//! stays after entries were taken from it is synced before the deletion ends, so the result
//! that follows rests on its changes (section 9.7, durable order).

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::{self, Config, DataList};
use crate::contract::{ClaimRepo, EntryPath, EntryType, ManifestEntry};
use crate::observe::Measure;
use crate::os::dir::{self, Kind, Own, Stat};
use crate::os::fds::{Scan, Way};
use crate::plan::{DataLists, Linked, descriptor_uses};
use crate::state::{self, Op};
use crate::testpoint;

/// How long the deleter goes between the scans and reloads of its [`Guard`].
pub const BATCH: Duration = Duration::from_secs(2);

/// The name of the target in its trash entry.
const T: &str = "t";

/// What the deletion's checks read beyond the manifest.
pub trait Guard {
    /// Runs before each batch: scans the trash entry and loads the configuration again when
    /// one of its files changed. An error names why the deletion stops: a process using the
    /// trash entry, or a read that is not complete.
    fn before_batch(&mut self) -> Result<(), String>;

    /// The patterns of the configuration last loaded, for the repository of the target.
    fn patterns(&self) -> &DataLists;
}

/// Why a deletion stopped before the end of its manifest.
#[derive(Debug)]
pub enum Stop {
    /// The guard found a process using the trash entry or a read that failed.
    Guard(String),
    /// A write it needs failed, after the reserve's retry where the error was lack of space.
    Write(io::Error),
    /// The trash entry or its hold could not be opened, or the manifest has no tree.
    Unreadable(String),
}

impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Guard(why) | Self::Unreadable(why) => f.write_str(why),
            Self::Write(err) => write!(f, "{err}"),
        }
    }
}

/// What became of one manifest entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    /// Not reached: the deletion stopped first.
    Left,
    /// Deleted from the hold.
    Deleted,
    /// No longer at its name when it was taken, or its directory no longer there.
    Gone,
    /// Kept: it failed its check and went back, stays in the hold, or is in a directory that
    /// was not entered, holds a repository marker, or left its manifest path.
    Kept,
}

/// What a deletion did.
#[derive(Debug)]
pub struct Deleted {
    /// The fate of each manifest entry, by its position in the manifest.
    pub fates: Vec<Fate>,
    /// What the unlinks are expected to have freed (section 9.7), `None` under a measure that
    /// reads nothing or when a reading failed.
    pub freed_bytes: Option<u64>,
    /// Why it stopped before the end of the manifest.
    pub stopped: Option<Stop>,
}

impl Deleted {
    /// The paths of the entries kept or left, relative to the target, in manifest order, the
    /// target itself left out.
    #[must_use]
    pub fn kept(&self, manifest: &[ManifestEntry]) -> Vec<String> {
        manifest
            .iter()
            .zip(&self.fates)
            .filter(|(entry, fate)| {
                matches!(fate, Fate::Kept | Fate::Left) && !rel_path(entry).as_os_str().is_empty()
            })
            .map(|(entry, _)| rel_path(entry).display().to_string())
            .collect()
    }
}

/// An entry's path relative to the target.
#[must_use]
pub fn rel_path(entry: &ManifestEntry) -> PathBuf {
    match &entry.path {
        EntryPath::Utf8(path) => PathBuf::from(path),
        EntryPath::Bytes(bytes) => PathBuf::from(OsStr::from_bytes(bytes)),
    }
}

/// The manifest as a tree: each entry's children by position, in name order.
pub(crate) struct Tree {
    pub(crate) root: usize,
    pub(crate) children: Vec<Vec<usize>>,
    pub(crate) names: Vec<OsString>,
}

impl Tree {
    /// The tree of `manifest`, which must hold the target (path `""`) once and, for every
    /// other entry, its parent directory before it.
    pub(crate) fn of(manifest: &[ManifestEntry]) -> Result<Self, String> {
        let mut at: HashMap<PathBuf, usize> = HashMap::new();
        let mut children = vec![Vec::new(); manifest.len()];
        let mut names = Vec::with_capacity(manifest.len());
        let mut root = None;
        for (pos, entry) in manifest.iter().enumerate() {
            let rel = rel_path(entry);
            if rel.as_os_str().is_empty() {
                if root.replace(pos).is_some() {
                    return Err("the manifest holds the target twice".into());
                }
                names.push(OsString::new());
            } else {
                let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else {
                    return Err(format!("the manifest entry {} has no name", rel.display()));
                };
                let parent = *at.get(parent).ok_or_else(|| {
                    format!("the manifest lists {} before its directory", rel.display())
                })?;
                if manifest[parent].kind != EntryType::Dir {
                    return Err(format!("{} is below a file", rel.display()));
                }
                children[parent].push(pos);
                names.push(name.to_os_string());
            }
            if at.insert(rel.clone(), pos).is_some() {
                return Err(format!("the manifest lists {} twice", rel.display()));
            }
        }
        let root = root.ok_or("the manifest does not hold the target")?;
        for list in &mut children {
            list.sort_by(|a, b| names[*a].cmp(&names[*b]));
        }
        Ok(Self {
            root,
            children,
            names,
        })
    }

    /// Every position at or below `pos`.
    fn subtree(&self, pos: usize) -> Vec<usize> {
        let mut out = vec![pos];
        let mut at = 0;
        while at < out.len() {
            out.extend(&self.children[out[at]]);
            at += 1;
        }
        out
    }
}

/// One deletion of a trash entry's `t` through its hold.
pub struct Deletion<'a> {
    /// The trash entry `<state>/trash/<plan>`.
    pub entry: &'a Path,
    pub manifest: &'a [ManifestEntry],
    /// The target's path relative to its worktree, which the patterns match.
    pub target_rel: &'a Path,
    /// The measure of the target's file system (section 8.5).
    pub measure: &'a Measure,
    /// The state dir's reserve file (section 9.7, "Space").
    pub reserve: &'a Path,
}

/// How a take, or the handling of a directory, ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Went {
    /// Handled; the deletion goes on with the next entry.
    On,
    /// The directory at this depth of the open chain (0 for `t`) is kept with everything
    /// below it: it left its manifest path, or it holds a repository marker. Nothing more is
    /// taken below it.
    Keep(usize),
    /// The deletion stopped.
    Stopped,
}

/// A directory of `t` the deletion has open, on the chain from `t` down.
struct Open {
    fd: File,
    pos: usize,
    /// Whether an entry was taken from it since it was last synced.
    changed: bool,
}

/// The state of a deletion under way.
struct Run<'a, 'g> {
    del: &'a Deletion<'a>,
    tree: Tree,
    guard: &'g mut dyn Guard,
    /// The trash entry, which `t` is taken from and every chain is verified from.
    entry: File,
    hold: File,
    fates: Vec<Fate>,
    freed: Option<u64>,
    own: Own,
    last_batch: Option<Instant>,
    stopped: Option<Stop>,
}

impl Deletion<'_> {
    /// Deletes what the manifest names in the trash entry's `t`, through its hold `h`.
    pub fn run(&self, guard: &mut dyn Guard) -> Deleted {
        let tree = match Tree::of(self.manifest) {
            Ok(tree) => tree,
            Err(why) => return self.stopped(Stop::Unreadable(why)),
        };
        let opened = (|| {
            let entry = dir::open(self.entry)?
                .ok_or_else(|| io::Error::other("the trash entry is not a directory"))?;
            let hold = dir::open_at(&entry, OsStr::new("h"))?
                .ok_or_else(|| io::Error::other("the hold is not a directory"))?;
            Ok::<_, io::Error>((entry, hold))
        })();
        let (entry, hold) = match opened {
            Ok(opened) => opened,
            Err(err) => {
                return self.stopped(Stop::Unreadable(format!("{}: {err}", self.entry.display())));
            }
        };
        let freed = match self.measure {
            Measure::PrivateSize | Measure::Extents | Measure::Links => Some(0),
            _ => None,
        };
        let mut run = Run {
            del: self,
            fates: vec![Fate::Left; self.manifest.len()],
            tree,
            guard,
            entry,
            hold,
            freed,
            own: self.measure.own(),
            last_batch: None,
            stopped: None,
        };
        run.go();
        Deleted {
            fates: run.fates,
            freed_bytes: run.freed,
            stopped: run.stopped,
        }
    }

    fn stopped(&self, stop: Stop) -> Deleted {
        Deleted {
            fates: vec![Fate::Left; self.manifest.len()],
            freed_bytes: None,
            stopped: Some(stop),
        }
    }
}

impl Run<'_, '_> {
    fn go(&mut self) {
        if !self.batch() {
            return;
        }
        let root = self.tree.root;
        let mut chain = Vec::new();
        if self.del.manifest[root].kind == EntryType::Dir {
            self.dir(&mut chain, root, OsStr::new(T));
        } else {
            self.take(&mut chain, root, OsStr::new(T));
        }
    }

    /// Whether the open directory `fd` is the manifest entry at `pos`.
    fn is(&self, fd: &File, pos: usize) -> bool {
        let entry = &self.del.manifest[pos];
        dir::stat_fd(fd).is_ok_and(|st| (st.dev, st.ino) == (entry.dev, entry.ino))
    }

    /// The directory the entries are taken from: the last of `chain`, or the trash entry.
    fn from<'c>(&'c self, chain: &'c [Open]) -> &'c File {
        chain.last().map_or(&self.entry, |open| &open.fd)
    }

    /// Handles the directory at `pos`, named `name` in the last directory of `chain`: enters
    /// it with its recorded identity, handles its children, and then takes it, which deletes
    /// it once it is empty. A directory it cannot enter is kept with everything below it.
    fn dir(&mut self, chain: &mut Vec<Open>, pos: usize, name: &OsStr) -> Went {
        let fd = match dir::open_at(self.from(chain), name) {
            Ok(Some(fd)) if self.is(&fd, pos) => fd,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                self.gone_all(pos);
                return Went::On;
            }
            // Not the directory recorded, or one it cannot open or list: not entered.
            Ok(_) | Err(_) => {
                self.keep_all(pos);
                return Went::On;
            }
        };
        let depth = chain.len();
        let marked = has_marker(&fd);
        chain.push(Open {
            fd,
            pos,
            changed: false,
        });
        let went = if marked {
            Went::Keep(depth)
        } else {
            self.children(chain)
        };
        let open = chain.pop().expect("pushed above");
        let went = match went {
            Went::On => self.take(chain, pos, name),
            Went::Keep(at) if at == depth => {
                self.keep_all(pos);
                Went::On
            }
            other => other,
        };
        if open.changed && went != Went::Stopped && self.fates[pos] != Fate::Deleted {
            let synced = open.fd.sync_all();
            if let Err(err) = state::logged(Op::Sync, &Self::describe(&open.fd), None, synced) {
                self.stop(Stop::Write(err));
                return Went::Stopped;
            }
        }
        went
    }

    /// Handles the children of the last directory of `chain`, in name order.
    fn children(&mut self, chain: &mut Vec<Open>) -> Went {
        let pos = chain.last().expect("a directory is open").pos;
        for child in self.tree.children[pos].clone() {
            if self.stopped.is_some() {
                return Went::Stopped;
            }
            let name = self.tree.names[child].clone();
            let went = if self.del.manifest[child].kind == EntryType::Dir {
                self.dir(chain, child, &name)
            } else {
                self.take(chain, child, &name)
            };
            if went != Went::On {
                return went;
            }
        }
        Went::On
    }

    /// Takes the entry at `pos`, named `name` in the last directory of `chain`, checks it in
    /// the hold, and deletes it there or puts it back.
    fn take(&mut self, chain: &mut [Open], pos: usize, name: &OsStr) -> Went {
        if !self.batch() {
            return Went::Stopped;
        }
        let index = OsString::from(self.del.manifest[pos].i.to_string());
        let from = self.from(chain);
        let taken = state::retry_with_reserve(self.del.reserve, || {
            let renamed = dir::rename_noreplace(from, name, &self.hold, &index);
            let source = Self::describe(from).join(name);
            state::logged(Op::Rename, &source, Some(&self.hold_path(&index)), renamed)
        });
        match taken {
            Ok(()) => {
                if let Some(open) = chain.last_mut() {
                    open.changed = true;
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                self.fates[pos] = Fate::Gone;
                return Went::On;
            }
            Err(err) if state::is_no_space(&err) => {
                self.stop(Stop::Write(err));
                return Went::Stopped;
            }
            Err(_) => {
                self.fates[pos] = Fate::Kept;
                return Went::On;
            }
        }
        if let Err(err) = testpoint::hit(testpoint::ACT_TAKEN) {
            self.stop(Stop::Guard(err.to_string()));
            return Went::Stopped;
        }
        match self.check(chain, pos, &index) {
            Check::Pass(st) => {
                if let Err(err) = testpoint::hit(testpoint::ACT_HELD) {
                    self.stop(Stop::Guard(err.to_string()));
                    return Went::Stopped;
                }
                let reading = self.reading(&index, &st);
                let directory = st.kind == Kind::Dir;
                let removed = dir::remove_at(&self.hold, &index, directory);
                let op = if directory { Op::Rmdir } else { Op::Unlink };
                match state::logged(op, &self.hold_path(&index), None, removed) {
                    Ok(()) => {
                        self.fates[pos] = Fate::Deleted;
                        self.freed = match (self.freed, reading) {
                            (Some(sum), Some(bytes)) => Some(sum.saturating_add(bytes)),
                            _ => None,
                        };
                        Went::On
                    }
                    Err(_) => self.put_back(chain, pos, name, &index, Went::On),
                }
            }
            Check::Fail => self.put_back(chain, pos, name, &index, Went::On),
            Check::Keep(depth) => self.put_back(chain, pos, name, &index, Went::Keep(depth)),
        }
    }

    /// Checks the entry at `pos`, now in the hold as `index`, taken from the last directory
    /// of `chain`.
    fn check(&self, chain: &[Open], pos: usize, index: &OsStr) -> Check {
        let entry = &self.del.manifest[pos];
        let Ok(st) = dir::stat_at(&self.hold, index) else {
            return Check::Fail;
        };
        let kind = match st.kind {
            Kind::Dir => Some(EntryType::Dir),
            Kind::File => Some(EntryType::File),
            Kind::Symlink => Some(EntryType::Symlink),
            _ => None,
        };
        let same = kind == Some(entry.kind)
            && (st.dev, st.ino) == (entry.dev, entry.ino)
            && (entry.kind == EntryType::Dir
                || (st.size == entry.size && st.mtime_ns == i128::from(entry.mtime_ns)));
        if !same {
            return Check::Fail;
        }
        if let Some(depth) = self.displaced(chain) {
            return Check::Keep(depth);
        }
        if let Some(depth) = chain.iter().position(|open| has_marker(&open.fd)) {
            return Check::Keep(depth);
        }
        // The chain verified, the entry's manifest path is where it was taken from.
        let patterns = self.guard.patterns();
        let target = self.del.target_rel;
        if !patterns.covers(DataList::Regenerable, target)
            || patterns.covers(DataList::Protected, &target.join(rel_path(entry)))
        {
            return Check::Fail;
        }
        Check::Pass(st)
    }

    /// The depth of the first directory of `chain` that is no longer at its manifest path:
    /// not named in its parent (the trash entry for `t`) as the directory with its recorded
    /// `dev` and `ino`. A read that fails counts as not there.
    fn displaced(&self, chain: &[Open]) -> Option<usize> {
        chain.iter().enumerate().position(|(depth, open)| {
            let (parent, name) = match depth.checked_sub(1) {
                Some(up) => (&chain[up].fd, self.tree.names[open.pos].as_os_str()),
                None => (&self.entry, OsStr::new(T)),
            };
            let recorded = &self.del.manifest[open.pos];
            !dir::stat_at(parent, name).is_ok_and(|st| {
                st.kind == Kind::Dir && (st.dev, st.ino) == (recorded.dev, recorded.ino)
            })
        })
    }

    /// What the measure reads, in the hold, for the entry about to be unlinked (section 9.7):
    /// a file whose last link this removes, by the measure of section 8.5, and under `links`
    /// a directory's blocks too. `None` when the reading fails or the measure reads nothing.
    fn reading(&self, index: &OsStr, st: &Stat) -> Option<u64> {
        let last = st.kind != Kind::Dir && st.nlink <= 1;
        match self.del.measure {
            Measure::PrivateSize | Measure::Extents => {
                if st.kind == Kind::File && last {
                    dir::own_at(&self.hold, index, (st.dev, st.ino), self.own)
                        .ok()
                        .flatten()
                } else {
                    Some(0)
                }
            }
            Measure::Links => Some(if st.kind == Kind::Dir || last {
                st.blocks.saturating_mul(512)
            } else {
                0
            }),
            _ => None,
        }
    }

    /// Puts the entry at `pos`, in the hold as `index`, back as `name` into the last directory
    /// of `chain`, wherever that directory is now; a name taken meanwhile leaves it in the
    /// hold. The directories the put-back changed are synced, so the result that follows rests
    /// on them (section 9.7, durable order). Returns `went` unless it stops.
    fn put_back(
        &mut self,
        chain: &mut [Open],
        pos: usize,
        name: &OsStr,
        index: &OsStr,
        went: Went,
    ) -> Went {
        self.fates[pos] = Fate::Kept;
        let into = self.from(chain);
        let back = state::retry_with_reserve(self.del.reserve, || {
            testpoint::fake_write("put_back")?;
            let renamed = dir::rename_noreplace(&self.hold, index, into, name);
            let to = Self::describe(into).join(name);
            state::logged(Op::Rename, &self.hold_path(index), Some(&to), renamed)
        });
        match back {
            Ok(()) => {
                let synced = state::logged(Op::Sync, &Self::describe(into), None, into.sync_all())
                    .and_then(|()| {
                        let hold = self.hold_path(OsStr::new(""));
                        state::logged(Op::Sync, &hold, None, self.hold.sync_all())
                    });
                if let Err(err) = synced {
                    self.stop(Stop::Write(err));
                    return Went::Stopped;
                }
                if let Some(open) = chain.last_mut() {
                    open.changed = false;
                }
            }
            Err(err) if state::is_no_space(&err) => {
                self.stop(Stop::Write(err));
                return Went::Stopped;
            }
            // The name is taken, or the rename failed otherwise: the entry stays in the
            // hold under its index, which the manifest maps to its path.
            Err(_) => {}
        }
        went
    }

    /// Runs the guard when a batch is due: when the deletion starts and every [`BATCH`].
    fn batch(&mut self) -> bool {
        if self.stopped.is_some() {
            return false;
        }
        let now = Instant::now() + testpoint::fake_clock_ahead();
        if self
            .last_batch
            .is_some_and(|last| now.saturating_duration_since(last) < BATCH)
        {
            return true;
        }
        self.last_batch = Some(now);
        match self.guard.before_batch() {
            Ok(()) => true,
            Err(why) => {
                self.stop(Stop::Guard(why));
                false
            }
        }
    }

    fn stop(&mut self, stop: Stop) {
        self.stopped.get_or_insert(stop);
    }

    /// Marks every entry at or below `pos` that is not handled yet kept.
    fn keep_all(&mut self, pos: usize) {
        for at in self.tree.subtree(pos) {
            if self.fates[at] == Fate::Left {
                self.fates[at] = Fate::Kept;
            }
        }
    }

    /// Marks every entry at or below `pos` gone.
    fn gone_all(&mut self, pos: usize) {
        for at in self.tree.subtree(pos) {
            self.fates[at] = Fate::Gone;
        }
    }

    /// The path of the open directory `fd`, for the write log; empty when nothing is logged.
    fn describe(fd: &File) -> PathBuf {
        if state::logging() {
            dir::current_path(fd).unwrap_or_default()
        } else {
            PathBuf::new()
        }
    }

    fn hold_path(&self, index: &OsStr) -> PathBuf {
        let hold = self.del.entry.join("h");
        if index.is_empty() {
            hold
        } else {
            hold.join(index)
        }
    }
}

/// What an entry's check found.
enum Check {
    Pass(Stat),
    Fail,
    /// The directory at this depth of the chain left its manifest path or holds a repository
    /// marker: the entry goes back, and that directory is kept.
    Keep(usize),
}

/// Whether the open directory `fd` holds a repository marker: a `.git` entry, or `HEAD`,
/// `objects`, and `refs` together (section 9.2's `no_nested_repository`). A name that cannot
/// be read counts as present.
fn has_marker(fd: &File) -> bool {
    let present = |name: &str| match dir::stat_at(fd, OsStr::new(name)) {
        Ok(_) => true,
        Err(err) => err.kind() != io::ErrorKind::NotFound,
    };
    present(".git") || (present("HEAD") && present("objects") && present("refs"))
}

/// A [`Guard`] over the user's processes and a configuration directory: it scans the trash
/// entry with the descriptor scan (section 9.7) and loads the configuration again whenever
/// one of its files changed, selecting the repository's profiles from what the claim records.
pub struct Watch {
    config_dir: PathBuf,
    repo: ClaimRepo,
    target_rel: PathBuf,
    /// The trash entry.
    entry: PathBuf,
    /// What the descriptor-use decision reads of the manifest's entries, by `dev` and `ino`.
    ids: HashMap<(u64, u64), Linked>,
    signature: Option<Vec<Signed>>,
    config: Option<Config>,
    patterns: DataLists,
    /// The processes the scans could not read, by PID, each with its first error.
    unscanned: BTreeMap<u32, String>,
}

/// What a configuration file's signature holds: its path and what `stat` reports of it,
/// `None` when it is missing.
type Signed = (PathBuf, Option<(u64, u64, u64, i64, i64, i64, i64)>);

impl Watch {
    /// A guard for the trash entry `entry` holding the target of `manifest`.
    #[must_use]
    pub fn new(
        config_dir: &Path,
        repo: &ClaimRepo,
        target_rel: &Path,
        entry: &Path,
        manifest: &[ManifestEntry],
    ) -> Self {
        let mut ids: HashMap<(u64, u64), Linked> = HashMap::new();
        for e in manifest {
            ids.entry((e.dev, e.ino))
                .and_modify(|linked| linked.names += 1)
                .or_insert(Linked {
                    names: 1,
                    nlink: e.nlink,
                    dir: e.kind == EntryType::Dir,
                });
        }
        Self {
            config_dir: config_dir.to_path_buf(),
            repo: repo.clone(),
            target_rel: target_rel.to_path_buf(),
            entry: entry.to_path_buf(),
            ids,
            signature: None,
            config: None,
            patterns: DataLists::default(),
            unscanned: BTreeMap::new(),
        }
    }

    /// The configuration last loaded, once one has.
    #[must_use]
    pub fn config(&self) -> Option<&Config> {
        self.config.as_ref()
    }

    /// The processes the scans so far could not read, in PID order, each with its first
    /// error: listed in the result, and not counted (section 9.7).
    #[must_use]
    pub fn unscanned(&self) -> Vec<(u32, String)> {
        self.unscanned
            .iter()
            .map(|(pid, why)| (*pid, why.clone()))
            .collect()
    }

    /// Loads the configuration the first time and whenever a file it reads changed. A
    /// signature that cannot be read, or a load that fails, is an error.
    pub fn reload(&mut self) -> Result<(), String> {
        let signature = self
            .signature()
            .map_err(|err| format!("the configuration's files: {err}"))?;
        if self.config.is_some() && self.signature.as_ref() == Some(&signature) {
            return Ok(());
        }
        self.signature = None;
        let config = Config::load(&self.config_dir)
            .map_err(|err| format!("the configuration does not load: {err}"))?;
        let common_dir = Path::new(&self.repo.common_dir);
        let remote = self.repo.remote.as_deref();
        self.patterns = DataLists::from_selection(&config.select(|profile| {
            profile.matches_common_dir(common_dir)
                || remote.is_some_and(|url| profile.matches_remote(url))
        }));
        self.config = Some(config);
        self.signature = Some(signature);
        if !self
            .patterns
            .covers(DataList::Regenerable, &self.target_rel)
        {
            return Err(format!(
                "no regenerable pattern of the configuration matches {}",
                self.target_rel.display()
            ));
        }
        Ok(())
    }

    /// What `stat` reports of each file [`Config::load`] reads, in order: `host.toml`, the
    /// `repos/` directory, and each entry of it named as a profile ([`config::names_profile`]),
    /// each through a final symbolic link as the load reads it. A missing `host.toml` or
    /// `repos/` is part of the signature, as the load reads them as empty; any other error
    /// is the signature's.
    fn signature(&self) -> io::Result<Vec<Signed>> {
        use std::os::unix::fs::MetadataExt;
        let read = |path: &Path| {
            std::fs::metadata(path).map(|m| {
                (
                    m.dev(),
                    m.ino(),
                    m.size(),
                    m.mtime(),
                    m.mtime_nsec(),
                    m.ctime(),
                    m.ctime_nsec(),
                )
            })
        };
        let optional = |path: &Path| match read(path) {
            Ok(stat) => Ok(Some(stat)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        };
        let host = self.config_dir.join("host.toml");
        let repos = self.config_dir.join("repos");
        let mut signed = vec![
            (host.clone(), optional(&host)?),
            (repos.clone(), optional(&repos)?),
        ];
        match std::fs::read_dir(&repos) {
            Ok(entries) => {
                let mut files = Vec::new();
                for entry in entries {
                    let path = entry?.path();
                    if config::names_profile(&path) {
                        files.push(path);
                    }
                }
                files.sort();
                for file in files {
                    let stat = read(&file)?;
                    signed.push((file, Some(stat)));
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        Ok(signed)
    }

    /// The uses of the trash entry that `scan` found (section 9.7), each described for the
    /// reason the deletion stops; the processes it could not read are added to
    /// [`Watch::unscanned`].
    fn users(&mut self, scan: &Scan) -> Result<Vec<String>, String> {
        for (pid, why) in &scan.unscanned {
            self.unscanned.entry(*pid).or_insert_with(|| why.clone());
        }
        let uses = descriptor_uses(scan, &self.ids, &self.entry)?;
        Ok(uses
            .into_iter()
            .map(|(pid, used)| {
                let way = match used.way {
                    Way::WorkingDirectory => "working directory".to_owned(),
                    Way::RootDirectory => "root directory".to_owned(),
                    Way::Executable => "executable".to_owned(),
                    Way::Descriptor(fd) => format!("descriptor {fd}"),
                };
                let path = used
                    .path
                    .as_deref()
                    .map_or_else(String::new, |p| format!(" on {}", p.display()));
                format!("process {pid} ({way}{path})")
            })
            .collect())
    }
}

impl Guard for Watch {
    fn before_batch(&mut self) -> Result<(), String> {
        self.reload()?;
        let scan = crate::os::fds::scan().map_err(|err| format!("the descriptor scan: {err}"))?;
        let users = self.users(&scan)?;
        if users.is_empty() {
            Ok(())
        } else {
            Err(format!("in use: {}", users.join(", ")))
        }
    }

    fn patterns(&self) -> &DataLists {
        &self.patterns
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Tree, Watch, rel_path};
    use crate::contract::{ClaimRepo, EntryPath, EntryType, ManifestEntry};
    use crate::os::fds::Scan;

    fn entry(i: u64, path: &str, kind: EntryType) -> ManifestEntry {
        ManifestEntry {
            i,
            path: EntryPath::Utf8(path.to_owned()),
            dev: 1,
            ino: i + 10,
            kind,
            size: 0,
            mtime_ns: 0,
            nlink: 1,
        }
    }

    /// The manifest's tree lists children in name order, and a manifest without the target,
    /// with an entry before its directory, below a file, or listed twice has no tree.
    #[test]
    fn the_manifest_makes_a_tree() {
        let dir = EntryType::Dir;
        let file = EntryType::File;
        let manifest = [
            entry(0, "", dir),
            entry(1, "b", dir),
            entry(2, "a", file),
            entry(3, "b/z", file),
            entry(4, "b/y", file),
        ];
        let tree = Tree::of(&manifest).unwrap();
        assert_eq!(tree.root, 0);
        assert_eq!(tree.children[0], [2, 1]);
        assert_eq!(tree.children[1], [4, 3]);
        assert_eq!(tree.subtree(1), [1, 4, 3]);
        assert_eq!(rel_path(&manifest[3]).to_str(), Some("b/z"));

        for (bad, what) in [
            (vec![entry(1, "a", file)], "no target"),
            (
                vec![entry(0, "", dir), entry(1, "a/b", file)],
                "before its directory",
            ),
            (
                vec![
                    entry(0, "", dir),
                    entry(1, "a", file),
                    entry(2, "a/b", file),
                ],
                "below a file",
            ),
            (
                vec![entry(0, "", dir), entry(1, "a", file), entry(2, "a", file)],
                "twice",
            ),
            (
                vec![entry(0, "", dir), entry(1, "", dir)],
                "the target twice",
            ),
        ] {
            assert!(Tree::of(&bad).is_err(), "{what}");
        }
    }

    /// The processes each scan could not read are kept across scans, each with its first
    /// error, for the result to list.
    #[test]
    fn the_unscanned_processes_are_kept_across_scans() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = ClaimRepo {
            common_dir: String::new(),
            remote: None,
        };
        let manifest = [entry(0, "", EntryType::Dir)];
        let mut watch = Watch::new(tmp.path(), &repo, Path::new("t"), tmp.path(), &manifest);
        for unscanned in [
            vec![(7, "EPERM"), (3, "EACCES")],
            vec![(7, "ESRCH"), (9, "EIO")],
        ] {
            let scan = Scan {
                unscanned: unscanned
                    .into_iter()
                    .map(|(pid, why)| (pid, why.to_owned()))
                    .collect(),
                ..Scan::default()
            };
            assert_eq!(watch.users(&scan).unwrap(), Vec::<String>::new());
        }
        assert_eq!(
            watch.unscanned(),
            [
                (3, "EACCES".to_owned()),
                (7, "EPERM".to_owned()),
                (9, "EIO".to_owned())
            ]
        );
    }
}
