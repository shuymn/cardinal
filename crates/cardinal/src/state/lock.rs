//! File locks with deadlines (`docs/architecture.md` section 4.3).
//!
//! Locks are `std::fs::File` locks: `flock` on Linux and macOS, `LockFileEx` on Windows. The
//! OS drops them when the holder dies. std opens files close-on-exec on Unix and
//! non-inheritable on Windows, so a child never inherits a lock. Lock files are always empty
//! and separate from data files. Every acquisition has a deadline.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Shared or exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockMode {
    Shared,
    Exclusive,
}

/// A held lock. Dropping it closes the file, which releases the lock.
#[derive(Debug)]
pub struct FileLock {
    file: File,
    path: PathBuf,
    mode: LockMode,
}

impl FileLock {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn mode(&self) -> LockMode {
        self.mode
    }

    /// The open lock file, for callers that need its metadata.
    #[must_use]
    pub fn file(&self) -> &File {
        &self.file
    }
}

/// Why a lock could not be taken.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("timed out waiting for the lock {}", .0.display())]
    TimedOut(PathBuf),
    #[error("cannot lock {}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
}

/// The state of a lock file as seen by a probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// Another open file description holds it.
    Held,
    /// Nobody holds it.
    Free,
    /// The file does not exist (counts as free for liveness).
    Missing,
}

const BACKOFF_START: Duration = Duration::from_millis(5);
const BACKOFF_MAX: Duration = Duration::from_millis(50);

fn open(path: &Path, create: bool) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .open(path)
}

fn io_error(path: &Path) -> impl Fn(io::Error) -> LockError + Copy + '_ {
    move |source| LockError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn try_on(file: &File, mode: LockMode) -> Result<bool, io::Error> {
    let result = match mode {
        LockMode::Shared => file.try_lock_shared(),
        LockMode::Exclusive => file.try_lock(),
    };
    match result {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(err)) => Err(err),
    }
}

/// Takes a lock on `path` (created if missing), waiting until `deadline` with backoff
/// (5 ms doubling to 50 ms). At least one attempt is made even if the deadline has passed.
pub fn lock(path: &Path, mode: LockMode, deadline: Instant) -> Result<FileLock, LockError> {
    let io_err = io_error(path);
    let file = open(path, true).map_err(io_err)?;
    let mut delay = BACKOFF_START;
    loop {
        if try_on(&file, mode).map_err(io_err)? {
            return Ok(FileLock {
                file,
                path: path.to_path_buf(),
                mode,
            });
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(LockError::TimedOut(path.to_path_buf()));
        }
        std::thread::sleep(delay.min(deadline - now));
        delay = (delay * 2).min(BACKOFF_MAX);
    }
}

/// Tries once to take a lock on `path` (created if missing). `Ok(None)` means it is held.
pub fn try_lock(path: &Path, mode: LockMode) -> Result<Option<FileLock>, LockError> {
    let io_err = io_error(path);
    let file = open(path, true).map_err(io_err)?;
    Ok(try_on(&file, mode).map_err(io_err)?.then(|| FileLock {
        file,
        path: path.to_path_buf(),
        mode,
    }))
}

/// Creates `path` with `create_new` and takes it exclusively; for claiming IDs whose lock
/// marks liveness (`live/<job>.lock`). Fails with `AlreadyExists` if the file exists.
pub fn create_locked(path: &Path) -> Result<FileLock, LockError> {
    let io_err = io_error(path);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(io_err)?;
    if try_on(&file, LockMode::Exclusive).map_err(io_err)? {
        Ok(FileLock {
            file,
            path: path.to_path_buf(),
            mode: LockMode::Exclusive,
        })
    } else {
        Err(io_err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "a freshly created lock file is already locked",
        )))
    }
}

/// Probes whether someone holds `path` without creating it. A free lock is taken for the
/// instant of the probe and released before returning.
pub fn probe(path: &Path) -> Result<Probe, LockError> {
    let file = match open(path, false) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Probe::Missing),
        Err(source) => return Err(io_error(path)(source)),
    };
    let free = try_on(&file, LockMode::Exclusive).map_err(io_error(path))?;
    Ok(if free { Probe::Free } else { Probe::Held })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{LockError, LockMode, Probe, create_locked, lock, probe, try_lock};

    #[test]
    fn exclusive_excludes_and_shared_shares() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.lock");
        let shared1 = try_lock(&path, LockMode::Shared).unwrap().unwrap();
        let shared2 = try_lock(&path, LockMode::Shared).unwrap().unwrap();
        assert!(try_lock(&path, LockMode::Exclusive).unwrap().is_none());
        drop((shared1, shared2));
        let exclusive = try_lock(&path, LockMode::Exclusive).unwrap().unwrap();
        assert!(try_lock(&path, LockMode::Shared).unwrap().is_none());
        assert_eq!(probe(&path).unwrap(), Probe::Held);
        drop(exclusive);
        assert_eq!(probe(&path).unwrap(), Probe::Free);
        assert_eq!(
            probe(&tmp.path().join("missing.lock")).unwrap(),
            Probe::Missing
        );
        assert!(
            !tmp.path().join("missing.lock").exists(),
            "probe never creates"
        );
    }

    #[test]
    fn waiting_respects_the_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.lock");
        let held = lock(&path, LockMode::Exclusive, Instant::now()).unwrap();
        let start = Instant::now();
        let err = lock(&path, LockMode::Shared, start + Duration::from_millis(120)).unwrap_err();
        let waited = start.elapsed();
        assert!(matches!(err, LockError::TimedOut(_)), "{err}");
        assert!(waited >= Duration::from_millis(120), "{waited:?}");
        assert!(waited < Duration::from_secs(2), "{waited:?}");
        drop(held);
        lock(&path, LockMode::Shared, Instant::now()).unwrap();
    }

    #[test]
    fn waiter_gets_the_lock_once_released() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.lock");
        let held = lock(&path, LockMode::Exclusive, Instant::now()).unwrap();
        let waiter = {
            let path = path.clone();
            std::thread::spawn(move || {
                lock(
                    &path,
                    LockMode::Exclusive,
                    Instant::now() + Duration::from_secs(10),
                )
                .is_ok()
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        drop(held);
        assert!(waiter.join().unwrap());
    }

    #[test]
    fn create_locked_claims_once() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("job.lock");
        let held = create_locked(&path).unwrap();
        assert_eq!(probe(&path).unwrap(), Probe::Held);
        let again = create_locked(&path).unwrap_err();
        assert!(
            matches!(&again, LockError::Io { source, .. } if source.kind() == std::io::ErrorKind::AlreadyExists),
            "{again}"
        );
        drop(held);
        assert_eq!(probe(&path).unwrap(), Probe::Free);
    }
}
