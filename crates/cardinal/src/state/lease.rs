//! Session leases (`docs/architecture.md` section 9.8): the lease file of one worktree, what
//! holds its entries, the upsert of one hook event, the fold over the cap, and the expiry the
//! inbox refresh runs.
//!
//! The file is replaced atomically under `leases/<key>.lock` (section 4.2, without fsync), so a
//! reader that reads it by name, without the lock, gets the old or the new file. Whether an
//! agent process is alive is the caller's to say ([`Liveness`], from a read of each PID taken
//! under the lock): this module reads files only. Every input it cannot read whole is unknown,
//! and an entry whose hold is unknown is kept.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::contract::{
    LEASE_MAX_ENTRIES, LeaseAgent, LeaseEntry, LeaseEvent, SessionLease, SessionLeaseSchema,
};

/// The most files of a session's `subagents/` directory read; more is unknown.
pub const MAX_SUBAGENT_FILES: usize = 4096;

/// The session ID of the entry a fold makes of lingering entries.
pub const FOLDED: &str = "cardinal:folded";

/// The largest lease file read; a larger one is neither assessed nor written (section 9.8).
/// Derived from the cap: an entry's fixed fields take under 300 bytes of JSON, its session ID
/// is a UUID and its transcript path at most `PATH_MAX` (4,096 bytes on Linux, 1,024 on macOS),
/// so [`LEASE_MAX_ENTRIES`] entries take under 4.5 MiB; twice that leaves room for escaped
/// characters. Read and parsed whole, it takes tens of milliseconds, within a hook's budget.
pub const LEASE_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Whether an agent process is alive with its recorded start time in its recorded boot:
/// `Some(true)` or `Some(false)`, `None` when it cannot be decided.
pub type Liveness<'a> = &'a mut dyn FnMut(&LeaseAgent) -> Option<bool>;

/// What one read of a PID, taken now, found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Process {
    /// No process has the PID, or it has exited.
    Gone,
    /// A running process with this start time (Unix seconds) and, when it could be read, its
    /// start time at the OS's finest resolution (`LeaseAgent::start_us`'s measure).
    Running { start_s: u64, start_us: Option<u64> },
    /// It could not be read whole, a start time of 0 included.
    Unknown,
}

/// Which start time tells a process from another one that took its PID in the same boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartIdentity {
    /// `start_us` when both reads have it, else `start_s` (macOS: both are the start time the
    /// kernel stored at the process's start, `pbi_start_tvsec` and `pbi_start_tvusec`).
    Seconds,
    /// `start_us` alone, unknown without it (Linux: `start_us` counts clock ticks from boot,
    /// while `start_s` adds them to `btime`, which moves when the wall clock is stepped).
    BootRelative,
}

impl StartIdentity {
    /// This platform's.
    pub const HOST: Self = if cfg!(target_os = "linux") {
        Self::BootRelative
    } else {
        Self::Seconds
    };

    /// Whether `a` and `b` started at the same time; `None` when this identity cannot tell.
    fn same(self, a: (u64, Option<u64>), b: (u64, Option<u64>)) -> Option<bool> {
        match (self, a.1, b.1) {
            (_, Some(a_us), Some(b_us)) => Some(a_us == b_us),
            (Self::BootRelative, _, _) => None,
            (Self::Seconds, _, _) if a.0 == 0 || b.0 == 0 => None,
            (Self::Seconds, _, _) => Some(a.0 == b.0),
        }
    }
}

/// The liveness of agents from this boot's ID and `probe`, a read of one PID taken when it is
/// called, each PID read once: an agent of another boot is not alive, one whose PID is gone or
/// runs with another start time by `identity` is not either, and anything else is undecided.
/// Without this boot's ID nothing is decided. Made under the lease lock for each file, so no
/// decision rests on a read from before the lock.
pub fn liveness(
    boot_id: Option<&str>,
    identity: StartIdentity,
    mut probe: impl FnMut(u32) -> Process,
) -> impl FnMut(&LeaseAgent) -> Option<bool> {
    let mut seen: BTreeMap<u32, Process> = BTreeMap::new();
    move |agent| {
        let boot_id = boot_id?;
        if agent.boot_id != boot_id {
            return Some(false);
        }
        match *seen.entry(agent.pid).or_insert_with(|| probe(agent.pid)) {
            Process::Gone => Some(false),
            Process::Unknown => None,
            Process::Running { start_s, start_us } => {
                identity.same((start_s, start_us), (agent.start_s, agent.start_us))
            }
        }
    }
}

/// What a lease file holds, as a writer or the refresh reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Read {
    /// No file: proves nothing by itself.
    Missing,
    Parsed(SessionLease),
    /// A file that does not parse, or one over the cap without the `overflow` marker, with its
    /// mtime when it can be read.
    Corrupt {
        mtime_ms: Option<u64>,
    },
}

/// Reads the lease file at `path`. An error other than `ENOENT` is returned: such a file is
/// neither missing nor known to be corrupt. So is a file larger than [`LEASE_MAX_BYTES`],
/// which is never read whole, and anything but a regular file: the path is `lstat`ed before
/// the open, so a link or a FIFO left at a lease's path is never opened and a hook never
/// blocks on it (writers only ever rename regular files into place), and the opened file is
/// checked again.
pub fn read(path: &Path) -> io::Result<Read> {
    use std::io::Read as _;

    let not_regular = || io::Error::other(format!("{}: not a regular file", path.display()));
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(not_regular()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Read::Missing),
        Err(err) => return Err(err),
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Read::Missing),
        Err(err) => return Err(err),
    };
    if !file.metadata()?.is_file() {
        return Err(not_regular());
    }
    let too_large = || {
        io::Error::other(format!(
            "{}: larger than {LEASE_MAX_BYTES} bytes",
            path.display()
        ))
    };
    if file.metadata()?.len() > LEASE_MAX_BYTES {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    file.take(LEASE_MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LEASE_MAX_BYTES {
        return Err(too_large());
    }
    Ok(match serde_json::from_slice::<SessionLease>(&bytes) {
        Ok(lease) if lease.overflow || lease.sessions.len() <= LEASE_MAX_ENTRIES => {
            Read::Parsed(lease)
        }
        _ => Read::Corrupt {
            mtime_ms: std::fs::symlink_metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
                .map(system_ms),
        },
    })
}

fn system_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// How an entry holds its worktree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hold {
    /// Its agent is alive with the recorded start time and boot ID, and it has not ended.
    Live,
    /// Less than `linger_s` after its last activity.
    Lingering,
    /// Neither: a writer or the refresh drops it.
    Expired,
    /// An input could not be read whole: kept.
    Unknown,
}

/// An entry's hold, with what it was decided from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Assessment {
    pub hold: Hold,
    /// The entry's liveness: `Some(false)` for an entry without an agent or one that ended.
    pub live: Option<bool>,
    /// Its last activity in milliseconds, at most `now_ms`; `None` when unknown.
    pub activity_ms: Option<u64>,
}

/// Assesses `entry` at `now_ms`. An `uncertain` entry lingers from `last_event_ms` alone; any
/// other lingers from the latest of `last_event_ms`, `ended_ms`, the transcript's mtime, and
/// the newest mtime among its subagents' transcripts. Times in the future count as now.
pub fn assess(entry: &LeaseEntry, now_ms: u64, linger_ms: u64, live: Liveness<'_>) -> Assessment {
    let lingers = |activity_ms: u64| now_ms < activity_ms.saturating_add(linger_ms);
    if entry.uncertain {
        let activity_ms = entry.last_event_ms.min(now_ms);
        return Assessment {
            hold: if lingers(activity_ms) {
                Hold::Lingering
            } else {
                Hold::Expired
            },
            live: Some(false),
            activity_ms: Some(activity_ms),
        };
    }
    let is_live = match (&entry.agent, entry.ended_ms) {
        (Some(agent), None) => live(agent),
        _ => Some(false),
    };
    let activity_ms = last_activity(entry).ok().map(|ms| ms.min(now_ms));
    let hold = if is_live == Some(true) {
        Hold::Live
    } else {
        match activity_ms {
            None => Hold::Unknown,
            Some(ms) if lingers(ms) => Hold::Lingering,
            Some(_) if is_live.is_none() => Hold::Unknown,
            Some(_) => Hold::Expired,
        }
    };
    Assessment {
        hold,
        live: is_live,
        activity_ms,
    }
}

/// The latest of `last_event_ms`, `ended_ms`, the transcript's mtime, and the newest mtime in
/// `<transcript without .jsonl>/subagents/`. An error names the input that could not be read
/// whole; a missing transcript or directory adds nothing.
pub fn last_activity(entry: &LeaseEntry) -> Result<u64, String> {
    let mut latest = entry.last_event_ms.max(entry.ended_ms.unwrap_or(0));
    let Some(transcript) = entry.transcript_path.as_deref().map(Path::new) else {
        return Ok(latest);
    };
    if !transcript.is_absolute() {
        return Ok(latest);
    }
    if let Some(ms) = mtime(transcript)? {
        latest = latest.max(ms);
    }
    let subagents = transcript.with_extension("").join("subagents");
    let entries = match std::fs::read_dir(&subagents) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(latest),
        Err(err) => return Err(format!("{}: {err}", subagents.display())),
    };
    let mut files = 0;
    for entry in entries {
        let entry = entry.map_err(|err| format!("{}: {err}", subagents.display()))?;
        files += 1;
        if files > MAX_SUBAGENT_FILES {
            return Err(format!(
                "{}: more than {MAX_SUBAGENT_FILES} files",
                subagents.display()
            ));
        }
        if let Some(ms) = mtime(&entry.path())? {
            latest = latest.max(ms);
        }
    }
    Ok(latest)
}

/// The mtime of `path` by `lstat`; `None` when it does not exist.
fn mtime(path: &Path) -> Result<Option<u64>, String> {
    match std::fs::symlink_metadata(path).and_then(|meta| meta.modified()) {
        Ok(time) => Ok(Some(system_ms(time))),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("{}: {err}", path.display())),
    }
}

/// One hook event for one session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub session_id: String,
    pub kind: LeaseEvent,
    /// The agent the hook found, with this boot's ID; `None` when it found none.
    pub agent: Option<LeaseAgent>,
    pub transcript_path: Option<String>,
    pub now_ms: u64,
}

/// How generation `new` stands to the entry's `old`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Generation {
    Newer,
    Same,
    /// Another PID started at the same time, as finely as both were read.
    Tie,
    Older,
}

/// Orders `new` against `old` (section 9.8, "Generations"): another boot ID is a later boot,
/// since a hook always records the current one; in one boot the later start is the newer,
/// compared in microseconds when both were read so, else in seconds. The same PID is the same
/// generation only at the same start, as finely as both were read.
fn generation(new: &LeaseAgent, old: &LeaseAgent) -> Generation {
    if new.boot_id != old.boot_id {
        return Generation::Newer;
    }
    let same_start = match (new.start_us, old.start_us) {
        (Some(new_us), Some(old_us)) => new_us == old_us,
        _ => new.start_s == old.start_s,
    };
    if new.pid == old.pid && same_start {
        return Generation::Same;
    }
    let order = match (new.start_us, old.start_us) {
        (Some(new_us), Some(old_us)) => new_us.cmp(&old_us),
        _ => new.start_s.cmp(&old.start_s),
    };
    match order {
        Ordering::Greater => Generation::Newer,
        Ordering::Less => Generation::Older,
        Ordering::Equal => Generation::Tie,
    }
}

/// Upserts the session's entry for `event` (section 9.8, "Generations"). Every event raises
/// `last_event_ms` to its time, never lowering it. An event of an older generation than the
/// entry's, a start that ties with a generation not proven gone, and an end from no agent or
/// another one of an entry with an agent do nothing else. Any other event sets `last_event`
/// (when it is not older than the entry's last event) and a transcript path it names;
/// `SessionStart` with an agent then sets the generation and clears `ended_ms`, and `SessionEnd`
/// sets `ended_ms` when its agent is exactly the entry's. A new entry that `SubagentStop`
/// creates has no agent and `ended_ms` now; one `SessionEnd` creates has its agent and
/// `ended_ms` now.
pub fn apply(sessions: &mut Vec<LeaseEntry>, event: &Event, live: Liveness<'_>) {
    let Some(entry) = sessions
        .iter_mut()
        .find(|entry| !entry.uncertain && entry.session_id == event.session_id)
    else {
        let (agent, ended_ms) = match event.kind {
            LeaseEvent::SessionStart => (event.agent.clone(), None),
            LeaseEvent::SessionEnd => (event.agent.clone(), Some(event.now_ms)),
            LeaseEvent::SubagentStop => (None, Some(event.now_ms)),
        };
        sessions.push(LeaseEntry {
            session_id: event.session_id.clone(),
            agent,
            uncertain: false,
            transcript_path: event.transcript_path.clone(),
            last_event: event.kind,
            last_event_ms: event.now_ms,
            ended_ms,
        });
        return;
    };
    let previous_ms = entry.last_event_ms;
    entry.last_event_ms = previous_ms.max(event.now_ms);
    let order = match (&event.agent, &entry.agent) {
        (Some(new), Some(old)) => Some(generation(new, old)),
        _ => None,
    };
    let stale = match (event.kind, order) {
        (LeaseEvent::SessionStart, Some(Generation::Tie)) => {
            let old = entry.agent.as_ref().expect("a tie has an agent");
            live(old) != Some(false)
        }
        (_, Some(Generation::Older)) | (LeaseEvent::SessionEnd, Some(Generation::Tie)) => true,
        (LeaseEvent::SessionEnd, None) => entry.agent.is_some(),
        _ => false,
    };
    if stale {
        return;
    }
    if event.now_ms >= previous_ms {
        entry.last_event = event.kind;
    }
    if event.transcript_path.is_some() {
        entry.transcript_path.clone_from(&event.transcript_path);
    }
    match (event.kind, &event.agent) {
        (LeaseEvent::SessionStart, Some(agent)) => {
            entry.agent = Some(agent.clone());
            entry.ended_ms = None;
        }
        (LeaseEvent::SessionEnd, Some(_)) if order == Some(Generation::Same) => {
            entry.ended_ms = Some(event.now_ms);
        }
        _ => {}
    }
}

/// Drops the entries that no longer hold, then folds lingering entries while more than
/// [`LEASE_MAX_ENTRIES`] remain (section 9.8): the oldest ordinary entries into the earlier
/// fold's entry ([`FOLDED`]) when it may fold, else into a new one, whose last activity is the
/// latest of theirs; never `keep`'s entry, a live one, one whose liveness or activity is
/// unknown, or an `uncertain` one. When those alone pass the cap, `overflow` is set;
/// otherwise it is cleared. Without `linger_ms` (the setting could not be read) nothing is
/// dropped or folded: the file is only marked `overflow` when it passes the cap.
///
/// Each entry's assessment reads its agent and its transcripts, so the work checks `deadline`
/// before each entry and before the fold: past it, `lease` is left as it was and
/// [`Unfinished`] returned.
pub fn prune(
    lease: &mut SessionLease,
    keep: Option<&str>,
    now_ms: u64,
    linger_ms: Option<u64>,
    live: Liveness<'_>,
    deadline: Option<Instant>,
) -> Result<(), Unfinished> {
    let passed = || deadline.is_some_and(|deadline| Instant::now() >= deadline);
    if let Some(linger_ms) = linger_ms {
        let mut assessed = Vec::with_capacity(lease.sessions.len());
        for entry in &lease.sessions {
            if passed() {
                return Err(Unfinished);
            }
            let assessment = assess(entry, now_ms, linger_ms, live);
            if assessment.hold != Hold::Expired {
                assessed.push((entry.clone(), assessment));
            }
        }
        if passed() {
            return Err(Unfinished);
        }
        lease.sessions = fold(assessed, keep);
    }
    lease.overflow = lease.sessions.len() > LEASE_MAX_ENTRIES;
    Ok(())
}

/// A [`prune`] its deadline stopped: the lease was left as it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unfinished;

fn fold(assessed: Vec<(LeaseEntry, Assessment)>, keep: Option<&str>) -> Vec<LeaseEntry> {
    let entries = |assessed: Vec<(LeaseEntry, Assessment)>| {
        assessed.into_iter().map(|(entry, _)| entry).collect()
    };
    if assessed.len() <= LEASE_MAX_ENTRIES {
        return entries(assessed);
    }
    let foldable = |(entry, a): &(LeaseEntry, Assessment)| {
        a.hold == Hold::Lingering
            && a.live == Some(false)
            && a.activity_ms.is_some()
            && !entry.uncertain
            && Some(entry.session_id.as_str()) != keep
    };
    // The oldest ordinary entries go into the earlier fold's entry, whatever its own last
    // activity, so with one the fold takes one entry fewer and never makes a second one.
    let mut order: Vec<usize> = (0..assessed.len())
        .filter(|&i| foldable(&assessed[i]))
        .collect();
    order.sort_by_key(|&i| assessed[i].1.activity_ms);
    let aggregate = order
        .iter()
        .position(|&i| assessed[i].0.session_id == FOLDED)
        .map(|at| order.remove(at));
    let excess = assessed.len() - LEASE_MAX_ENTRIES;
    order.truncate(if aggregate.is_some() {
        excess
    } else {
        excess + 1
    });
    if order.len() < if aggregate.is_some() { 1 } else { 2 } {
        return entries(assessed);
    }
    order.extend(aggregate);
    let mut chosen = vec![false; assessed.len()];
    for &i in &order {
        chosen[i] = true;
    }
    let (folded, assessed): (Vec<_>, Vec<_>) = assessed
        .into_iter()
        .zip(chosen)
        .partition(|(_, chosen)| *chosen);
    let folded: Vec<(LeaseEntry, Assessment)> = folded.into_iter().map(|(e, _)| e).collect();
    let assessed: Vec<(LeaseEntry, Assessment)> = assessed.into_iter().map(|(e, _)| e).collect();
    let (latest, latest_ms) = folded
        .iter()
        .filter_map(|(entry, a)| a.activity_ms.map(|ms| (entry, ms)))
        .max_by_key(|(_, ms)| *ms)
        .map(|(entry, ms)| (entry.last_event, ms))
        .expect("folded entries, each with its activity");
    let mut sessions: Vec<LeaseEntry> = entries(assessed);
    sessions.push(LeaseEntry {
        session_id: FOLDED.to_owned(),
        agent: None,
        uncertain: false,
        transcript_path: None,
        last_event: latest,
        last_event_ms: latest_ms,
        ended_ms: Some(latest_ms),
    });
    sessions
}

/// The file a writer makes of a corrupt one (section 9.8, "A corrupt file"): an `uncertain`
/// entry standing for whatever the file held, lingering from its mtime, or from `now_ms`, the
/// repair, when that cannot be read, is already `linger_ms` old, or `linger_ms` is unknown.
#[must_use]
pub fn after_corrupt(
    worktree: &Path,
    mtime_ms: Option<u64>,
    now_ms: u64,
    linger_ms: Option<u64>,
) -> SessionLease {
    let last_event_ms = mtime_ms
        .filter(|&ms| linger_ms.is_some_and(|linger| now_ms < ms.saturating_add(linger)))
        .unwrap_or(now_ms);
    let mut lease = empty(worktree);
    lease.sessions.push(LeaseEntry {
        session_id: "cardinal:uncertain".to_owned(),
        agent: None,
        uncertain: true,
        transcript_path: None,
        last_event: LeaseEvent::SessionEnd,
        last_event_ms,
        ended_ms: Some(last_event_ms),
    });
    lease
}

/// An empty lease of `worktree`.
#[must_use]
pub fn empty(worktree: &Path) -> SessionLease {
    SessionLease {
        schema: SessionLeaseSchema,
        worktree: worktree.to_string_lossy().into_owned(),
        overflow: false,
        sessions: Vec::new(),
    }
}

/// Whether `stem`, a file name in `leases/` without `.json` or `.lock`, is a worktree key as
/// [`super::worktree_key`] writes it: 16 lowercase hex digits. Nothing else there is a lease.
#[must_use]
pub fn is_key(stem: &str) -> bool {
    stem.len() == 16 && stem.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The worktrees of a `cwd`, and the paths that could not be read on the way.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Discovery {
    /// Nearest first.
    pub roots: Vec<PathBuf>,
    /// A path whose resolution or `.git` entry could not be read for a reason other than
    /// `ENOENT`, with the error: a worktree may be missing from `roots`.
    pub errors: Vec<String>,
}

/// The worktree roots of `cwd` (section 9.8, "Writers"): the path resolved, then every
/// directory above it with a `.git` entry (file or directory), nearest first. A `cwd` inside a
/// submodule or a nested worktree maps to it and to each worktree around it. Empty outside any
/// worktree and when `cwd` does not exist. A `cwd` that cannot be resolved for another reason
/// is resolved from its nearest parent that can be, and that error and each `.git` entry that
/// cannot be read but for `ENOENT` are returned with the roots found.
#[must_use]
pub fn worktrees_of(cwd: &Path) -> Discovery {
    discover(cwd, &|path| dunce::canonicalize(path), &|path| {
        std::fs::symlink_metadata(path).map(drop)
    })
}

fn discover(
    cwd: &Path,
    canonical: &dyn Fn(&Path) -> io::Result<PathBuf>,
    git_entry: &dyn Fn(&Path) -> io::Result<()>,
) -> Discovery {
    let mut found = Discovery::default();
    let start = match canonical(cwd) {
        Ok(start) => start,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return found,
        Err(err) => {
            found.errors.push(format!("{}: {err}", cwd.display()));
            // Only a plain absolute path's parents are its parents.
            if !cwd.is_absolute() || cwd.components().any(|c| c == Component::ParentDir) {
                return found;
            }
            let Some(start) = cwd.ancestors().skip(1).find_map(|dir| canonical(dir).ok()) else {
                return found;
            };
            start
        }
    };
    for dir in start.ancestors() {
        let entry = dir.join(".git");
        match git_entry(&entry) {
            Ok(()) => found.roots.push(dir.to_path_buf()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => found.errors.push(format!("{}: {err}", entry.display())),
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        Event, FOLDED, Hold, Process, StartIdentity, after_corrupt, apply, assess, empty, is_key,
        liveness, prune,
    };
    use crate::contract::{LEASE_MAX_ENTRIES, LeaseAgent, LeaseEntry, LeaseEvent};

    const LINGER: u64 = 1000;

    fn agent(pid: u32, start_s: u64, boot: &str) -> LeaseAgent {
        LeaseAgent {
            pid,
            start_s,
            start_us: None,
            boot_id: boot.to_owned(),
        }
    }

    fn event(kind: LeaseEvent, agent: Option<LeaseAgent>, now_ms: u64) -> Event {
        Event {
            session_id: "s".into(),
            kind,
            agent,
            transcript_path: None,
            now_ms,
        }
    }

    fn entry(
        id: &str,
        agent: Option<LeaseAgent>,
        last_event_ms: u64,
        ended_ms: Option<u64>,
    ) -> LeaseEntry {
        LeaseEntry {
            session_id: id.into(),
            agent,
            uncertain: false,
            transcript_path: None,
            last_event: LeaseEvent::SessionEnd,
            last_event_ms,
            ended_ms,
        }
    }

    /// Undecided liveness: what an event must not take for proof of an end.
    fn undecided(_: &LeaseAgent) -> Option<bool> {
        None
    }

    #[test]
    fn generations() {
        let mut live = undecided;
        let old = agent(10, 100, "b");
        let new = agent(20, 200, "b");
        let mut sessions = Vec::new();
        apply(
            &mut sessions,
            &event(LeaseEvent::SessionStart, Some(old.clone()), 1),
            &mut live,
        );
        assert_eq!(sessions[0].agent.as_ref(), Some(&old));
        assert_eq!(sessions[0].ended_ms, None);

        // A resume by a newer process sets its generation and clears an end.
        apply(
            &mut sessions,
            &event(LeaseEvent::SessionEnd, Some(old.clone()), 2),
            &mut live,
        );
        assert_eq!(sessions[0].ended_ms, Some(2));
        apply(
            &mut sessions,
            &event(LeaseEvent::SessionStart, Some(new.clone()), 3),
            &mut live,
        );
        assert_eq!(
            (sessions[0].agent.as_ref(), sessions[0].ended_ms),
            (Some(&new), None)
        );

        // A late end and a late start of the older process only touch the time.
        apply(
            &mut sessions,
            &event(LeaseEvent::SessionEnd, Some(old.clone()), 4),
            &mut live,
        );
        apply(
            &mut sessions,
            &event(LeaseEvent::SessionStart, Some(old.clone()), 5),
            &mut live,
        );
        assert_eq!(
            (sessions[0].agent.as_ref(), sessions[0].ended_ms),
            (Some(&new), None)
        );
        assert_eq!(sessions[0].last_event_ms, 5);
        // An end from no known agent ends nothing.
        apply(
            &mut sessions,
            &event(LeaseEvent::SessionEnd, None, 6),
            &mut live,
        );
        assert_eq!(sessions[0].ended_ms, None);
        // A start of another boot is newer whatever its start time.
        let reboot = agent(5, 1, "c");
        apply(
            &mut sessions,
            &event(LeaseEvent::SessionStart, Some(reboot.clone()), 7),
            &mut live,
        );
        assert_eq!(sessions[0].agent.as_ref(), Some(&reboot));
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn created_entries() {
        let mut live = undecided;
        let mut sessions = Vec::new();
        apply(
            &mut sessions,
            &event(LeaseEvent::SubagentStop, Some(agent(1, 1, "b")), 9),
            &mut live,
        );
        assert_eq!(
            (sessions[0].agent.as_ref(), sessions[0].ended_ms),
            (None, Some(9))
        );
        let mut sessions = Vec::new();
        apply(
            &mut sessions,
            &event(LeaseEvent::SessionEnd, Some(agent(1, 1, "b")), 9),
            &mut live,
        );
        assert_eq!(sessions[0].ended_ms, Some(9));
    }

    #[test]
    fn holds() {
        let a = agent(1, 1, "b");
        let mut alive = |_: &LeaseAgent| Some(true);
        let mut dead = |_: &LeaseAgent| Some(false);
        let mut unknown = |_: &LeaseAgent| None;
        let live_entry = entry("s", Some(a.clone()), 0, None);
        assert_eq!(
            assess(&live_entry, 5000, LINGER, &mut alive).hold,
            Hold::Live
        );
        assert_eq!(
            assess(&live_entry, 5000, LINGER, &mut dead).hold,
            Hold::Expired
        );
        assert_eq!(
            assess(&live_entry, 5000, LINGER, &mut unknown).hold,
            Hold::Unknown
        );
        assert_eq!(
            assess(&live_entry, 500, LINGER, &mut unknown).hold,
            Hold::Lingering
        );
        let ended = entry("s", Some(a), 0, Some(4500));
        assert_eq!(
            assess(&ended, 5000, LINGER, &mut alive).hold,
            Hold::Lingering
        );
        assert_eq!(assess(&ended, 5500, LINGER, &mut alive).hold, Hold::Expired);
        // A future time counts as now.
        let future = entry("s", None, 9000, None);
        let assessed = assess(&future, 5000, LINGER, &mut dead);
        assert_eq!(
            (assessed.hold, assessed.activity_ms),
            (Hold::Lingering, Some(5000))
        );
        let uncertain = LeaseEntry {
            uncertain: true,
            ..entry("u", None, 0, None)
        };
        assert_eq!(
            assess(&uncertain, 999, LINGER, &mut dead).hold,
            Hold::Lingering
        );
        assert_eq!(
            assess(&uncertain, 1000, LINGER, &mut dead).hold,
            Hold::Expired
        );
    }

    /// A fold takes only lingering entries whose liveness is decided, oldest first, and marks
    /// overflow when the rest alone pass the cap.
    #[test]
    fn fold_keeps_live_entries() {
        let n = LEASE_MAX_ENTRIES + 3;
        let mut lease = empty(std::path::Path::new("/w"));
        for i in 0..n {
            let a = (i % 2 == 0).then(|| agent(u32::try_from(i).unwrap(), 1, "b"));
            lease
                .sessions
                .push(entry(&format!("s{i}"), a, 100 + i as u64, None));
        }
        // Even entries have a live agent.
        let mut live = |a: &LeaseAgent| a.pid.is_multiple_of(2).then_some(true);
        prune(&mut lease, Some("s1"), 1000, Some(10_000), &mut live, None).unwrap();
        assert_eq!(lease.sessions.len(), LEASE_MAX_ENTRIES);
        assert!(!lease.overflow);
        let folded = lease
            .sessions
            .iter()
            .find(|e| e.session_id == FOLDED)
            .unwrap();
        // s3, s5, s7, s9 folded (s1 is the writer's own): the latest of theirs.
        assert_eq!(folded.last_event_ms, 109);
        for gone in ["s3", "s5", "s7", "s9"] {
            assert!(
                lease.sessions.iter().all(|e| e.session_id != gone),
                "{gone}"
            );
        }
        for kept in ["s0", "s1", "s2", "s11"] {
            assert!(
                lease.sessions.iter().any(|e| e.session_id == kept),
                "{kept}"
            );
        }

        // Only live entries: nothing folds, and the file overflows.
        let mut lease = empty(std::path::Path::new("/w"));
        for i in 0..n {
            lease
                .sessions
                .push(entry(&format!("s{i}"), Some(agent(1, 1, "b")), 0, None));
        }
        let mut alive = |_: &LeaseAgent| Some(true);
        prune(&mut lease, None, 1000, Some(10), &mut alive, None).unwrap();
        assert_eq!(lease.sessions.len(), n);
        assert!(lease.overflow);
        // Undecidable liveness folds nothing either.
        let mut unknown = |_: &LeaseAgent| None;
        prune(&mut lease, None, 5, Some(10), &mut unknown, None).unwrap();
        assert_eq!(lease.sessions.len(), n);
        assert!(lease.overflow);
    }

    fn timed(agent: LeaseAgent, start_us: u64) -> LeaseAgent {
        LeaseAgent {
            start_us: Some(start_us),
            ..agent
        }
    }

    /// `last_event_ms` only rises: an event sampled before another hook's write lowers nothing.
    /// An event of an older generation keeps the entry's transcript and last event kind.
    #[test]
    fn events_never_roll_back() {
        let mut live = undecided;
        let new = agent(20, 200, "b");
        let mut sessions = vec![LeaseEntry {
            transcript_path: Some("/t/new.jsonl".into()),
            last_event: LeaseEvent::SessionStart,
            ..entry("s", Some(new.clone()), 500, None)
        }];
        let late = event(LeaseEvent::SubagentStop, None, 400);
        apply(&mut sessions, &late, &mut live);
        assert_eq!(sessions[0].last_event_ms, 500, "never lowered");
        assert_eq!(sessions[0].last_event, LeaseEvent::SessionStart);

        let older = Event {
            transcript_path: Some("/t/old.jsonl".into()),
            ..event(LeaseEvent::SessionEnd, Some(agent(10, 100, "b")), 600)
        };
        apply(&mut sessions, &older, &mut live);
        let e = &sessions[0];
        assert_eq!(e.last_event_ms, 600);
        assert_eq!(e.last_event, LeaseEvent::SessionStart, "kind kept");
        assert_eq!(e.transcript_path.as_deref(), Some("/t/new.jsonl"));
        assert_eq!((e.agent.as_ref(), e.ended_ms), (Some(&new), None));
        // Control: the entry's own generation sets both.
        let own = Event {
            transcript_path: Some("/t/new2.jsonl".into()),
            ..event(LeaseEvent::SessionEnd, Some(new.clone()), 700)
        };
        apply(&mut sessions, &own, &mut live);
        let e = &sessions[0];
        assert_eq!(
            (e.last_event, e.transcript_path.as_deref(), e.ended_ms),
            (LeaseEvent::SessionEnd, Some("/t/new2.jsonl"), Some(700))
        );
    }

    /// Two starts in one second are ordered by their finer start times; an exact tie keeps the
    /// entry's generation unless it is proven gone.
    #[test]
    fn same_second_starts() {
        let existing = timed(agent(20, 100, "b"), 100_000_500);
        let earlier = timed(agent(10, 100, "b"), 100_000_100);
        let tie = timed(agent(30, 100, "b"), 100_000_500);
        let start = |agent: &LeaseAgent| event(LeaseEvent::SessionStart, Some(agent.clone()), 9);
        let fresh = || vec![entry("s", Some(existing.clone()), 1, None)];

        let mut sessions = fresh();
        apply(&mut sessions, &start(&earlier), &mut undecided);
        assert_eq!(sessions[0].agent.as_ref(), Some(&existing), "older by µs");
        for mut live in [
            Box::new(|_: &LeaseAgent| None) as Box<dyn FnMut(&LeaseAgent) -> Option<bool>>,
            Box::new(|_: &LeaseAgent| Some(true)),
        ] {
            let mut sessions = fresh();
            apply(&mut sessions, &start(&tie), &mut *live);
            assert_eq!(sessions[0].agent.as_ref(), Some(&existing), "a tie keeps");
        }
        let mut sessions = fresh();
        apply(&mut sessions, &start(&tie), &mut |_: &LeaseAgent| {
            Some(false)
        });
        assert_eq!(
            sessions[0].agent.as_ref(),
            Some(&tie),
            "the old one is gone"
        );
        // Control: a later start by µs in the same second is newer.
        let later = timed(agent(10, 100, "b"), 100_000_900);
        let mut sessions = fresh();
        apply(&mut sessions, &start(&later), &mut undecided);
        assert_eq!(sessions[0].agent.as_ref(), Some(&later));
        // Without the finer time a tie in seconds keeps the entry's generation.
        let mut sessions = vec![entry("s", Some(agent(20, 100, "b")), 1, None)];
        apply(&mut sessions, &start(&agent(10, 100, "b")), &mut undecided);
        assert_eq!(sessions[0].agent.as_ref().unwrap().pid, 20);
    }

    /// Liveness decides only on a process that is gone or runs with a readable start time; a
    /// start time of 0 or an unreadable process is undecided, and each PID is read once.
    #[test]
    fn liveness_decides_only_on_proof() {
        let a = agent(7, 100, "b");
        let mut reads = 0;
        let check = |found: Process, agent: &LeaseAgent| {
            liveness(Some("b"), StartIdentity::Seconds, |_| found)(agent)
        };
        assert_eq!(check(Process::Gone, &a), Some(false));
        assert_eq!(
            check(
                Process::Running {
                    start_s: 101,
                    start_us: None
                },
                &a
            ),
            Some(false)
        );
        assert_eq!(
            check(
                Process::Running {
                    start_s: 100,
                    start_us: None
                },
                &a
            ),
            Some(true)
        );
        assert_eq!(
            check(
                Process::Running {
                    start_s: 0,
                    start_us: None
                },
                &a
            ),
            None
        );
        assert_eq!(check(Process::Unknown, &a), None);
        assert_eq!(check(Process::Unknown, &agent(7, 100, "c")), Some(false));
        assert_eq!(
            liveness(None, StartIdentity::Seconds, |_| Process::Gone)(&a),
            None
        );
        let mut live = liveness(Some("b"), StartIdentity::Seconds, |_| {
            reads += 1;
            Process::Gone
        });
        live(&a);
        live(&a);
        drop(live);
        assert_eq!(reads, 1);

        // An agent whose start time reads 0, past its linger, is neither dropped nor folded.
        let mut lease = empty(Path::new("/w"));
        lease.sessions = (0..=LEASE_MAX_ENTRIES)
            .map(|i| entry(&format!("s{i}"), None, 100 + i as u64, Some(100)))
            .collect();
        lease.sessions.push(entry("zero", Some(a.clone()), 0, None));
        let mut zero = liveness(Some("b"), StartIdentity::Seconds, |_| Process::Running {
            start_s: 0,
            start_us: None,
        });
        prune(&mut lease, None, 5000, Some(10_000), &mut zero, None).unwrap();
        assert!(lease.sessions.iter().any(|e| e.session_id == "zero"));
        prune(&mut lease, None, 50_000, Some(10_000), &mut zero, None).unwrap();
        assert_eq!(lease.sessions.len(), 1, "{:?}", lease.sessions);
        assert_eq!(lease.sessions[0].session_id, "zero");
        // Control: proven gone, it expires.
        prune(
            &mut lease,
            None,
            50_000,
            Some(10_000),
            &mut liveness(Some("b"), StartIdentity::Seconds, |_| Process::Gone),
            None,
        )
        .unwrap();
        assert_eq!(lease.sessions, []);
    }

    /// The earlier fold's entry is folded by its own last activity: older entries go into it
    /// first, and it never makes a second one.
    #[test]
    fn fold_is_oldest_first_with_the_aggregate() {
        let mut dead = |_: &LeaseAgent| Some(false);
        let mut lease = empty(Path::new("/w"));
        // The aggregate is younger than the twenty entries.
        lease.sessions.push(entry(FOLDED, None, 900, Some(900)));
        for i in 0..=LEASE_MAX_ENTRIES {
            lease
                .sessions
                .push(entry(&format!("s{i}"), None, 100 + i as u64, None));
        }
        prune(&mut lease, None, 1000, Some(10_000), &mut dead, None).unwrap();
        assert_eq!(lease.sessions.len(), LEASE_MAX_ENTRIES);
        let folded: Vec<_> = lease
            .sessions
            .iter()
            .filter(|e| e.session_id == FOLDED)
            .collect();
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].last_event_ms, 900);
        for gone in ["s0", "s1"] {
            assert!(
                lease.sessions.iter().all(|e| e.session_id != gone),
                "{gone}"
            );
        }
        assert!(lease.sessions.iter().any(|e| e.session_id == "s2"));
    }

    /// Without the linger setting a writer drops and folds nothing; past the cap it marks the
    /// file overflowing.
    #[test]
    fn no_setting_prunes_nothing() {
        let mut dead = |_: &LeaseAgent| Some(false);
        let mut lease = empty(Path::new("/w"));
        lease.sessions = (0..=LEASE_MAX_ENTRIES)
            .map(|i| entry(&format!("s{i}"), None, 0, Some(0)))
            .collect();
        prune(&mut lease, None, 1_000_000, None, &mut dead, None).unwrap();
        assert_eq!(lease.sessions.len(), LEASE_MAX_ENTRIES + 1);
        assert!(lease.overflow);
        // Control: with it, every expired entry goes.
        prune(&mut lease, None, 1_000_000, Some(10), &mut dead, None).unwrap();
        assert!(lease.sessions.is_empty() && !lease.overflow);
    }

    /// The `uncertain` entry lingers from the corrupt file's mtime, or from the repair when the
    /// mtime is unknown, already past the linger, or the linger unknown.
    #[test]
    fn uncertain_entry_lingers_from_the_repair_when_needed() {
        let at = |mtime, linger| {
            after_corrupt(Path::new("/w"), mtime, 5000, linger).sessions[0].last_event_ms
        };
        assert_eq!(at(Some(4500), Some(1000)), 4500);
        assert_eq!(at(Some(3000), Some(1000)), 5000);
        assert_eq!(at(None, Some(1000)), 5000);
        assert_eq!(at(Some(4500), None), 5000);
    }

    #[test]
    fn keys() {
        assert!(is_key(&crate::state::worktree_key(Path::new("/w"))));
        for other in [
            "export",
            "0123456789ABCDEF",
            "0123456789abcde",
            "0123456789abcdef0",
        ] {
            assert!(!is_key(other), "{other}");
        }
    }

    /// Discovery records any error but `ENOENT` and goes on: an unreadable `.git` entry, and a
    /// `cwd` that cannot be resolved, resolved from its nearest parent that can. Unix only: the
    /// fakes name Unix absolute paths, which Windows does not take as absolute, and no hook
    /// there looks for worktrees (section 9.8, "Windows").
    #[cfg(unix)]
    #[test]
    fn discovery_reports_what_it_could_not_read() {
        use std::io;
        use std::path::PathBuf;

        use super::{Discovery, discover};

        let denied = || io::Error::from(io::ErrorKind::PermissionDenied);
        let canonical = |p: &Path| -> io::Result<PathBuf> {
            if p.starts_with("/r/blocked") {
                Err(denied())
            } else {
                Ok(p.to_path_buf())
            }
        };
        let git = |p: &Path| -> io::Result<()> {
            match p.to_str().unwrap() {
                "/r/.git" => Ok(()),
                "/r/inner/.git" => Err(denied()),
                _ => Err(io::ErrorKind::NotFound.into()),
            }
        };
        let found = discover(Path::new("/r/inner/src"), &canonical, &git);
        assert_eq!(found.roots, [PathBuf::from("/r")]);
        assert_eq!(found.errors.len(), 1, "{:?}", found.errors);
        assert!(found.errors[0].starts_with("/r/inner/.git"));

        let found = discover(Path::new("/r/blocked/deep"), &canonical, &git);
        assert_eq!(found.roots, [PathBuf::from("/r")]);
        assert!(found.errors[0].starts_with("/r/blocked/deep"));
        // A missing cwd is no error, and a path with `..` is not walked lexically.
        let gone = |_: &Path| -> io::Result<PathBuf> { Err(io::ErrorKind::NotFound.into()) };
        assert_eq!(
            discover(Path::new("/r/x"), &gone, &git),
            Discovery::default()
        );
        let found = discover(Path::new("/r/blocked/../x"), &canonical, &git);
        assert!(found.roots.is_empty() && found.errors.len() == 1);
        // Control: all readable.
        let all = |p: &Path| -> io::Result<()> {
            if p == Path::new("/r/.git") || p == Path::new("/r/inner/.git") {
                Ok(())
            } else {
                Err(io::ErrorKind::NotFound.into())
            }
        };
        let found = discover(Path::new("/r/inner/src"), &canonical, &all);
        assert_eq!(
            found.roots,
            [PathBuf::from("/r/inner"), PathBuf::from("/r")]
        );
        assert_eq!(found.errors, [] as [String; 0]);
    }
    /// Linux (`BootRelative`): a process is told by its start since boot alone, so a wall-clock
    /// step that moved `start_s` leaves it live, and without that start it is undecided. macOS
    /// (`Seconds`, control) compares the seconds when no finer start was read.
    #[test]
    fn clock_steps_move_no_start() {
        let a = LeaseAgent {
            start_us: Some(5_000_000),
            ..agent(7, 100, "b")
        };
        let check = |identity, start_s, start_us| {
            liveness(Some("b"), identity, |_| Process::Running {
                start_s,
                start_us,
            })(&a)
        };
        let linux = StartIdentity::BootRelative;
        assert_eq!(check(linux, 101, Some(5_000_000)), Some(true), "a step");
        assert_eq!(check(linux, 100, Some(5_010_000)), Some(false), "another");
        assert_eq!(check(linux, 100, None), None, "no start since boot");
        let unread = LeaseAgent {
            start_us: None,
            ..a.clone()
        };
        let mut live = liveness(Some("b"), linux, |_| Process::Running {
            start_s: 100,
            start_us: Some(5_000_000),
        });
        assert_eq!(live(&unread), None, "none recorded");
        let mac = StartIdentity::Seconds;
        assert_eq!(check(mac, 100, None), Some(true));
        assert_eq!(check(mac, 101, None), Some(false));
        assert_eq!(check(mac, 100, Some(5_000_001)), Some(false), "finer first");
    }

    /// The same PID in the same second with another finer start is another process: its
    /// `SessionEnd` ends nothing. The exact start ends the entry (control).
    #[test]
    fn same_pid_and_second_is_not_the_same_start() {
        let entered = timed(agent(10, 100, "b"), 100_000_100);
        let fresh = || vec![entry("s", Some(entered.clone()), 1, None)];
        let end = |agent: &LeaseAgent| event(LeaseEvent::SessionEnd, Some(agent.clone()), 9);
        for other in [100_000_900, 100_000_050] {
            let mut sessions = fresh();
            let reused = timed(agent(10, 100, "b"), other);
            apply(&mut sessions, &end(&reused), &mut undecided);
            assert_eq!(sessions[0].ended_ms, None, "{other}");
            assert_eq!(sessions[0].agent.as_ref(), Some(&entered), "{other}");
        }
        let mut sessions = fresh();
        apply(&mut sessions, &end(&entered), &mut undecided);
        assert_eq!(sessions[0].ended_ms, Some(9));
    }

    /// A prune past its deadline leaves the lease as it was; without one it drops what expired
    /// (control).
    #[test]
    fn prune_stops_at_its_deadline() {
        let mut dead = |_: &LeaseAgent| Some(false);
        let mut lease = empty(Path::new("/w"));
        lease.sessions = (0..10)
            .map(|i| entry(&format!("s{i}"), None, 0, Some(0)))
            .collect();
        let before = lease.clone();
        let passed = std::time::Instant::now();
        assert_eq!(
            prune(
                &mut lease,
                None,
                1_000_000,
                Some(10),
                &mut dead,
                Some(passed)
            ),
            Err(super::Unfinished)
        );
        assert_eq!(lease, before);
        prune(&mut lease, None, 1_000_000, Some(10), &mut dead, None).unwrap();
        assert_eq!(lease.sessions, []);
    }
}
