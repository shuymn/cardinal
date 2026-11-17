//! Explanation documents (`docs/architecture.md` section 10.2): where a decision came from,
//! as configuration keys, rules, observations, and records.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::classify;
use crate::config::{self, Config, Selection};
use crate::contract::{
    ActResult, CheckResult, ClassifyDecision, ClassifyRepo, ClassifyRequest, ClassifyRequestSchema,
    ClassifyResponse, ClassifyTool, Error, ErrorCode, Explain, ExplainKind, ExplainSchema,
    ExplainSource, ExplainSubject, Plan, Record, RecordKind, Reservation, ToolName,
};
use crate::inbox;
use crate::plan;
use crate::record;
use crate::state::{self, Probe, StateDir};

/// What to explain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subject {
    Job(String),
    Plan(String),
    Inbox(String),
    /// A classification and rewrite trace without running anything.
    Command {
        cwd: Option<PathBuf>,
        command: String,
    },
}

/// `cardinal explain`.
pub fn explain(config: &Config, state: &StateDir, subject: &Subject) -> Result<Explain, Error> {
    match subject {
        Subject::Job(id) => job(state, id),
        Subject::Plan(id) => plan(config, state, id),
        Subject::Inbox(key) => inbox_item(state, key),
        Subject::Command { cwd, command } => command_trace(config, cwd.as_deref(), command),
    }
}

fn records(state: &StateDir) -> Result<Vec<Record>, Error> {
    record::read_all(state).map_err(|err| {
        Error::new(
            ErrorCode::StateUnavailable,
            format!("{}: {err}", state.records_dir().display()),
        )
    })
}

/// The JSON name of a `snake_case` enum value.
fn name(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn record_source(record: &Record) -> ExplainSource {
    ExplainSource::Record {
        record: serde_json::to_value(record)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default(),
    }
}

fn document(
    kind: ExplainKind,
    id: String,
    summary: String,
    sources: Vec<ExplainSource>,
    outcome: Option<String>,
) -> Explain {
    Explain {
        schema: ExplainSchema,
        subject: ExplainSubject { kind, id },
        summary,
        sources,
        rewritten: None,
        outcome,
    }
}

/// `explain job ID`: the job's records and reservation. A job with `job_started` but no
/// `job_finished` whose runner no longer holds its live lock is `runner_died`.
fn job(state: &StateDir, prefix: &str) -> Result<Explain, Error> {
    let records = records(state)?;
    let mut ids: Vec<String> = records.iter().filter_map(|r| r.job_id.clone()).collect();
    if let Ok(entries) = std::fs::read_dir(state.reservations_dir()) {
        ids.extend(entries.filter_map(Result::ok).filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(".json"))
                .map(str::to_owned)
        }));
    }
    ids.sort();
    ids.dedup();
    let id = state::resolve_prefix(&ids, prefix)?;
    let own: Vec<&Record> = records
        .iter()
        .filter(|r| r.job_id.as_deref() == Some(&id))
        .collect();
    let reservation: Option<Reservation> = std::fs::read(state.reservation(&id))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let find = |kind: RecordKind| own.iter().rev().find(|r| r.kind == kind).copied();
    let runner_alive = matches!(state::probe(&state.live_lock(&id)), Ok(Probe::Held));

    let outcome = if let Some(finished) = find(RecordKind::JobFinished) {
        Some(
            match (finished.data.get("signal"), finished.data.get("exit")) {
                (Some(signal), _) if !signal.is_null() => format!("signaled:{signal}"),
                (_, Some(exit)) if !exit.is_null() => format!("exited:{exit}"),
                _ => "finished".to_owned(),
            },
        )
    } else if let Some(failure) = find(RecordKind::JobInfraFailure) {
        Some(format!("infra_failure:{}", text(failure, "reason")))
    } else if find(RecordKind::JobStarted).is_some() {
        Some(
            if runner_alive {
                "running"
            } else {
                "runner_died"
            }
            .to_owned(),
        )
    } else if let Some(fail_open) = find(RecordKind::JobFailOpen) {
        Some(format!("fail_open:{}", text(fail_open, "reason")))
    } else if reservation.is_some() {
        Some(
            if runner_alive {
                "waiting"
            } else {
                "runner_died"
            }
            .to_owned(),
        )
    } else {
        None
    };

    let mut summary = Vec::new();
    if let Some(finished) = find(RecordKind::JobFinished) {
        let admission = finished
            .data
            .get("admission")
            .and_then(|v| v.as_str())
            .map_or("admitted", |a| match a {
                "overflow" => "started over budget",
                _ => "admitted within budget",
            });
        let waited = finished
            .data
            .get("waited_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        summary.push(format!("{admission} after {waited} ms"));
    }
    if let Some(outcome) = &outcome {
        summary.push(outcome.clone());
    }
    if let Some(reservation) = &reservation {
        summary.push(format!("reservation {:?}", reservation.state).to_lowercase());
    }
    let mut sources: Vec<ExplainSource> = own.iter().map(|r| record_source(r)).collect();
    if let Some(reservation) = &reservation {
        sources.push(ExplainSource::Observation {
            command: vec!["read".into(), state.reservation(&id).display().to_string()],
            exit: None,
            result: serde_json::to_string(reservation).unwrap_or_default(),
        });
    }
    if own.is_empty() && reservation.is_none() {
        return Err(Error::new(ErrorCode::NotFound, format!("no job `{id}`")));
    }
    Ok(document(
        ExplainKind::Job,
        id,
        if summary.is_empty() {
            "no outcome recorded".to_owned()
        } else {
            summary.join("; ")
        },
        sources,
        outcome,
    ))
}

fn text(record: &Record, field: &str) -> String {
    match record.data.get(field) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => "unknown".to_owned(),
    }
}

/// `explain plan ID`: the plan's checks, its act result, and its records, plus the
/// `[unit.<name>] act` key of a unit whose `act` was turned into a dry run.
fn plan(config: &Config, state: &StateDir, prefix: &str) -> Result<Explain, Error> {
    let plan: Plan = plan::load(state, prefix)?;
    let result: Option<ActResult> = std::fs::read(state.plan_result(&plan.id))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let claimed = state.plan_claim(&plan.id).exists();
    let records = records(state)?;
    let own: Vec<&Record> = records
        .iter()
        .filter(|r| r.plan_id.as_deref() == Some(plan.id.as_str()))
        .collect();
    let mut sources: Vec<ExplainSource> = Vec::new();
    for record in &own {
        if record.kind == RecordKind::Planned
            && let Some(unit) = record
                .data
                .get("caller")
                .and_then(|c| c.as_str())
                .and_then(|c| c.strip_prefix("unit:"))
        {
            sources.push(ExplainSource::Config {
                file: config.dir.join("host.toml").display().to_string(),
                key: format!("unit.{unit}.act"),
                value: serde_json::Value::Bool(config.host.unit_may_act(unit)),
            });
        }
        sources.push(record_source(record));
    }
    let failing: Vec<&str> = plan
        .checks
        .iter()
        .filter(|c| c.result != CheckResult::Pass)
        .map(|c| c.name.as_str())
        .collect();
    let mut summary = format!("{} of {}", name(plan.kind), plan.target.path);
    if failing.is_empty() {
        summary.push_str("; every check passed when planned");
    } else {
        let _ = write!(
            summary,
            "; not passing when planned: {}",
            failing.join(", ")
        );
    }
    let outcome = match (&result, claimed) {
        (Some(result), _) => {
            let _ = write!(summary, "; act by {}", result.caller);
            Some(name(result.status))
        }
        (None, true) => Some("interrupted".to_owned()),
        (None, false) => None,
    };
    Ok(document(
        ExplainKind::Plan,
        plan.id,
        summary,
        sources,
        outcome,
    ))
}

/// `explain inbox KEY`: the item and its records.
fn inbox_item(state: &StateDir, key: &str) -> Result<Explain, Error> {
    let item = inbox::read_all(state)?
        .into_iter()
        .map(|(_, item)| item)
        .find(|item| item.key == key)
        .ok_or_else(|| {
            Error::new(
                ErrorCode::NotFound,
                format!("no inbox item has the key `{key}`"),
            )
        })?;
    let mut sources = vec![ExplainSource::Observation {
        command: vec!["refresh".into()],
        exit: None,
        result: serde_json::Value::Object(item.evidence.clone()).to_string(),
    }];
    sources.extend(
        records(state)?
            .iter()
            .filter(|r| r.inbox_key.as_deref() == Some(key))
            .map(record_source),
    );
    let status = name(item.status);
    let summary = format!(
        "{} {status}, seen {} time{}",
        name(item.kind),
        item.seen,
        if item.seen == 1 { "" } else { "s" }
    );
    let outcome = item.resolution.as_ref().map(|r| {
        let note = if r.note.is_empty() {
            String::new()
        } else {
            format!(": {}", r.note)
        };
        format!("{status} by {}{note}", r.by)
    });
    Ok(document(
        ExplainKind::Inbox,
        item.key.clone(),
        summary,
        sources,
        outcome,
    ))
}

/// How long `explain command` gives each stage's classification.
const CLASSIFY_DEADLINE: Duration = Duration::from_secs(2);

/// `explain command [--cwd DIR] -- STRING`: the stages and their classification, without
/// running anything. Profiles are selected by path; matching by remote needs git and is left
/// to the hook.
fn command_trace(config: &Config, cwd: Option<&Path>, command: &str) -> Result<Explain, Error> {
    let cwd = match cwd {
        Some(cwd) => cwd.to_path_buf(),
        None => std::env::current_dir()
            .map_err(|err| Error::new(ErrorCode::InvalidArgument, err.to_string()))?,
    };
    let root = state::find_worktree_root(&cwd);
    let common = root.as_deref().and_then(config::git_common_dir);
    let selection: Selection<'_> =
        config.select(|p| common.as_deref().is_some_and(|c| p.matches_common_dir(c)));
    let stages = classify::find_stages(command)?;
    let mut sources = Vec::new();
    let mut classified = Vec::new();
    for stage in &stages {
        let request = ClassifyRequest {
            schema: ClassifyRequestSchema,
            command: command.to_owned(),
            stage: command
                .get(stage.span.clone())
                .unwrap_or_default()
                .to_owned(),
            argv: stage.words.clone(),
            cwd: cwd.display().to_string(),
            repo: common
                .as_ref()
                .zip(root.as_ref())
                .map(|(c, r)| ClassifyRepo {
                    common_dir: c.display().to_string(),
                    main_worktree: r.display().to_string(),
                    remote: None,
                }),
            tool: ClassifyTool {
                name: ToolName::Bash,
                timeout_ms: 120_000,
                background: false,
            },
        };
        let response =
            classify::classify(&request, &selection, Instant::now() + CLASSIFY_DEADLINE)?;
        if response.decision == ClassifyDecision::Wrap {
            let rule = response.rule.clone().unwrap_or_default();
            let pattern = selection
                .rules()
                .find(|(_, r)| r.qualified_id == rule)
                .and_then(|(_, r)| r.patterns.first())
                .map_or_else(String::new, |p| p.text.clone());
            sources.push(ExplainSource::Rule {
                rule,
                pattern,
                normalized_argv: request.argv.clone(),
            });
            classified.push((stage, response));
        }
    }
    let mut summary = format!("{} stages, {} classified", stages.len(), classified.len());
    let mut rewritten = None;
    if !classified.is_empty() {
        let runner = std::env::current_exe().unwrap_or_default();
        match rewrite(command, &classified, &runner) {
            Ok(text) => rewritten = Some(text),
            Err(skips) => {
                let _ = write!(summary, "; not rewritten: {}", skips.join(", "));
            }
        }
    }
    let mut doc = document(
        ExplainKind::Command,
        command.to_owned(),
        summary,
        sources,
        None,
    );
    doc.rewritten = rewritten;
    Ok(doc)
}

/// The hook's rewrite (sections 6.4 and 7.2): the command text, its classified stages, and the
/// runner path in; the rewritten command, or the reasons it or its stages stay unwrapped, out.
/// A stand-in until the classification module provides the function, which the hook and
/// `explain` then share; until then every classified command reports `rewrite_unavailable`.
fn rewrite(
    command: &str,
    classified: &[(&classify::Stage, ClassifyResponse)],
    runner: &Path,
) -> Result<String, Vec<String>> {
    let _ = (command, classified, runner);
    Err(vec!["rewrite_unavailable".to_owned()])
}
