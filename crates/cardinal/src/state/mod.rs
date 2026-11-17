//! The state directory (`docs/architecture.md` section 4): paths, the layout, atomic writes,
//! locks with deadlines, IDs and keys, and JSONL files.

mod fs;
mod id;
mod lock;

use std::io;
use std::path::{Path, PathBuf};

pub use fs::{
    Durability, append_line, create_new_durable, parse_jsonl, read_jsonl, remove_older_than,
    sync_parent, write_atomic,
};
pub use id::{
    PrefixError, find_worktree_root, fnv1a64, is_id, key, new_id, now_ms, resolve_prefix,
    worktree_key,
};
pub use lock::{FileLock, LockError, LockMode, Probe, create_locked, lock, probe, try_lock};

use crate::contract::{Error, ErrorCode};

/// The state layout version in `VERSION`.
pub const LAYOUT_VERSION: &str = "1";

/// A state directory error. Every variant maps to `state_unavailable`.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("{}: {message}", path.display())]
    Invalid { path: PathBuf, message: String },
}

impl From<StateError> for Error {
    fn from(err: StateError) -> Self {
        Error::new(ErrorCode::StateUnavailable, err.to_string())
    }
}

impl From<LockError> for Error {
    fn from(err: LockError) -> Self {
        Error::new(ErrorCode::StateUnavailable, err.to_string())
    }
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> StateError + '_ {
    move |source| StateError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// An opened state directory: its layout exists, `VERSION` matches, and `host-id` is known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateDir {
    root: PathBuf,
    host_id: String,
}

impl StateDir {
    /// Opens `root`, creating the layout, `VERSION`, and `host-id` if missing. The first
    /// process to create `VERSION` or `host-id` wins: a fully written temporary file is
    /// hard-linked into place, and a process that loses reads the winner's file.
    pub fn open(root: &Path) -> Result<Self, StateError> {
        if !root.is_absolute() {
            return Err(StateError::Invalid {
                path: root.to_path_buf(),
                message: "the state directory must be absolute".into(),
            });
        }
        let root = root.to_path_buf();
        for dir in [
            root.clone(),
            root.join("live"),
            root.join("reservations"),
            root.join("worktrees"),
            root.join("sessions"),
            root.join("inbox"),
            root.join("plans"),
            root.join("records"),
            root.join("tmp"),
        ] {
            std::fs::create_dir_all(&dir).map_err(io_err(&dir))?;
        }
        let tmp = root.join("tmp");

        let version = create_once(&tmp, &root.join("VERSION"), || {
            format!("{LAYOUT_VERSION}\n")
        })?;
        if version.trim() != LAYOUT_VERSION {
            return Err(StateError::Invalid {
                path: root.join("VERSION"),
                message: format!("layout version {} is not {LAYOUT_VERSION}", version.trim()),
            });
        }
        let host_id = create_once(&tmp, &root.join("host-id"), || format!("{}\n", new_id()))?;
        let host_id = host_id.trim().to_owned();
        if !is_id(&host_id) {
            return Err(StateError::Invalid {
                path: root.join("host-id"),
                message: "not 16 lowercase hex characters".into(),
            });
        }
        Ok(Self { root, host_id })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// This host's ID, created once with the state directory.
    #[must_use]
    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    /// `tmp/`: temporary files for atomic writes (same file system as the targets).
    #[must_use]
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// `admission.lock`: the admission critical section. Never deleted.
    #[must_use]
    pub fn admission_lock(&self) -> PathBuf {
        self.root.join("admission.lock")
    }

    #[must_use]
    pub fn live_dir(&self) -> PathBuf {
        self.root.join("live")
    }

    /// `live/<job>.lock`: held exclusively by the owning runner for its lifetime.
    #[must_use]
    pub fn live_lock(&self, job: &str) -> PathBuf {
        self.live_dir().join(format!("{job}.lock"))
    }

    #[must_use]
    pub fn reservations_dir(&self) -> PathBuf {
        self.root.join("reservations")
    }

    /// `reservations/<job>.json` (`cardinal.reservation.v1`).
    #[must_use]
    pub fn reservation(&self, job: &str) -> PathBuf {
        self.reservations_dir().join(format!("{job}.json"))
    }

    #[must_use]
    pub fn worktrees_dir(&self) -> PathBuf {
        self.root.join("worktrees")
    }

    /// `worktrees/<key>.lock`: shared = in use, exclusive = being reclaimed. Never deleted.
    #[must_use]
    pub fn worktree_lock(&self, key: &str) -> PathBuf {
        self.worktrees_dir().join(format!("{key}.lock"))
    }

    #[must_use]
    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    /// `sessions/<session>.json` (`cardinal.session-snapshot.v1`).
    #[must_use]
    pub fn session(&self, session: &str) -> PathBuf {
        self.sessions_dir()
            .join(format!("{}.json", file_safe(session)))
    }

    #[must_use]
    pub fn inbox_dir(&self) -> PathBuf {
        self.root.join("inbox")
    }

    /// `inbox/.lock`: serializes inbox upserts. Never deleted.
    #[must_use]
    pub fn inbox_lock(&self) -> PathBuf {
        self.inbox_dir().join(".lock")
    }

    /// `inbox/<key-hash>.json` for an inbox key; `probe` 0 is the plain hash, collisions use
    /// `-1`, `-2`, …
    #[must_use]
    pub fn inbox_item(&self, inbox_key: &str, probe: u32) -> PathBuf {
        let hash = key(inbox_key.as_bytes());
        let name = if probe == 0 {
            format!("{hash}.json")
        } else {
            format!("{hash}-{probe}.json")
        };
        self.inbox_dir().join(name)
    }

    #[must_use]
    pub fn plans_dir(&self) -> PathBuf {
        self.root.join("plans")
    }

    /// `plans/<plan>/`, created with `create_dir` to claim the plan ID.
    #[must_use]
    pub fn plan_dir(&self, plan: &str) -> PathBuf {
        self.plans_dir().join(plan)
    }

    /// `plans/<plan>/plan.json` (`cardinal.plan.v1`, immutable).
    #[must_use]
    pub fn plan_file(&self, plan: &str) -> PathBuf {
        self.plan_dir(plan).join("plan.json")
    }

    /// `plans/<plan>/claim`, created once by the `act` that executes the plan.
    #[must_use]
    pub fn plan_claim(&self, plan: &str) -> PathBuf {
        self.plan_dir(plan).join("claim")
    }

    /// `plans/<plan>/result.json` (`cardinal.act-result.v1`).
    #[must_use]
    pub fn plan_result(&self, plan: &str) -> PathBuf {
        self.plan_dir(plan).join("result.json")
    }

    #[must_use]
    pub fn records_dir(&self) -> PathBuf {
        self.root.join("records")
    }

    /// `records/records.jsonl`: the current record file.
    #[must_use]
    pub fn records_file(&self) -> PathBuf {
        self.records_dir().join("records.jsonl")
    }

    /// `records/records.<n>.jsonl`: rotated record files, `n` = 1 is the newest.
    #[must_use]
    pub fn rotated_records(&self, n: u32) -> PathBuf {
        self.records_dir().join(format!("records.{n}.jsonl"))
    }

    /// `records/.rotate.lock`: serializes rotation. Never deleted.
    #[must_use]
    pub fn rotate_lock(&self) -> PathBuf {
        self.records_dir().join(".rotate.lock")
    }

    /// `jobs/`: reserved for phase 2 accepted jobs; not created in phase 0.
    #[must_use]
    pub fn jobs_dir(&self) -> PathBuf {
        self.root.join("jobs")
    }

    /// `tickets/`: reserved for phase 2 queue tickets; not created in phase 0.
    #[must_use]
    pub fn tickets_dir(&self) -> PathBuf {
        self.root.join("tickets")
    }

    /// `reserve`: reserved for the phase 1 disk reserve file; not created in phase 0.
    #[must_use]
    pub fn reserve_file(&self) -> PathBuf {
        self.root.join("reserve")
    }

    /// Writes `bytes` to `target` atomically through `tmp/`.
    pub fn write_atomic(
        &self,
        target: &Path,
        bytes: &[u8],
        durability: Durability,
    ) -> Result<(), StateError> {
        write_atomic(&self.tmp_dir(), target, bytes, durability).map_err(io_err(target))
    }

    /// Removes files in `tmp/` older than one hour (run by `plan` and `inbox`).
    pub fn gc_tmp(&self) -> Result<usize, StateError> {
        let dir = self.tmp_dir();
        remove_older_than(&dir, std::time::Duration::from_secs(3600)).map_err(io_err(&dir))
    }
}

/// Session IDs come from hook input; anything outside `[A-Za-z0-9._-]` (and a leading `.`)
/// is replaced so the ID cannot name another path.
fn file_safe(value: &str) -> String {
    let mut safe: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() || safe.starts_with('.') {
        safe.insert(0, '_');
    }
    safe
}

/// Reads `path`, creating it first if missing: `contents()` is written to a temporary file
/// that is hard-linked into place; if another process won, its file is read instead.
fn create_once(
    tmp_dir: &Path,
    path: &Path,
    contents: impl FnOnce() -> String,
) -> Result<String, StateError> {
    match std::fs::read_to_string(path) {
        Ok(text) => return Ok(text),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_err(path)(source)),
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = tmp_dir.join(format!("{name}.{}.{}", std::process::id(), new_id()));
    std::fs::write(&tmp, contents()).map_err(io_err(&tmp))?;
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(source) => return Err(io_err(path)(source)),
    }
    std::fs::read_to_string(path).map_err(io_err(path))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::{StateDir, file_safe};

    #[test]
    fn open_creates_the_layout_once() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::open(tmp.path()).unwrap();
        for dir in [
            "live",
            "reservations",
            "worktrees",
            "sessions",
            "inbox",
            "plans",
            "records",
            "tmp",
        ] {
            assert!(tmp.path().join(dir).is_dir(), "{dir}");
        }
        for reserved in ["jobs", "tickets", "reserve"] {
            assert!(!tmp.path().join(reserved).exists(), "{reserved}");
        }
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("VERSION")).unwrap(),
            "1\n"
        );
        let again = StateDir::open(tmp.path()).unwrap();
        assert_eq!(state.host_id(), again.host_id());
        assert_eq!(std::fs::read_dir(state.tmp_dir()).unwrap().count(), 0);
    }

    #[test]
    fn concurrent_first_opens_agree_on_the_host_id() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let barrier = Arc::new(Barrier::new(8));
        let ids: Vec<String> = (0..8)
            .map(|_| {
                let (root, barrier) = (root.clone(), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    barrier.wait();
                    StateDir::open(&root).unwrap().host_id().to_owned()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert!(ids.windows(2).all(|pair| pair[0] == pair[1]), "{ids:?}");
    }

    #[test]
    fn wrong_version_or_relative_root_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("VERSION"), "2\n").unwrap();
        assert!(StateDir::open(tmp.path()).is_err());
        assert!(StateDir::open(std::path::Path::new("relative")).is_err());
    }

    #[test]
    fn session_ids_cannot_escape_the_sessions_dir() {
        assert_eq!(file_safe("abc-123_x.y"), "abc-123_x.y");
        assert_eq!(file_safe("../../etc/passwd"), "_.._.._etc_passwd");
        assert_eq!(file_safe(".."), "_..");
        assert_eq!(file_safe(""), "_");
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::open(tmp.path()).unwrap();
        assert_eq!(
            state.session("../x").parent().unwrap(),
            state.sessions_dir()
        );
    }

    #[test]
    fn inbox_item_paths_probe_on_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::open(tmp.path()).unwrap();
        let first = state.inbox_item("recovery_needed:abc", 0);
        let second = state.inbox_item("recovery_needed:abc", 1);
        assert_ne!(first, second);
        assert!(second.to_string_lossy().ends_with("-1.json"));
    }
}
