//! `hook claude pre-tool-use` (`docs/architecture.md` section 6.3): classify a Bash call
//! and rewrite its classified stages, or print nothing. The hook writes nothing to the state
//! dir on the way to a rewrite and takes no reservation (C1); only a configuration error or a
//! missed deadline leaves a record. The deadline is checked once more after the output is
//! serialized, right before it is written.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use super::repo::{Origin, Repo};
use super::string;
use crate::classify::{self, BuiltIn, CallIds, Context, Refusal, Rewritten};
use crate::config::Config;
use crate::contract::{ClassifyRepo, ClassifyTool, ToolName};
use crate::testpoint;

/// The Bash timeout default and ceiling when the environment does not set them
/// (`BASH_DEFAULT_TIMEOUT_MS`, `BASH_MAX_TIMEOUT_MS`).
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;

/// Why the hook printed nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Pass {
    NotBash,
    NoCommand,
    AutoMode,
    /// `config_invalid`, or a runner path the rewrite cannot use; recorded as `config_error`.
    Config(String),
    Refused(Refusal),
    NothingWrapped,
    /// Recorded as `hook_error`.
    DeadlineMissed {
        elapsed_ms: u64,
    },
    /// Recorded as `hook_error`.
    Failed(String),
}

/// The fields of the hook input the hook reads; `tool_input` is kept whole.
pub(super) struct Call {
    pub tool_input: Map<String, Value>,
    pub command: String,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub tool_use_id: Option<String>,
    pub cwd: Option<String>,
}

/// Checks the input (step 1 of section 6.3) without configuration.
pub(super) fn parse(doc: &Map<String, Value>) -> Result<Call, Pass> {
    if doc.get("tool_name").and_then(Value::as_str) != Some("Bash") {
        return Err(Pass::NotBash);
    }
    if doc.get("permission_mode").and_then(Value::as_str) == Some("auto") {
        return Err(Pass::AutoMode);
    }
    let tool_input = doc
        .get("tool_input")
        .and_then(Value::as_object)
        .ok_or(Pass::NoCommand)?
        .clone();
    let command = string(&tool_input, "command").ok_or(Pass::NoCommand)?;
    Ok(Call {
        tool_input,
        command,
        session_id: string(doc, "session_id"),
        agent_id: string(doc, "agent_id"),
        tool_use_id: string(doc, "tool_use_id"),
        cwd: string(doc, "cwd"),
    })
}

fn positive(value: Option<String>) -> Option<u64> {
    value?.trim().parse::<u64>().ok().filter(|n| *n > 0)
}

/// The timeout written into each spec (section 6.3, step 4): `tool_input.timeout` if it is a
/// positive number, else `BASH_DEFAULT_TIMEOUT_MS`, else 120000; capped at the larger of
/// `BASH_MAX_TIMEOUT_MS` (else 600000) and the default.
pub(super) fn timeout_ms(
    tool_input: &Map<String, Value>,
    var: &dyn Fn(&str) -> Option<String>,
) -> u64 {
    let default = positive(var("BASH_DEFAULT_TIMEOUT_MS")).unwrap_or(DEFAULT_TIMEOUT_MS);
    let cap = positive(var("BASH_MAX_TIMEOUT_MS"))
        .unwrap_or(MAX_TIMEOUT_MS)
        .max(default);
    let requested = tool_input.get("timeout").and_then(|value| {
        value.as_u64().filter(|n| *n > 0).or_else(|| {
            let ms = value.as_f64().filter(|ms| ms.is_finite() && *ms > 0.0)?;
            // Clamped to the cap first, so the conversion cannot overflow.
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss,
                reason = "the value is positive, finite, and at most the cap"
            )]
            Some(ms.ceil().min(cap as f64) as u64)
        })
    });
    requested.unwrap_or(default).min(cap)
}

/// What the hook needs besides its input: the loaded configuration, the runner path, the
/// environment, and the deadline.
pub(super) struct Settings<'a> {
    pub config: &'a Config,
    pub runner: &'a str,
    pub var: &'a dyn Fn(&str) -> Option<String>,
    pub deadline: Instant,
    pub start: Instant,
    /// The process cwd, used when the input has none.
    pub fallback_cwd: Option<PathBuf>,
}

/// Classifies and rewrites (steps 2 and 3 of section 6.3); returns the hook output document.
pub(super) fn evaluate(input: &Call, settings: &Settings<'_>) -> Result<Value, Pass> {
    let config = settings.config;
    let stages = classify::tokenize(&input.command).map_err(Pass::Refused)?;

    let cwd = input
        .cwd
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| settings.fallback_cwd.clone());
    let repo = cwd.as_deref().and_then(Repo::locate);
    let path_matched = |profile: &crate::config::Profile| {
        repo.as_ref()
            .is_some_and(|repo| profile.matches_common_dir(&repo.common_dir))
    };
    let remote_needed = repo.is_some()
        && config.profiles.iter().any(|profile| {
            profile
                .matcher
                .as_ref()
                .is_some_and(|m| !m.remotes.is_empty())
                && !path_matched(profile)
        })
        && classify::rules_could_match(&stages, &config.profiles);
    let remote = match (&repo, remote_needed) {
        (Some(repo), true) => {
            match super::repo::origin(&config.host.observe.git, &repo.root, settings.deadline) {
                Origin::Url(url) => Some(url),
                Origin::Unknown => None,
                Origin::TimedOut => return Err(missed(settings.start)),
            }
        }
        _ => None,
    };
    let selection = config.select(|profile| {
        path_matched(profile)
            || remote
                .as_deref()
                .is_some_and(|url| profile.matches_remote(url))
    });

    let timeout_ms = timeout_ms(&input.tool_input, settings.var);
    let background = input.tool_input.get("run_in_background") == Some(&Value::Bool(true));
    let tool = ClassifyTool {
        name: ToolName::Bash,
        timeout_ms,
        background,
    };
    let classify_repo = repo.as_ref().map(|repo| ClassifyRepo {
        common_dir: path_text(&repo.common_dir),
        main_worktree: path_text(&repo.main_worktree()),
        remote: remote.as_deref().and_then(crate::config::normalize_remote),
    });
    let cwd_text = cwd.as_deref().map_or_else(String::new, path_text);
    let ctx = Context {
        command: &input.command,
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
        settings.deadline,
    );

    if Instant::now() > settings.deadline {
        return Err(missed(settings.start));
    }
    testpoint::hit(testpoint::HOOK_CLASSIFIED).map_err(|err| Pass::Failed(err.to_string()))?;

    let ids = CallIds {
        session_id: input.session_id.as_deref(),
        agent_id: input.agent_id.as_deref(),
        tool_use_id: input.tool_use_id.as_deref(),
    };
    let rewritten = match classify::rewrite(
        &ctx,
        &outcomes,
        settings.runner,
        &config.host.admission.default_resources,
        &ids,
    ) {
        Rewritten::Command(command) => command,
        Rewritten::Unchanged(_) => return Err(Pass::NothingWrapped),
    };
    let mut updated = input.tool_input.clone();
    updated.insert("command".into(), Value::String(rewritten));
    Ok(serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "updatedInput": updated,
        }
    }))
}

pub(super) fn missed(start: Instant) -> Pass {
    Pass::DeadlineMissed {
        elapsed_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The deadline of section 6.3, step 2: `hook.deadline_ms` from `start`.
pub(super) fn deadline(config: &Config, start: Instant) -> Instant {
    start + Duration::from_millis(config.host.hook.deadline_ms)
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value, json};

    use super::{Pass, parse, timeout_ms};

    fn object(value: &Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn input_checks() {
        let ok = json!({"tool_name": "Bash", "tool_input": {"command": "make"}, "permission_mode": "default"});
        assert!(parse(&object(&ok)).is_ok());
        for (doc, pass) in [
            (
                json!({"tool_name": "Read", "tool_input": {"command": "make"}}),
                Pass::NotBash,
            ),
            (json!({"tool_input": {"command": "make"}}), Pass::NotBash),
            (
                json!({"tool_name": "Bash", "tool_input": {"command": 1}}),
                Pass::NoCommand,
            ),
            (
                json!({"tool_name": "Bash", "tool_input": "make"}),
                Pass::NoCommand,
            ),
            (json!({"tool_name": "Bash"}), Pass::NoCommand),
            (
                json!({"tool_name": "Bash", "tool_input": {"command": "make"}, "permission_mode": "auto"}),
                Pass::AutoMode,
            ),
        ] {
            assert_eq!(parse(&object(&doc)).err(), Some(pass), "{doc}");
        }
    }

    #[test]
    fn timeouts() {
        type Var<'a> = &'a dyn Fn(&str) -> Option<String>;
        let none = |_: &str| None;
        let env = |name: &str| match name {
            "BASH_DEFAULT_TIMEOUT_MS" => Some("200000".to_owned()),
            "BASH_MAX_TIMEOUT_MS" => Some("300000".to_owned()),
            _ => None,
        };
        let small_max = |name: &str| match name {
            "BASH_DEFAULT_TIMEOUT_MS" => Some("200000".to_owned()),
            "BASH_MAX_TIMEOUT_MS" => Some("100000".to_owned()),
            _ => None,
        };
        let garbage = |_: &str| Some("x".to_owned());
        let cases: &[(Value, Var<'_>, u64)] = &[
            (json!({}), &none, 120_000),
            (json!({"timeout": 5000}), &none, 5000),
            (json!({"timeout": 1.5}), &none, 2),
            (json!({"timeout": 0}), &none, 120_000),
            (json!({"timeout": -5}), &none, 120_000),
            (json!({"timeout": "5000"}), &none, 120_000),
            (json!({"timeout": 9_000_000}), &none, 600_000),
            (json!({"timeout": 1e300}), &none, 600_000),
            (json!({}), &env, 200_000),
            (json!({"timeout": 400_000}), &env, 300_000),
            (json!({"timeout": 400_000}), &small_max, 200_000),
            (json!({}), &garbage, 120_000),
        ];
        for (input, var, expected) in cases {
            assert_eq!(timeout_ms(&object(input), *var), *expected, "{input}");
        }
    }
}
