//! The lease writes of `session-start`, `subagent-stop`, and `session-end`
//! (`docs/architecture.md` section 9.8, "Writers"): the session's entry upserted in the lease
//! file of every worktree around the input's `cwd`.
//!
//! The agent never waits on cardinal (G4): a hook takes no lock but `leases/<key>.lock`, waits
//! for it at most until its deadline, and appends no record while it holds it, so it never
//! takes `records/.lock` before its lease write. A hook that cannot take the lock in time or
//! cannot write leaves the file as it was and returns a `hook_error` for its caller to append.
//! Nothing is written on Windows.

use std::path::Path;
use std::time::Instant;

use serde_json::{Map, Value};

use crate::contract::{LeaseAgent, LeaseEvent, RecordKind};
use crate::os;
use crate::state::{self, Durability, LockError, LockMode, StateDir, lease};
use crate::testpoint;

/// A record to append once every lease lock is released.
pub(super) struct Pending {
    pub kind: RecordKind,
    pub data: Map<String, Value>,
}

/// The input of one session hook.
pub(super) struct Input<'a> {
    pub kind: LeaseEvent,
    pub session_id: &'a str,
    pub cwd: &'a Path,
    pub transcript_path: Option<String>,
}

/// When a hook started, how long it waits for each lease lock, and its deadline.
#[derive(Clone, Copy, Debug)]
pub(super) struct Times {
    pub start: Instant,
    pub lock_deadline: Instant,
    /// Past it no lease is assessed or written: the previous file stays.
    pub deadline: Instant,
}

/// Upserts the session's entry in the lease of every worktree around `input.cwd`, waiting for
/// each lock until `times.lock_deadline` and assessing each file until `times.deadline`.
/// `linger_s` is `None` when the setting could not be read: then nothing but the session's own
/// entry changes. Returns the records to append. Nothing on `windows`.
pub(super) fn update(
    input: &Input<'_>,
    windows: bool,
    state: impl FnOnce() -> Option<StateDir>,
    linger_s: Option<u64>,
    times: Times,
) -> Vec<Pending> {
    if windows {
        return Vec::new();
    }
    let found = lease::worktrees_of(input.cwd);
    let mut pending = Vec::new();
    if !found.errors.is_empty() {
        pending.push(error(
            input.kind,
            "lease_discovery_incomplete",
            &found.errors.join("; "),
            None,
            None,
        ));
    }
    if found.roots.is_empty() {
        return pending;
    }
    let _ = testpoint::hit(testpoint::HOOK_LEASE_AGENT);
    let boot_id = os::boot_id();
    let event = lease::Event {
        session_id: input.session_id.to_owned(),
        kind: input.kind,
        agent: find_agent(boot_id.as_deref()),
        transcript_path: input.transcript_path.clone(),
        now_ms: state::now_ms(),
    };
    let Some(state) = state() else {
        pending.push(error(
            input.kind,
            "lease_write_failed",
            "the state dir cannot be opened",
            None,
            None,
        ));
        return pending;
    };
    for root in &found.roots {
        // No worktree is started past the deadline: a free lock would otherwise let the hook
        // read, upsert, and publish another lease after its budget.
        if std::time::Instant::now() >= times.deadline {
            pending.push(error(
                input.kind,
                "lease_write_failed",
                "the hook's budget ran out before this worktree",
                Some(root),
                None,
            ));
            continue;
        }
        let write = Write {
            state: &state,
            root,
            event: &event,
            linger_ms: linger_s.map(|s| s.saturating_mul(1000)),
        };
        // Each process is read again under each lock: nothing is decided from an older read.
        let mut live = lease::liveness(boot_id.as_deref(), lease::StartIdentity::HOST, probe);
        match write.run(times, &mut live, &mut pending) {
            Ok(()) => {}
            Err(Failure::LockTimeout) => {
                let waited = u64::try_from(times.start.elapsed().as_millis()).unwrap_or(u64::MAX);
                pending.push(error(
                    input.kind,
                    "lease_lock_timeout",
                    &format!(
                        "leases/{}.lock not taken in time",
                        state::worktree_key(root)
                    ),
                    Some(root),
                    Some(waited),
                ));
            }
            Err(Failure::Write(detail)) => {
                pending.push(error(
                    input.kind,
                    "lease_write_failed",
                    &detail,
                    Some(root),
                    None,
                ));
            }
        }
    }
    pending
}

/// One read of `pid`, now (section 9.8): what a lease decision may rest on.
fn probe(pid: u32) -> lease::Process {
    match os::probe(pid) {
        os::Probe::Gone => lease::Process::Gone,
        os::Probe::Running { start_s, start_us } => lease::Process::Running { start_s, start_us },
        os::Probe::Unknown => lease::Process::Unknown,
    }
}

enum Failure {
    LockTimeout,
    Write(String),
}

/// One worktree's lease update.
struct Write<'a> {
    state: &'a StateDir,
    root: &'a Path,
    event: &'a lease::Event,
    linger_ms: Option<u64>,
}

impl Write<'_> {
    fn run(
        &self,
        times: Times,
        live: lease::Liveness<'_>,
        pending: &mut Vec<Pending>,
    ) -> Result<(), Failure> {
        let key = state::worktree_key(self.root);
        let dir = self.state.leases_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|err| Failure::Write(format!("{}: {err}", dir.display())))?;
        let lock = state::lock(
            &self.state.lease_lock(&key),
            LockMode::Exclusive,
            times.lock_deadline,
        );
        let _lock = match lock {
            Ok(lock) => lock,
            Err(LockError::TimedOut(_)) => return Err(Failure::LockTimeout),
            Err(err) => return Err(Failure::Write(err.to_string())),
        };
        let path = self.state.lease(&key);
        let mut corrupt = None;
        let mut current = match lease::read(&path) {
            Ok(lease::Read::Missing) => lease::empty(self.root),
            Ok(lease::Read::Parsed(current)) => current,
            Ok(lease::Read::Corrupt { mtime_ms }) => {
                let mut data = Map::new();
                data.insert("worktree".into(), self.root.to_string_lossy().into());
                data.insert("path".into(), path.to_string_lossy().into());
                data.insert("mtime_ms".into(), mtime_ms.into());
                corrupt = Some(Pending {
                    kind: RecordKind::LeaseCorrupt,
                    data,
                });
                // Lingering from the repair, not from the event: the lock wait came between.
                lease::after_corrupt(self.root, mtime_ms, state::now_ms(), self.linger_ms)
            }
            Err(err) => return Err(Failure::Write(format!("{}: {err}", path.display()))),
        };
        lease::apply(&mut current.sessions, self.event, live);
        lease::prune(
            &mut current,
            Some(&self.event.session_id),
            self.event.now_ms,
            self.linger_ms,
            live,
            Some(times.deadline),
        )
        .map_err(|lease::Unfinished| {
            Failure::Write(format!(
                "{}: not assessed within the hook's budget",
                path.display()
            ))
        })?;
        let bytes = serde_json::to_vec(&current).map_err(|err| Failure::Write(err.to_string()))?;
        // A file no reader would read whole is never written: the previous one stays.
        if bytes.len() as u64 > lease::LEASE_MAX_BYTES {
            return Err(Failure::Write(format!(
                "{}: would be larger than {} bytes",
                path.display(),
                lease::LEASE_MAX_BYTES
            )));
        }
        // Nothing is published past the deadline, whatever the assessment skipped: the
        // previous file stays.
        if std::time::Instant::now() >= times.deadline {
            return Err(Failure::Write(format!(
                "{}: not published within the hook's budget",
                path.display()
            )));
        }
        testpoint::fake_write("lease")
            .map_err(|err| Failure::Write(format!("{}: {err}", path.display())))?;
        self.state
            .write_atomic(&path, &bytes, Durability::Volatile)
            .map_err(|err| Failure::Write(err.to_string()))?;
        // Reported only once the replacement is in place.
        pending.extend(corrupt);
        Ok(())
    }
}

/// A `hook_error` record of a lease update.
fn error(
    kind: LeaseEvent,
    reason: &str,
    detail: &str,
    worktree: Option<&Path>,
    waited_ms: Option<u64>,
) -> Pending {
    let mut data = Map::new();
    data.insert(
        "event".into(),
        serde_json::to_value(kind).unwrap_or_default(),
    );
    data.insert("reason".into(), reason.into());
    data.insert("detail".into(), detail.into());
    data.insert(
        "worktree".into(),
        worktree
            .map(|root| root.to_string_lossy().into_owned())
            .into(),
    );
    if let Some(waited_ms) = waited_ms {
        data.insert("waited_ms".into(), waited_ms.into());
    }
    Pending {
        kind: RecordKind::HookError,
        data,
    }
}

/// The hook's agent (section 9.8, "The agent"): its parent, or its parent's parent when the
/// parent is a shell, and only when that process's executable is named `claude` (on Linux the
/// file `/proc/<pid>/exe` names, which a process cannot rename); never PID 1, and none
/// without this boot's ID or a readable start time.
#[cfg(unix)]
fn find_agent(boot_id: Option<&str>) -> Option<LeaseAgent> {
    use super::session::is_shell;
    use crate::os::Detail;

    /// The name of Claude Code's executable, the only process a hook records as its agent.
    const AGENT_NAME: &str = "claude";

    let boot_id = boot_id?;
    let mut pid = std::os::unix::process::parent_id();
    for through_shell in [false, true] {
        if pid <= 1 {
            return None;
        }
        let process = os::process(pid, Detail::default()).filter(|p| !p.zombie)?;
        let name = if cfg!(target_os = "linux") {
            os::exe_name(pid)
        } else {
            Some(process.name.clone())
        };
        if name.as_deref() == Some(AGENT_NAME) {
            return (process.start_s != 0).then(|| LeaseAgent {
                pid,
                start_s: process.start_s,
                start_us: os::start_us(pid),
                boot_id: boot_id.to_owned(),
            });
        }
        if through_shell || !is_shell(&process.name) {
            return None;
        }
        pid = process.parent?;
    }
    None
}

#[cfg(not(unix))]
fn find_agent(_boot_id: Option<&str>) -> Option<LeaseAgent> {
    None
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Instant;

    use super::{Input, update};
    use crate::contract::LeaseEvent;

    /// Windows writes no lease (section 9.8): the hook returns before it looks for worktrees
    /// or opens the state dir, where elsewhere the same input in a worktree does.
    #[test]
    fn windows_writes_no_lease() {
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
        let input = Input {
            kind: LeaseEvent::SessionStart,
            session_id: "s",
            cwd,
            transcript_path: None,
        };
        let now = Instant::now();
        let wait = super::Times {
            start: now,
            lock_deadline: now,
            deadline: now,
        };
        let pending = update(
            &input,
            true,
            || panic!("the state dir was opened"),
            Some(1),
            wait,
        );
        assert!(pending.is_empty());
        // Control: elsewhere the hook goes on to the state dir, which here cannot be opened.
        let pending = update(&input, false, || None, Some(1), wait);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].data["reason"], "lease_write_failed");
    }
}
