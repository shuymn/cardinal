//! Write discipline (`docs/architecture.md` section 4.2) and JSONL files (section 4.7).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use serde::de::DeserializeOwned;

/// Whether a write survives a power loss.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durability {
    /// Not fsynced: reservations, live locks, records.
    Volatile,
    /// fsync of the file, then of its parent directory on Unix: inbox items, and the plan
    /// claims and act results of the deletion phase, which phase 0 never writes.
    Durable,
}

/// Writes `bytes` to `target` atomically: a temporary file in `tmp_dir` (same file system),
/// flushed, then renamed over the target. Readers see the old or the new file, never a mix.
pub fn write_atomic(
    tmp_dir: &Path,
    target: &Path,
    bytes: &[u8],
    durability: Durability,
) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no file name"))?;
    let mut tmp_name = name.to_os_string();
    tmp_name.push(format!(
        ".{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = tmp_dir.join(tmp_name);
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(bytes)?;
        file.flush()?;
        if durability == Durability::Durable {
            file.sync_all()?;
        }
        drop(file);
        std::fs::rename(&tmp, target)?;
        if durability == Durability::Durable {
            sync_parent(target)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Creates `path` with `create_new`, writes `bytes`, and fsyncs file and parent: the
/// claim-once write the deletion phase uses for plan claims; phase 0 never writes one. Fails
/// with `AlreadyExists` if the file exists.
pub fn create_new_durable(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    sync_parent(path)
}

/// fsyncs the directory containing `path` (Unix only; Windows flushes files only).
pub fn sync_parent(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let parent = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Removes regular files in `dir` last modified more than `max_age` ago. Returns how many
/// were removed. Errors on single entries are skipped.
pub fn remove_older_than(dir: &Path, max_age: Duration) -> io::Result<usize> {
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
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
        Durability, append_line, create_new_durable, open_append, read_jsonl, remove_older_than,
        write_atomic,
    };

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
    fn durable_create_is_once_only() {
        let tmp = tempfile::tempdir().unwrap();
        let claim = tmp.path().join("claim");
        create_new_durable(&claim, b"{}").unwrap();
        let err = create_new_durable(&claim, b"{}").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
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

    #[test]
    fn old_files_are_removed() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("fresh"), b"").unwrap();
        assert_eq!(
            remove_older_than(tmp.path(), Duration::from_secs(3600)).unwrap(),
            0
        );
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            remove_older_than(tmp.path(), Duration::from_millis(1)).unwrap(),
            1
        );
    }
}
