//! The `cardinal` binary: help, versions, usage errors, and the errors every command answers
//! with: a `cardinal.error.v1` document on stdout, a line on stderr, and exit 1.

mod support;

use std::path::Path;

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

/// `cardinal --version`: the package version, followed by the source commit in a build from a
/// checkout (`build.rs`, `tests/version.rs`). The commit is part of the binary: with no
/// program on `PATH` the output is the same.
#[test]
fn version_names_the_source_commit() {
    let dirs = Dirs::new();
    let version = |path: Option<&str>| {
        let mut cmd = dirs.cmd();
        if let Some(path) = path {
            cmd.env("PATH", path);
        }
        let output = cmd.arg("--version").assert().success().get_output().clone();
        String::from_utf8(output.stdout).unwrap()
    };
    let text = version(None);
    assert_eq!(version(Some("")), text);
    let line = text.strip_suffix('\n').expect("one line");
    let package = format!("cardinal {}", env!("CARGO_PKG_VERSION"));
    let rest = line
        .strip_prefix(&package)
        .unwrap_or_else(|| panic!("{line:?} does not start with {package:?}"));
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.git")
        .exists();
    if !checkout {
        assert_eq!(rest, "", "{line:?} names a commit, built from no checkout");
        return;
    }
    let (id, date) = rest
        .strip_prefix(" (")
        .and_then(|r| r.strip_suffix(')'))
        .and_then(|r| r.split_once(' '))
        .unwrap_or_else(|| panic!("{line:?} names no commit, built from a checkout"));
    let id = id.strip_suffix("-dirty").unwrap_or(id);
    assert!(
        id.len() >= 9
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "{line:?}: {id:?} is no abbreviated commit ID"
    );
    let digits: Vec<&str> = date.split('-').collect();
    assert!(
        matches!(digits[..], [y, m, d] if y.len() == 4 && m.len() == 2 && d.len() == 2)
            && digits.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit())),
        "{line:?}: {date:?} is no YYYY-MM-DD date"
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

/// A25: `[act] execute` may grant `reclaim_dir` to the owner and `unit:disk-reclaim` only. A
/// grant to the arbiter, of `reclaim_worktree`, or to another caller makes every command
/// fail with `config_invalid`, `act` included, so no such grant ever executes.
#[test]
fn act_execute_refuses_the_arbiter_and_worktrees() {
    let dirs = Dirs::new();
    for (grant, says) in [
        ("arbiter = [\"reclaim_dir\"]", "arbiter holds no grant"),
        ("owner = [\"reclaim_worktree\"]", "never executed"),
        (
            "\"unit:disk-reclaim\" = [\"reclaim_worktree\"]",
            "never executed",
        ),
        (
            "\"unit:other\" = [\"reclaim_dir\"]",
            "not a caller that executes",
        ),
        ("owner = [\"everything\"]", "not a plan kind that executes"),
    ] {
        std::fs::write(
            dirs.config().join("host.toml"),
            format!("[act]\nexecute = {{ {grant} }}\n"),
        )
        .unwrap();
        for args in [&["inventory"][..], &["act", "abcd", "--caller", "owner"]] {
            let assert = dirs.cmd().args(args).assert().code(1);
            let doc = json(&assert.get_output().stdout);
            assert_eq!(doc["error"]["code"], "config_invalid", "{grant}, {args:?}");
            let message = doc["error"]["message"].as_str().unwrap();
            assert!(message.contains(says), "{grant}: {message}");
        }
    }
    std::fs::write(
        dirs.config().join("host.toml"),
        "[act]\nexecute = { owner = [\"reclaim_dir\"], \"unit:disk-reclaim\" = [] }\n",
    )
    .unwrap();
    let assert = dirs
        .cmd()
        .args(["act", "abcd", "--caller", "owner"])
        .assert()
        .code(1);
    assert_eq!(
        json(&assert.get_output().stdout)["error"]["code"],
        "not_found"
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
