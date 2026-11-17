//! Explanation documents (`docs/architecture.md` section 10.2): where a decision came from,
//! as configuration keys, rules, observations, and records.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::adapter::claude::{self, Repo};
use crate::admission;
use crate::classify::{self, BuiltIn, CallIds, Context, Origin, Rewritten};
use crate::config::Config;
use crate::contract::{
    CheckResult, ClassifyTool, Error, ErrorCode, Explain, ExplainKind, ExplainSchema,
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
        Subject::Job(id) => job(config, state, id),
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
/// `job_finished` whose runner no longer holds its live lock is `runner_died`. The
/// reservations are read under the admission lock; when it is not free in time, the command
/// fails as it does when the records cannot be read.
fn job(config: &Config, state: &StateDir, prefix: &str) -> Result<Explain, Error> {
    let records = records(state)?;
    let reservations = admission::snapshot(state, &config.host)?;
    let mut ids: Vec<String> = records.iter().filter_map(|r| r.job_id.clone()).collect();
    ids.extend(reservations.iter().map(|r| r.job_id.clone()));
    ids.sort();
    ids.dedup();
    let id = state::resolve_prefix(&ids, prefix)?;
    let own: Vec<&Record> = records
        .iter()
        .filter(|r| r.job_id.as_deref() == Some(&id))
        .collect();
    let reservation: Option<Reservation> = reservations.into_iter().find(|r| r.job_id == id);
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

/// What a plan's checks were evaluated from: the configuration keys, each check's facts as
/// observations (the field, its value, and the tool or read that produced it), and what could
/// not be observed. Each `act` record carries the same for its own evaluation.
fn plan_sources(plan: &Plan) -> Vec<ExplainSource> {
    let config = plan
        .provenance
        .config
        .iter()
        .map(|key| ExplainSource::Config {
            file: key.file.clone(),
            key: key.key.clone(),
            value: key.value.clone(),
        });
    let facts = plan.checks.iter().flat_map(|check| {
        check
            .facts
            .iter()
            .map(move |fact| ExplainSource::Observation {
                command: vec![fact.source.clone()],
                exit: None,
                result: format!("{}: {} = {}", check.name, fact.name, fact.value),
            })
    });
    let errors = plan
        .provenance
        .errors
        .iter()
        .map(|error| ExplainSource::Observation {
            command: vec![error.source.clone()],
            exit: None,
            result: format!("error: {}", error.message),
        });
    config.chain(facts).chain(errors).collect()
}

/// `explain plan ID`: the plan's checks, its act result, and its records, plus the
/// `[unit.<name>] act` key of a unit whose `act` was turned into a dry run.
fn plan(config: &Config, state: &StateDir, prefix: &str) -> Result<Explain, Error> {
    let plan: Plan = plan::load(state, prefix)?;
    let records = records(state)?;
    let own: Vec<&Record> = records
        .iter()
        .filter(|r| r.plan_id.as_deref() == Some(plan.id.as_str()))
        .collect();
    let mut sources = plan_sources(&plan);
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
    // `act` reports without claiming; its latest record tells the outcome.
    let latest = own
        .iter()
        .rev()
        .find(|r| matches!(r.kind, RecordKind::Act | RecordKind::Planned));
    let outcome = latest.map(|record| {
        let status = text(record, "status");
        let _ = write!(summary, "; {status} by {}", text(record, "caller"));
        status
    });
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

/// `explain command [--cwd DIR] -- STRING`: each stage's classification and the rewrite the
/// hook would produce for a foreground Bash call of `command` in `cwd` that names no timeout,
/// session, or tool use, or null with the reasons in the summary. The trace and the rewrite
/// come from the functions the hook classifies and rewrites with (`classify::select`,
/// `decide`, `trace`, and `rewrite`), and the repository, its origin, and the timeout from
/// the hook's own lookups under the hook's deadline, so the two cannot drift apart.
fn command_trace(config: &Config, cwd: Option<&Path>, command: &str) -> Result<Explain, Error> {
    let deadline = Instant::now() + Duration::from_millis(config.host.hook.deadline_ms);
    let cwd = match cwd {
        Some(cwd) => cwd.to_path_buf(),
        None => std::env::current_dir()
            .map_err(|err| Error::new(ErrorCode::InvalidArgument, err.to_string()))?,
    };
    let (sources, summary, rewritten) = match classify::tokenize(command) {
        Ok(stages) => classify_command(config, &cwd, command, stages, deadline),
        Err(refusal) => (Vec::new(), format!("not rewritten: {refusal}"), None),
    };
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

/// Classifies the stages of `command` as the hook does (sections 6.3 and 7): the rule,
/// pattern, normalized argv, and profile key of each wrapped stage as sources, a summary
/// with why each other stage stays unwrapped, and the rewritten command.
fn classify_command(
    config: &Config,
    cwd: &Path,
    command: &str,
    stages: Vec<classify::Stage>,
    deadline: Instant,
) -> (Vec<ExplainSource>, String, Option<String>) {
    let repo = Repo::locate(cwd);
    let origin = || match &repo {
        Some(repo) => claude::origin(&config.host.observe.git, &repo.root, deadline),
        None => Origin::Unknown,
    };
    let common_dir = repo.as_ref().map(|repo| repo.common_dir.as_path());
    let Some((selection, remote)) = classify::select(config, &stages, common_dir, origin) else {
        return (Vec::new(), "not rewritten: deadline_missed".into(), None);
    };
    let var = |name: &str| std::env::var(name).ok();
    let tool = ClassifyTool {
        name: ToolName::Bash,
        timeout_ms: claude::timeout_ms(&serde_json::Map::new(), &var),
        background: false,
    };
    let classify_repo = common_dir.map(|dir| classify::request_repo(dir, remote.as_deref()));
    let cwd_text = cwd.to_string_lossy();
    let ctx = Context {
        command,
        cwd: &cwd_text,
        repo: classify_repo.as_ref(),
        tool: &tool,
    };
    let outcomes = classify::decide(
        stages,
        &ctx,
        &selection,
        &BuiltIn {
            selection: &selection,
        },
        deadline,
    );

    let traces = classify::trace(command, &outcomes, &selection);
    let mut sources = Vec::new();
    let mut summary = format!(
        "{} stages, {} wrapped",
        traces.len(),
        traces.iter().filter(|t| t.skip.is_none()).count()
    );
    for trace in &traces {
        if let Some(skip) = trace.skip {
            let _ = write!(
                summary,
                "; stage {} unwrapped: {}",
                trace.index,
                skip.as_str()
            );
            continue;
        }
        let Some(matched) = &trace.matched else {
            continue;
        };
        sources.push(ExplainSource::Rule {
            rule: matched.rule.clone(),
            pattern: matched.pattern.clone(),
            normalized_argv: trace.argv.clone().unwrap_or_default(),
        });
        let id = matched
            .rule
            .strip_prefix(&format!("{}/", matched.profile))
            .unwrap_or(&matched.rule);
        sources.push(ExplainSource::Config {
            file: matched.file.display().to_string(),
            key: format!("rule.{id}.commands"),
            value: matched.pattern.clone().into(),
        });
    }
    let rewritten = match hook_rewrite(config, &ctx, &outcomes, deadline) {
        Ok(text) => Some(text),
        Err(reason) => {
            let _ = write!(summary, "; not rewritten: {reason}");
            None
        }
    };
    (sources, summary, rewritten)
}

/// The rewrite the hook would print for `outcomes`, or why it prints nothing: on Windows, with
/// the hook disabled, with an unusable runner path, past the hook's deadline, or with no
/// stage wrapped.
fn hook_rewrite(
    config: &Config,
    ctx: &Context<'_>,
    outcomes: &[classify::StageOutcome],
    deadline: Instant,
) -> Result<String, String> {
    if cfg!(windows) {
        return Err("the hook does not rewrite on Windows".into());
    }
    if !config.host.hook.enabled {
        return Err("hook.enabled is false".into());
    }
    let runner = config
        .host
        .hook
        .runner_path()
        .map_err(|err| format!("config_invalid: {err}"))?;
    if Instant::now() > deadline {
        return Err("deadline_missed".into());
    }
    match classify::rewrite(
        ctx,
        outcomes,
        &runner,
        &config.host.admission.default_resources,
        &CallIds::default(),
    ) {
        Rewritten::Command(text) => Ok(text),
        Rewritten::Unchanged(_) => Err("no stage is wrapped".into()),
    }
}
