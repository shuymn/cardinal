//! The state directory (`docs/architecture.md` section 4): paths, the layout, atomic writes,
//! locks with deadlines, IDs and keys, and JSONL files.

#[cfg(test)]
mod fnv_vectors;
mod fs;
mod id;
mod lock;

use std::io;
use std::path::{Path, PathBuf};

pub(crate) use fs::temp_name;
pub use fs::{
    Durability, append_line, create_new_durable, open_append, parse_jsonl, read_jsonl,
    remove_temp_files_older_than, sync_parent, write_atomic,
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
            root.join("reconcile"),
            root.join("disk"),
            root.join("arbiter"),
            root.join("ui"),
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

    /// `executions/`: one directory per plan an execution worked on (section 9.7). Unit X
    /// creates it; the first release does not.
    #[must_use]
    pub fn executions_dir(&self) -> PathBuf {
        self.root.join("executions")
    }

    /// `executions/<plan>/`: an execution's records.
    #[must_use]
    pub fn execution_dir(&self, plan: &str) -> PathBuf {
        self.executions_dir().join(plan)
    }

    /// `executions/<plan>/act.lock`: empty; held by the `act` or the recovery working on the
    /// plan's execution. Removed only with its directory (section 4.3).
    #[must_use]
    pub fn execution_lock(&self, plan: &str) -> PathBuf {
        self.execution_dir(plan).join("act.lock")
    }

    /// `executions/<plan>/manifest` (`cardinal.act-manifest.v1`).
    #[must_use]
    pub fn execution_manifest(&self, plan: &str) -> PathBuf {
        self.execution_dir(plan).join("manifest")
    }

    /// `executions/<plan>/claim` (`cardinal.act-claim.v1`).
    #[must_use]
    pub fn execution_claim(&self, plan: &str) -> PathBuf {
        self.execution_dir(plan).join("claim")
    }

    /// `executions/<plan>/result.json` (`cardinal.act-result.v1`).
    #[must_use]
    pub fn execution_result(&self, plan: &str) -> PathBuf {
        self.execution_dir(plan).join("result.json")
    }

    /// `trash/`: where an execution moves its targets, on the state dir's mount (section
    /// 9.7). Unit X creates it; the first release does not.
    #[must_use]
    pub fn trash_dir(&self) -> PathBuf {
        self.root.join("trash")
    }

    /// `trash/<plan>/`: the trash entry an execution owns.
    #[must_use]
    pub fn trash_entry(&self, plan: &str) -> PathBuf {
        self.trash_dir().join(plan)
    }

    /// `trash/<plan>/t`: the target as moved out of its worktree.
    #[must_use]
    pub fn trash_target(&self, plan: &str) -> PathBuf {
        self.trash_entry(plan).join("t")
    }

    /// `trash/<plan>/h`: the hold the target's entries pass through while they are deleted.
    #[must_use]
    pub fn trash_hold(&self, plan: &str) -> PathBuf {
        self.trash_entry(plan).join("h")
    }

    /// `reconcile.lock`: one reconcile pass at a time. Never deleted.
    #[must_use]
    pub fn reconcile_lock(&self) -> PathBuf {
        self.root.join("reconcile.lock")
    }

    #[must_use]
    pub fn reconcile_dir(&self) -> PathBuf {
        self.root.join("reconcile")
    }

    /// `reconcile/state.json` (`cardinal.reconcile-state.v1`): pass times. Written under
    /// `reconcile.lock`.
    #[must_use]
    pub fn reconcile_state(&self) -> PathBuf {
        self.reconcile_dir().join("state.json")
    }

    /// `reconcile/observation.json` (`cardinal.inventory.v1`): the last full observation.
    /// Written under `reconcile.lock`.
    #[must_use]
    pub fn reconcile_observation(&self) -> PathBuf {
        self.reconcile_dir().join("observation.json")
    }

    #[must_use]
    pub fn disk_dir(&self) -> PathBuf {
        self.root.join("disk")
    }

    /// `disk/<volume>.json` (`cardinal.disk-state.v1`), `volume` the key of a volume's
    /// identity (section 10.4). Written under `reconcile.lock`.
    #[must_use]
    pub fn disk_state(&self, volume: &str) -> PathBuf {
        self.disk_dir().join(format!("{}.json", file_safe(volume)))
    }

    #[must_use]
    pub fn arbiter_dir(&self) -> PathBuf {
        self.root.join("arbiter")
    }

    /// `arbiter/.lock`: held by the arbiter launcher (section 10.5). Never deleted.
    #[must_use]
    pub fn arbiter_lock(&self) -> PathBuf {
        self.arbiter_dir().join(".lock")
    }

    /// `arbiter/last.json` (`cardinal.arbiter-pass.v1`): the last arbiter pass. Written under
    /// `arbiter/.lock`.
    #[must_use]
    pub fn arbiter_last(&self) -> PathBuf {
        self.arbiter_dir().join("last.json")
    }

    /// `arbiter/<pass>.log`: an arbiter pass's output, capped.
    #[must_use]
    pub fn arbiter_log(&self, pass: &str) -> PathBuf {
        self.arbiter_dir().join(format!("{}.log", file_safe(pass)))
    }

    #[must_use]
    pub fn ui_dir(&self) -> PathBuf {
        self.root.join("ui")
    }

    /// `ui/index.html`: the read-only web UI's page (section 10.6).
    #[must_use]
    pub fn ui_page(&self) -> PathBuf {
        self.ui_dir().join("index.html")
    }

    /// `ui/snapshot.json` (`cardinal.snapshot.v1`), the snapshot the page embeds.
    #[must_use]
    pub fn ui_snapshot(&self) -> PathBuf {
        self.ui_dir().join("snapshot.json")
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

    /// `records/.lock`: appenders hold it shared from opening the current file until their
    /// write is done; rotation takes it exclusively. Never deleted.
    #[must_use]
    pub fn records_lock(&self) -> PathBuf {
        self.records_dir().join(".lock")
    }

    /// `jobs/`: reserved for phase 2 accepted jobs; not created.
    #[must_use]
    pub fn jobs_dir(&self) -> PathBuf {
        self.root.join("jobs")
    }

    /// `tickets/`: reserved for phase 2 queue tickets; not created.
    #[must_use]
    pub fn tickets_dir(&self) -> PathBuf {
        self.root.join("tickets")
    }

    /// `reserve`: the reserve file of the state dir's volume (section 9.7). Unit X writes it;
    /// the first release does not.
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

    /// Removes the temporary files of [`write_atomic`] in `tmp/` older than one hour, and no
    /// other file (run by `plan`, `inbox`, and `reconcile`).
    pub fn gc_tmp(&self) -> Result<usize, StateError> {
        let dir = self.tmp_dir();
        remove_temp_files_older_than(&dir, std::time::Duration::from_secs(3600))
            .map_err(io_err(&dir))
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
    let tmp = tmp_dir.join(temp_name(path.file_name().unwrap_or_default()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(io_err(&tmp))?;
    let written = std::io::Write::write_all(&mut file, contents().as_bytes());
    drop(file);
    let linked = written.map(|()| std::fs::hard_link(&tmp, path));
    // The file created above, which no other writer has.
    let _ = std::fs::remove_file(&tmp);
    match linked.map_err(io_err(&tmp))? {
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
            "reconcile",
            "disk",
            "arbiter",
            "ui",
            "records",
            "tmp",
        ] {
            assert!(tmp.path().join(dir).is_dir(), "{dir}");
        }
        for reserved in ["jobs", "tickets", "reserve", "executions", "trash"] {
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

    /// Every path of the layout in `docs/architecture.md` section 4.1, relative to the root.
    #[test]
    fn paths_follow_the_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::open(tmp.path()).unwrap();
        let rel = |path: std::path::PathBuf| {
            path.strip_prefix(tmp.path())
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        };
        let job = "0123456789abcdef";
        for (path, expected) in [
            (state.admission_lock(), "admission.lock"),
            (state.live_lock(job), "live/0123456789abcdef.lock"),
            (state.reservation(job), "reservations/0123456789abcdef.json"),
            (state.worktree_lock(job), "worktrees/0123456789abcdef.lock"),
            (state.session("s"), "sessions/s.json"),
            (state.inbox_lock(), "inbox/.lock"),
            (state.plan_file(job), "plans/0123456789abcdef/plan.json"),
            (
                state.execution_lock(job),
                "executions/0123456789abcdef/act.lock",
            ),
            (
                state.execution_manifest(job),
                "executions/0123456789abcdef/manifest",
            ),
            (
                state.execution_claim(job),
                "executions/0123456789abcdef/claim",
            ),
            (
                state.execution_result(job),
                "executions/0123456789abcdef/result.json",
            ),
            (state.trash_target(job), "trash/0123456789abcdef/t"),
            (state.trash_hold(job), "trash/0123456789abcdef/h"),
            (state.reconcile_lock(), "reconcile.lock"),
            (state.reconcile_state(), "reconcile/state.json"),
            (state.reconcile_observation(), "reconcile/observation.json"),
            (state.disk_state(job), "disk/0123456789abcdef.json"),
            (state.arbiter_lock(), "arbiter/.lock"),
            (state.arbiter_last(), "arbiter/last.json"),
            (state.arbiter_log(job), "arbiter/0123456789abcdef.log"),
            (state.ui_page(), "ui/index.html"),
            (state.ui_snapshot(), "ui/snapshot.json"),
            (state.records_file(), "records/records.jsonl"),
            (state.rotated_records(1), "records/records.1.jsonl"),
            (state.records_lock(), "records/.lock"),
            (state.jobs_dir(), "jobs"),
            (state.tickets_dir(), "tickets"),
            (state.reserve_file(), "reserve"),
        ] {
            assert_eq!(rel(path), expected);
        }
        // Names that come from outside stay inside their directory.
        assert_eq!(
            state.arbiter_log("../x").parent().unwrap(),
            state.arbiter_dir()
        );
        assert_eq!(state.disk_state("../x").parent().unwrap(), state.disk_dir());
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
