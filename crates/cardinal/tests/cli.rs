//! The `cardinal` binary: help, versions, usage errors, and the `not_implemented` answer of
//! every command whose body belongs to a later work unit.

mod support;

use support::cardinal::Dirs;

fn json(stdout: &[u8]) -> serde_json::Value {
    let text = std::str::from_utf8(stdout).unwrap();
    assert_eq!(
        text.lines().count(),
        1,
        "one JSON document on one line: {text}"
    );
    serde_json::from_str(text).unwrap()
}

#[test]
fn help_lists_every_command() {
    let dirs = Dirs::new();
    let output = dirs
        .cmd()
        .arg("--help")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let help = String::from_utf8(output).unwrap();
    for command in [
        "run",
        "hook",
        "inventory",
        "inbox",
        "plan",
        "act",
        "explain",
    ] {
        assert!(
            help.lines()
                .any(|line| line.trim_start().starts_with(command)),
            "{command} missing from:\n{help}"
        );
    }
}

#[test]
fn version_is_text() {
    let dirs = Dirs::new();
    let output = dirs
        .cmd()
        .arg("--version")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        String::from_utf8(output).unwrap().trim(),
        format!("cardinal {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn unimplemented_commands_answer_not_implemented_with_exit_1() {
    let dirs = Dirs::new();
    // Commands with written bodies are tested with their modules (`tests/act_single.rs`,
    // `tests/inbox.rs`, `tests/explain.rs`, and `tests/plan_act.rs`, which runs `plan` only
    // on a fixture repository); these still wait for the modules they call.
    let commands: &[&[&str]] = &[
        &["inbox"],
        &["explain", "command", "--cwd", "/w", "--", "pnpm test"],
    ];
    for args in commands {
        let assert = dirs.cmd().args(*args).assert().code(1);
        let output = assert.get_output();
        let doc = json(&output.stdout);
        assert_eq!(doc["schema"], "cardinal.error.v1", "{args:?}");
        assert_eq!(doc["error"]["code"], "not_implemented", "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).starts_with("cardinal: "),
            "{args:?}: a human-readable line on stderr"
        );
    }
}

#[test]
fn hooks_always_exit_0() {
    let dirs = Dirs::new();
    for event in ["pre-tool-use", "subagent-stop", "session-end"] {
        // Empty input is an error like any other: no output, exit 0.
        let assert = dirs
            .cmd()
            .args(["hook", "claude", event])
            .assert()
            .success();
        assert_eq!(
            String::from_utf8_lossy(&assert.get_output().stdout),
            "",
            "{event}"
        );
    }
    for bad in [
        &["hook"][..],
        &["hook", "claude"],
        &["hook", "claude", "unknown-event"],
        &["hook", "claude", "pre-tool-use", "--bogus"],
    ] {
        let assert = dirs.cmd().args(bad).assert().success();
        assert!(assert.get_output().stdout.is_empty(), "{bad:?}");
    }
}

#[test]
fn usage_errors_exit_2() {
    let dirs = Dirs::new();
    for bad in [
        &["bogus"][..],
        &["run", "true"],
        &["act"],
        &["act", "abcd", "--caller", "nobody"],
        &["inbox", "resolve", "k"],
        &["wait", "abcd"],
    ] {
        dirs.cmd().args(bad).assert().code(2);
    }
}

#[test]
fn invalid_config_is_config_invalid() {
    let dirs = Dirs::new();
    std::fs::write(dirs.config().join("host.toml"), "typo = 1\n").unwrap();
    let assert = dirs.cmd().arg("inventory").assert().code(1);
    let doc = json(&assert.get_output().stdout);
    assert_eq!(doc["error"]["code"], "config_invalid");
    assert!(
        doc["error"]["message"]
            .as_str()
            .unwrap()
            .contains("host.toml")
    );
}

#[test]
fn unusable_state_dir_is_state_unavailable() {
    let dirs = Dirs::new();
    let file = dirs.config().join("not-a-dir");
    std::fs::write(&file, b"").unwrap();
    let assert = dirs
        .cmd()
        .env("CARDINAL_STATE_DIR", &file)
        .arg("inventory")
        .assert()
        .code(1);
    assert_eq!(
        json(&assert.get_output().stdout)["error"]["code"],
        "state_unavailable"
    );
    let assert = dirs
        .cmd()
        .env("CARDINAL_STATE_DIR", "relative/state")
        .arg("inventory")
        .assert()
        .code(1);
    assert_eq!(
        json(&assert.get_output().stdout)["error"]["code"],
        "state_unavailable"
    );
}
