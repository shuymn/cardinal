//! Records: appending, rotation, and reading (`docs/architecture.md` section 4.7).
//!
//! One `cardinal.record.v1` object per line, appended with a single `write`. An appender
//! holds `records/.lock` shared from opening the current file until its write is done, and
//! rotation takes it exclusively, so no rename or removal happens while a descriptor is open:
//! a delayed appender cannot write into a file a later rotation has replaced. When the
//! current file exceeds the size limit, the appender that notices rotates it, or leaves it to
//! the next append when the lock stays busy. Readers skip lines that do not parse. A record
//! failure is never surfaced to a command and never causes a rerun: callers ignore the
//! returned error.

use std::io;
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
        let line = serde_json::to_vec(record)?;
        let path = self.state.records_file();
        let append_error = |source| RecordError::Append {
            path: path.clone(),
            source,
        };
        let shared = state::lock(
            &self.state.records_lock(),
            LockMode::Shared,
            Instant::now() + APPEND_WAIT,
        )
        .map_err(|err| append_error(io::Error::other(err)))?;
        let file = state::open_append(&path).map_err(append_error)?;
        testpoint::hit(testpoint::RECORD_OPENED)
            .map_err(|err| append_error(io::Error::other(err)))?;
        state::append_line(&file, &line).map_err(append_error)?;
        drop((file, shared));
        self.rotate_if_needed().map_err(RecordError::Rotate)
    }

    fn rotate_if_needed(&self) -> io::Result<()> {
        let current = self.state.records_file();
        if !self.over_limit()? {
            return Ok(());
        }
        let lock = match state::lock(
            &self.state.records_lock(),
            LockMode::Exclusive,
            Instant::now() + ROTATE_WAIT,
        ) {
            Ok(lock) => lock,
            // Appenders are still writing or another appender is rotating; the next append
            // rotates.
            Err(LockError::TimedOut(_)) => return Ok(()),
            Err(err) => return Err(io::Error::other(err)),
        };
        // Someone may have rotated between the size check and the lock.
        if self.over_limit()? {
            if self.keep == 1 {
                std::fs::remove_file(&current)?;
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
