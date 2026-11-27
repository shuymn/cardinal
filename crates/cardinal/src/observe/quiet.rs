//! The quiet period (`docs/architecture.md` section 9.8): whether any activity signal of a
//! worktree changed within `[sessions] quiet_s`, read from the worktree and its git dir.
//!
//! The signals read here are the git dir's `index`, `HEAD`, and `logs/HEAD` with the time of
//! its last entry, a linked worktree's `<git_dir>/gitdir`, each path `git status` listed, and
//! each ignored entry the observation listed: its own mtime and its top-level entries' for an
//! entry that is not a regenerable target, and only its top-level entries' for a regenerable
//! target, whose own times `act`'s move and restore change. Every entry below the regenerable
//! targets is read by `plan`'s walk of each target into a [`Newest`] (`plan`'s checks), which
//! `act` reads the same way. The worktree root's mtime and the directories above a target are
//! never read. Paths are read as git spelled them, never through a lossy string.
//!
//! Absence is never idleness: a read that fails other than with `ENOENT`, and a cap passed
//! without a recent signal, leave the worktree `unknown`, never quiet. An mtime in the future
//! counts as now.

use std::collections::{BTreeMap, HashSet};
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::contract::{IgnoredClass, IgnoredEntry, StatusPath, Worktree};

/// The most paths `git status` lists that are read; more with none of them recent is unknown.
pub const MAX_STATUS_PATHS: usize = 1000;
/// The most entries read directly in one ignored entry; more is unknown.
pub const MAX_ENTRIES_PER_IGNORED: usize = 4096;
/// The most entries read directly in the ignored entries of one worktree; more is unknown.
pub const MAX_ENTRIES_PER_WORKTREE: usize = 65_536;
/// The tail of `logs/HEAD` read for its last entry; a last line longer than this is unknown.
const REFLOG_TAIL: u64 = 64 * 1024;
/// Why a read with a deadline stopped there: unknown.
pub const LATE: &str = "the last read did not finish in time";

/// Whether `deadline` has passed; never without one.
#[must_use]
pub fn past(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|by| Instant::now() >= by)
}

/// The time a quiet period is judged at, and its length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub now_ms: u64,
    pub quiet_ms: u64,
}

impl Window {
    /// Whether a signal written at `at_ms` is newer than the quiet period: within `quiet_ms`
    /// of `now_ms`, or in the future, which counts as now.
    #[must_use]
    pub fn recent(self, at_ms: u64) -> bool {
        at_ms.min(self.now_ms).saturating_add(self.quiet_ms) > self.now_ms
    }
}

/// What the signals show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Quiet {
    /// Every signal read whole is older than the quiet period.
    Quiet,
    /// A signal is newer than the quiet period, named.
    Active(String),
    /// No signal is recent, and one could not be read whole, named.
    Unknown(String),
}

impl Quiet {
    /// `self` with `other`, a further reading: activity first, then the first unknown.
    #[must_use]
    pub fn and(self, other: Self) -> Self {
        match (self, other) {
            (active @ Self::Active(_), _) | (_, active @ Self::Active(_)) => active,
            (unknown @ Self::Unknown(_), _) | (_, unknown @ Self::Unknown(_)) => unknown,
            (Self::Quiet, Self::Quiet) => Self::Quiet,
        }
    }
}

/// The newest mtime among the entries below a worktree's regenerable targets, never a
/// target's own (section 9.8), kept so that a later read can judge it again under another
/// window; an entry removed while they were read, which is activity at any window; and why a
/// part of them could not be read, if one could not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Newest {
    /// The newest mtime read, in milliseconds, and its entry.
    pub at: Option<(u64, PathBuf)>,
    /// The first entry removed while the entries were read.
    pub vanished: Option<PathBuf>,
    /// Why the entries were not all read, the first reason.
    pub unknown: Option<String>,
}

impl Newest {
    /// Adds the entry at `path`, written at `at_ms`.
    pub fn add(&mut self, at_ms: u64, path: &Path) {
        if self.at.as_ref().is_none_or(|(newest, _)| at_ms > *newest) {
            self.at = Some((at_ms, path.to_path_buf()));
        }
    }

    /// Records that the entry at `path` was removed while the entries were read.
    pub fn vanish(&mut self, path: &Path) {
        self.vanished.get_or_insert_with(|| path.to_path_buf());
    }

    /// Records that a part could not be read, `why`.
    pub fn fail(&mut self, why: impl Into<String>) {
        self.unknown.get_or_insert_with(|| why.into());
    }

    /// `self` with `other`, a further reading.
    #[must_use]
    pub fn merge(mut self, other: &Self) -> Self {
        if let Some((at_ms, path)) = &other.at {
            self.add(*at_ms, path);
        }
        if let Some(path) = &other.vanished {
            self.vanish(path);
        }
        if let Some(why) = &other.unknown {
            self.fail(why.clone());
        }
        self
    }

    /// What the reading shows at `window`: activity when the newest entry is recent or an
    /// entry was removed during the read, else unknown when a part was not read, else quiet.
    #[must_use]
    pub fn judge(&self, window: Window) -> Quiet {
        match (&self.at, &self.vanished, &self.unknown) {
            (Some((at_ms, path)), _, _) if window.recent(*at_ms) => {
                Quiet::Active(format!("{}, changed at {at_ms} ms", path.display()))
            }
            (_, Some(path), _) => {
                Quiet::Active(format!("{}, removed while it was read", path.display()))
            }
            (_, _, Some(why)) => Quiet::Unknown(why.clone()),
            _ => Quiet::Quiet,
        }
    }
}

/// The paths of two `git status` listings as one, each path once, a tracked deletion in
/// either kept (section 9.8, step 8).
#[must_use]
pub fn union_status(first: &[StatusPath], second: &[StatusPath]) -> Vec<StatusPath> {
    let mut paths: BTreeMap<&Path, bool> = BTreeMap::new();
    for listed in first.iter().chain(second) {
        *paths.entry(listed.path.as_path()).or_default() |= listed.deleted;
    }
    paths
        .into_iter()
        .map(|(path, deleted)| StatusPath {
            path: path.to_path_buf(),
            deleted,
        })
        .collect()
}

/// `listed` with each ignored entry of `fresh` (absolute, as git spelled them) it does not
/// hold, as an entry no regenerable pattern matched, whose own mtime is read too (section
/// 9.8, step 8); `listed`'s paths are looked up by native path in a hash set.
#[must_use]
pub fn union_ignored(listed: &[IgnoredEntry], fresh: Vec<PathBuf>) -> Vec<IgnoredEntry> {
    let held: HashSet<&Path> = listed.iter().filter_map(|e| e.native.as_deref()).collect();
    let mut all = listed.to_vec();
    for path in fresh {
        if held.contains(path.as_path()) {
            continue;
        }
        all.push(IgnoredEntry {
            path: path.to_string_lossy().into_owned(),
            class: IgnoredClass::Unclassifiable,
            regenerable_match: false,
            allocated_bytes: None,
            aliased_bytes: None,
            exclusive_bytes: None,
            exclusive_measure: None,
            native: Some(path),
        });
    }
    all
}

/// The signals of one worktree to read.
#[derive(Clone, Copy, Debug)]
pub struct Signals<'a> {
    /// The worktree root (absolute).
    pub root: &'a Path,
    /// The worktree's git dir; `None` when it could not be read, which is unknown.
    pub git_dir: Option<&'a Path>,
    /// A linked worktree, whose git dir holds `gitdir`.
    pub linked: bool,
    /// What `git status` listed; `None` when it was not read, which is unknown.
    pub status_paths: Option<&'a [StatusPath]>,
    /// The ignored entries the observation listed, absolute.
    pub ignored: &'a [IgnoredEntry],
    /// An ignored entry left out (absolute): an executing `act`'s target after its move, whose
    /// entries step 5's comparison decided (section 9.8).
    pub skip: Option<&'a Path>,
    /// When the read must have finished: checked before each stage, each path, and each entry,
    /// and past it the read is unknown ([`LATE`]). A file system call that blocks is not
    /// bounded by it.
    pub deadline: Option<Instant>,
}

impl<'a> Signals<'a> {
    /// The signals of an observed `worktree`, `skip` left out.
    #[must_use]
    pub fn of(worktree: &'a Worktree, skip: Option<&'a Path>) -> Self {
        Self {
            root: Path::new(&worktree.path),
            git_dir: worktree.git_dir.as_deref().map(Path::new),
            linked: !worktree.main,
            status_paths: worktree.status_paths.as_deref(),
            ignored: &worktree.ignored,
            skip,
            deadline: None,
        }
    }
}

/// Reads `signals` at `window` (section 9.8): activity at the first recent signal; otherwise
/// unknown when a read failed, a cap was passed, or the deadline passed; otherwise quiet.
#[must_use]
pub fn read(signals: &Signals<'_>, window: Window) -> Quiet {
    let mut found = Quiet::Quiet;
    let stages: [fn(&Signals<'_>, Window) -> Quiet; 3] = [git_dir, status_paths, ignored];
    for stage in stages {
        if past(signals.deadline) {
            return found.and(Quiet::Unknown(LATE.into()));
        }
        found = found.and(stage(signals, window));
        if matches!(found, Quiet::Active(_)) {
            break;
        }
    }
    found
}

/// The git dir's `index`, `HEAD`, a linked worktree's `gitdir`, and `logs/HEAD` with the time
/// of its last entry.
fn git_dir(signals: &Signals<'_>, window: Window) -> Quiet {
    let Some(git_dir) = signals.git_dir else {
        return Quiet::Unknown("the worktree's git dir was not read".into());
    };
    let mut found = Quiet::Quiet;
    let mut names = vec!["index", "HEAD"];
    if signals.linked {
        names.push("gitdir");
    }
    for name in names {
        found = found.and(mtime_signal(&git_dir.join(name), window));
        if matches!(found, Quiet::Active(_)) {
            return found;
        }
    }
    found.and(reflog_signal(
        &git_dir.join("logs/HEAD"),
        &read_reflog(&git_dir.join("logs/HEAD")),
        window,
    ))
}

/// The reflog at `path` as a signal, from `read`, one read of it: its mtime, then the time of
/// its last entry; missing adds nothing.
fn reflog_signal(path: &Path, read: &Result<Option<Reflog>, String>, window: Window) -> Quiet {
    match read {
        Ok(Some(reflog)) if window.recent(reflog.modified_ms) => Quiet::Active(format!(
            "{}, changed at {} ms",
            path.display(),
            reflog.modified_ms
        )),
        Ok(Some(Reflog {
            entry_ms: Some(at_ms),
            ..
        })) if window.recent(*at_ms) => Quiet::Active(format!(
            "the last entry of {} at {at_ms} ms",
            path.display()
        )),
        Ok(_) => Quiet::Quiet,
        Err(why) => Quiet::Unknown(why.clone()),
    }
}

/// Each path `git status` listed, stopping at the first recent one: a tracked deletion counts
/// as now, and so does a listed path that is gone since. More than [`MAX_STATUS_PATHS`]
/// paths with none of them recent is unknown.
fn status_paths(signals: &Signals<'_>, window: Window) -> Quiet {
    let Some(paths) = signals.status_paths else {
        return Quiet::Unknown("git status was not read whole".into());
    };
    let mut found = Quiet::Quiet;
    for listed in paths.iter().take(MAX_STATUS_PATHS) {
        if past(signals.deadline) {
            return found.and(Quiet::Unknown(LATE.into()));
        }
        let path = signals.root.join(&listed.path);
        if listed.deleted {
            return Quiet::Active(format!("{}, a tracked deletion", path.display()));
        }
        found = found.and(match mtime(&path) {
            Ok(Some(at_ms)) if window.recent(at_ms) => {
                Quiet::Active(format!("{}, changed at {at_ms} ms", path.display()))
            }
            Ok(Some(_)) => Quiet::Quiet,
            Ok(None) => Quiet::Active(format!(
                "{}, which git status listed, is gone",
                path.display()
            )),
            Err(why) => Quiet::Unknown(why),
        });
        if matches!(found, Quiet::Active(_)) {
            return found;
        }
    }
    if paths.len() > MAX_STATUS_PATHS {
        return found.and(Quiet::Unknown(format!(
            "git status listed {} paths, more than {MAX_STATUS_PATHS}, none of the first recent",
            paths.len()
        )));
    }
    found
}

/// Each ignored entry: its own mtime unless it is a regenerable target, and its top-level
/// entries' mtimes, at most [`MAX_ENTRIES_PER_IGNORED`] in each and
/// [`MAX_ENTRIES_PER_WORKTREE`] in all; a regenerable target's are read whole and count
/// against neither cap, as the entries below a target whose whole walk the plans and step 5
/// read already.
fn ignored(signals: &Signals<'_>, window: Window) -> Quiet {
    let cap = MAX_ENTRIES_PER_WORKTREE;
    let mut found = Quiet::Quiet;
    let mut read = 0usize;
    for entry in signals.ignored {
        if past(signals.deadline) {
            return found.and(Quiet::Unknown(LATE.into()));
        }
        let Some(path) = entry.native.as_deref() else {
            return found.and(Quiet::Unknown(format!(
                "{}: the path git listed was not kept",
                entry.path
            )));
        };
        if signals.skip == Some(path) {
            continue;
        }
        if !entry.regenerable_match {
            found = found.and(mtime_signal(path, window));
            if matches!(found, Quiet::Active(_)) {
                return found;
            }
        }
        let counted = (!entry.regenerable_match).then_some((&mut read, cap));
        found = found.and(top_level(path, window, signals.deadline, counted));
        if matches!(found, Quiet::Active(_)) {
            return found;
        }
        if read > cap {
            return found.and(Quiet::Unknown(format!(
                "the ignored entries hold more than {cap} entries"
            )));
        }
    }
    found
}

/// The entries directly in the directory `dir`, each `lstat`ed, never following a link: a
/// path that is not a directory has none. The names are read first, then each entry, as
/// `plan`'s walk lists a directory: `dir` gone before it is opened, an entry gone before its
/// `lstat`, and `dir` removed or no longer the directory listed once its entries are read, is
/// activity now, as a status path gone is. With `counted`, at most [`MAX_ENTRIES_PER_IGNORED`] are read, each
/// counted in `read` up to `cap`; without, all. Past `deadline`, checked before each name and
/// each entry, it is unknown.
fn top_level(
    dir: &Path,
    window: Window,
    deadline: Option<Instant>,
    mut counted: Option<(&mut usize, usize)>,
) -> Quiet {
    let opened = match Opened::open(dir) {
        Ok(Some(opened)) => opened,
        Ok(None) => return Quiet::Quiet,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Quiet::Active(format!("{}, listed, is gone", dir.display()));
        }
        Err(err) => return Quiet::Unknown(format!("{}: {err}", dir.display())),
    };
    // What ends the reading of the names before the last: unknown, or quiet when the
    // worktree's cap ends it, which the caller reports.
    let mut stopped = None;
    let mut more = |n: usize| {
        if past(deadline) {
            stopped = Some(Quiet::Unknown(LATE.into()));
            return false;
        }
        if let Some((read, cap)) = &mut counted {
            if n >= MAX_ENTRIES_PER_IGNORED {
                stopped = Some(Quiet::Unknown(format!(
                    "{} holds more than {MAX_ENTRIES_PER_IGNORED} entries",
                    dir.display()
                )));
                return false;
            }
            **read += 1;
            if **read > *cap {
                stopped = Some(Quiet::Quiet);
                return false;
            }
        }
        true
    };
    let (names, gone) = match opened.names(&mut more) {
        Ok(read) => read,
        Err(err) => return Quiet::Unknown(format!("{}: {err}", dir.display())),
    };
    if let Err(err) = crate::testpoint::hit(crate::testpoint::QUIET_DIR_LISTED) {
        return Quiet::Unknown(err.to_string());
    }
    let mut found = Quiet::Quiet;
    for name in &names {
        if past(deadline) {
            return found.and(Quiet::Unknown(LATE.into()));
        }
        let path = dir.join(name);
        found = found.and(match opened.mtime(name, &path) {
            Ok(Some(at_ms)) if window.recent(at_ms) => {
                Quiet::Active(format!("{}, changed at {at_ms} ms", path.display()))
            }
            Ok(Some(_)) => Quiet::Quiet,
            Ok(None) => Quiet::Active(format!("{}, removed while it was read", path.display())),
            Err(why) => Quiet::Unknown(why),
        });
        if matches!(found, Quiet::Active(_)) {
            return found;
        }
    }
    let replaced = || {
        Quiet::Active(format!(
            "{}, removed or replaced while it was read",
            dir.display()
        ))
    };
    if gone {
        return replaced();
    }
    match opened.still_named(dir) {
        Ok(true) => {}
        Ok(false) => return replaced(),
        Err(err) => return found.and(Quiet::Unknown(format!("{}: {err}", dir.display()))),
    }
    match stopped {
        Some(stop) => found.and(stop),
        None => found,
    }
}

/// A directory [`top_level`] lists: on Unix the descriptor it opened, without following a
/// final symbolic link, whose entries are read relative to it and whose identity is checked
/// against its name once they are read ([`crate::os::dir`]); elsewhere its path.
#[cfg(unix)]
struct Opened(std::fs::File);

#[cfg(unix)]
impl Opened {
    /// The directory at `dir`; `None` when the entry there is not a directory.
    fn open(dir: &Path) -> io::Result<Option<Self>> {
        Ok(crate::os::dir::open(dir)?.map(Self))
    }

    /// The names read while `more` allows, and whether the directory was removed meanwhile.
    fn names(
        &self,
        more: &mut dyn FnMut(usize) -> bool,
    ) -> io::Result<(Vec<std::ffi::OsString>, bool)> {
        let read = crate::os::dir::names_while(&self.0, more)?;
        Ok((read.names, read.gone))
    }

    /// The mtime of the entry `name`, at `path`, in milliseconds; `None` when it is gone.
    fn mtime(&self, name: &std::ffi::OsStr, path: &Path) -> Result<Option<u64>, String> {
        match crate::os::dir::stat_at(&self.0, name) {
            Ok(st) => Ok(Some(
                u64::try_from(st.mtime_ns.max(0) / 1_000_000).unwrap_or(u64::MAX),
            )),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(format!("{}: {err}", path.display())),
        }
    }

    /// Whether `dir` still names the directory opened.
    fn still_named(&self, dir: &Path) -> io::Result<bool> {
        crate::os::dir::still_named(&self.0, crate::os::dir::At::path(dir))
    }
}

#[cfg(not(unix))]
struct Opened(PathBuf);

#[cfg(not(unix))]
impl Opened {
    fn open(dir: &Path) -> io::Result<Option<Self>> {
        Ok(std::fs::symlink_metadata(dir)?
            .is_dir()
            .then(|| Self(dir.to_path_buf())))
    }

    fn names(
        &self,
        more: &mut dyn FnMut(usize) -> bool,
    ) -> io::Result<(Vec<std::ffi::OsString>, bool)> {
        let mut names = Vec::new();
        let listing = match std::fs::read_dir(&self.0) {
            Ok(listing) => listing,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok((names, true)),
            Err(err) => return Err(err),
        };
        for item in listing {
            if !more(names.len()) {
                break;
            }
            names.push(item?.file_name());
        }
        Ok((names, false))
    }

    fn mtime(&self, name: &std::ffi::OsStr, _path: &Path) -> Result<Option<u64>, String> {
        mtime(&self.0.join(name))
    }

    /// Whether the path is a directory still; no identity is read.
    fn still_named(&self, _dir: &Path) -> io::Result<bool> {
        match std::fs::symlink_metadata(&self.0) {
            Ok(meta) => Ok(meta.is_dir()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }
}

/// One `lstat`ed path as a signal: missing adds nothing.
fn mtime_signal(path: &Path, window: Window) -> Quiet {
    match mtime(path) {
        Ok(Some(at_ms)) if window.recent(at_ms) => {
            Quiet::Active(format!("{}, changed at {at_ms} ms", path.display()))
        }
        Ok(_) => Quiet::Quiet,
        Err(why) => Quiet::Unknown(why),
    }
}

/// The mtime of `path` by `lstat`, in milliseconds; `None` when it does not exist. A time
/// before the epoch reads as 0.
pub fn mtime(path: &Path) -> Result<Option<u64>, String> {
    match std::fs::symlink_metadata(path).and_then(|meta| meta.modified()) {
        Ok(time) => Ok(Some(system_ms(time))),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("{}: {err}", path.display())),
    }
}

fn system_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// What one read of a reflog found: the file's mtime and the time of its last entry, both in
/// milliseconds, the entry `None` for an empty reflog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reflog {
    modified_ms: u64,
    entry_ms: Option<u64>,
}

/// The reflog at `path` (each entry `<old> <new> <ident> <seconds> <tz>`, then a tab and the
/// message), its mtime and the time of its last entry read from one open file; `None` when the
/// file is missing. The file is opened as a regular file only, never through a final symbolic
/// link and never waiting in a FIFO. A last line that is not complete (no newline), does not
/// parse, or is longer than [`REFLOG_TAIL`] is an error, and so is a reflog that changed while
/// its tail was read, twice: appended to, rewritten, or replaced at its name; an entry appended
/// meanwhile, or a reflog renamed over the old one, is read on the second try, the mtime with
/// it. The name gone after a read found the file is an error, never a missing reflog.
fn read_reflog(path: &Path) -> Result<Option<Reflog>, String> {
    read_reflog_with(path, &mut || {})
}

/// The identity, length, and mtime of an open reflog.
type Stamp = ((u64, u64), u64, SystemTime);

/// [`read_reflog`], `between` run after each read of the tail and before the check that the
/// file did not change.
fn read_reflog_with(path: &Path, between: &mut dyn FnMut()) -> Result<Option<Reflog>, String> {
    let failed = |why: &dyn std::fmt::Display| format!("{}: {why}", path.display());
    // `open_regular` names the path in the error it makes for what is not a regular file.
    let open = || {
        crate::os::open_regular(std::fs::OpenOptions::new().read(true), path).map_err(|err| {
            match err.kind() {
                io::ErrorKind::Other => (err.kind(), err.to_string()),
                kind => (kind, failed(&err)),
            }
        })
    };
    let stamp = |file: &std::fs::File| -> Result<Stamp, String> {
        let id = crate::os::file_id(file).map_err(|err| failed(&err))?;
        let meta = file.metadata().map_err(|err| failed(&err))?;
        Ok((id, meta.len(), meta.modified().map_err(|err| failed(&err))?))
    };
    let changed = || failed(&"it changed while its last entry was read");
    for attempt in 0..2 {
        let mut file = match open() {
            Ok(file) => file,
            Err((io::ErrorKind::NotFound, _)) if attempt == 0 => return Ok(None),
            Err((io::ErrorKind::NotFound, _)) => return Err(changed()),
            Err((_, why)) => return Err(why),
        };
        let before = stamp(&file)?;
        let len = before.1;
        let start = len.saturating_sub(REFLOG_TAIL);
        file.seek(SeekFrom::Start(start))
            .map_err(|err| failed(&err))?;
        let mut tail = Vec::new();
        (&file)
            .take(REFLOG_TAIL)
            .read_to_end(&mut tail)
            .map_err(|err| failed(&err))?;
        between();
        // The tail reached the end the length named, the file read is as it was, and the
        // name still holds it: nothing was appended, rewritten, or renamed over it meanwhile.
        let named = match open() {
            Ok(named) => Some(stamp(&named)?),
            Err((io::ErrorKind::NotFound, _)) => None,
            Err((_, why)) => return Err(why),
        };
        if start + tail.len() as u64 == len && stamp(&file)? == before && named == Some(before) {
            return Ok(Some(Reflog {
                modified_ms: system_ms(before.2),
                entry_ms: parse_last_entry(&tail, start, &failed)?,
            }));
        }
    }
    Err(changed())
}

/// The time of the last entry in `tail`, the reflog read from byte `start` to its end.
fn parse_last_entry(
    tail: &[u8],
    start: u64,
    failed: &dyn Fn(&dyn std::fmt::Display) -> String,
) -> Result<Option<u64>, String> {
    if tail.is_empty() {
        return Ok(None);
    }
    let Some(text) = tail.strip_suffix(b"\n") else {
        return Err(failed(&"its last entry is not complete"));
    };
    let line = match text.iter().rposition(|&b| b == b'\n') {
        Some(at) => &text[at + 1..],
        None if start == 0 => text,
        None => return Err(failed(&"its last entry is longer than the tail read")),
    };
    let head = line.split(|&b| b == b'\t').next().unwrap_or_default();
    let head = std::str::from_utf8(head).map_err(|_| failed(&"its last entry is not UTF-8"))?;
    let mut fields = head.rsplit(' ');
    let _tz = fields.next();
    let seconds = fields
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| failed(&"its last entry has no time"))?;
    Ok(Some(seconds.saturating_mul(1000)))
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::path::{Path, PathBuf};

    use super::{
        Newest, Quiet, Reflog, Signals, Window, read_reflog, read_reflog_with, reflog_signal,
        union_ignored, union_status,
    };
    use crate::contract::StatusPath;

    /// A directory of its own under the temp dir, removed when dropped.
    struct Temp(PathBuf);

    impl Temp {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("cardinal-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Sets the mtime of the file or directory at `path`. Windows sets a time only through a
    /// handle with `FILE_WRITE_ATTRIBUTES` (`0x100`), and opens a directory only with
    /// `FILE_FLAG_BACKUP_SEMANTICS` (`0x0200_0000`); `File::open` asks for neither.
    fn set_mtime(path: &Path, ms: u64) {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        std::os::windows::fs::OpenOptionsExt::custom_flags(
            std::os::windows::fs::OpenOptionsExt::access_mode(&mut options, 0x100),
            0x0200_0000,
        );
        options
            .open(path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms))
            .unwrap();
    }

    const OLD_MS: u64 = 1_000_000_000_000;

    /// The time of the last entry [`read_reflog_with`] read of `path`, `between` run as it
    /// runs it.
    fn last_entry_with(path: &Path, between: &mut dyn FnMut()) -> Result<Option<u64>, String> {
        read_reflog_with(path, between).map(|read| read.and_then(|r| r.entry_ms))
    }

    /// The time of the last entry of the reflog at `path`.
    fn last_entry(path: &Path) -> Result<Option<u64>, String> {
        read_reflog(path).map(|read| read.and_then(|r| r.entry_ms))
    }

    /// Section 9.8: a kept reading is judged again at another window: an entry old under one
    /// `quiet_ms` is recent under a longer one; an unknown part is unknown unless the newest
    /// entry is recent.
    #[test]
    fn a_kept_reading_is_judged_again() {
        let mut newest = Newest::default();
        newest.add(9_000, Path::new("/a"));
        newest.add(5_000, Path::new("/b"));
        let short = Window {
            now_ms: 10_000,
            quiet_ms: 500,
        };
        let long = Window {
            quiet_ms: 2_000,
            ..short
        };
        assert_eq!(newest.judge(short), Quiet::Quiet);
        assert!(matches!(newest.judge(long), Quiet::Active(s) if s.starts_with("/a")));
        newest.fail("unread");
        assert_eq!(newest.judge(short), Quiet::Unknown("unread".into()));
        assert!(matches!(newest.judge(long), Quiet::Active(_)));
        // An entry removed during the read is activity at any window, however long ago the
        // read began.
        newest.vanish(Path::new("/c"));
        assert!(matches!(newest.judge(short), Quiet::Active(s) if s.starts_with("/c")));
    }

    /// Section 9.8, step 8: two listings of one path count it once, a deletion in either kept.
    #[test]
    fn status_listings_are_one() {
        let path = |p: &str, deleted| StatusPath {
            path: p.into(),
            deleted,
        };
        let first = [path("a", false), path("b", false)];
        let second = [path("b", true), path("c", false)];
        assert_eq!(
            union_status(&first, &second),
            [path("a", false), path("b", true), path("c", false)]
        );
    }

    /// Section 9.8: an ignored entry whose path git spelled was not kept, as in an observation
    /// read back from a file, is unknown, never read by a lossy spelling; kept, it is read
    /// (control).
    #[test]
    fn an_ignored_path_not_kept_is_unknown() {
        let tmp = Temp::new("ignored-native");
        let dist = tmp.0.join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let entries = union_ignored(&[], vec![dist.clone()]);
        let signals = |ignored| Signals {
            root: &tmp.0,
            git_dir: None,
            linked: false,
            status_paths: Some(&[]),
            ignored,
            skip: None,
            deadline: None,
        };
        let window = Window {
            now_ms: OLD_MS + 10_000_000,
            quiet_ms: 1_000,
        };
        set_mtime(&dist, OLD_MS);
        assert_eq!(super::ignored(&signals(&entries), window), Quiet::Quiet);
        let mut lost = entries.clone();
        lost[0].native = None;
        assert!(matches!(
            super::ignored(&signals(&lost), window),
            Quiet::Unknown(why) if why.contains("not kept")
        ));
    }

    /// Section 9.8: the ignored entries' top-level entries past [`MAX_ENTRIES_PER_WORKTREE`]
    /// (65,536) in all, none recent and none past the cap of one entry, are unknown, never
    /// quiet; at the cap they are quiet (control), and so they are beside a regenerable target
    /// of 4,097 entries, which counts against neither cap. The files are made now and judged
    /// at a window far later, so no mtime is set.
    #[test]
    fn the_worktree_cap_is_unknown() {
        assert_eq!(super::MAX_ENTRIES_PER_WORKTREE, 65_536);
        let per_entry = super::MAX_ENTRIES_PER_IGNORED;
        let tmp = Temp::new("ignored-cap");
        let mut dirs = Vec::new();
        for d in 0..super::MAX_ENTRIES_PER_WORKTREE / per_entry {
            let dir = tmp.0.join(format!("d{d}"));
            std::fs::create_dir_all(&dir).unwrap();
            for i in 0..per_entry {
                std::fs::File::create(dir.join(format!("f{i}"))).unwrap();
            }
            dirs.push(dir);
        }
        let last = tmp.0.join("last");
        std::fs::create_dir_all(&last).unwrap();
        dirs.push(last.clone());
        let regenerable = tmp.0.join("node_modules");
        std::fs::create_dir_all(&regenerable).unwrap();
        for i in 0..=per_entry {
            std::fs::File::create(regenerable.join(format!("f{i}"))).unwrap();
        }
        let mut entries = union_ignored(&[], vec![regenerable]);
        entries[0].regenerable_match = true;
        let entries = union_ignored(&entries, dirs);
        let signals = Signals {
            root: &tmp.0,
            git_dir: None,
            linked: false,
            status_paths: Some(&[]),
            ignored: &entries,
            skip: None,
            deadline: None,
        };
        let window = Window {
            now_ms: super::system_ms(std::time::SystemTime::now()) + 10_000_000,
            quiet_ms: 1_000,
        };
        assert_eq!(super::ignored(&signals, window), Quiet::Quiet);
        std::fs::File::create(last.join("f")).unwrap();
        assert!(matches!(
            super::ignored(&signals, window),
            Quiet::Unknown(why) if why.contains("more than 65536 entries")
        ));
    }

    /// Section 9.8: an entry appended while the tail is read is read on the second try; one
    /// appended during both tries is unknown, never the entry before it.
    #[test]
    fn a_reflog_appended_meanwhile() {
        let tmp = Temp::new("reflog-append");
        let path = tmp.0.join("HEAD");
        let a = "0".repeat(40);
        let line = |s: u64| format!("{a} {a} A U Thor <a@example.com> {s} +0000\tcommit: x\n");
        std::fs::write(&path, line(1_700_000_000)).unwrap();
        let append = |s: u64| {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(line(s).as_bytes()).unwrap();
        };
        let mut n = 0;
        let read = last_entry_with(&path, &mut || {
            if n == 0 {
                append(1_700_000_100);
            }
            n += 1;
        });
        assert_eq!(read, Ok(Some(1_700_000_100_000)));
        let mut s = 1_700_000_200;
        let read = last_entry_with(&path, &mut || {
            append(s);
            s += 1;
        });
        assert!(read.unwrap_err().contains("changed while"));
        assert_eq!(last_entry(&path), Ok(Some(1_700_000_201_000)));
    }

    /// Section 9.8: the reflog's path gone after the file was opened is unknown, never a
    /// missing reflog; a reflog missing from the start has no entry (control).
    #[test]
    fn a_reflog_removed_meanwhile() {
        let tmp = Temp::new("reflog-remove");
        let path = tmp.0.join("HEAD");
        let a = "0".repeat(40);
        let line = format!("{a} {a} A U Thor <a@example.com> 1700000000 +0000\tcommit: x\n");
        std::fs::write(&path, line).unwrap();
        let read = last_entry_with(&path, &mut || {
            let _ = std::fs::remove_file(&path);
        });
        assert!(read.is_err(), "{read:?}");
        assert_eq!(last_entry(&path), Ok(None));
    }

    /// Section 9.8: the reflog's mtime and its last entry are judged from one read. A reflog
    /// renamed over `logs/HEAD` while the old one is read, written just now with an old last
    /// entry (old history rewritten), is activity by the mtime the second try read, never quiet
    /// by the old file's mtime with the new file's entry; with the old mtime set on the
    /// replacement it is quiet (control).
    #[test]
    fn a_replaced_reflog_is_judged_by_its_own_mtime() {
        let tmp = Temp::new("reflog-replace-mtime");
        let path = tmp.0.join("HEAD");
        let a = "0".repeat(40);
        let line = |s: u64| format!("{a} {a} A U Thor <a@example.com> {s} +0000\tcommit: x\n");
        let old_s = OLD_MS / 1000;
        let window = Window {
            now_ms: OLD_MS + 10_000_000,
            quiet_ms: 1_000,
        };
        let read = |recent: bool| {
            std::fs::write(&path, line(old_s)).unwrap();
            set_mtime(&path, OLD_MS);
            let new = tmp.0.join("HEAD.new");
            std::fs::write(&new, line(old_s + 1)).unwrap();
            set_mtime(&new, if recent { window.now_ms } else { OLD_MS });
            let mut n = 0;
            let read = read_reflog_with(&path, &mut || {
                if n == 0 {
                    std::fs::rename(&new, &path).unwrap();
                }
                n += 1;
            });
            assert_eq!(
                read.as_ref().map(|r| r.map(|r| r.entry_ms)),
                Ok(Some(Some((old_s + 1) * 1000)))
            );
            reflog_signal(&path, &read, window)
        };
        assert!(
            matches!(read(true), Quiet::Active(ref why) if why.contains("changed at")),
            "{:?}",
            read(true)
        );
        assert_eq!(read(false), Quiet::Quiet);
        // Each part alone is a signal.
        let reflog = |modified_ms, entry_ms| {
            Ok(Some(Reflog {
                modified_ms,
                entry_ms,
            }))
        };
        assert!(matches!(
            reflog_signal(&path, &reflog(OLD_MS, Some(window.now_ms)), window),
            Quiet::Active(_)
        ));
        assert_eq!(
            reflog_signal(&path, &reflog(OLD_MS, None), window),
            Quiet::Quiet
        );
    }

    /// Section 9.8: a symbolic link at `logs/HEAD` is never followed: reading it is unknown,
    /// never the old entry of the reflog it names, which read by its own path is quiet
    /// (control).
    #[cfg(unix)]
    #[test]
    fn a_reflog_link_is_not_followed() {
        let tmp = Temp::new("reflog-link");
        let real = tmp.0.join("real");
        let a = "0".repeat(40);
        std::fs::write(
            &real,
            format!("{a} {a} A U Thor <a@example.com> 1000000000 +0000\tcommit: x\n"),
        )
        .unwrap();
        set_mtime(&real, OLD_MS);
        let link = tmp.0.join("HEAD");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let window = Window {
            now_ms: OLD_MS + 10_000_000,
            quiet_ms: 1_000,
        };
        assert_eq!(
            reflog_signal(&real, &read_reflog(&real), window),
            Quiet::Quiet
        );
        let through = read_reflog(&link);
        assert!(through.is_err(), "{through:?}");
        assert!(matches!(
            reflog_signal(&link, &through, window),
            Quiet::Unknown(_)
        ));
    }

    /// An empty window holds nothing, a future time included, which counts as now; a window
    /// holds a time within it, and a future one (control).
    #[test]
    fn future_times_count_as_now() {
        let window = Window {
            now_ms: 10_000,
            quiet_ms: 2_000,
        };
        assert!(window.recent(9_000));
        assert!(window.recent(u64::MAX), "a future mtime counts as now");
        assert!(!window.recent(8_000));
        assert!(!window.recent(0));
        let empty = Window {
            quiet_ms: 0,
            ..window
        };
        assert!(!empty.recent(10_000));
        assert!(!empty.recent(10_001), "a future mtime counts as now");
    }

    #[test]
    fn activity_comes_before_an_unknown() {
        let active = Quiet::Active("a".into());
        let unknown = Quiet::Unknown("u".into());
        assert_eq!(unknown.clone().and(active.clone()), active);
        assert_eq!(Quiet::Quiet.and(unknown.clone()), unknown);
        assert_eq!(Quiet::Quiet.and(Quiet::Quiet), Quiet::Quiet);
    }

    #[test]
    fn the_last_reflog_entry() {
        let dir = std::env::temp_dir().join(format!("cardinal-reflog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("HEAD");
        let a = "0".repeat(40);
        std::fs::write(
            &path,
            format!(
                "{a} {a} A U Thor <a@example.com> 1700000000 +0900\tcommit (initial): x\n\
                 {a} {a} A U Thor <a@example.com> 1700000123 -0100\tcommit: y\n"
            ),
        )
        .unwrap();
        assert_eq!(last_entry(&path), Ok(Some(1_700_000_123_000)));
        std::fs::write(&path, "").unwrap();
        assert_eq!(last_entry(&path), Ok(None));
        std::fs::write(&path, "garbage\n").unwrap();
        assert!(last_entry(&path).is_err());
        // A last line torn before its newline is incomplete, though its time parses.
        std::fs::write(
            &path,
            format!("{a} {a} A U Thor <a@example.com> 1700000123 -0"),
        )
        .unwrap();
        assert!(last_entry(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(last_entry(&path), Ok(None));
    }
}
