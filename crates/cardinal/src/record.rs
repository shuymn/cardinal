//! Records: appending, rotation, and reading (`docs/architecture.md` section 4.7).
//!
//! One `cardinal.record.v1` object per line, appended with a single `write`. An appender
//! holds `records/.lock` shared from opening the current file until its write is done, and
//! rotation takes it exclusively, so no rename or removal happens while an appender's
//! descriptor is open: a delayed appender cannot write into a file a later rotation has
//! replaced. When the current file exceeds the size limit, the appender that notices rotates
//! it, or leaves it to the next append when the lock stays busy. A reader that must not read a
//! record twice ([`open_files`]) holds the lock shared only while it opens the files and takes
//! their lengths, and reads those lengths afterwards: an append lands past them, and a
//! rotation renames a file without changing it. Readers skip lines that do not parse. A record
//! failure is never surfaced to a command and never causes a rerun: callers ignore the
//! returned error.

use std::fs::File;
use std::io::{self, Read as _};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::contract::{Record, RecordKind};
use crate::state::{self, LockError, LockMode, StateDir, now_ms};
use crate::testpoint;

/// How long an appender waits for a rotation to finish before giving up on its record.
const APPEND_WAIT: Duration = Duration::from_secs(1);

/// How long a rotation waits for appenders to finish their writes before leaving the
/// rotation to the next append.
const ROTATE_WAIT: Duration = Duration::from_millis(100);

/// Where and how records are written.
#[derive(Clone, Debug)]
pub struct Recorder {
    state: StateDir,
    max_bytes: u64,
    keep: u32,
}

/// Why a record could not be written.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error("cannot serialize the record: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("cannot append to {}: {source}", path.display())]
    Append { path: PathBuf, source: io::Error },
    #[error("cannot rotate records: {0}")]
    Rotate(io::Error),
}

impl Recorder {
    /// `max_bytes` and `keep` come from `[records]` in `host.toml`. `keep` counts the current
    /// file; values below 1 are treated as 1.
    #[must_use]
    pub fn new(state: &StateDir, max_bytes: u64, keep: u32) -> Self {
        Self {
            state: state.clone(),
            max_bytes,
            keep: keep.max(1),
        }
    }

    /// A record of `kind` stamped with this host and the current time.
    #[must_use]
    pub fn record(&self, kind: RecordKind) -> Record {
        Record::new(kind, self.state.host_id(), now_ms())
    }

    /// Appends `record`, then rotates if the current file has grown past the limit.
    pub fn append(&self, record: &Record) -> Result<(), RecordError> {
        self.append_until(record, Instant::now() + APPEND_WAIT)
    }

    /// [`Self::append`] waiting for a rotation, and rotating, only until `deadline` (and at most
    /// the usual waits): once it has passed, the lock is tried once and the record dropped if it
    /// is held. For a hook within its budget (section 9.8).
    pub fn append_until(&self, record: &Record, deadline: Instant) -> Result<(), RecordError> {
        let deadline = deadline.min(Instant::now() + APPEND_WAIT);
        let line = serde_json::to_vec(record)?;
        let path = self.state.records_file();
        let append_error = |source| RecordError::Append {
            path: path.clone(),
            source,
        };
        let shared = state::lock(&self.state.records_lock(), LockMode::Shared, deadline)
            .map_err(|err| append_error(io::Error::other(err)))?;
        let file = open_append(&path).map_err(append_error)?;
        testpoint::hit(testpoint::RECORD_OPENED)
            .map_err(|err| append_error(io::Error::other(err)))?;
        state::append_line(&file, &line).map_err(append_error)?;
        drop((file, shared));
        self.rotate_if_needed(deadline.min(Instant::now() + ROTATE_WAIT))
            .map_err(RecordError::Rotate)
    }

    fn rotate_if_needed(&self, deadline: Instant) -> io::Result<()> {
        let current = self.state.records_file();
        if !self.over_limit()? {
            return Ok(());
        }
        let lock = match state::lock(&self.state.records_lock(), LockMode::Exclusive, deadline) {
            Ok(lock) => lock,
            // Appenders are still writing or another appender is rotating; the next append
            // rotates.
            Err(LockError::TimedOut(_)) => return Ok(()),
            Err(err) => return Err(io::Error::other(err)),
        };
        // Someone may have rotated between the size check and the lock.
        if self.over_limit()? {
            if self.keep == 1 {
                // Moved out of the way first: Windows keeps a removed file's name while a reader
                // still has it open, and no append could create the file meanwhile.
                let removed = set_aside(&self.state);
                std::fs::rename(&current, &removed)?;
                std::fs::remove_file(&removed)?;
            } else {
                for n in (1..self.keep - 1).rev() {
                    rename_if_exists(
                        &self.state.rotated_records(n),
                        &self.state.rotated_records(n + 1),
                    )?;
                }
                std::fs::rename(&current, self.state.rotated_records(1))?;
            }
        }
        drop(lock);
        Ok(())
    }

    fn over_limit(&self) -> io::Result<bool> {
        match std::fs::metadata(self.state.records_file()) {
            Ok(meta) => Ok(meta.len() > self.max_bytes),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }
}

/// `path` opened for [`state::append_line`], created if missing, as a regular file only
/// (`os::open_regular`): a FIFO or a link left at `records.jsonl` is an error, never written
/// into or through.
fn open_append(path: &std::path::Path) -> io::Result<File> {
    crate::os::open_regular(
        std::fs::OpenOptions::new()
            .append(true)
            .read(true)
            .create(true),
        path,
    )
}

/// Where a rotation that keeps one file moves `records.jsonl` before it removes it: a new
/// temporary name in `tmp/`, which the cleanup of `tmp/` sweeps should the removal never come
/// (a crash between the two).
#[must_use]
pub fn set_aside(state: &StateDir) -> PathBuf {
    state
        .tmp_dir()
        .join(state::temp_name(std::ffi::OsStr::new("records.jsonl")))
}

fn rename_if_exists(from: &std::path::Path, to: &std::path::Path) -> io::Result<()> {
    match std::fs::rename(from, to) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Reads every record, oldest first: rotated files from the highest number down, then the
/// current file. Lines that do not parse as `cardinal.record.v1` are skipped.
pub fn read_all(state: &StateDir) -> io::Result<Vec<Record>> {
    let mut records = Vec::new();
    for path in files(state)? {
        records.extend(state::read_jsonl::<Record>(&path)?);
    }
    Ok(records)
}

/// A record file [`open_files`] opened, with the length it had then.
#[derive(Debug)]
pub struct OpenFile {
    pub path: PathBuf,
    file: File,
    len: u64,
}

impl OpenFile {
    /// The file's content up to the length it had when it was opened, whatever has been
    /// appended or rotated since.
    pub fn read(&self) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::with_capacity(usize::try_from(self.len).unwrap_or_default());
        (&self.file).take(self.len).read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

/// Opens the record files, newest first, and takes their lengths under `records/.lock`, held
/// shared as an appender holds it until `deadline`, so no rotation renames a file meanwhile
/// and no record is in two of them. The lock is released on return: reading the files, however
/// slowly, keeps no appender or rotation waiting. A listed file that cannot be opened, a
/// missing one included, comes with its error, so running out of descriptors costs the oldest
/// files; a missing current file is left out.
pub fn open_files(state: &StateDir, deadline: Instant) -> io::Result<Vec<io::Result<OpenFile>>> {
    let _no_rotation =
        state::lock(&state.records_lock(), LockMode::Shared, deadline).map_err(io::Error::other)?;
    let current = state.records_file();
    let listed = files(state).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("{}: {err}", state.records_dir().display()),
        )
    })?;
    let mut opened = Vec::new();
    for path in listed.into_iter().rev() {
        let open = File::open(&path).and_then(|file| {
            let len = file.metadata()?.len();
            Ok((file, len))
        });
        opened.push(match open {
            Ok((file, len)) => Ok(OpenFile { path, file, len }),
            Err(err) if err.kind() == io::ErrorKind::NotFound && path == current => continue,
            Err(err) => Err(io::Error::new(
                err.kind(),
                format!("{}: {err}", path.display()),
            )),
        });
    }
    Ok(opened)
}

/// The record files, oldest first: the rotated files `records/` holds, from the highest
/// number down, whatever `keep` is now, then the current file, which may not exist.
pub fn files(state: &StateDir) -> io::Result<Vec<PathBuf>> {
    let mut rotated: Vec<(u32, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(state.records_dir())? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(n) = name
            .strip_prefix("records.")
            .and_then(|rest| rest.strip_suffix(".jsonl"))
            .and_then(|n| n.parse::<u32>().ok())
        {
            rotated.push((n, entry.path()));
        }
    }
    rotated.sort_by_key(|(n, _)| std::cmp::Reverse(*n));
    Ok(rotated
        .into_iter()
        .map(|(_, path)| path)
        .chain(std::iter::once(state.records_file()))
        .collect())
}
