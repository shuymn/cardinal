//! The reserve's retry (`docs/architecture.md` section 9.7, "Space"): a write an execution or
//! its recovery needs that fails with `ENOSPC` or `EDQUOT` deletes the state dir's reserve file
//! and is retried once, at once, which narrows the window in which another writer can take the
//! freed space without closing it. Space is released only when a write needs it, right before
//! that write is tried again. A write that fails with no reserve left, or again after its
//! retry, fails with its error, and the caller stops where it is. Writing the reserve is a
//! pass's (section 10.4).

use std::io::{self, Write as _};
use std::path::Path;

use super::fs::{Op, logged, sync_dir, temp_name};

/// Whether `err` says the volume or the user's quota has no room: `ENOSPC` or `EDQUOT`.
#[must_use]
pub fn is_no_space(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
    )
}

/// Runs `write`; when it fails for lack of space, deletes the reserve file at `reserve` and,
/// once that has succeeded, runs `write` again, once. A missing reserve, or one that cannot be
/// deleted, leaves the first error. Any other error is returned as it is, the reserve kept.
pub fn retry<T>(reserve: &Path, mut write: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    match write() {
        Err(err) if is_no_space(&err) => {
            match logged(Op::Unlink, reserve, None, std::fs::remove_file(reserve)) {
                Ok(()) => write(),
                Err(_) => Err(err),
            }
        }
        done => done,
    }
}

/// Writes the reserve file at `reserve`, `size` bytes of written zeros, never sparse, owned by
/// the user with mode 0600: a new temporary file in `tmp_dir`, on the same volume, named as
/// [`write_atomic`](super::write_atomic) names its own so the cleanup of temporary files
/// removes one a killed pass left, written in blocks, synced, and renamed into place, with
/// the directory synced. A write that fails removes the temporary file it created and leaves
/// no reserve.
pub fn write(tmp_dir: &Path, reserve: &Path, size: u64) -> io::Result<()> {
    write_through(&staging(tmp_dir), reserve, size)
}

/// A new temporary name for the reserve in `tmp_dir`.
fn staging(tmp_dir: &Path) -> std::path::PathBuf {
    tmp_dir.join(temp_name(std::ffi::OsStr::new("reserve")))
}

/// [`write`] through the temporary file `tmp`. A `tmp` that exists already is not this
/// call's, and is left as it is.
fn write_through(tmp: &Path, reserve: &Path, size: u64) -> io::Result<()> {
    const BLOCK: usize = 1024 * 1024;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(tmp)?;
    let filled = (|| {
        let block = vec![0_u8; BLOCK];
        let mut left = size;
        while left > 0 {
            let n = usize::try_from(left).map_or(BLOCK, |left| left.min(BLOCK));
            file.write_all(&block[..n])?;
            left -= n as u64;
        }
        file.sync_all()
    })();
    drop(file);
    if let Err(err) = filled.and_then(|()| std::fs::rename(tmp, reserve)) {
        let _ = std::fs::remove_file(tmp);
        return Err(err);
    }
    match reserve.parent() {
        Some(dir) => sync_dir(dir),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io;

    use super::{is_no_space, retry, staging, write, write_through};

    /// A reserve's temporary file that a killed pass left is one the cleanup of temporary
    /// files removes once it is old.
    #[test]
    fn an_abandoned_reserve_is_collected() {
        let tmp = tempfile::tempdir().unwrap();
        let left = staging(tmp.path());
        std::fs::write(&left, vec![0_u8; 4096]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let removed = crate::state::remove_temp_files_older_than(
            tmp.path(),
            std::time::Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(removed, 1);
        assert!(!left.exists());
    }

    /// A temporary name that exists already is another's: the write fails and leaves it.
    #[test]
    fn a_taken_staging_name_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let taken = staging(tmp.path());
        std::fs::write(&taken, b"theirs").unwrap();
        let reserve = tmp.path().join("reserve");
        assert!(write_through(&taken, &reserve, 4096).is_err());
        assert_eq!(std::fs::read(&taken).unwrap(), b"theirs");
        assert!(!reserve.exists());
    }

    /// The reserve is the size asked for, of written blocks, not sparse, and leaves no
    /// temporary file; one that cannot be written leaves no reserve and no temporary file.
    #[test]
    fn the_reserve_is_written_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let reserve = tmp.path().join("reserve");
        let size = 3 * 1024 * 1024 + 512;
        write(tmp.path(), &reserve, size).unwrap();
        let meta = std::fs::metadata(&reserve).unwrap();
        assert_eq!(meta.len(), size);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
            assert!(meta.blocks() * 512 >= size, "{} blocks", meta.blocks());
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
        let names: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["reserve"]);

        let missing = tmp.path().join("missing");
        let elsewhere = tmp.path().join("missing/reserve");
        assert!(write(tmp.path(), &elsewhere, 4096).is_err());
        assert!(!missing.exists());
        let names: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["reserve"]);
    }

    fn full() -> io::Error {
        io::Error::new(io::ErrorKind::StorageFull, "no space")
    }

    /// A write that fails for lack of space deletes the reserve and runs once more, at once;
    /// a second failure is the result, and the reserve is gone either way.
    #[test]
    fn a_full_volume_spends_the_reserve_and_retries_once() {
        let tmp = tempfile::tempdir().unwrap();
        let reserve = tmp.path().join("reserve");
        for (fails, ok) in [(1, true), (2, false)] {
            std::fs::write(&reserve, vec![0_u8; 4096]).unwrap();
            let calls = Cell::new(0);
            let done = retry(&reserve, || {
                calls.set(calls.get() + 1);
                if calls.get() <= fails {
                    Err(full())
                } else {
                    Ok(())
                }
            });
            assert_eq!(done.is_ok(), ok, "{fails} failures");
            assert_eq!(calls.get(), 2, "{fails} failures");
            assert!(!reserve.exists(), "{fails} failures");
        }
    }

    /// Without a reserve the first failure stands and nothing is retried; an error that is not
    /// about space neither spends the reserve nor retries.
    #[test]
    fn nothing_to_spend_or_nothing_to_spend_it_on() {
        let tmp = tempfile::tempdir().unwrap();
        let reserve = tmp.path().join("reserve");
        let calls = Cell::new(0);
        let err = retry(&reserve, || -> io::Result<()> {
            calls.set(calls.get() + 1);
            Err(io::Error::new(io::ErrorKind::QuotaExceeded, "quota"))
        })
        .unwrap_err();
        assert!(is_no_space(&err));
        assert_eq!(calls.get(), 1);

        std::fs::write(&reserve, b"r").unwrap();
        let calls = Cell::new(0);
        let err = retry(&reserve, || -> io::Result<()> {
            calls.set(calls.get() + 1);
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        })
        .unwrap_err();
        assert!(!is_no_space(&err));
        assert_eq!(calls.get(), 1);
        assert!(reserve.exists());
    }

    /// The kernel's own `ENOSPC` (28) and `EDQUOT` (122 on Linux, 69 on macOS) count as no
    /// space.
    #[cfg(unix)]
    #[test]
    fn the_kernel_errors_count() {
        let edquot = if cfg!(target_os = "linux") { 122 } else { 69 };
        for errno in [28, edquot] {
            assert!(is_no_space(&io::Error::from_raw_os_error(errno)), "{errno}");
        }
        assert!(!is_no_space(&io::Error::from_raw_os_error(1)));
    }
}
