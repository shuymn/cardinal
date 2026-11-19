//! Write discipline (`docs/architecture.md` section 4.2) and JSONL files (section 4.7).

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use serde::de::DeserializeOwned;

/// Whether a write survives a power loss.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durability {
    /// Not fsynced: reservations, live locks, records.
    Volatile,
    /// fsync of the file, then of its parent directory on Unix (`F_FULLFSYNC` on macOS, which
    /// std's `File::sync_all` issues): inbox items, and an execution's manifest, claim, and
    /// result (section 9.7).
    Durable,
}

/// The prefix of the temporary files [`write_atomic`] and the state dir's create-once writes
/// create, the only files [`remove_temp_files_older_than`] removes.
pub const TEMP_PREFIX: &str = ".cardinal-tmp.";

/// Writes `bytes` to `target` atomically: a temporary file `<TEMP_PREFIX><name>.<pid>.<n>` in
/// `tmp_dir` (same file system), flushed, then renamed over the target. Readers see the old
/// or the new file, never a mix.
pub fn write_atomic(
    tmp_dir: &Path,
    target: &Path,
    bytes: &[u8],
    durability: Durability,
) -> io::Result<()> {
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no file name"))?;
    write_through(&tmp_dir.join(temp_name(name)), target, bytes, durability)
}

/// [`write_atomic`] through the temporary file `tmp`. A `tmp` that exists already is
/// another writer's and fails the write untouched; the file this write created is removed
/// when the write fails before its rename, and never after it.
fn write_through(
    tmp: &Path,
    target: &Path,
    bytes: &[u8],
    durability: Durability,
) -> io::Result<()> {
    Staged::write_through(tmp, target, bytes, durability)?.publish()?;
    if durability == Durability::Durable {
        sync_parent(target)?;
    }
    Ok(())
}

/// A file written under a temporary name and not yet renamed into place: the first half of
/// an atomic replace (section 4.2). An execution publishes its claim in two steps, so the
/// rename that publishes `committed` comes after the checks that follow the write (section
/// 9.7, step 8): [`Staged::publish`] renames it into place, and the caller syncs the parent
/// directory itself ([`sync_parent`]), so a failed sync is told apart from a failed rename.
/// A rename that failed can be tried again, as the reserve's retry does (section 9.7,
/// "Space"). A staged file dropped before it is published is removed; one left by a process
/// that died stays for the cleanup of temporary files.
#[derive(Debug)]
pub struct Staged {
    tmp: PathBuf,
    target: PathBuf,
    published: bool,
}

impl Staged {
    /// Writes `bytes` to a new temporary file for `target` in `tmp_dir` and syncs it.
    pub fn write(tmp_dir: &Path, target: &Path, bytes: &[u8]) -> io::Result<Self> {
        let name = target.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "target has no file name")
        })?;
        Self::write_through(
            &tmp_dir.join(temp_name(name)),
            target,
            bytes,
            Durability::Durable,
        )
    }

    /// Writes `bytes` to `tmp`, created new, flushes it, and syncs it when `durability` asks.
    /// A `tmp` that exists already is another writer's and fails the write untouched; the file
    /// this write created is removed when the write fails.
    fn write_through(
        tmp: &Path,
        target: &Path,
        bytes: &[u8],
        durability: Durability,
    ) -> io::Result<Self> {
        let mut file = logged(
            Op::Create,
            tmp,
            None,
            OpenOptions::new().write(true).create_new(true).open(tmp),
        )?;
        let staged = Self {
            tmp: tmp.to_path_buf(),
            target: target.to_path_buf(),
            published: false,
        };
        let write = file.write_all(bytes).and_then(|()| file.flush());
        logged(Op::Write, tmp, None, write)?;
        if durability == Durability::Durable {
            logged(Op::Sync, tmp, None, file.sync_all())?;
        }
        Ok(staged)
    }

    /// The temporary file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.tmp
    }

    /// Renames the file into place, which publishes it. A rename that fails leaves the
    /// temporary file for another try, or for the drop to remove.
    pub fn publish(&mut self) -> io::Result<()> {
        logged(
            Op::Rename,
            &self.tmp,
            Some(&self.target),
            std::fs::rename(&self.tmp, &self.target),
        )?;
        self.published = true;
        Ok(())
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.published {
            // The file this write created, which no other writer has.
            let _ = logged(Op::Unlink, &self.tmp, None, std::fs::remove_file(&self.tmp));
        }
    }
}

/// fsyncs the directory containing `path` (Unix only; Windows flushes files only).
pub fn sync_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => sync_dir(parent),
        _ => sync_dir(Path::new(".")),
    }
}

/// fsyncs the directory `dir` (Unix only; Windows flushes files only): `F_FULLFSYNC` on
/// macOS, which std's `File::sync_all` issues on directories too.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let synced = File::open(dir).and_then(|file| file.sync_all());
        logged(Op::Sync, dir, None, synced)?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// fsyncs the file at `path`, opened read-only.
pub fn sync_file(path: &Path) -> io::Result<()> {
    let synced = File::open(path).and_then(|file| file.sync_all());
    logged(Op::Sync, path, None, synced)
}

/// A file system operation the write log records (section 13.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// A file created.
    Create,
    /// Bytes written to a file.
    Write,
    /// A directory created.
    Mkdir,
    /// An entry renamed: `path` to `to`.
    Rename,
    /// A file or a symbolic link removed.
    Unlink,
    /// A directory removed.
    Rmdir,
    /// A file or directory synced.
    Sync,
}

/// Records `op` on `path` (and `to`, for a rename) with its result in the write log, and
/// passes the result on. With `CARDINAL_TEST_DIR` set, the log is `<dir>/writes.log`: one JSON
/// line per operation, with this process's ID, which the durability tests check against the
/// durable order of section 9.7 (section 13.7). Outside tests it does nothing.
pub fn logged<T>(op: Op, path: &Path, to: Option<&Path>, result: io::Result<T>) -> io::Result<T> {
    if let Some(log) = write_log() {
        let line = serde_json::json!({
            "pid": std::process::id(),
            "op": op,
            "path": path,
            "to": to,
            "error": result.as_ref().err().map(ToString::to_string),
        });
        if let Ok(mut file) = log.lock() {
            let mut bytes = line.to_string().into_bytes();
            bytes.push(b'\n');
            let _ = file.write_all(&bytes);
        }
    }
    result
}

/// Whether [`logged`] writes the write log, so a caller can skip building the paths it names.
#[must_use]
pub fn logging() -> bool {
    write_log().is_some()
}

/// The write log's open file, when `CARDINAL_TEST_DIR` is set and the log can be opened.
fn write_log() -> Option<&'static Mutex<File>> {
    static LOG: OnceLock<Option<Mutex<File>>> = OnceLock::new();
    LOG.get_or_init(|| {
        let dir = std::env::var_os("CARDINAL_TEST_DIR").filter(|dir| !dir.is_empty())?;
        let file = open_append(&Path::new(&dir).join("writes.log")).ok()?;
        Some(Mutex::new(file))
    })
    .as_ref()
}

/// Removes the temporary files of [`write_atomic`] in `dir` last modified more than `max_age`
/// ago: regular files whose names it creates, and nothing else, so a state dir that overlaps
/// another directory loses none of its files. Returns how many were removed. Errors on single
/// entries are skipped.
pub fn remove_temp_files_older_than(dir: &Path, max_age: Duration) -> io::Result<usize> {
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        if !is_temp_name(&entry.file_name()) {
            continue;
        }
        // Not following a symbolic link: `write_atomic` creates none.
        let Ok(meta) = entry.metadata() else { continue };
        let old = meta
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > max_age);
        if meta.is_file() && old && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// A new temporary file name for the target `name`: [`TEMP_PREFIX`], the name, this
/// process's ID, and a counter.
pub(crate) fn temp_name(name: &OsStr) -> OsString {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut temp = OsString::from(TEMP_PREFIX);
    temp.push(name);
    temp.push(format!(
        ".{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    temp
}

/// Whether `name` is one [`temp_name`] gives: [`TEMP_PREFIX`], a name, a PID, and a counter.
fn is_temp_name(name: &OsStr) -> bool {
    let Some(rest) = name.to_str().and_then(|n| n.strip_prefix(TEMP_PREFIX)) else {
        return false;
    };
    let mut parts = rest.rsplitn(3, '.');
    let digits = |part: Option<&str>| {
        part.is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    };
    digits(parts.next()) && digits(parts.next()) && parts.next().is_some_and(|n| !n.is_empty())
}

/// Opens `path` in append mode for [`append_line`], creating it if needed.
pub fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .append(true)
        .read(true)
        .create(true)
        .open(path)
}

/// Appends one line with a single `write` on `file`, opened with [`open_append`], so
/// concurrent appenders do not interleave. `line` must not contain a newline; one is added.
///
/// If the file does not end with a newline (a crash tore the last append), the line is
/// written after a newline so it starts a line of its own and only the torn line is lost.
pub fn append_line(file: &File, line: &[u8]) -> io::Result<()> {
    debug_assert!(!line.contains(&b'\n'));
    let torn = !ends_with_newline(file)?;
    let mut buf = Vec::with_capacity(line.len() + 2);
    if torn {
        buf.push(b'\n');
    }
    buf.extend_from_slice(line);
    buf.push(b'\n');
    let written = (&*file).write(&buf)?;
    if written == buf.len() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!("short append: {written} of {} bytes", buf.len()),
        ))
    }
}

fn ends_with_newline(file: &File) -> io::Result<bool> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(true);
    }
    let mut last = [0u8; 1];
    read_at(file, &mut last, len - 1)?;
    Ok(last[0] == b'\n')
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    // seek_read moves the file pointer, which append-mode writes ignore.
    let read = std::os::windows::fs::FileExt::seek_read(file, buf, offset)?;
    if read == buf.len() {
        Ok(())
    } else {
        Err(io::ErrorKind::UnexpectedEof.into())
    }
}

/// Reads a JSONL file, skipping lines that are empty or do not parse as `T`. A missing file
/// reads as empty.
pub fn read_jsonl<T: DeserializeOwned>(path: &Path) -> io::Result<Vec<T>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    Ok(parse_jsonl(&bytes))
}

/// Parses JSONL bytes, skipping lines that are empty or do not parse as `T`.
#[must_use]
pub fn parse_jsonl<T: DeserializeOwned>(bytes: &[u8]) -> Vec<T> {
    bytes
        .split(|&b| b == b'\n')
        .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
        .filter_map(|line| serde_json::from_slice(line).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        Durability, Staged, append_line, open_append, read_jsonl, remove_temp_files_older_than,
        temp_name, write_atomic, write_through,
    };

    /// A file at the temporary name fails the write and is left as it was; the write's own
    /// temporary file is removed when its rename fails.
    #[test]
    fn a_write_removes_only_its_own_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        let taken = tmp.path().join(temp_name("target".as_ref()));
        std::fs::write(&taken, "another's").unwrap();
        let err = write_through(&taken, &target, b"new", Durability::Durable).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists, "{err}");
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "another's");
        assert!(!target.exists());

        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("inside"), "x").unwrap();
        let own = tmp.path().join(temp_name("target".as_ref()));
        write_through(&own, &target, b"new", Durability::Volatile).unwrap_err();
        assert!(!own.exists());
    }

    /// A staged file is synced under its temporary name and appears at its target only when
    /// published; one dropped unpublished leaves nothing, and one whose rename fails keeps its
    /// temporary file for another try, which publishes it, or for the drop to remove.
    #[test]
    fn a_staged_file_appears_only_when_published() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_dir = tmp.path().join("tmp");
        std::fs::create_dir(&tmp_dir).unwrap();
        let target = tmp.path().join("claim");
        std::fs::write(&target, b"claimed").unwrap();

        let mut staged = Staged::write(&tmp_dir, &target, b"committed").unwrap();
        assert_eq!(std::fs::read(staged.path()).unwrap(), b"committed");
        assert_eq!(std::fs::read(&target).unwrap(), b"claimed");
        staged.publish().unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"committed");
        assert_eq!(std::fs::read_dir(&tmp_dir).unwrap().count(), 0);

        drop(Staged::write(&tmp_dir, &target, b"dropped").unwrap());
        assert_eq!(std::fs::read(&target).unwrap(), b"committed");
        assert_eq!(std::fs::read_dir(&tmp_dir).unwrap().count(), 0);

        let missing = tmp.path().join("missing/claim");
        let mut staged = Staged::write(&tmp_dir, &missing, b"x").unwrap();
        staged.publish().unwrap_err();
        assert_eq!(std::fs::read(staged.path()).unwrap(), b"x");
        std::fs::create_dir(missing.parent().unwrap()).unwrap();
        staged.publish().unwrap();
        assert_eq!(std::fs::read(&missing).unwrap(), b"x");
        drop(staged);
        assert_eq!(std::fs::read_dir(&tmp_dir).unwrap().count(), 0);

        let mut staged = Staged::write(&tmp_dir, &tmp.path().join("gone/claim"), b"y").unwrap();
        staged.publish().unwrap_err();
        drop(staged);
        assert_eq!(std::fs::read_dir(&tmp_dir).unwrap().count(), 0);
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_dir = tmp.path().join("tmp");
        std::fs::create_dir(&tmp_dir).unwrap();
        let target = tmp.path().join("doc.json");
        write_atomic(&tmp_dir, &target, b"one", Durability::Volatile).unwrap();
        write_atomic(&tmp_dir, &target, b"two", Durability::Durable).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(&tmp_dir).unwrap().count(), 0);
    }

    #[test]
    fn failed_atomic_write_removes_its_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_dir = tmp.path().join("tmp");
        std::fs::create_dir(&tmp_dir).unwrap();
        let target = tmp.path().join("missing-dir/doc.json");
        assert!(write_atomic(&tmp_dir, &target, b"x", Durability::Volatile).is_err());
        assert_eq!(std::fs::read_dir(&tmp_dir).unwrap().count(), 0);
    }

    #[test]
    fn append_after_a_torn_tail_starts_a_new_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("r.jsonl");
        append_line(&open_append(&path).unwrap(), b"1").unwrap();
        std::fs::write(&path, b"1\n{\"torn\":").unwrap();
        append_line(&open_append(&path).unwrap(), b"2").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"1\n{\"torn\":\n2\n");
        assert_eq!(read_jsonl::<u32>(&path).unwrap(), [1, 2]);
        assert_eq!(
            read_jsonl::<u32>(&tmp.path().join("absent")).unwrap(),
            Vec::<u32>::new()
        );
    }

    /// Old temporary files of `write_atomic` are removed, and only those: files of other
    /// names, a name with the prefix alone, and a link stay however old.
    #[test]
    fn old_temp_files_are_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let own = ".cardinal-tmp.state.json.123.4";
        let others = [
            "notes.txt",
            "state.json.123.4",
            ".cardinal-tmp.",
            ".cardinal-tmp.state.json",
            ".cardinal-tmp.state.json.x.4",
        ];
        for name in std::iter::once(own).chain(others) {
            std::fs::write(tmp.path().join(name), b"").unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            tmp.path().join("notes.txt"),
            tmp.path().join(".cardinal-tmp.link.1.2"),
        )
        .unwrap();
        assert_eq!(
            remove_temp_files_older_than(tmp.path(), Duration::from_secs(3600)).unwrap(),
            0
        );
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            remove_temp_files_older_than(tmp.path(), Duration::from_millis(1)).unwrap(),
            1
        );
        assert!(!tmp.path().join(own).exists());
        for name in others {
            assert!(tmp.path().join(name).exists(), "{name}");
        }
    }

    /// The names `write_atomic` writes through are the ones the cleanup removes.
    #[test]
    fn the_cleanup_matches_the_names_written() {
        for target in ["state.json", "a.b.c", "1.2"] {
            let name = super::temp_name(std::ffi::OsStr::new(target));
            assert!(super::is_temp_name(&name), "{name:?}");
        }
    }
}
