//! Starting the arbiter (`docs/architecture.md` section 10.5): the trigger a pass runs at
//! step 7, and `cardinal arbiter run`, the launcher that records a pass, assigns its due
//! items, and runs `[arbiter] command` under `arbiter/.lock` with a deadline.
//!
//! The launcher writes `arbiter/last.json` before it assigns anything and the pass's outcome
//! into its items before it records the end there, so a launcher that dies at any point
//! strands no item: an item never assigned stays due, and an assigned one keeps the outcome
//! written into it or gets `abandoned` (section 10.5, A19). A pass whose launcher died counts
//! as running until `timeout_s` after it started; the next trigger or launcher after that
//! records it `abandoned`, signalling nothing. A pass whose due items were all closed before
//! the launcher assigned them runs no session: the launcher puts the previous `last.json`
//! back. Both read `last.json` under `arbiter/.lock`, and refuse to start a pass when it
//! exists but cannot be read; both also raise `arbiter_failing` a launcher that died after
//! recording the third failure in a row did not.

use std::ffi::OsString;
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::contract::{
    ArbiterPass, ArbiterPassSchema, ArbiterStart, Error, ErrorCode, InboxKind, InboxSubject,
    PassOutcome, RecordKind,
};
use crate::inbox::{self, NewItem, Raised};
use crate::os::{self, child};
use crate::record::{self, Recorder};
use crate::state::{self, Durability, LockMode, StateDir};
use crate::testpoint;

/// The environment variable that names the pass a session runs for.
const PASS_VAR: &str = "CARDINAL_ARBITER_PASS";

/// The most bytes of a session's output kept in its log; a session that writes more to
/// stdout or stderr is ended (`failed:output_too_large`).
const LOG_CAP: u64 = 1 << 20;

/// Failed, timed-out, or abandoned passes in a row that raise `arbiter_failing`.
const FAILING_AFTER: u64 = 3;

/// Step 7: starts `cardinal arbiter run` detached when an item is due and `min_interval_s`
/// has passed since the last pass started. An error, starting nothing, when `last.json`
/// exists but cannot be read.
pub fn trigger(config: &Config, state: &StateDir) -> Result<ArbiterStart, Error> {
    trigger_with(config, state, &start_launcher)
}

/// [`trigger`] with `start` starting the launcher.
fn trigger_with(
    config: &Config,
    state: &StateDir,
    start: &dyn Fn() -> Result<u32, Error>,
) -> Result<ArbiterStart, Error> {
    let now = state::now_ms();
    {
        // The launcher's lock, so no launcher starts while the last pass is settled; released
        // before one is started.
        let Some(_lock) = state::try_lock(&state.arbiter_lock(), LockMode::Exclusive)? else {
            return Ok(ArbiterStart::Running);
        };
        let last = match settle(config, state, now)? {
            Last::Running(_) => return Ok(ArbiterStart::Running),
            Last::Settled(last) => last,
        };
        if inbox::due(config, state, last.as_ref(), now)?.is_empty() {
            return Ok(ArbiterStart::NotNeeded);
        }
        if !interval_passed(config, last.as_ref(), now) {
            return Ok(ArbiterStart::Interval);
        }
    }
    start()?;
    Ok(ArbiterStart::Started)
}

/// Starts `cardinal arbiter run` detached; returns its PID.
fn start_launcher() -> Result<u32, Error> {
    let exe = std::env::current_exe().map_err(|err| {
        Error::new(
            ErrorCode::Internal,
            format!("cannot find the cardinal executable: {err}"),
        )
    })?;
    os::spawn_detached(&exe, &["arbiter".into(), "run".into()])
        .map_err(|err| Error::new(ErrorCode::Internal, err.to_string()))
}

/// `cardinal arbiter run`: one bounded arbiter pass, or the running one when
/// `arbiter/.lock` is held or a pass whose launcher died is within `timeout_s` of its start.
/// Refused with `not_found` when no item is due, `min_interval_s` has not passed, or every
/// due item was closed before it was assigned, and with `state_unavailable` when `last.json`
/// exists but cannot be read.
pub fn run(config: &Config, state: &StateDir) -> Result<ArbiterPass, Error> {
    let Some(lock) = state::try_lock(&state.arbiter_lock(), LockMode::Exclusive)? else {
        return read_last(state)?.ok_or_else(|| {
            Error::new(
                ErrorCode::NotFound,
                "another arbiter launcher holds arbiter/.lock and has recorded no pass yet",
            )
        });
    };
    injected(testpoint::hit(testpoint::ARBITER_LOCKED))?;
    let now = state::now_ms();
    let last = match settle(config, state, now)? {
        Last::Running(pass) => return Ok(pass),
        Last::Settled(last) => last,
    };
    let due = inbox::due(config, state, last.as_ref(), now)?;
    if due.is_empty() {
        return Err(Error::new(
            ErrorCode::NotFound,
            "no inbox item is due for an arbiter pass",
        ));
    }
    if !interval_passed(config, last.as_ref(), now) {
        return Err(Error::new(
            ErrorCode::NotFound,
            format!(
                "the last arbiter pass started less than [arbiter] min_interval_s ({} s) ago",
                config.host.arbiter.min_interval_s
            ),
        ));
    }

    let id = state::new_id();
    let log = state.arbiter_log(&id);
    let mut pass = ArbiterPass {
        schema: ArbiterPassSchema,
        id,
        started_ms: now,
        ended_ms: None,
        outcome: None,
        items: due.into_iter().map(|item| item.key).collect(),
        log: log.display().to_string(),
        failures_in_a_row: last.as_ref().map_or(0, |last| last.failures_in_a_row),
    };
    write_last(state, &pass)?;
    injected(testpoint::hit(testpoint::ARBITER_RECORDED))?;

    let mut notes = Vec::new();
    let outcome = match inbox::assign(state, &pass.id, &pass.items, now) {
        Ok(assigned) if assigned.is_empty() => {
            // Each due item was closed after it was found due: no session, and no pass.
            restore_last(state, last.as_ref())?;
            return Err(Error::new(
                ErrorCode::NotFound,
                "no inbox item is due for an arbiter pass: each one found due was closed before it was assigned",
            ));
        }
        Ok(assigned) => {
            pass.items = assigned;
            session(config, state, &pass, &mut notes)
        }
        Err(err) => {
            notes.push(format!("assigning the items failed: {}", err.message));
            PassOutcome::Failed("assign".into())
        }
    };
    let ended = state::now_ms();
    if let Err(err) = inbox::record_outcome(config, state, &pass.id, &outcome, ended) {
        // Each item of the pass still ends when `last.json` records the end (section 10.5).
        notes.push(format!(
            "writing the outcome into the items failed: {}",
            err.message
        ));
    }
    injected(testpoint::hit(testpoint::ARBITER_OUTCOMES))?;
    finish(config, state, &mut pass, outcome, ended)?;
    append_log(&log, &notes);
    drop(lock);
    Ok(pass)
}

/// The last pass, once one whose launcher died is settled.
enum Last {
    /// That pass is within `timeout_s` of its start.
    Running(ArbiterPass),
    /// No pass is running; the last one, if any.
    Settled(Option<ArbiterPass>),
}

/// Reads `last.json`, holding `arbiter/.lock`; a pass recorded without an end belongs to a
/// launcher that died, and once `timeout_s` has passed since it started it is recorded
/// `abandoned`: in those of its items that have no outcome, with one end time, then in
/// `last.json`. A pass recorded with an end gets the `arbiter_failing` its launcher died
/// before raising.
fn settle(config: &Config, state: &StateDir, now: u64) -> Result<Last, Error> {
    let Some(mut last) = read_last(state)? else {
        return Ok(Last::Settled(None));
    };
    if last.ended_ms.is_some() {
        alert(config, state, &last)?;
        return Ok(Last::Settled(Some(last)));
    }
    let ended = deadline_ms(config, &last);
    if now < ended {
        return Ok(Last::Running(last));
    }
    // Items that already have an outcome keep it.
    inbox::record_outcome(config, state, &last.id, &PassOutcome::Abandoned, ended)?;
    finish(config, state, &mut last, PassOutcome::Abandoned, ended)?;
    Ok(Last::Settled(Some(last)))
}

/// Records how `pass` ended in `last.json` and an `arbiter_pass` record, then raises
/// `arbiter_failing` as [`alert`] does.
fn finish(
    config: &Config,
    state: &StateDir,
    pass: &mut ArbiterPass,
    outcome: PassOutcome,
    ended: u64,
) -> Result<(), Error> {
    pass.failures_in_a_row = if outcome == PassOutcome::Completed {
        0
    } else {
        pass.failures_in_a_row.saturating_add(1)
    };
    pass.ended_ms = Some(ended);
    pass.outcome = Some(outcome);
    write_last(state, pass)?;
    let recorder = recorder(config, state);
    let mut entry = recorder.record(RecordKind::ArbiterPass);
    entry.data = as_object(&pass);
    let _ = recorder.append(&entry);
    alert(config, state, pass)
}

/// Raises `arbiter_failing` when the ended pass `last` makes at least three failures in a
/// row and no occurrence is open (one without `ended_ms`): an alert a launcher died before
/// raising is raised once, under the same key, and a failure after the third raises none.
fn alert(config: &Config, state: &StateDir, last: &ArbiterPass) -> Result<(), Error> {
    let Some(ended) = last.ended_ms else {
        return Ok(());
    };
    if last.failures_in_a_row < FAILING_AFTER {
        return Ok(());
    }
    let open = inbox::read_all(state)?
        .into_iter()
        .any(|(_, item)| item.kind == InboxKind::ArbiterFailing && item.ended_ms.is_none());
    if open {
        return Ok(());
    }
    raise_failing(config, state, last, ended)
}

/// Raises `arbiter_failing` for the pass that made it three or more in a row, with the
/// failed passes and their logs as evidence.
fn raise_failing(
    config: &Config,
    state: &StateDir,
    pass: &ArbiterPass,
    since_ms: u64,
) -> Result<(), Error> {
    let mut passes: Vec<ArbiterPass> = record::read_all(state)
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| entry.kind == RecordKind::ArbiterPass)
        .filter_map(|entry| serde_json::from_value(entry.data.into()).ok())
        .filter(|recorded: &ArbiterPass| recorded.id != pass.id)
        .collect();
    let earlier = usize::try_from(FAILING_AFTER - 1).unwrap_or(usize::MAX);
    passes = passes.split_off(passes.len().saturating_sub(earlier));
    passes.push(pass.clone());
    let logs: Vec<&str> = passes.iter().map(|failed| failed.log.as_str()).collect();
    let evidence = serde_json::json!({ "passes": passes, "logs": logs });
    let item = NewItem {
        // One occurrence of the condition (section 10.1): the pass that raised it, and the
        // time it ended.
        key: format!("arbiter_failing:{}@{since_ms}", pass.id),
        kind: InboxKind::ArbiterFailing,
        subject: InboxSubject::default(),
        evidence: evidence.as_object().cloned().unwrap_or_default(),
    };
    let (stored, raised) = inbox::raise_item(state, &item)?;
    if raised == Raised::New {
        let recorder = recorder(config, state);
        let mut entry = recorder.record(RecordKind::InboxRaised);
        entry.inbox_key = Some(stored.key);
        entry.data = as_object(&serde_json::json!({ "kind": stored.kind }));
        let _ = recorder.append(&entry);
    }
    Ok(())
}

/// Runs `[arbiter] command` for `pass` through `os::child`: in a process group of its own,
/// in `arbiter/`, with this process's environment and `CARDINAL_ARBITER_PASS`, killed at
/// `timeout_s` after the pass started. Its output goes to the pass's log.
fn session(
    config: &Config,
    state: &StateDir,
    pass: &ArbiterPass,
    notes: &mut Vec<String>,
) -> PassOutcome {
    let Some((program, args)) = config.host.arbiter.command.split_first() else {
        notes.push("[arbiter] command is empty".into());
        return PassOutcome::Failed("no_command".into());
    };
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(name, _)| !name.eq_ignore_ascii_case(PASS_VAR))
        .collect();
    env.push((PASS_VAR.into(), pass.id.clone().into()));
    let left = deadline_ms(config, pass).saturating_sub(state::now_ms());
    let limits = child::Limits {
        deadline: Instant::now() + Duration::from_millis(left),
        max_output: LOG_CAP,
    };
    let log = Path::new(&pass.log);
    match child::run(program, &args, &env, &state.arbiter_dir(), limits) {
        Ok(output) => {
            let mut bytes = output.stdout;
            bytes.extend_from_slice(&output.stderr);
            bytes.truncate(usize::try_from(LOG_CAP).unwrap_or(usize::MAX));
            if let Err(err) = std::fs::write(log, &bytes) {
                notes.push(format!("cannot write the log: {err}"));
            }
            match output.code {
                Some(0) => PassOutcome::Completed,
                Some(code) => PassOutcome::Failed(format!("exit {code}")),
                None => PassOutcome::Failed("signal".into()),
            }
        }
        Err(err) => {
            notes.push(err.to_string());
            match err {
                child::ChildError::Timeout { .. } => PassOutcome::TimedOut,
                child::ChildError::OutputTooLarge { .. } => {
                    PassOutcome::Failed("output_too_large".into())
                }
                child::ChildError::Spawn { .. } | child::ChildError::Setup { .. } => {
                    PassOutcome::Failed("not_started".into())
                }
                child::ChildError::Io { .. } => PassOutcome::Failed("io".into()),
            }
        }
    }
}

/// Appends the launcher's own notes to the pass's log.
fn append_log(log: &Path, notes: &[String]) {
    if notes.is_empty() {
        return;
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    else {
        return;
    };
    for note in notes {
        let _ = writeln!(file, "cardinal arbiter: {note}");
    }
}

/// When `pass` stops counting as running: `timeout_s` after it started.
fn deadline_ms(config: &Config, pass: &ArbiterPass) -> u64 {
    pass.started_ms
        .saturating_add(config.host.arbiter.timeout_s.saturating_mul(1000))
}

/// Whether `min_interval_s` has passed since the last pass started.
fn interval_passed(config: &Config, last: Option<&ArbiterPass>, now: u64) -> bool {
    last.is_none_or(|last| {
        now >= last
            .started_ms
            .saturating_add(config.host.arbiter.min_interval_s.saturating_mul(1000))
    })
}

/// The pass `arbiter/last.json` records: `None` when no pass ever ran, an error when the file
/// exists but cannot be read or parsed.
fn read_last(state: &StateDir) -> Result<Option<ArbiterPass>, Error> {
    let path = state.arbiter_last();
    let unreadable = |err: &dyn std::fmt::Display| {
        Error::new(
            ErrorCode::StateUnavailable,
            format!("{}: {err}", path.display()),
        )
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(unreadable(&err)),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|err| unreadable(&err))
}

/// Replaces `arbiter/last.json`; not fsynced (section 4.2).
fn write_last(state: &StateDir, pass: &ArbiterPass) -> Result<(), Error> {
    let bytes =
        serde_json::to_vec(pass).map_err(|err| Error::new(ErrorCode::Internal, err.to_string()))?;
    Ok(state.write_atomic(&state.arbiter_last(), &bytes, Durability::Volatile)?)
}

/// Puts back the `last.json` a pass that ran no session replaced: `last` written as any pass
/// is, or no file when no pass ran before.
fn restore_last(state: &StateDir, last: Option<&ArbiterPass>) -> Result<(), Error> {
    if let Some(last) = last {
        return write_last(state, last);
    }
    let path = state.arbiter_last();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(Error::new(
            ErrorCode::StateUnavailable,
            format!("{}: {err}", path.display()),
        )),
    }
}

fn recorder(config: &Config, state: &StateDir) -> Recorder {
    Recorder::new(
        state,
        config.host.records.max_bytes,
        config.host.records.keep,
    )
}

fn as_object(value: &impl serde::Serialize) -> serde_json::Map<String, serde_json::Value> {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn injected(result: Result<(), testpoint::Injected>) -> Result<(), Error> {
    result.map_err(|err| Error::new(ErrorCode::Internal, err.to_string()))
}

/// The trigger's decisions. `cardinal reconcile` reaches the trigger only through the pass of
/// section 10.3, so these call it directly, with a launcher that is not started; the
/// launcher's own behaviour is covered through the binary in `tests/arbiter_launch.rs`.
#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use cardinal_test_support::Scope;

    use super::{read_last, trigger_with, write_last};
    use crate::config::Config;
    use crate::contract::{
        ArbiterPass, ArbiterPassSchema, ArbiterStart, Error, ErrorCode, InboxKind, InboxStatus,
        InboxSubject, PassOutcome,
    };
    use crate::inbox::{self, NewItem};
    use crate::state::{self, LockMode, StateDir};

    struct Env {
        _scope: Scope,
        config: Config,
        state: StateDir,
    }

    /// A state dir and a config whose `[arbiter]` table is `arbiter`.
    fn env(arbiter: &str) -> Env {
        let scope = Scope::new();
        let config_dir = scope.path("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("host.toml"),
            format!("[arbiter]\n{arbiter}\n"),
        )
        .unwrap();
        let config = Config::load(&config_dir).unwrap();
        let state = StateDir::open(&scope.path("state")).unwrap();
        Env {
            _scope: scope,
            config,
            state,
        }
    }

    fn raise(e: &Env, key: &str, kind: InboxKind) {
        let item = NewItem {
            key: key.into(),
            kind,
            subject: InboxSubject::default(),
            evidence: serde_json::Map::new(),
        };
        inbox::raise(&e.state, &item).unwrap();
    }

    /// The trigger's decision, and how many launchers it started.
    fn try_trigger(e: &Env) -> (Result<ArbiterStart, Error>, u32) {
        let started = Cell::new(0);
        let start = || -> Result<u32, Error> {
            started.set(started.get() + 1);
            Ok(1)
        };
        let decision = trigger_with(&e.config, &e.state, &start);
        (decision, started.get())
    }

    fn trigger(e: &Env) -> (ArbiterStart, u32) {
        let (decision, started) = try_trigger(e);
        (decision.unwrap(), started)
    }

    /// A pass in `last.json` as a launcher leaves it, without an end when it died; an ended
    /// one completed, or failed when `failures` is not 0.
    fn last(e: &Env, id: &str, started_ms: u64, ended: bool, failures: u64) {
        let outcome = if failures == 0 {
            PassOutcome::Completed
        } else {
            PassOutcome::Failed("exit 1".into())
        };
        let pass = ArbiterPass {
            schema: ArbiterPassSchema,
            id: id.into(),
            started_ms,
            ended_ms: ended.then_some(started_ms),
            outcome: ended.then_some(outcome),
            items: Vec::new(),
            log: String::new(),
            failures_in_a_row: failures,
        };
        write_last(&e.state, &pass).unwrap();
    }

    const PINNED: &str = "timeout_s = 60\nmin_interval_s = 0\nretry_s = 3600";

    /// A19, A23: a launcher starts only for a due item, never for the owner's kinds.
    #[test]
    fn starts_only_for_due_items() {
        let e = env(PINNED);
        assert_eq!(trigger(&e), (ArbiterStart::NotNeeded, 0));
        for (key, kind) in [
            ("disk_low:v", InboxKind::DiskLow),
            ("disk_critical:v", InboxKind::DiskCritical),
            ("arbiter_failing:p", InboxKind::ArbiterFailing),
            ("reclaim_stranded:p", InboxKind::ReclaimStranded),
        ] {
            raise(&e, key, kind);
        }
        assert_eq!(trigger(&e), (ArbiterStart::NotNeeded, 0));
        raise(&e, "reclaimable:/w@h", InboxKind::Reclaimable);
        assert_eq!(trigger(&e), (ArbiterStart::Started, 1));
    }

    /// At most one pass per `min_interval_s`.
    #[test]
    fn waits_for_the_interval() {
        let e = env("timeout_s = 60\nmin_interval_s = 3600\nretry_s = 3600");
        raise(&e, "reclaimable:/w@h", InboxKind::Reclaimable);
        last(&e, "p1", state::now_ms(), true, 0);
        assert_eq!(trigger(&e), (ArbiterStart::Interval, 0));
        last(&e, "p1", state::now_ms() - 3_600_000, true, 0);
        assert_eq!(trigger(&e), (ArbiterStart::Started, 1));
    }

    /// A20: a pass runs while its launcher holds the lock, and after the launcher died until
    /// `timeout_s` after the pass started.
    #[test]
    fn reports_a_running_pass() {
        let e = env(PINNED);
        raise(&e, "reclaimable:/w@h", InboxKind::Reclaimable);
        let held = state::try_lock(&e.state.arbiter_lock(), LockMode::Exclusive)
            .unwrap()
            .unwrap();
        assert_eq!(trigger(&e), (ArbiterStart::Running, 0));
        drop(held);
        last(&e, "p1", state::now_ms() - 59_000, false, 0);
        assert_eq!(trigger(&e), (ArbiterStart::Running, 0));
        assert_eq!(read_last(&e.state).unwrap().unwrap().outcome, None);
    }

    /// A `last.json` that exists but cannot be read is an error, and no launcher starts: a pass
    /// history the trigger cannot read never lets a pass start before `min_interval_s`.
    #[test]
    fn refuses_an_unreadable_history() {
        let e = env(PINNED);
        raise(&e, "reclaimable:/w@h", InboxKind::Reclaimable);
        std::fs::write(e.state.arbiter_last(), b"{\"schema\":").unwrap();
        let (decision, started) = try_trigger(&e);
        assert_eq!(decision.unwrap_err().code, ErrorCode::StateUnavailable);
        assert_eq!(started, 0);
    }

    /// A20: the trigger raises the `arbiter_failing` a launcher died before raising after it
    /// recorded the third or a later failure in a row, once; a later failure raises none while
    /// it is open.
    #[test]
    fn replays_the_alert() {
        let e = env(PINNED);
        let failing = |e: &Env| -> Vec<String> {
            inbox::read_all(&e.state)
                .unwrap()
                .into_iter()
                .filter(|(_, item)| item.kind == InboxKind::ArbiterFailing)
                .map(|(_, item)| item.key)
                .collect()
        };
        let ended = state::now_ms() - 1_000;
        last(&e, "p3", ended, true, 3);
        assert_eq!(trigger(&e), (ArbiterStart::NotNeeded, 0));
        assert_eq!(failing(&e), [format!("arbiter_failing:p3@{ended}")]);
        assert_eq!(trigger(&e), (ArbiterStart::NotNeeded, 0));
        last(&e, "p4", ended + 1, true, 4);
        assert_eq!(trigger(&e), (ArbiterStart::NotNeeded, 0));
        assert_eq!(failing(&e), [format!("arbiter_failing:p3@{ended}")]);
        // More than three in a row and no occurrence open: raised too.
        let e = env(PINNED);
        last(&e, "p5", ended, true, 5);
        assert_eq!(trigger(&e), (ArbiterStart::NotNeeded, 0));
        assert_eq!(failing(&e), [format!("arbiter_failing:p5@{ended}")]);
    }

    /// A19, A20: past `timeout_s`, the trigger records the dead launcher's pass `abandoned`
    /// in `last.json` and in its items that have no outcome, all with one end time; an item
    /// that has one keeps it. The third failure in a row raises `arbiter_failing`.
    #[test]
    fn records_an_abandoned_pass() {
        let e = env(PINNED);
        let started = state::now_ms() - 61_000;
        raise(&e, "reclaimable:/a@h", InboxKind::Reclaimable);
        raise(&e, "reclaimable:/b@h", InboxKind::Reclaimable);
        let keys = ["reclaimable:/a@h".to_owned(), "reclaimable:/b@h".to_owned()];
        inbox::assign(&e.state, "p1", &keys, started).unwrap();
        // The launcher wrote b's outcome before it died: b keeps it.
        let b = inbox::read_all(&e.state)
            .unwrap()
            .into_iter()
            .find(|(_, item)| item.key == keys[1])
            .unwrap();
        let mut b_item = b.1;
        b_item.arbiter.as_mut().unwrap().outcome = Some(PassOutcome::Completed);
        b_item.arbiter.as_mut().unwrap().ended_ms = Some(started + 1);
        std::fs::write(&b.0, serde_json::to_vec(&b_item).unwrap()).unwrap();
        last(&e, "p1", started, false, 2);

        assert_eq!(trigger(&e), (ArbiterStart::NotNeeded, 0));
        let pass = read_last(&e.state).unwrap().unwrap();
        assert_eq!(pass.outcome, Some(PassOutcome::Abandoned));
        assert_eq!(pass.ended_ms, Some(started + 60_000));
        assert_eq!(pass.failures_in_a_row, 3);
        let items = inbox::read_all(&e.state).unwrap();
        let arbiter = |key: &str| {
            items
                .iter()
                .find(|(_, item)| item.key == key)
                .and_then(|(_, item)| item.arbiter.clone())
                .unwrap()
        };
        assert_eq!(arbiter(&keys[0]).outcome, Some(PassOutcome::Abandoned));
        assert_eq!(arbiter(&keys[0]).ended_ms, Some(started + 60_000));
        assert_eq!(arbiter(&keys[1]).outcome, Some(PassOutcome::Completed));
        assert_eq!(arbiter(&keys[1]).ended_ms, Some(started + 1));
        let failing: Vec<_> = items
            .iter()
            .filter(|(_, item)| item.kind == InboxKind::ArbiterFailing)
            .collect();
        assert_eq!(failing.len(), 1);
        assert_eq!(
            failing[0].1.key,
            format!("arbiter_failing:p1@{}", started + 60_000)
        );
        assert_eq!(failing[0].1.status, InboxStatus::Open);
    }
}
