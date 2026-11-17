//! IDs, keys, prefixes, worktree discovery, and time (`docs/architecture.md` sections 4.2 and
//! 4.6).

use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A new 16-hex-character ID from a randomly keyed `SipHash` of time, PID, and a counter. IDs
/// are claimed by exclusive creation; a collision retries with a new ID.
#[must_use]
pub fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::hash::RandomState::new().build_hasher();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    hasher.write_u128(nanos);
    hasher.write_u32(std::process::id());
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    format!("{:016x}", hasher.finish())
}

/// Whether `id` has the form of a cardinal ID (16 lowercase hex characters).
#[must_use]
pub fn is_id(id: &str) -> bool {
    id.len() == 16
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// FNV-1a 64. Hand-written so keys stay stable across Rust releases.
#[must_use]
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;
    bytes.iter().fold(OFFSET, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(PRIME)
    })
}

/// A key: FNV-1a 64 of `bytes` as 16 hex characters.
#[must_use]
pub fn key(bytes: &[u8]) -> String {
    format!("{:016x}", fnv1a64(bytes))
}

/// The worktree key of a canonical worktree path.
#[must_use]
pub fn worktree_key(canonical: &Path) -> String {
    key(&path_bytes(canonical))
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

/// The worktree of `cwd`: the nearest directory, walking up from the canonical `cwd`, that
/// has a `.git` entry (file or directory). `None` outside any worktree.
#[must_use]
pub fn find_worktree_root(cwd: &Path) -> Option<PathBuf> {
    let start = dunce::canonicalize(cwd).ok()?;
    start
        .ancestors()
        .find(|dir| std::fs::symlink_metadata(dir.join(".git")).is_ok())
        .map(Path::to_path_buf)
}

/// Why an ID prefix did not resolve.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PrefixError {
    #[error("`{0}` is shorter than 4 characters")]
    TooShort(String),
    #[error("no ID starts with `{0}`")]
    NotFound(String),
    #[error("`{0}` matches more than one ID")]
    Ambiguous(String),
}

impl From<PrefixError> for crate::contract::Error {
    fn from(err: PrefixError) -> Self {
        use crate::contract::ErrorCode;
        let code = match err {
            PrefixError::TooShort(_) => ErrorCode::InvalidArgument,
            PrefixError::NotFound(_) => ErrorCode::NotFound,
            PrefixError::Ambiguous(_) => ErrorCode::AmbiguousId,
        };
        Self::new(code, err.to_string())
    }
}

/// Resolves a unique prefix of at least 4 characters among `ids`. An exact match wins.
pub fn resolve_prefix<I, S>(ids: I, prefix: &str) -> Result<String, PrefixError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    if prefix.chars().count() < 4 {
        return Err(PrefixError::TooShort(prefix.to_owned()));
    }
    let mut found: Option<String> = None;
    for id in ids {
        let id = id.as_ref();
        if id == prefix {
            return Ok(id.to_owned());
        }
        if id.starts_with(prefix) {
            if found.is_some() {
                return Err(PrefixError::Ambiguous(prefix.to_owned()));
            }
            found = Some(id.to_owned());
        }
    }
    found.ok_or_else(|| PrefixError::NotFound(prefix.to_owned()))
}

/// Milliseconds since the Unix epoch.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{
        PrefixError, find_worktree_root, fnv1a64, is_id, key, new_id, resolve_prefix, worktree_key,
    };

    #[test]
    fn fnv1a64_matches_reference_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
        assert_eq!(key(b"a"), "af63dc4c8601ec8c");
    }

    #[test]
    fn ids_are_well_formed_and_distinct() {
        let ids: HashSet<String> = (0..1000).map(|_| new_id()).collect();
        assert_eq!(ids.len(), 1000);
        assert!(ids.iter().all(|id| is_id(id)));
        assert!(!is_id("0123456789ABCDEF"));
        assert!(!is_id("0123"));
    }

    #[test]
    fn prefixes() {
        let ids = ["abcd000000000001", "abcd000000000002", "ef01000000000000"];
        assert_eq!(resolve_prefix(ids, "ef01").unwrap(), ids[2]);
        assert_eq!(
            resolve_prefix(ids, "abcd0000000000").unwrap_err(),
            PrefixError::Ambiguous("abcd0000000000".into())
        );
        assert_eq!(resolve_prefix(ids, ids[0]).unwrap(), ids[0]);
        assert!(matches!(
            resolve_prefix(ids, "abc"),
            Err(PrefixError::TooShort(_))
        ));
        assert!(matches!(
            resolve_prefix(ids, "ffff"),
            Err(PrefixError::NotFound(_))
        ));
    }

    #[test]
    fn worktree_root_is_the_nearest_dot_git_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let nested = repo.join("sub/linked");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(nested.join("deep")).unwrap();
        std::fs::write(nested.join(".git"), "gitdir: elsewhere\n").unwrap();
        let canonical_repo = dunce::canonicalize(&repo).unwrap();
        let canonical_nested = dunce::canonicalize(&nested).unwrap();
        assert_eq!(
            find_worktree_root(&repo.join("sub")),
            Some(canonical_repo.clone())
        );
        assert_eq!(
            find_worktree_root(&nested.join("deep")),
            Some(canonical_nested)
        );
        assert_ne!(worktree_key(&canonical_repo), worktree_key(&nested));
        assert_eq!(worktree_key(&canonical_repo).len(), 16);
    }
}
