//! The Claude Code adapter: hook input and output, the session hooks' lease writes, and the
//! session-end snapshot (`docs/architecture.md` sections 6 and 9.8). Classification and the rewrite itself are `classify`'s.
//!
//! The CLI exits 0 for every hook, whatever these return, and writes the output of
//! `pre_tool_use` to stdout only when it returns `Ok`.

mod hook;
mod lease;
mod repo;
mod session;

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::config::{self, Base, Config, HostConfig};
use crate::contract::{Error, ErrorCode, LeaseEvent, RecordKind};
use crate::os::{self, Detail, ProcessTable};
use crate::record::{RecordError, Recorder};
use crate::state::{Durability, StateDir, now_ms};
use crate::testpoint;

use hook::{Pass, Settings};

pub(crate) use hook::timeout_ms;
pub(crate) use repo::{Repo, origin};

/// The most hook input read from stdin.
const MAX_INPUT: u64 = 1024 * 1024;

/// The environment the hook resolves its directories and settings from.
type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

fn process_env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidArgument, message)
}

/// Reads at most 1 MiB of hook input as a JSON object.
fn read_input(input: &mut dyn Read) -> Result<Map<String, Value>, Error> {
    let mut bytes = Vec::new();
    input
        .take(MAX_INPUT + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| invalid(format!("cannot read the hook input: {err}")))?;
    if bytes.len() as u64 > MAX_INPUT {
        return Err(invalid("the hook input exceeds 1 MiB"));
    }
    match serde_json::from_slice(&bytes) {
        Ok(Value::Object(doc)) => Ok(doc),
        Ok(_) => Err(invalid("the hook input is not a JSON object")),
        Err(err) => Err(invalid(format!("the hook input is not JSON: {err}"))),
    }
}

fn string(doc: &Map<String, Value>, key: &str) -> Option<String> {
    doc.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// Appends one record to the state dir from `env`, with the record limits of `host` (else
/// the defaults). Failures are returned for the caller to ignore.
fn append(
    env: Env<'_>,
    host: Option<&HostConfig>,
    kind: RecordKind,
    session_id: Option<String>,
    data: Map<String, Value>,
) -> Result<(), Error> {
    append_until(env, host, kind, session_id, data, None)
}

/// [`append`], waiting for a rotation at most until `deadline` when one is given: a record
/// that cannot be appended by then is dropped.
fn append_until(
    env: Env<'_>,
    host: Option<&HostConfig>,
    kind: RecordKind,
    session_id: Option<String>,
    data: Map<String, Value>,
    deadline: Option<Instant>,
) -> Result<(), Error> {
    append_line(env, host, kind, session_id, data, deadline).map_err(|failed| failed.error)
}

/// An append that failed, and whether its line was written all the same: only the rotation
/// after it failed.
struct AppendFailed {
    error: Error,
    written: bool,
}

/// [`append_until`], telling a record dropped from one appended whose rotation failed.
fn append_line(
    env: Env<'_>,
    host: Option<&HostConfig>,
    kind: RecordKind,
    session_id: Option<String>,
    data: Map<String, Value>,
    deadline: Option<Instant>,
) -> Result<(), AppendFailed> {
    let state = open_state(env).map_err(|error| AppendFailed {
        error,
        written: false,
    })?;
    let defaults;
    let host = if let Some(host) = host {
        host
    } else {
        defaults = HostConfig::defaults();
        &defaults
    };
    let recorder = Recorder::new(&state, host.records.max_bytes, host.records.keep);
    let mut record = recorder.record(kind);
    record.session_id = session_id;
    record.data = data;
    match deadline {
        Some(deadline) => recorder.append_until(&record, deadline),
        None => recorder.append(&record),
    }
    .map_err(|err| AppendFailed {
        written: matches!(err, RecordError::Rotate(_)),
        error: Error::new(ErrorCode::StateUnavailable, err.to_string()),
    })
}

fn open_state(env: Env<'_>) -> Result<StateDir, Error> {
    let dir = config::resolve(Base::State, cfg!(windows), env)
        .map_err(|err| Error::new(ErrorCode::StateUnavailable, err.to_string()))?;
    Ok(StateDir::open(&dir)?)
}

/// `cardinal hook claude pre-tool-use`: reads the hook input from `input` and writes the hook
/// output (or nothing) to `output`.
pub fn pre_tool_use(input: &mut dyn Read, output: &mut dyn Write) -> Result<(), Error> {
    run_pre_tool_use(input, output, cfg!(windows), &process_env, Instant::now())
}

fn run_pre_tool_use(
    input: &mut dyn Read,
    output: &mut dyn Write,
    windows: bool,
    env: Env<'_>,
    start: Instant,
) -> Result<(), Error> {
    // Section 5.5: phase 0 does not rewrite on Windows.
    if windows {
        return Ok(());
    }
    let doc = match read_input(input) {
        Ok(doc) => doc,
        Err(err) => {
            record_pass(env, None, &Pass::Invalid(err.message.clone()), None, None);
            return Err(err);
        }
    };
    let input = match hook::parse(&doc) {
        Ok(input) => input,
        Err(pass) => {
            let (session_id, tool_use_id) =
                (string(&doc, "session_id"), string(&doc, "tool_use_id"));
            record_pass(env, None, &pass, session_id, tool_use_id);
            return Err(invalid(format!("{pass:?}")));
        }
    };
    let record = |host: Option<&HostConfig>, pass: &Pass| {
        let ids = (input.session_id.clone(), input.tool_use_id.clone());
        record_pass(env, host, pass, ids.0, ids.1);
    };

    let config =
        match config::resolve(Base::Config, windows, env).and_then(|dir| Config::load(&dir)) {
            Ok(config) => config,
            Err(err) => {
                let message = err.to_string();
                record(None, &Pass::Config(message.clone()));
                return Err(Error::new(ErrorCode::ConfigInvalid, message));
            }
        };
    if !config.host.hook.enabled {
        return Ok(());
    }
    let runner = match config.host.hook.runner_path() {
        Ok(runner) => runner,
        Err(err) => {
            let message = err.to_string();
            record(Some(&config.host), &Pass::Config(message.clone()));
            return Err(Error::new(ErrorCode::ConfigInvalid, message));
        }
    };
    let var = |name: &str| env(name).and_then(|value| value.into_string().ok());
    let settings = Settings {
        config: &config,
        runner: &runner,
        var: &var,
        deadline: hook::deadline(&config, start),
        start,
        fallback_cwd: std::env::current_dir().ok(),
    };
    match hook::evaluate(&input, &settings) {
        Ok(doc) => {
            let mut line = serde_json::to_vec(&doc)
                .map_err(|err| Error::new(ErrorCode::Internal, err.to_string()))?;
            line.push(b'\n');
            // Rewriting and serializing a large command takes time too.
            if Instant::now() > settings.deadline {
                record(Some(&config.host), &hook::missed(start));
                return Ok(());
            }
            output
                .write_all(&line)
                .map_err(|err| Error::new(ErrorCode::Internal, err.to_string()))
        }
        Err(pass) => {
            record(Some(&config.host), &pass);
            Ok(())
        }
    }
}

/// Records input the hook cannot use, a configuration error, or a hook failure; other
/// reasons to print nothing are normal and leave no record.
fn record_pass(
    env: Env<'_>,
    host: Option<&HostConfig>,
    pass: &Pass,
    session_id: Option<String>,
    tool_use_id: Option<String>,
) {
    let (kind, reason, detail) = match pass {
        Pass::Invalid(message) => (RecordKind::HookError, "invalid_input", message.clone()),
        Pass::NoCommand => (
            RecordKind::HookError,
            "invalid_input",
            "the Bash call has no string command".to_owned(),
        ),
        Pass::Config(message) => (RecordKind::ConfigError, "config_invalid", message.clone()),
        Pass::DeadlineMissed { elapsed_ms } => (
            RecordKind::HookError,
            "deadline_missed",
            format!("classification took {elapsed_ms} ms"),
        ),
        Pass::Failed(message) => (RecordKind::HookError, "failed", message.clone()),
        _ => return,
    };
    let mut data = Map::new();
    data.insert("event".into(), "pre_tool_use".into());
    data.insert("reason".into(), reason.into());
    data.insert("detail".into(), detail.into());
    data.insert("tool_use_id".into(), tool_use_id.into());
    let _ = append(env, host, kind, session_id, data);
}

/// The data of a `subagent_stopped` or `session_ended` record.
fn end_data(doc: &Map<String, Value>) -> Map<String, Value> {
    ["agent_id", "agent_type", "reason", "cwd"]
        .into_iter()
        .map(|key| (key.to_owned(), string(doc, key).into()))
        .collect()
}

/// `host.toml` alone: no repository profile can make a session hook fall back to defaults,
/// and neither can a `host.toml` entry that is there but cannot be read, a symbolic link
/// whose target is missing included.
fn load_host() -> Option<HostConfig> {
    config::config_dir()
        .and_then(|dir| HostConfig::load_present(&dir))
        .ok()
}

/// How long `session-start` and `subagent-stop` wait for a lease lock (section 9.8).
const LEASE_WAIT: Duration = Duration::from_secs(1);

/// The budget of `session-start` and `subagent-stop` from their start to their last write
/// (section 9.8), well within the `timeout: 5` of section 6.2's settings.
const HOOK_BUDGET: Duration = Duration::from_secs(2);

/// How long `session-end` waits for a lease lock, within `SessionEnd`'s budget (section 9.8).
const LEASE_WAIT_AT_END: Duration = Duration::from_millis(250);

/// `SessionEnd`'s budget from the hook's start to its last write (section 9.8): Claude Code's
/// default 1.5 s, less 100 ms for the process's start before `start` and its exit.
const SESSION_END_BUDGET: Duration = Duration::from_millis(1400);

/// The least of `SessionEnd`'s budget its snapshot starts with: the process listing cannot
/// stop part-way. On the macOS development host (1,019 processes, a debug build) the listing
/// with command lines, the snapshot, and its serialization took 9 ms at the median and 14 ms
/// at most over 20 runs; 100 ms leaves about seven times that for the write and a busier host.
const SNAPSHOT_MIN: Duration = Duration::from_millis(100);

/// Updates the session's leases for `kind` (section 9.8) within `times`; returns the records
/// to append once every lease lock is released. Input without a session ID or a `cwd` writes
/// nothing.
fn update_leases(
    doc: &Map<String, Value>,
    kind: LeaseEvent,
    host: Option<&HostConfig>,
    times: lease::Times,
) -> Vec<lease::Pending> {
    let (Some(session_id), Some(cwd)) = (string(doc, "session_id"), string(doc, "cwd")) else {
        return Vec::new();
    };
    let input = lease::Input {
        kind,
        session_id: &session_id,
        cwd: Path::new(&cwd),
        transcript_path: string(doc, "transcript_path"),
    };
    lease::update(
        &input,
        cfg!(windows),
        || open_state(&process_env).ok(),
        host.map(|host| host.sessions.linger_s),
        times,
    )
}

/// Appends the records a lease update returned until `deadline`; returns how many were
/// dropped. A record appended whose rotation then failed is not dropped.
fn append_pending(
    host: Option<&HostConfig>,
    session_id: Option<&String>,
    pending: Vec<lease::Pending>,
    deadline: Instant,
) -> usize {
    pending
        .into_iter()
        .filter(|record| {
            append_line(
                &process_env,
                host,
                record.kind,
                session_id.cloned(),
                record.data.clone(),
                Some(deadline),
            )
            .is_err_and(|failed| !failed.written)
        })
        .count()
}

/// The data of the hook's own record, with how many of its diagnostics were dropped.
fn end_record(doc: &Map<String, Value>, dropped: usize) -> Map<String, Value> {
    let mut data = end_data(doc);
    if dropped > 0 {
        data.insert("dropped_records".into(), dropped.into());
    }
    data
}

/// `cardinal hook claude session-start`: upserts the session's leases; appends no record of
/// its own.
pub fn session_start(input: &mut dyn Read) -> Result<(), Error> {
    let start = Instant::now();
    let deadline = start + HOOK_BUDGET;
    let doc = read_input(input)?;
    let host = load_host();
    let pending = update_leases(
        &doc,
        LeaseEvent::SessionStart,
        host.as_ref(),
        lease::Times {
            start,
            lock_deadline: (start + LEASE_WAIT).min(deadline),
            deadline,
        },
    );
    append_pending(
        host.as_ref(),
        string(&doc, "session_id").as_ref(),
        pending,
        deadline,
    );
    Ok(())
}

/// `cardinal hook claude subagent-stop`: upserts the session's leases, then records the end;
/// never stops processes.
pub fn subagent_stop(input: &mut dyn Read) -> Result<(), Error> {
    let start = Instant::now();
    let deadline = start + HOOK_BUDGET;
    let doc = read_input(input)?;
    let host = load_host();
    let pending = update_leases(
        &doc,
        LeaseEvent::SubagentStop,
        host.as_ref(),
        lease::Times {
            start,
            lock_deadline: (start + LEASE_WAIT).min(deadline),
            deadline,
        },
    );
    let session_id = string(&doc, "session_id");
    let dropped = append_pending(host.as_ref(), session_id.as_ref(), pending, deadline);
    append_until(
        &process_env,
        host.as_ref(),
        RecordKind::SubagentStopped,
        session_id,
        end_record(&doc, dropped),
        Some(deadline),
    )
}

/// `cardinal hook claude session-end`: upserts the session's leases, writes the session
/// snapshot, and records the end, in that order, all within one budget from its start: each
/// step takes only what the steps before it left, and a step past the budget is dropped, the
/// snapshot never started; never stops processes.
pub fn session_end(input: &mut dyn Read) -> Result<(), Error> {
    let start = Instant::now();
    let deadline = start + SESSION_END_BUDGET;
    let doc = read_input(input)?;
    let session_id = string(&doc, "session_id");
    let host = load_host();
    // The lease comes first (section 9.8): it takes no lock but the lease file's, and the
    // snapshot and the records come after it within `SessionEnd`'s budget.
    let pending = update_leases(
        &doc,
        LeaseEvent::SessionEnd,
        host.as_ref(),
        lease::Times {
            start,
            lock_deadline: (start + LEASE_WAIT_AT_END).min(deadline),
            deadline,
        },
    );
    let _ = testpoint::hit(testpoint::HOOK_SNAPSHOT_ADMISSION);
    // The snapshot comes before the records: it must list the processes as close to the end as
    // possible, and the record append may wait for a rotation. It starts only with
    // `SNAPSHOT_MIN` left, since the listing cannot stop part-way.
    let snapshot = session_id
        .as_deref()
        .filter(|_| deadline.saturating_duration_since(Instant::now()) >= SNAPSHOT_MIN)
        .map(|id| {
            let table = ProcessTable::snapshot(Detail {
                cmd: true,
                cwd: false,
            })
            .map_err(|err| Error::new(ErrorCode::Internal, err.to_string()))?;
            let reason = string(&doc, "reason").unwrap_or_default();
            let Some(snapshot) =
                session::snapshot(&table, os::current_pid(), id, &reason, now_ms(), deadline)
            else {
                return Ok(());
            };
            let state = open_state(&process_env)?;
            let bytes = serde_json::to_vec(&snapshot)
                .map_err(|err| Error::new(ErrorCode::Internal, err.to_string()))?;
            state
                .write_atomic(&state.session(id), &bytes, Durability::Volatile)
                .map_err(Error::from)
        });
    let dropped = append_pending(host.as_ref(), session_id.as_ref(), pending, deadline);
    let recorded = append_until(
        &process_env,
        host.as_ref(),
        RecordKind::SessionEnded,
        session_id,
        end_record(&doc, dropped),
        Some(deadline),
    );
    snapshot.transpose().and(recorded)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::run_pre_tool_use;

    /// On Windows the hook returns before it reads its input or configuration: input that
    /// fails everywhere else passes there with empty output.
    #[test]
    fn windows_never_rewrites() {
        let input = b"not json";
        let none = |_: &str| None;
        let mut output = Vec::new();
        run_pre_tool_use(&mut &input[..], &mut output, true, &none, Instant::now()).unwrap();
        assert_eq!(output, b"");
        let mut output = Vec::new();
        run_pre_tool_use(&mut &input[..], &mut output, false, &none, Instant::now()).unwrap_err();
        assert_eq!(output, b"");
    }

    #[test]
    fn input_is_bounded_and_must_be_an_object() {
        let big = vec![b' '; 1024 * 1024 + 1];
        assert!(super::read_input(&mut &big[..]).is_err());
        assert!(super::read_input(&mut &b"[1]"[..]).is_err());
        assert!(super::read_input(&mut &b"{"[..]).is_err());
        assert!(super::read_input(&mut &b" {} "[..]).is_ok());
    }
}
