//! The `cardinal` binary: help, versions, usage errors, and the errors every command answers
//! with: a `cardinal.error.v1` document on stdout, a line on stderr, and exit 1.

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
        "reconcile",
        "arbiter",
        "ui",
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
        &["reconcile", "--trigger", "hourly"],
        &["arbiter"],
        &["ui", "extra"],
    ] {
        dirs.cmd().args(bad).assert().code(2);
    }
}

/// `--owner-approved` is a usage error unless the caller is the owner (section 9.3); with
/// `--caller owner` it parses and the command runs (here, finding no plan).
#[test]
fn owner_approved_needs_the_owner() {
    let dirs = Dirs::new();
    for caller in [
        &[][..],
        &["--caller", "arbiter"],
        &["--caller", "unit:disk-reclaim"],
    ] {
        let mut args = vec!["act", "abcd", "--owner-approved"];
        args.extend_from_slice(caller);
        let assert = dirs.cmd().args(&args).assert().code(2);
        let output = assert.get_output();
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("--caller owner"),
            "{args:?}"
        );
    }
    let assert = dirs
        .cmd()
        .args(["act", "abcd", "--owner-approved", "--caller", "owner"])
        .assert()
        .code(1);
    assert_eq!(
        json(&assert.get_output().stdout)["error"]["code"],
        "not_found"
    );
    let assert = dirs
        .cmd()
        .args(["act", "abcd", "--squash-merged"])
        .assert()
        .code(1);
    assert_eq!(
        json(&assert.get_output().stdout)["error"]["code"],
        "not_found"
    );
}

/// The phase 1 entry point whose body a later unit writes, `cardinal ui`, answers
/// `not_implemented` as a `cardinal.error.v1` document with exit 1.
#[test]
fn unbuilt_entry_points_answer_not_implemented() {
    let dirs = Dirs::new();
    let assert = dirs.cmd().arg("ui").assert().code(1);
    let doc = json(&assert.get_output().stdout);
    assert_eq!(doc["schema"], "cardinal.error.v1");
    assert_eq!(doc["error"]["code"], "not_implemented");
}

#[test]
fn invalid_config_is_config_invalid() {
    let dirs = Dirs::new();
    std::fs::write(dirs.config().join("host.toml"), "typo = 1\n").unwrap();
    let assert = dirs.cmd().arg("inventory").assert().code(1);
    let output = assert.get_output();
    let doc = json(&output.stdout);
    assert_eq!(doc["schema"], "cardinal.error.v1");
    assert_eq!(doc["error"]["code"], "config_invalid");
    assert!(
        String::from_utf8_lossy(&output.stderr).starts_with("cardinal: "),
        "a human-readable line on stderr"
    );
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
