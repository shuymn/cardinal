//! `explain` (`docs/architecture.md` section 10.2): where a job's outcome, a plan's result,
//! and an inbox item came from, as records and configuration keys.

mod fakes;
mod support;

use cardinal::act::ActOptions;
use cardinal::contract::{
    Caller, ErrorCode, ExplainKind, ExplainSource, IgnoredClass, InboxKind, InboxSubject, PlanKind,
    RecordKind, ResolutionOutcome,
};
use cardinal::explain::{ProfileKey, StageTrace, Subject, Tracer, explain, explain_with};
use cardinal::record::Recorder;
use fakes::{Setup, arbiter, write};

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

    let job = |id: &str| explain(&s.config, &s.state, &Subject::Job(id.into())).unwrap();
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

    let ambiguous = explain(&s.config, &s.state, &Subject::Job("aaaa".into())).unwrap_err();
    assert_eq!(ambiguous.code, ErrorCode::AmbiguousId);
    let missing = explain(&s.config, &s.state, &Subject::Job("ffff".into())).unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound);
}

/// The latest run is the plan's outcome.
#[test]
fn plan_shows_its_result_and_records() {
    let s = Setup::new();
    let wt = s.worktree("explained", &[("target", IgnoredClass::Regenerable)]);
    write(&wt, "target/x", "x");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
    let before = explain(&s.config, &s.state, &Subject::Plan(plan.id.clone())).unwrap();
    assert_eq!(before.outcome, None, "not acted on yet");
    s.act(&plan.id, &arbiter()).unwrap();
    let doc = explain(&s.config, &s.state, &Subject::Plan(plan.id[..5].into())).unwrap();
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
    let doc = explain(&s.config, &s.state, &Subject::Plan(plan.id.clone())).unwrap();
    assert_eq!(doc.outcome.as_deref(), Some("rejected"));
    assert_eq!(records_in(&doc), ["plan_created", "act", "act"]);
}

/// A unit's dry run names the configuration key that turned it into one (C16).
#[test]
fn gated_plan_names_the_unit_key() {
    let s = Setup::new();
    let wt = s.worktree("gated", &[("target", IgnoredClass::Regenerable)]);
    write(&wt, "target/x", "x");
    let plan = s.plan(PlanKind::ReclaimDir, &wt.join("target"));
    let unit = ActOptions {
        caller: Caller::Unit("disk".into()),
        ..arbiter()
    };
    s.act(&plan.id, &unit).unwrap();
    let doc = explain(&s.config, &s.state, &Subject::Plan(plan.id.clone())).unwrap();
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
        &s.config,
        &s.state,
        "outlived:s1",
        ResolutionOutcome::Dismissed,
        Some("the dev server is meant to keep running"),
    )
    .unwrap();
    let doc = explain(&s.config, &s.state, &Subject::Inbox("outlived:s1".into())).unwrap();
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
    let missing = explain(&s.config, &s.state, &Subject::Inbox("nope".into())).unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound);
}

/// Through the binary: one `cardinal.explain.v1` document.
#[test]
fn cli_prints_an_explain_document() {
    let s = Setup::new();
    let wt = s.worktree("cli", &[("target", IgnoredClass::Regenerable)]);
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

/// A profile whose rule lists another command first, with `npx` as a wrapper.
const WEB_PROFILE: &str = "[classify]\nwrappers = [[\"npx\"]]\n\n[[rule]]\nid = \"test\"\ncommands = [\"cargo test\", \"vitest\"]\nclass = \"test\"\n";

/// The classification of `npx vitest` under [`WEB_PROFILE`], as the classification module
/// reports it: `npx` unwrapped, `vitest` matched.
struct FakeTracer {
    profile: std::path::PathBuf,
}

impl Tracer for FakeTracer {
    fn stages(
        &self,
        command: &str,
    ) -> Result<Vec<cardinal::classify::Stage>, cardinal::contract::Error> {
        cardinal::classify::find_stages(command)
    }

    fn trace(
        &self,
        request: &cardinal::contract::ClassifyRequest,
        _: &cardinal::config::Selection<'_>,
    ) -> Result<StageTrace, cardinal::contract::Error> {
        assert_eq!(request.argv, ["npx", "vitest"]);
        Ok(StageTrace {
            response: cardinal::contract::ClassifyResponse {
                schema: cardinal::contract::ClassifyResponseSchema,
                decision: cardinal::contract::ClassifyDecision::Wrap,
                class: Some("test".into()),
                rule: Some("web/test".into()),
                resources: None,
                reason: "matched".into(),
            },
            normalized_argv: vec!["vitest".into()],
            pattern: Some("vitest".into()),
            profile: Some(ProfileKey {
                file: self.profile.clone(),
                key: "rule.test.commands".into(),
            }),
        })
    }

    fn rewrite(
        &self,
        command: &str,
        classified: &[(&cardinal::classify::Stage, StageTrace)],
        _: &std::path::Path,
    ) -> Result<String, Vec<String>> {
        assert_eq!(classified.len(), 1);
        Ok(format!("wrapped: {command}"))
    }
}

/// `explain command` reports what the classification matched: the normalized argv, the pattern
/// that matched (not the rule's first one), and the profile key, with the rewrite.
#[test]
fn command_shows_what_matched() {
    let s = Setup::new();
    write(s.dirs.config(), "repos/web.toml", WEB_PROFILE);
    let config = cardinal::config::Config::load(s.dirs.config()).unwrap();
    let profile = s.dirs.config().join("repos/web.toml");
    let tracer = FakeTracer {
        profile: profile.clone(),
    };
    let subject = Subject::Command {
        cwd: Some(s.main.clone()),
        command: "npx vitest".into(),
    };
    let doc = explain_with(&config, &s.state, &subject, &tracer).unwrap();
    assert_eq!(doc.subject.kind, ExplainKind::Command);
    assert!(
        doc.sources.contains(&ExplainSource::Rule {
            rule: "web/test".into(),
            pattern: "vitest".into(),
            normalized_argv: vec!["vitest".into()],
        }),
        "{:#?}",
        doc.sources
    );
    assert!(
        doc.sources.contains(&ExplainSource::Config {
            file: profile.display().to_string(),
            key: "rule.test.commands".into(),
            value: "vitest".into(),
        }),
        "{:#?}",
        doc.sources
    );
    assert_eq!(doc.rewritten.as_deref(), Some("wrapped: npx vitest"));
}

/// `explain plan` shows what the plan's checks were evaluated from: each fact with its value
/// and source, the profile keys used, and what could not be observed. Every `act` record
/// carries the same for its own evaluation.
#[test]
fn plan_shows_its_provenance() {
    let s = Setup::new();
    let wt = s.worktree("traced", &[("node_modules", IgnoredClass::Regenerable)]);
    let mut world = s.fake.read();
    world.worktrees[0].repo.gh_error = Some("HTTP 502".into());
    s.fake.write(&world);
    let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
    let doc = explain(&s.config, &s.state, &Subject::Plan(plan.id.clone())).unwrap();
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
    assert!(observed("gh", "error: HTTP 502"), "{:#?}", doc.sources);
    // The profile path as the config loader lists it: one component per join, so the
    // separators are the platform's.
    let profile = s
        .dirs
        .config()
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
    assert_eq!(act.data["provenance"]["errors"][0]["message"], "HTTP 502");
    assert_eq!(act.data["provenance"]["config"][0]["file"], profile);
}
