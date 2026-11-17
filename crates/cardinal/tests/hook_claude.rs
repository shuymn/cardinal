//! `cardinal hook claude pre-tool-use` (`docs/architecture.md` section 6.3): the rewrite and
//! its spec, the fields it keeps, the deadline, the cases that print nothing, and profile
//! matching through the repository. Claims C1, C5, C6, and A5 (section 16).
//!
//! Rewriting, and the records the hook appends on the way, are Unix-only in phase 0: on
//! Windows the hook stops before it reads its input (section 6.3), which
//! `windows_not_rewritten` checks.

mod hook_support;
mod support;

use std::path::Path;
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use cardinal::contract::{Record, RecordKind};
#[cfg(unix)]
use cardinal::state::StateDir;
use hook_support::{PROFILE, RUNNER, configure, host, input, run_hook};
#[cfg(unix)]
use hook_support::{rewritten, specs};
use serde_json::Value;
#[cfg(unix)]
use serde_json::json;
use support::cardinal::Dirs;

#[cfg(unix)]
fn records(dirs: &Dirs) -> Vec<Record> {
    let state = StateDir::open(dirs.state()).expect("open the state dir");
    cardinal::record::read_all(&state).expect("read records")
}

fn configured(extra_host: &str) -> Dirs {
    let dirs = Dirs::new();
    configure(&dirs, &host(RUNNER, extra_host), &[("generic", PROFILE)]);
    dirs
}

/// C1: a classified stage is rewritten to run through `cardinal run`, and the hook leaves
/// the state dir untouched: no reservation, nothing at all.
#[cfg(unix)]
#[test]
fn rewrite_takes_no_reservation() {
    let dirs = configured("");
    let doc = run_hook(
        dirs.cmd(),
        &input(
            "cd sub && pnpm test 2>&1 | tail -5 || echo failed",
            Path::new("/w"),
        ),
    )
    .expect("a rewrite");
    let spec = &specs(rewritten(&doc))[0];
    let expected = format!(
        "cd sub && if [ -f {RUNNER} ] && [ -x {RUNNER} ]; then {RUNNER} run --spec-b64 {} -- pnpm test 2>&1; else pnpm test 2>&1; fi | tail -5 || echo failed",
        spec.encode_b64()
    );
    assert_eq!(rewritten(&doc), expected);
    assert!(
        !dirs.state().exists(),
        "the hook created {}",
        dirs.state().display()
    );
}

/// The spec carries the class, rule, resources, timeout, background flag, stage, and the
/// call's IDs; resources come from the rule or `default_resources`.
#[cfg(unix)]
#[test]
fn spec_carries_the_call() {
    let dirs = configured("[admission]\ndefault_resources = { cpu = 2, memory_mb = 100 }\n");
    let mut call = input("make all; vitest run --reporter dot", Path::new("/w"));
    call["agent_id"] = json!("agent-7");
    call["tool_input"]["timeout"] = json!(5000);
    call["tool_input"]["run_in_background"] = json!(true);
    let doc = run_hook(dirs.cmd(), &call).expect("a rewrite");
    let found = specs(rewritten(&doc));
    assert_eq!(found.len(), 2);
    let (make, vitest) = (&found[0], &found[1]);
    assert_eq!(make.rule.as_deref(), Some("generic/build"));
    assert_eq!(make.class.as_deref(), Some("build"));
    assert_eq!(
        serde_json::to_value(&make.resources).unwrap(),
        json!({"cpu": 2, "memory_mb": 100})
    );
    assert_eq!((make.stage_index, make.stage.as_str()), (0, "make all"));
    assert_eq!(vitest.rule.as_deref(), Some("generic/test"));
    assert_eq!(
        serde_json::to_value(&vitest.resources).unwrap(),
        json!({"cpu": 3})
    );
    assert_eq!(
        (vitest.stage_index, vitest.stage.as_str()),
        (1, "vitest run --reporter dot")
    );
    for spec in &found {
        assert_eq!(spec.timeout_ms, 5000);
        assert!(spec.background);
        assert_eq!(spec.session_id.as_deref(), Some("session-1"));
        assert_eq!(spec.agent_id.as_deref(), Some("agent-7"));
        assert_eq!(spec.tool_use_id.as_deref(), Some("toolu_01"));
    }
}

/// The timeout falls back to `BASH_DEFAULT_TIMEOUT_MS` and is capped by
/// `BASH_MAX_TIMEOUT_MS`, both from the hook's environment.
#[cfg(unix)]
#[test]
fn timeout_comes_from_the_environment() {
    let dirs = configured("");
    let mut call = input("make", Path::new("/w"));
    call["tool_input"]
        .as_object_mut()
        .unwrap()
        .remove("timeout");
    let mut command = dirs.cmd();
    command.env("BASH_DEFAULT_TIMEOUT_MS", "200000");
    let doc = run_hook(command, &call).unwrap();
    assert_eq!(specs(rewritten(&doc))[0].timeout_ms, 200_000);

    call["tool_input"]["timeout"] = json!(900_000);
    let mut command = dirs.cmd();
    command.env("BASH_MAX_TIMEOUT_MS", "300000");
    let doc = run_hook(command, &call).unwrap();
    assert_eq!(specs(rewritten(&doc))[0].timeout_ms, 300_000);
}

/// C6: `updatedInput` is the original `tool_input` with only `command` replaced, unknown
/// fields included; no permission decision is returned.
#[cfg(unix)]
#[test]
fn other_fields_preserved() {
    let dirs = configured("");
    let mut call = input("make", Path::new("/w"));
    call["tool_input"]["description"] = json!("Build ü \"quoted\"");
    call["tool_input"]["timeout"] = json!(1.5);
    call["tool_input"]["future_field"] = json!({"nested": [1, null, "x"], "flag": true});
    let doc = run_hook(dirs.cmd(), &call).expect("a rewrite");
    let output = doc["hookSpecificOutput"].as_object().unwrap();
    assert_eq!(
        output.keys().collect::<Vec<_>>(),
        ["hookEventName", "updatedInput"]
    );
    assert_eq!(output["hookEventName"], "PreToolUse");
    let mut updated = output["updatedInput"].clone();
    assert_ne!(updated["command"], call["tool_input"]["command"]);
    updated["command"] = call["tool_input"]["command"].clone();
    assert_eq!(updated, call["tool_input"]);
    assert_eq!(doc.as_object().unwrap().len(), 1);
}

/// C5: a hook whose deadline passes after classification, while it rewrites and serializes
/// the command, writes nothing and records why; the same input within the deadline is
/// rewritten.
#[cfg(unix)]
#[test]
fn deadline_no_rewrite() {
    use std::io::{Read, Write};
    use std::process::Stdio;

    use support::testpoints::TestPoints;

    let dirs = Dirs::new();
    configure(
        &dirs,
        &format!("[hook]\nrunner = \"{RUNNER}\"\ndeadline_ms = 300\n"),
        &[("generic", PROFILE)],
    );
    let call = serde_json::to_vec(&input("make", Path::new("/w"))).unwrap();

    let points = TestPoints::new(dirs.scope());
    let mut command = dirs.cmd();
    command
        .args(["hook", "claude", "pre-tool-use"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    points.apply(&mut command, &[cardinal::testpoint::HOOK_CLASSIFIED], None);
    let mut child = command.spawn().expect("start the hook");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(&call).unwrap();
    drop(stdin);
    points.wait_reached(
        cardinal::testpoint::HOOK_CLASSIFIED,
        child.id(),
        Duration::from_secs(10),
    );
    std::thread::sleep(Duration::from_millis(400));
    points.release(cardinal::testpoint::HOOK_CLASSIFIED);
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&stdout), "", "past the deadline");
    let errors: Vec<Record> = records(&dirs)
        .into_iter()
        .filter(|r| r.kind == RecordKind::HookError)
        .collect();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].data["reason"], "deadline_missed");
    assert_eq!(errors[0].session_id.as_deref(), Some("session-1"));

    let control = Dirs::new();
    configure(
        &control,
        &format!("[hook]\nrunner = \"{RUNNER}\"\ndeadline_ms = 10000\n"),
        &[("generic", PROFILE)],
    );
    assert!(run_hook(control.cmd(), &input("make", Path::new("/w"))).is_some());
}

/// A5: `auto` mode is not rewritten.
#[cfg(unix)]
#[test]
fn auto_mode_not_rewritten() {
    let dirs = configured("");
    let mut call = input("make", Path::new("/w"));
    assert!(run_hook(dirs.cmd(), &call).is_some(), "control");
    call["permission_mode"] = json!("auto");
    assert_eq!(run_hook(dirs.cmd(), &call), None);
    for mode in ["default", "acceptEdits", "dontAsk", "bypassPermissions"] {
        call["permission_mode"] = json!(mode);
        assert!(run_hook(dirs.cmd(), &call).is_some(), "{mode}");
    }
}

/// A5: on Windows the hook prints nothing, even for a classified command.
#[cfg(windows)]
#[test]
fn windows_not_rewritten() {
    let dirs = configured("");
    assert_eq!(
        run_hook(dirs.cmd(), &input("make", Path::new("C:\\w"))),
        None
    );
}

/// A5: the hook exits 0 with empty stdout on malformed input, failures, and every other
/// reason not to rewrite.
mod never_blocks {
    use std::path::Path;

    #[cfg(unix)]
    use cardinal::contract::RecordKind;
    use serde_json::json;

    use super::configured;
    #[cfg(unix)]
    use super::hook_support::host;
    use super::hook_support::{PROFILE, RUNNER, configure, input, output_with_stdin, run_hook};
    #[cfg(unix)]
    use super::records;
    use super::support::cardinal::Dirs;

    fn raw(dirs: &Dirs, stdin: &[u8]) {
        let output = output_with_stdin(dirs.cmd().args(["hook", "claude", "pre-tool-use"]), stdin);
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(String::from_utf8_lossy(&output.stdout), "");
        assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    }

    #[test]
    fn malformed_input() {
        let dirs = configured("");
        let mut oversized = serde_json::to_vec(&input("make", Path::new("/w"))).unwrap();
        oversized.extend(std::iter::repeat_n(b' ', 1024 * 1024));
        for stdin in [
            b"".to_vec(),
            b"not json".to_vec(),
            b"[1]".to_vec(),
            b"{\"tool_name\":\"Bash\"".to_vec(),
            b"\xff\xfe".to_vec(),
            oversized,
        ] {
            raw(&dirs, &stdin);
        }
        for call in [
            json!({"tool_name": "Bash"}),
            json!({"tool_name": "Bash", "tool_input": {"command": 7}}),
            json!({"tool_name": "Bash", "tool_input": "make"}),
            json!({"tool_name": "Read", "tool_input": {"command": "make"}}),
        ] {
            assert_eq!(run_hook(dirs.cmd(), &call), None, "{call}");
        }
        // Each input the hook cannot use leaves a record; another tool's call does not.
        #[cfg(unix)]
        {
            let found = records(&dirs);
            assert_eq!(found.len(), 9, "{found:?}");
            for record in &found {
                assert_eq!(record.kind, RecordKind::HookError);
                assert_eq!(record.data["reason"], "invalid_input");
            }
        }
    }

    #[test]
    fn refused_or_unclassified_commands() {
        let dirs = configured("");
        for command in [
            "echo $(date) && make",
            "if true; then make; fi",
            "cat <<EOF\nmake\nEOF",
            "ls | tail",
            "CI=1 make",
            "make &",
            "sleep 1",
            "make | vitest",
            "make 'unbalanced",
            "",
        ] {
            assert_eq!(
                run_hook(dirs.cmd(), &input(command, Path::new("/w"))),
                None,
                "{command:?}"
            );
        }
        assert!(!dirs.state().exists(), "nothing was recorded");
    }

    #[test]
    fn disabled_hook() {
        let dirs = Dirs::new();
        configure(
            &dirs,
            &format!("[hook]\nenabled = false\nrunner = \"{RUNNER}\"\n"),
            &[("generic", PROFILE)],
        );
        assert_eq!(run_hook(dirs.cmd(), &input("make", Path::new("/w"))), None);
    }

    #[cfg(unix)]
    #[test]
    fn invalid_config_is_recorded() {
        let dirs = Dirs::new();
        configure(&dirs, "[hook]\ntypo = 1\n", &[("generic", PROFILE)]);
        assert_eq!(run_hook(dirs.cmd(), &input("make", Path::new("/w"))), None);
        let found = records(&dirs);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, RecordKind::ConfigError);
        assert!(
            found[0].data["detail"]
                .as_str()
                .unwrap()
                .contains("host.toml")
        );
    }

    #[cfg(unix)]
    #[test]
    fn invalid_runner_path_is_recorded() {
        for runner in [
            "relative/cardinal",
            "/opt/with space/cardinal",
            "/opt/$HOME/cardinal",
        ] {
            let dirs = Dirs::new();
            configure(&dirs, &host(runner, ""), &[("generic", PROFILE)]);
            assert_eq!(
                run_hook(dirs.cmd(), &input("make", Path::new("/w"))),
                None,
                "{runner}"
            );
            let found = records(&dirs);
            assert_eq!(found.len(), 1, "{runner}");
            assert_eq!(found[0].kind, RecordKind::ConfigError);
        }
    }

    /// A directory passes the shell's `-x` test, so the hook refuses it as the runner.
    #[cfg(unix)]
    #[test]
    fn directory_runner_is_recorded() {
        let dirs = Dirs::new();
        let runner = dirs.scope().path("cardinal");
        std::fs::create_dir(&runner).unwrap();
        configure(
            &dirs,
            &host(runner.to_str().unwrap(), ""),
            &[("generic", PROFILE)],
        );
        assert_eq!(run_hook(dirs.cmd(), &input("make", Path::new("/w"))), None);
        let found = records(&dirs);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, RecordKind::ConfigError);
        assert!(
            found[0].data["detail"]
                .as_str()
                .unwrap()
                .contains("is a directory")
        );
    }

    #[test]
    fn unusable_state_dir() {
        let dirs = Dirs::new();
        configure(&dirs, "[hook]\ntypo = 1\n", &[("generic", PROFILE)]);
        let file = dirs.config().join("not-a-dir");
        std::fs::write(&file, b"").unwrap();
        let mut command = dirs.cmd();
        command.env("CARDINAL_STATE_DIR", &file);
        assert_eq!(run_hook(command, &input("make", Path::new("/w"))), None);
        let mut command = dirs.cmd();
        command.env("CARDINAL_STATE_DIR", "relative");
        assert_eq!(run_hook(command, &input("make", Path::new("/w"))), None);
    }

    #[test]
    fn missing_config_and_state_locations() {
        let dirs = configured("");
        let mut command = dirs.cmd();
        command
            .env_remove("CARDINAL_CONFIG_DIR")
            .env_remove("CARDINAL_STATE_DIR")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("HOME")
            .env_remove("LOCALAPPDATA");
        assert_eq!(run_hook(command, &input("make", Path::new("/w"))), None);
    }

    #[cfg(unix)]
    #[test]
    fn injected_failure() {
        let dirs = configured("");
        let points = super::support::testpoints::TestPoints::new(dirs.scope());
        let mut command = dirs.cmd();
        command
            .env("CARDINAL_TEST_DIR", points.dir())
            .env("CARDINAL_TEST_FAIL", cardinal::testpoint::HOOK_CLASSIFIED);
        assert_eq!(run_hook(command, &input("make", Path::new("/w"))), None);
        let found = records(&dirs);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, RecordKind::HookError);
        assert_eq!(found[0].data["reason"], "failed");
    }
}

fn shipped_settings() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/claude/settings.json");
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// The shipped settings name hook commands the CLI accepts, with explicit timeouts.
#[test]
fn shipped_settings_name_the_hook_commands() {
    let settings = shipped_settings();
    let mut events = Vec::new();
    for (event, entries) in settings["hooks"].as_object().unwrap() {
        for entry in entries.as_array().unwrap() {
            for hook in entry["hooks"].as_array().unwrap() {
                let command = hook["command"].as_str().unwrap();
                let args: Vec<&str> = command.split(' ').collect();
                assert_eq!(args[..3], ["cardinal", "hook", "claude"], "{command}");
                <cardinal::cli::Cli as clap::Parser>::try_parse_from(&args)
                    .unwrap_or_else(|err| panic!("{command}: {err}"));
                assert!(hook["timeout"].as_u64().unwrap() <= 5, "{command}");
                events.push((event.clone(), args[3].to_owned()));
            }
        }
    }
    events.sort();
    assert_eq!(
        events,
        [
            ("PreToolUse".to_owned(), "pre-tool-use".to_owned()),
            ("SessionEnd".to_owned(), "session-end".to_owned()),
            ("SubagentStop".to_owned(), "subagent-stop".to_owned()),
        ]
    );
    assert_eq!(
        settings["hooks"]["PreToolUse"][0]["matcher"], "Bash",
        "only Bash calls reach the hook"
    );
}

/// Whether the Bash permission rule `rule` (`Bash`, `Bash(prefix:*)`, or `Bash(pattern)` with
/// `*` matching any text) matches the command `command`.
fn bash_rule_matches(rule: &str, command: &str) -> bool {
    fn glob(pattern: &str, text: &str) -> bool {
        match pattern.split_once('*') {
            None => pattern == text,
            Some((head, rest)) => {
                text.starts_with(head)
                    && (head.len()..=text.len())
                        .any(|at| text.is_char_boundary(at) && glob(rest, &text[at..]))
            }
        }
    }
    let Some(body) = rule.strip_prefix("Bash") else {
        return false;
    };
    let Some(pattern) = body.strip_prefix('(').and_then(|b| b.strip_suffix(')')) else {
        return body.is_empty();
    };
    match pattern.strip_suffix(":*") {
        Some(prefix) => command.starts_with(prefix),
        None => glob(pattern, command),
    }
}

/// When an agent supplies `R run -- probe` with `Bash(probe)` denied, cardinal's shipped
/// settings shall not authorize execution without explicit approval: they hold no allow or
/// ask entry whose pattern can match a runner invocation, and no mode that skips approval.
#[test]
fn shipped_settings_grant_no_runner_execution() {
    let settings = shipped_settings();
    let permissions = &settings["permissions"];
    let invocations = [
        "cardinal run -- probe",
        "/usr/local/bin/cardinal run -- probe",
        "/opt/cardinal/bin/cardinal run --spec-b64 eyJ9 -- probe",
    ];
    for list in ["allow", "ask"] {
        for entry in permissions[list].as_array().into_iter().flatten() {
            let rule = entry.as_str().expect("a permission rule is a string");
            for invocation in invocations {
                assert!(
                    !bash_rule_matches(rule, invocation),
                    "permissions.{list} entry {rule} matches {invocation}"
                );
            }
        }
    }
    assert_ne!(permissions["defaultMode"], "bypassPermissions");

    // The matcher finds the rules the settings must not hold.
    for rule in [
        "Bash",
        "Bash(*)",
        "Bash(cardinal run:*)",
        "Bash(/usr/local/bin/cardinal run:*)",
        "Bash(cardinal run *)",
        "Bash(* run -- probe)",
    ] {
        assert!(
            invocations.iter().any(|i| bash_rule_matches(rule, i)),
            "{rule}"
        );
    }
    assert!(!bash_rule_matches("Bash(probe)", "cardinal run -- probe"));
    assert!(!bash_rule_matches("Read(*)", "cardinal run -- probe"));
}

/// Profile matching by repository: path matching without git, remote matching through git
/// with a constructed environment and the hook's deadline, and fallbacks when git fails.
#[cfg(unix)]
mod repository {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use cardinal_test_support::Scope;
    use cardinal_test_support::fixture::Fixture;
    use cardinal_test_support::process::{PidGuard, wait_for};

    use super::hook_support::{
        PROFILE, RUNNER, cardinal, configure, executable, input, rewritten, run_hook, specs,
    };
    use super::support::cardinal::Dirs;

    const APP: &str = "[match]\nremotes = [\"github.com/o/app\"]\n[[rule]]\nid = \"build\"\ncommands = [\"make\"]\nclass = \"app-build\"\n";

    /// The deadline of the tests whose git outlives it: long enough for a freshly written
    /// script to start on a loaded host, which some OSes check before its first run.
    const DEADLINE_MS: u64 = 3000;

    fn host(git: &str, deadline_ms: u64) -> String {
        format!(
            "[hook]\nrunner = \"{RUNNER}\"\ndeadline_ms = {deadline_ms}\n[observe]\ngit = \"{git}\"\n"
        )
    }

    /// The rule of the stage `make`, or `None` when nothing was rewritten.
    fn rule(doc: Option<serde_json::Value>) -> Option<String> {
        specs(rewritten(&doc?))[0].rule.clone()
    }

    /// A directory with an empty `.git` directory: a repository as far as the hook's
    /// common-dir lookup goes, without running git.
    fn bare_dot_git(dir: &Path) -> PathBuf {
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        dunce::canonicalize(repo).unwrap()
    }

    #[test]
    fn profile_matched_by_remote_ignores_inherited_git_vars() {
        let fixture = Fixture::new();
        let app = fixture.init("app");
        fixture.remote_add_url(&app, "origin", "https://github.com/o/app.git");
        let decoy = fixture.init("decoy");
        fixture.remote_add_url(&decoy, "origin", "https://github.com/o/decoy.git");
        std::fs::create_dir_all(app.join("sub")).unwrap();

        let dirs = Dirs::new();
        let decoy_profile = APP
            .replace("o/app", "o/decoy")
            .replace("app-build", "decoy-build");
        configure(
            &dirs,
            &host("git", 10_000),
            &[
                ("app", APP),
                ("decoy", &decoy_profile),
                ("generic", PROFILE),
            ],
        );
        let run = |extra: &[(&str, &Path)]| {
            let mut command = cardinal(fixture.scope(), &dirs, &app);
            for (name, value) in extra {
                command.env(name, value);
            }
            rule(run_hook(command, &input("make", &app.join("sub"))))
        };
        assert_eq!(run(&[]).as_deref(), Some("app/build"));
        let decoy_git = decoy.join(".git");
        let decoy_index = decoy_git.join("index");
        assert_eq!(
            run(&[
                ("GIT_DIR", &decoy_git),
                ("GIT_WORK_TREE", &decoy),
                ("GIT_INDEX_FILE", &decoy_index),
            ])
            .as_deref(),
            Some("app/build"),
            "the hook reads the cwd's repository, not GIT_DIR"
        );
        // Outside the matched repository only the generic profile applies.
        assert_eq!(
            rule(run_hook(dirs.cmd(), &input("make", fixture.root()))).as_deref(),
            Some("generic/build")
        );
    }

    #[test]
    fn profile_matched_by_path_needs_no_git() {
        let dir = Scope::new();
        let repo = bare_dot_git(dir.root());
        let dirs = Dirs::new();
        let app = format!(
            "[match]\npaths = [\"{}\"]\n[[rule]]\nid = \"build\"\ncommands = [\"make\"]\nclass = \"app-build\"\n",
            repo.display()
        );
        configure(
            &dirs,
            &host("/nonexistent/git", 10_000),
            &[("app", &app), ("generic", PROFILE)],
        );
        assert_eq!(
            rule(run_hook(dirs.cmd(), &input("make", &repo.join("sub")))).as_deref(),
            Some("app/build")
        );
    }

    /// git runs only when a rule could match a stage, and its failures fall back to the
    /// profiles that match without it.
    #[test]
    fn git_runs_only_when_needed_and_failures_fall_back() {
        let dir = Scope::new();
        let repo = bare_dot_git(dir.root());
        let calls = dir.root().join("calls");
        let cases = [
            (
                "ok",
                "printf 'git@github.com:o/app.git\\n'",
                Some("app/build"),
            ),
            ("fails", "exit 1", Some("generic/build")),
            ("garbage", "printf 'not a remote\\n'", Some("generic/build")),
            ("two-lines", "printf 'a\\nb\\n'", Some("generic/build")),
        ];
        for (name, body, expected) in cases {
            let git = dir.root().join(format!("git-{name}"));
            executable(
                &git,
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n{body}\n",
                    calls.display()
                ),
            );
            let dirs = Dirs::new();
            configure(
                &dirs,
                &host(git.to_str().unwrap(), 10_000),
                &[("app", APP), ("generic", PROFILE)],
            );
            let _ = std::fs::remove_file(&calls);
            assert_eq!(
                run_hook(dirs.cmd(), &input("ls | tail", &repo)),
                None,
                "{name}"
            );
            assert!(
                !calls.exists(),
                "{name}: git ran though no rule could match"
            );
            assert_eq!(
                rule(run_hook(dirs.cmd(), &input("make", &repo))).as_deref(),
                expected,
                "{name}"
            );
            let logged = std::fs::read_to_string(&calls).unwrap();
            assert_eq!(
                logged.trim(),
                format!(
                    "--no-optional-locks -c core.fsmonitor=false -C {} remote get-url origin",
                    repo.display()
                ),
                "{name}"
            );
        }
        let dirs = Dirs::new();
        configure(
            &dirs,
            &host("/nonexistent/git", 10_000),
            &[("app", APP), ("generic", PROFILE)],
        );
        assert_eq!(
            rule(run_hook(dirs.cmd(), &input("make", &repo))).as_deref(),
            Some("generic/build"),
            "git that cannot start"
        );
    }

    fn pid_from(file: &Path) -> u32 {
        wait_for("the pid file", Duration::from_secs(10), || {
            std::fs::read_to_string(file).ok()?.trim().parse().ok()
        })
    }

    /// A git that outlives the deadline is killed and reaped, and the hook rewrites nothing.
    #[test]
    fn slow_git_is_killed_at_the_deadline() {
        let dir = Scope::new();
        let repo = bare_dot_git(dir.root());
        let pid_file = dir.root().join("git.pid");
        let git = dir.root().join("git-slow");
        executable(
            &git,
            &format!(
                "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
                pid_file.display()
            ),
        );
        let dirs = Dirs::new();
        configure(
            &dirs,
            &host(git.to_str().unwrap(), DEADLINE_MS),
            &[("app", APP), ("generic", PROFILE)],
        );
        let start = Instant::now();
        assert_eq!(run_hook(dirs.cmd(), &input("make", &repo)), None);
        assert!(start.elapsed() < Duration::from_secs(10));
        let pid = pid_from(&pid_file);
        let _guard = PidGuard::new(dirs.scope(), pid);
        assert!(
            cardinal::os::process(pid, cardinal::os::Detail::default()).is_none(),
            "git {pid} survived the hook"
        );
        let found = super::records(&dirs);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].data["reason"], "deadline_missed");
    }

    /// A descendant of git that keeps its stdout open does not hold the hook past the
    /// deadline, and ends with git's process group. The hook runs in a group an idle process
    /// leads, as under a shell without job control, so only the hook ends the descendant
    /// while that group lives on.
    #[test]
    fn held_output_pipe_does_not_block_the_hook() {
        let dir = Scope::new();
        let repo = bare_dot_git(dir.root());
        let pid_file = dir.root().join("grandchild.pid");
        let git = dir.root().join("git-detached");
        executable(
            &git,
            &format!(
                "#!/bin/sh\nsleep 30 &\necho $! > '{}'\nprintf 'https://github.com/o/app\\n'\nexit 0\n",
                pid_file.display()
            ),
        );
        let dirs = Dirs::new();
        configure(
            &dirs,
            &host(git.to_str().unwrap(), DEADLINE_MS),
            &[("app", APP), ("generic", PROFILE)],
        );
        let leader = dirs
            .scope()
            .command("sleep", dirs.scope().root())
            .arg("60")
            .spawn()
            .expect("start the group leader");
        let mut hook = dirs.cmd();
        hook.join_group(&leader);
        let start = Instant::now();
        assert_eq!(run_hook(hook, &input("make", &repo)), None);
        let pid = pid_from(&pid_file);
        let _guard = PidGuard::new(dirs.scope(), pid);
        assert!(start.elapsed() < Duration::from_secs(10));
        wait_for(
            "the end of git's grandchild",
            Duration::from_secs(5),
            || cardinal::os::start_time_s(pid).is_none().then_some(()),
        );
        drop(leader);
    }
}
