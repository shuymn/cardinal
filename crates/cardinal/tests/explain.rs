//! `explain` (`docs/architecture.md` section 10.2): where a job's outcome, a plan's result,
//! and an inbox item came from, as records and configuration keys.

mod hook_support;
mod observe_common;
mod plan_support;
mod support;

use cardinal::act::ActOptions;
use cardinal::contract::{
    Caller, ErrorCode, ExplainKind, ExplainSource, InboxKind, InboxSubject, PlanKind, RecordKind,
    ResolutionOutcome,
};
use cardinal::explain::{Subject, explain};
use cardinal::record::Recorder;
use observe_common::Scene;
use plan_support::{Setup, arbiter, write};

fn recorder(s: &Setup) -> Recorder {
    Recorder::new(&s.state, 1 << 20, 2)
}

fn append(s: &Setup, kind: RecordKind, job: &str, data: &serde_json::Value) {
    let recorder = recorder(s);
    let mut record = recorder.record(kind);
    record.job_id = Some(job.into());
    record.data.clone_from(data.as_object().unwrap());
    recorder.append(&record).unwrap();
}

fn records_in(doc: &cardinal::contract::Explain) -> Vec<String> {
    doc.sources
        .iter()
        .filter_map(|s| match s {
            ExplainSource::Record { record } => record["kind"].as_str().map(str::to_owned),
            _ => None,
        })
        .collect()
}

#[test]
fn job_outcomes_come_from_records() {
    let s = Setup::new();
    append(
        &s,
        RecordKind::JobStarted,
        "aaaa000000000001",
        &serde_json::json!({}),
    );
    append(
        &s,
        RecordKind::JobStarted,
        "aaaa000000000002",
        &serde_json::json!({}),
    );
    append(
        &s,
        RecordKind::JobFinished,
        "aaaa000000000002",
        &serde_json::json!({"exit": 0, "signal": null, "waited_ms": 12, "admission": "within_budget"}),
    );
    append(
        &s,
        RecordKind::JobFailOpen,
        "aaaa000000000003",
        &serde_json::json!({"reason": "lock_timeout"}),
    );
    append(
        &s,
        RecordKind::JobStarted,
        "aaaa000000000004",
        &serde_json::json!({}),
    );
    let _live = cardinal::state::create_locked(&s.state.live_lock("aaaa000000000004")).unwrap();

    let job = |id: &str| explain(&s.config(), &s.state, &Subject::Job(id.into())).unwrap();
    let died = job("aaaa000000000001");
    assert_eq!(died.subject.kind, ExplainKind::Job);
    assert_eq!(died.outcome.as_deref(), Some("runner_died"));
    assert_eq!(records_in(&died), ["job_started"]);

    let finished = job("aaaa000000000002");
    assert_eq!(finished.outcome.as_deref(), Some("exited:0"));
    assert!(
        finished
            .summary
            .contains("admitted within budget after 12 ms"),
        "{}",
        finished.summary
    );
    assert_eq!(records_in(&finished), ["job_started", "job_finished"]);

    assert_eq!(
        job("aaaa000000000003").outcome.as_deref(),
        Some("fail_open:lock_timeout")
    );
    assert_eq!(job("aaaa000000000004").outcome.as_deref(), Some("running"));

    let ambiguous = explain(&s.config(), &s.state, &Subject::Job("aaaa".into())).unwrap_err();
    assert_eq!(ambiguous.code, ErrorCode::AmbiguousId);
    let missing = explain(&s.config(), &s.state, &Subject::Job("ffff".into())).unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound);
}

/// The latest run is the plan's outcome.
#[test]
fn plan_shows_its_result_and_records() {
    let s = Setup::new();
    let wt = s.worktree("explained");
    write(&wt, "target/x", "x");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
    let before = explain(&s.config(), &s.state, &Subject::Plan(plan.id.clone())).unwrap();
    assert_eq!(before.outcome, None, "not acted on yet");
    s.act(&plan.id, &arbiter()).unwrap();
    let doc = explain(&s.config(), &s.state, &Subject::Plan(plan.id[..5].into())).unwrap();
    assert_eq!(doc.subject.id, plan.id);
    assert_eq!(doc.outcome.as_deref(), Some("dry_run"));
    assert!(
        doc.summary.contains("dry_run by arbiter"),
        "{}",
        doc.summary
    );
    assert_eq!(records_in(&doc), ["plan_created", "act"]);

    write(&wt, "target/proptest-regressions/x.txt", "keep");
    s.act(&plan.id, &arbiter()).unwrap();
    let doc = explain(&s.config(), &s.state, &Subject::Plan(plan.id.clone())).unwrap();
    assert_eq!(doc.outcome.as_deref(), Some("rejected"));
    assert_eq!(records_in(&doc), ["plan_created", "act", "act"]);
}

/// A unit's dry run names the configuration key that turned it into one (C16).
#[test]
fn gated_plan_names_the_unit_key() {
    let s = Setup::new();
    let wt = s.worktree("gated");
    write(&wt, "target/x", "x");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
    let unit = ActOptions {
        caller: Caller::Unit("disk".into()),
        ..arbiter()
    };
    s.act(&plan.id, &unit).unwrap();
    let doc = explain(&s.config(), &s.state, &Subject::Plan(plan.id.clone())).unwrap();
    assert!(
        doc.sources.iter().any(|source| matches!(
            source,
            ExplainSource::Config { key, value, .. }
                if key == "unit.disk.act" && *value == serde_json::Value::Bool(false)
        )),
        "{:#?}",
        doc.sources
    );
    assert_eq!(records_in(&doc), ["plan_created", "planned"]);
}

#[test]
fn inbox_item_shows_evidence_and_resolution() {
    let s = Setup::new();
    let item = cardinal::inbox::NewItem {
        key: "outlived:s1".into(),
        kind: InboxKind::OutlivedProcesses,
        subject: InboxSubject {
            session_id: Some("s1".into()),
            ..InboxSubject::default()
        },
        evidence: serde_json::json!({"processes": [{"pid": 7}]})
            .as_object()
            .unwrap()
            .clone(),
    };
    cardinal::inbox::raise(&s.state, &item).unwrap();
    cardinal::inbox::resolve(
        &s.config(),
        &s.state,
        "outlived:s1",
        ResolutionOutcome::Dismissed,
        Some("the dev server is meant to keep running"),
    )
    .unwrap();
    let doc = explain(&s.config(), &s.state, &Subject::Inbox("outlived:s1".into())).unwrap();
    assert_eq!(doc.subject.kind, ExplainKind::Inbox);
    assert_eq!(
        doc.outcome.as_deref(),
        Some("dismissed by cli: the dev server is meant to keep running")
    );
    assert!(doc.sources.iter().any(|source| matches!(
        source,
        ExplainSource::Observation { result, .. } if result.contains("\"pid\":7")
    )));
    assert_eq!(records_in(&doc), ["inbox_resolved"]);
    let missing = explain(&s.config(), &s.state, &Subject::Inbox("nope".into())).unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound);
}

/// Through the binary: one `cardinal.explain.v1` document.
#[test]
fn cli_prints_an_explain_document() {
    let s = Setup::new();
    let wt = s.worktree("cli");
    write(&wt, "target/x", "x");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
    let assert = s
        .cardinal()
        .args(["explain", "plan", &plan.id])
        .assert()
        .success();
    let doc: serde_json::Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(doc["schema"], "cardinal.explain.v1");
    assert_eq!(
        doc["subject"],
        serde_json::json!({"kind": "plan", "id": plan.id})
    );
    let assert = s
        .cardinal()
        .args(["explain", "job", "abcd"])
        .assert()
        .code(1);
    let doc: serde_json::Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(doc["error"]["code"], "not_found");
}

/// A profile for the scene's `origin` whose rule lists another command first, with `npx` as
/// a wrapper.
const WEB_PROFILE: &str = "[match]\nremotes = [\"github.com/acme/app\"]\n[classify]\nwrappers = [[\"npx\"]]\n[[rule]]\nid = \"test\"\ncommands = [\"cargo test\", \"vitest\"]\nclass = \"test\"\n";

/// A scene with [`WEB_PROFILE`] as `repos/web.toml` and `host.toml` with `observe` appended to
/// `[observe]`, then a `[hook]` table naming a runner path, a deadline long enough for a
/// loaded machine, and `hook`. The runner path is in the fixture on Unix; on Windows, where
/// a temp path is not `/`-absolute, it is a fixed one. These tests never run the runner.
fn command_scene_with(observe: &str, hook: &str) -> Scene {
    let mut scene = Scene::new();
    let runner = if cfg!(windows) {
        hook_support::RUNNER.to_owned()
    } else {
        hook_support::runner_path(&scene.fx.path("bin/cardinal"))
    };
    scene.observe_config(&format!(
        "{observe}[hook]\nrunner = \"{runner}\"\ndeadline_ms = 10000\n{hook}"
    ));
    scene.profile("web", WEB_PROFILE);
    scene
}

/// [`command_scene_with`] without extra `[observe]` keys.
fn command_scene(hook: &str) -> Scene {
    command_scene_with("", hook)
}

/// The sources `explain command` lists for a stage that [`WEB_PROFILE`]'s rule wraps by its
/// `vitest` pattern, with `argv` as the normalized argv.
fn vitest_sources(scene: &Scene, argv: &[&str]) -> Vec<ExplainSource> {
    vec![
        ExplainSource::Rule {
            rule: "web/test".into(),
            pattern: "vitest".into(),
            normalized_argv: argv.iter().map(|a| (*a).to_owned()).collect(),
        },
        ExplainSource::Config {
            file: scene
                .config_dir
                .join("repos")
                .join("web.toml")
                .display()
                .to_string(),
            key: "rule.test.commands".into(),
            value: "vitest".into(),
        },
    ]
}

/// Variables set in the environment of both `explain command` and the hook.
type Vars<'a> = &'a [(&'a str, &'a str)];

/// `cardinal explain command --cwd CWD -- COMMAND` with `vars` set.
fn explain_command(
    scene: &Scene,
    cwd: &std::path::Path,
    command: &str,
    vars: Vars<'_>,
) -> serde_json::Value {
    let mut explain = scene.cardinal();
    explain
        .args(["explain", "command", "--cwd"])
        .arg(cwd)
        .args(["--", command]);
    for (name, value) in vars {
        explain.env(name, value);
    }
    let output = explain.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}

/// What `cardinal hook claude pre-tool-use` prints as the rewrite of a Bash call of `command`
/// in `cwd` that carries no session, agent, or tool-use ID and no timeout, with `vars` set;
/// `None` when it prints nothing.
fn hook_command(
    scene: &Scene,
    cwd: &std::path::Path,
    command: &str,
    vars: Vars<'_>,
) -> Option<String> {
    let input = serde_json::json!({
        "cwd": cwd,
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": command, "run_in_background": false},
    });
    let mut hook = scene.cardinal();
    for (name, value) in vars {
        hook.env(name, value);
    }
    hook_support::run_hook(hook, &input).map(|doc| hook_support::rewritten(&doc).to_owned())
}

/// Why `explain command` reports no rewrite on Windows, where the hook prints nothing.
const NOT_ON_WINDOWS: &str = "not rewritten: the hook does not rewrite on Windows";

/// Asserts that `doc`, the `explain command` document of `command`, agrees with `hook`, the
/// hook's rewrite of the same call: its rewrite on Unix, where `summary` is explain's summary;
/// nothing on Windows, where `rewritten` is null and the summary goes on with why. Returns the
/// hook's rewrite.
fn assert_agrees(
    doc: &serde_json::Value,
    hook: Option<String>,
    summary: &str,
    command: &str,
) -> Option<String> {
    if cfg!(windows) {
        assert_eq!(hook, None, "{command}");
        assert_eq!(doc["rewritten"], serde_json::Value::Null, "{doc:#}");
        assert_eq!(doc["summary"], format!("{summary}; {NOT_ON_WINDOWS}"));
        return None;
    }
    let hook = hook.unwrap_or_else(|| panic!("the hook rewrites {command}"));
    assert!(hook.contains("--spec-b64 "), "{hook}");
    assert_eq!(doc["summary"], summary, "{command}");
    assert_eq!(doc["rewritten"], hook.as_str(), "{command}");
    Some(hook)
}

/// `explain command` traces a command as the hook classifies it, through the profile its
/// `origin` selects: for each wrapped stage the rule, the pattern that matched (not the
/// rule's first one), the normalized argv, and the profile key; why each other stage stays
/// unwrapped; and the rewrite, which is the hook's for the same command, configuration, and
/// environment, with the timeout both resolve from `BASH_DEFAULT_TIMEOUT_MS` and
/// `BASH_MAX_TIMEOUT_MS`. On Windows the hook prints nothing, and so does the rewrite.
#[test]
fn command_trace_matches_the_hook() {
    let scene = command_scene("");
    let wt = scene.worktree("traced", "traced");
    let timeouts: [(Vars<'_>, u64); 2] = [
        (&[], 120_000),
        (
            &[
                ("BASH_DEFAULT_TIMEOUT_MS", "200000"),
                ("BASH_MAX_TIMEOUT_MS", "300000"),
            ],
            200_000,
        ),
    ];
    for (vars, timeout_ms) in timeouts {
        for (cwd, command, sources, summary) in [
            (
                &scene.repo,
                "npx vitest run",
                vitest_sources(&scene, &["vitest", "run"]),
                "1 stages, 1 wrapped",
            ),
            (
                &wt,
                "echo start && npx vitest --run | tee log",
                vitest_sources(&scene, &["vitest", "--run"]),
                "3 stages, 1 wrapped; stage 0 unwrapped: shell_command; stage 2 unwrapped: unclassified",
            ),
        ] {
            let doc = explain_command(&scene, cwd, command, vars);
            assert_eq!(doc["subject"]["kind"], "command", "{doc:#}");
            let found: Vec<ExplainSource> = serde_json::from_value(doc["sources"].clone()).unwrap();
            assert_eq!(found, sources, "{command}");
            let hook = hook_command(&scene, cwd, command, vars);
            if let Some(hook) = assert_agrees(&doc, hook, summary, command) {
                let specs = hook_support::specs(&hook);
                assert_eq!(specs.len(), 1, "{hook}");
                assert_eq!(specs[0].timeout_ms, timeout_ms, "{vars:?}");
            }
        }
    }
}

/// The origin is read under the hook's deadline, not observation's `git_timeout_ms`: with a
/// git slower than that timeout but within the deadline, both select the profile the
/// `origin` matches.
#[cfg(unix)]
#[test]
fn command_trace_reads_the_origin_as_the_hook_does() {
    let tools = cardinal_test_support::Scope::new();
    let git = tools.root().join("git-slow");
    tools.started_script(&git, "sleep 1\nexec git \"$@\"\n");
    let mut scene = command_scene_with(&format!("git = '{}'\n", git.display()), "");
    scene.git_timeout_ms = 100;
    let command = "npx vitest run";
    let doc = explain_command(&scene, &scene.repo, command, &[]);
    let found: Vec<ExplainSource> = serde_json::from_value(doc["sources"].clone()).unwrap();
    assert_eq!(found, vitest_sources(&scene, &["vitest", "run"]), "{doc:#}");
    let hook = hook_command(&scene, &scene.repo, command, &[]);
    assert_agrees(&doc, hook, "1 stages, 1 wrapped", command);
}

/// When the hook prints nothing, `rewritten` is null and the summary says why: no stage is
/// wrapped, the command is outside the shell subset the hook rewrites, or the hook is off.
/// On Windows a command the tokenizer accepts is not rewritten because the hook does not
/// rewrite there, whatever the rest of the configuration says.
#[test]
fn command_not_rewritten_says_why() {
    let scene = command_scene("");
    let echo = if cfg!(windows) {
        NOT_ON_WINDOWS
    } else {
        "not rewritten: no stage is wrapped"
    };
    for (command, summary) in [
        (
            "echo hi",
            format!("1 stages, 0 wrapped; stage 0 unwrapped: shell_command; {echo}"),
        ),
        (
            "npx vitest $(cat list)",
            "not rewritten: `$(` is outside the supported shell subset".to_owned(),
        ),
    ] {
        let doc = explain_command(&scene, &scene.repo, command, &[]);
        assert_eq!(doc["rewritten"], serde_json::Value::Null, "{doc:#}");
        assert_eq!(doc["summary"], summary, "{command}");
        assert_eq!(
            hook_command(&scene, &scene.repo, command, &[]),
            None,
            "{command}"
        );
    }

    let off = command_scene("enabled = false\n");
    let doc = explain_command(&off, &off.repo, "npx vitest", &[]);
    assert_eq!(doc["rewritten"], serde_json::Value::Null, "{doc:#}");
    let reason = if cfg!(windows) {
        NOT_ON_WINDOWS
    } else {
        "not rewritten: hook.enabled is false"
    };
    assert_eq!(doc["summary"], format!("1 stages, 1 wrapped; {reason}"));
    assert_eq!(hook_command(&off, &off.repo, "npx vitest", &[]), None);
}

/// `explain plan` shows what the plan's checks were evaluated from: each fact with its value
/// and source, the profile keys used, and what could not be observed. Every `act` record
/// carries the same for its own evaluation.
#[test]
fn plan_shows_its_provenance() {
    let s = Setup::new();
    let wt = s.worktree("traced");
    s.gh_fails("merged", 1);
    s.gh_stderr("merged", "HTTP 502");
    let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
    let doc = explain(&s.config(), &s.state, &Subject::Plan(plan.id.clone())).unwrap();
    let observed = |command: &str, result: &str| {
        doc.sources.iter().any(|source| {
            matches!(source, ExplainSource::Observation { command: c, exit: None, result: r }
                if c == &[command] && r == result)
        })
    };
    assert!(
        observed("git", "clean: worktree.facts.dirty = \"no\""),
        "{:#?}",
        doc.sources
    );
    assert!(observed(
        "gh",
        "no_open_pr: worktree.facts.open_pr = \"no\""
    ));
    assert!(
        observed("gh", "error: gh exited with 1: HTTP 502"),
        "{:#?}",
        doc.sources
    );
    // The profile path as the config loader lists it: one component per join, so the
    // separators are the platform's.
    let profile = s
        .config_dir
        .join("repos")
        .join("generic.toml")
        .display()
        .to_string();
    assert!(
        doc.sources.iter().any(|source| matches!(
            source,
            ExplainSource::Config { file, key, value }
                if *file == profile && key == "data.protected"
                    && value.as_array().is_some_and(|v| v.contains(&"fuzz/corpus".into()))
        )),
        "{:#?}",
        doc.sources
    );

    let dry_run = ActOptions {
        dry_run: true,
        ..arbiter()
    };
    s.act(&plan.id, &dry_run).unwrap();
    let records = cardinal::record::read_all(&s.state).unwrap();
    let act = records
        .iter()
        .find(|r| r.kind == RecordKind::Act && r.plan_id.as_deref() == Some(plan.id.as_str()))
        .expect("an act record");
    let clean = act.data["checks"]
        .as_array()
        .and_then(|checks| checks.iter().find(|c| c["name"] == "clean"))
        .expect("the act record lists its checks");
    assert_eq!(clean["facts"][0]["name"], "worktree.facts.dirty");
    assert_eq!(clean["facts"][0]["source"], "git");
    assert_eq!(
        act.data["provenance"]["errors"][0]["message"],
        "gh exited with 1: HTTP 502"
    );
    assert_eq!(act.data["provenance"]["config"][0]["file"], profile);
}
