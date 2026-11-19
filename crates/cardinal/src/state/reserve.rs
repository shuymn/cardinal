//! The reserve's retry (`docs/architecture.md` section 9.7, "Space"): a write an execution or
//! its recovery needs that fails with `ENOSPC` or `EDQUOT` deletes the state dir's reserve file
//! and is retried once, at once, which narrows the window in which another writer can take the
//! freed space without closing it. Space is released only when a write needs it, right before
//! that write is tried again. A write that fails with no reserve left, or again after its
//! retry, fails with its error, and the caller stops where it is. Writing the reserve is a
//! pass's (section 10.4).

use std::io;
use std::path::Path;

use super::fs::{Op, logged};

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

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io;

    use super::{is_no_space, retry};

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
