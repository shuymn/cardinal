//! The Claude Code adapter: hook input and output and the session-end snapshot
//! (`docs/architecture.md` section 6). Classification and the rewrite itself are `classify`'s.
//!
//! The CLI exits 0 for every hook, whatever these return, and writes the output of
//! `pre_tool_use` to stdout only when it returns `Ok`.

mod hook;
mod repo;
mod session;

use std::io::{Read, Write};
use std::path::Path;
use std::time::Instant;

use serde_json::{Map, Value};

use crate::config::{self, Config, HostConfig};
use crate::contract::{Error, ErrorCode, RecordKind};
use crate::os::{self, Detail, ProcessTable};
use crate::record::Recorder;
use crate::state::{Durability, StateDir, now_ms};

use hook::{Pass, Settings};

/// The most hook input read from stdin.
const MAX_INPUT: u64 = 1024 * 1024;

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

/// Appends one record to the state dir from the environment, with the record limits of
/// `host` (else the defaults). Failures are returned for the caller to ignore.
fn append(
    host: Option<&HostConfig>,
    kind: RecordKind,
    session_id: Option<String>,
    data: Map<String, Value>,
) -> Result<(), Error> {
    let state = open_state()?;
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
    recorder
        .append(&record)
        .map_err(|err| Error::new(ErrorCode::StateUnavailable, err.to_string()))
}

fn open_state() -> Result<StateDir, Error> {
    let dir = config::state_dir()
        .map_err(|err| Error::new(ErrorCode::StateUnavailable, err.to_string()))?;
    Ok(StateDir::open(&dir)?)
}

/// `cardinal hook claude pre-tool-use`: reads the hook input from `input` and writes the hook
/// output (or nothing) to `output`.
pub fn pre_tool_use(input: &mut dyn Read, output: &mut dyn Write) -> Result<(), Error> {
    let start = Instant::now();
    let var = |name: &str| std::env::var(name).ok();
    run_pre_tool_use(input, output, cfg!(windows), &var, start)
}

fn run_pre_tool_use(
    input: &mut dyn Read,
    output: &mut dyn Write,
    windows: bool,
    var: &dyn Fn(&str) -> Option<String>,
    start: Instant,
) -> Result<(), Error> {
    // Section 5.5: phase 0 does not rewrite on Windows.
    if windows {
        return Ok(());
    }
    let doc = read_input(input)?;
    let input = hook::parse(&doc).map_err(|pass| invalid(format!("{pass:?}")))?;

    let config = match config::config_dir().and_then(|dir| Config::load(&dir)) {
        Ok(config) => config,
        Err(err) => {
            let message = err.to_string();
            record_pass(None, &input, &Pass::Config(message.clone()));
            return Err(Error::new(ErrorCode::ConfigInvalid, message));
        }
    };
    if !config.host.hook.enabled {
        return Ok(());
    }
    // A directory passes the shell's `-x` test; the rewrite's `-f` test would only ever take
    // the fallback.
    let runner = match config.host.hook.runner_path() {
        Ok(runner) if Path::new(&runner).is_dir() => Err(format!(
            "the runner path {runner} is a directory, not the cardinal binary"
        )),
        Ok(runner) => Ok(runner),
        Err(err) => Err(err.to_string()),
    };
    let runner = match runner {
        Ok(runner) => runner,
        Err(message) => {
            record_pass(Some(&config.host), &input, &Pass::Config(message.clone()));
            return Err(Error::new(ErrorCode::ConfigInvalid, message));
        }
    };
    let settings = Settings {
        config: &config,
        runner: &runner,
        var,
        deadline: hook::deadline(&config, start),
        start,
        fallback_cwd: std::env::current_dir().ok(),
    };
    match hook::evaluate(&input, &settings) {
        Ok(doc) => {
            let mut line = serde_json::to_vec(&doc)
                .map_err(|err| Error::new(ErrorCode::Internal, err.to_string()))?;
            line.push(b'\n');
            output
                .write_all(&line)
                .map_err(|err| Error::new(ErrorCode::Internal, err.to_string()))
        }
        Err(pass) => {
            record_pass(Some(&config.host), &input, &pass);
            Ok(())
        }
    }
}

/// Records a configuration error or a hook failure; other reasons to print nothing are
/// normal and leave no record.
fn record_pass(host: Option<&HostConfig>, input: &hook::Call, pass: &Pass) {
    let (kind, reason, detail) = match pass {
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
    data.insert("tool_use_id".into(), input.tool_use_id.clone().into());
    let _ = append(host, kind, input.session_id.clone(), data);
}

/// The data of a `subagent_stopped` or `session_ended` record.
fn end_data(doc: &Map<String, Value>) -> Map<String, Value> {
    ["agent_id", "agent_type", "reason", "cwd"]
        .into_iter()
        .map(|key| (key.to_owned(), string(doc, key).into()))
        .collect()
}

fn load_host() -> Option<HostConfig> {
    config::config_dir()
        .and_then(|dir| Config::load(&dir))
        .ok()
        .map(|config| config.host)
}

/// `cardinal hook claude subagent-stop`: records the end; never stops processes.
pub fn subagent_stop(input: &mut dyn Read) -> Result<(), Error> {
    let doc = read_input(input)?;
    let host = load_host();
    append(
        host.as_ref(),
        RecordKind::SubagentStopped,
        string(&doc, "session_id"),
        end_data(&doc),
    )
}

/// `cardinal hook claude session-end`: records the end and writes the session snapshot;
/// never stops processes.
pub fn session_end(input: &mut dyn Read) -> Result<(), Error> {
    let doc = read_input(input)?;
    let session_id = string(&doc, "session_id");
    let host = load_host();
    // The snapshot comes first: it must list the processes as close to the end as possible,
    // and the record append may wait for a rotation.
    let snapshot = session_id.as_deref().map(|id| {
        let table = ProcessTable::snapshot(Detail {
            cmd: true,
            cwd: false,
        })
        .map_err(|err| Error::new(ErrorCode::Internal, err.to_string()))?;
        let reason = string(&doc, "reason").unwrap_or_default();
        let Some(snapshot) = session::snapshot(&table, os::current_pid(), id, &reason, now_ms())
        else {
            return Ok(());
        };
        let state = open_state()?;
        let bytes = serde_json::to_vec(&snapshot)
            .map_err(|err| Error::new(ErrorCode::Internal, err.to_string()))?;
        state
            .write_atomic(&state.session(id), &bytes, Durability::Volatile)
            .map_err(Error::from)
    });
    let recorded = append(
        host.as_ref(),
        RecordKind::SessionEnded,
        session_id,
        end_data(&doc),
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
