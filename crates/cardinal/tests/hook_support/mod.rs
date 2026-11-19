//! Helpers shared by the hook tests: configuration, hook input, running the hook, the real
//! runner and the probe tools that execute rewritten commands, and the jobs the runner
//! started (`docs/architecture.md` section 13.5). A test crate declares `mod support;` and
//! `mod hook_support;`.
//!
//! Every child a hook test starts goes through `cardinal_test_support` (section 13.3): the
//! hook through [`Dirs::cmd`], or [`cardinal`] in a directory of another scope, with a
//! constructed environment and a working directory inside a scope, never the environment or
//! the directory the tests run in.

#![allow(
    dead_code,
    reason = "each test crate uses a different subset of the helpers"
)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use cardinal::contract::{Record, RecordKind, RunSpec};
use cardinal::state::StateDir;
use cardinal_test_support::{Command, Scope, bin_exe};
use serde_json::{Value, json};

use crate::support::cardinal::Dirs;

/// A runner path for tests that only look at the rewritten text.
pub const RUNNER: &str = "/opt/cardinal/bin/cardinal";

/// A generic profile with the rules the hook tests use. `probe` is the test tool of
/// [`probe`]; the other names never run.
pub const PROFILE: &str = r#"
[classify]
wrappers = [["npx"], ["pnpm", "exec"]]
[[rule]]
id = "test"
commands = ["probe", "pnpm test", "vitest"]
except = ["--watch"]
class = "test"
resources = { cpu = 3 }
[[rule]]
id = "build"
commands = ["make"]
class = "build"
"#;

/// `host.toml` with `runner`, a generous deadline (tests start many processes at once),
/// disk lines below any free space, so a runner's job-end check never starts a `reconcile`
/// pass (`docs/architecture.md` section 5.2), and `extra` appended.
pub fn host(runner: &str, extra: &str) -> String {
    format!(
        "[budget]\ncpu = 8\n[disk]\nwarning_mib = 1\nrecovery_mib = 2\ncritical_mib = 0\n\
         [hook]\nrunner = \"{runner}\"\ndeadline_ms = 10000\n{extra}"
    )
}

/// Writes `host.toml` and `repos/<name>.toml` files into the config dir.
pub fn configure(dirs: &Dirs, host: &str, profiles: &[(&str, &str)]) {
    std::fs::write(dirs.config().join("host.toml"), host).expect("write host.toml");
    let repos = dirs.config().join("repos");
    std::fs::create_dir_all(&repos).expect("create repos/");
    for (name, text) in profiles {
        std::fs::write(repos.join(format!("{name}.toml")), text).expect("write a profile");
    }
}

/// A `PreToolUse` input for a Bash call.
pub fn input(command: &str, cwd: &Path) -> Value {
    json!({
        "session_id": "session-1",
        "transcript_path": "/tmp/transcript.jsonl",
        "cwd": cwd,
        "permission_mode": "default",
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {
            "command": command,
            "description": "Run it",
            "timeout": 120_000,
            "run_in_background": false
        },
        "tool_use_id": "toolu_01"
    })
}

/// `cardinal` in `dir` inside `scope`, with `dirs`' state and config dirs: for a hook that
/// runs in a repository of a fixture.
pub fn cardinal(scope: &Scope, dirs: &Dirs, dir: &Path) -> Command {
    let mut command = scope.command(bin_exe("cardinal"), dir);
    command
        .env("CARDINAL_STATE_DIR", dirs.state())
        .env("CARDINAL_CONFIG_DIR", dirs.config());
    command
}

/// Runs `command` to completion with `stdin` written to its standard input and its stdout
/// and stderr captured. A child that stops reading early (the hook at its input limit) cuts
/// the write short.
pub fn output_with_stdin(command: &mut Command, stdin: &[u8]) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the child");
    let mut pipe = child.stdin.take().expect("piped stdin");
    let _ = pipe.write_all(stdin);
    drop(pipe);
    child.wait_with_output().expect("wait for the child")
}

/// Runs `cardinal hook claude pre-tool-use` with `input` on stdin. Asserts exit 0 and at
/// most one line of output; returns the parsed output, `None` when it printed nothing.
pub fn run_hook(mut command: Command, input: &Value) -> Option<Value> {
    command.args(["hook", "claude", "pre-tool-use"]);
    let output = output_with_stdin(
        &mut command,
        &serde_json::to_vec(input).expect("serialize the input"),
    );
    assert_eq!(output.status.code(), Some(0), "the hook exits 0");
    assert!(
        output.stderr.is_empty(),
        "the hook writes nothing to stderr"
    );
    parse_output(&output.stdout)
}

/// Parses hook stdout: nothing, or one JSON document on one line.
pub fn parse_output(stdout: &[u8]) -> Option<Value> {
    if stdout.is_empty() {
        return None;
    }
    let text = std::str::from_utf8(stdout).expect("UTF-8 output");
    assert_eq!(text.lines().count(), 1, "one line: {text}");
    Some(serde_json::from_str(text).expect("JSON output"))
}

/// The rewritten command of a hook output.
pub fn rewritten(doc: &Value) -> &str {
    doc["hookSpecificOutput"]["updatedInput"]["command"]
        .as_str()
        .expect("updatedInput.command")
}

/// Every run spec in a rewritten command, in order.
pub fn specs(command: &str) -> Vec<RunSpec> {
    command
        .split("--spec-b64 ")
        .skip(1)
        .map(|rest| {
            let encoded = rest.split(' ').next().expect("a spec");
            RunSpec::decode_b64(encoded).expect("a valid spec")
        })
        .collect()
}

/// Writes an executable file.
pub fn executable(path: &Path, contents: &str) {
    std::fs::write(path, contents).expect("write an executable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("make it executable");
    }
}

/// `dir/cardinal`, a symbolic link to the `cardinal` binary under test: the real runner at a
/// path the `[hook] runner` rule accepts wherever the build put the binary, which a test can
/// replace with a file or a directory without touching the binary.
#[cfg(unix)]
pub fn real_runner(dir: &Path) -> PathBuf {
    let path = dir.join("cardinal");
    std::os::unix::fs::symlink(bin_exe("cardinal"), &path).expect("link the runner");
    path
}

/// The `job_started` records of the runs with the state dir `state`, one per runner that
/// started its stage; none before the first run. Panics if a runner failed open or reported
/// an infrastructure failure: these tests expect every run to be admitted.
pub fn started_jobs(state: &Path) -> Vec<Record> {
    if !state.exists() {
        return Vec::new();
    }
    let state = StateDir::open(state).expect("open the state dir");
    let records = cardinal::record::read_all(&state).expect("read records");
    for record in &records {
        assert!(
            !matches!(
                record.kind,
                RecordKind::JobFailOpen | RecordKind::JobInfraFailure
            ),
            "{record:#?}"
        );
    }
    records
        .into_iter()
        .filter(|r| r.kind == RecordKind::JobStarted)
        .collect()
}

/// The test tool `probe` in `dir`: prints its arguments and its physical cwd; `--err` writes
/// a line to stderr, `--exit N` sets its status, `--stdin` copies stdin, `--env` prints its
/// sorted environment without `_` and `SHLVL`, which the shell running a script sets itself.
/// Writing to stderr only on request keeps the output of pipelines deterministic.
pub fn probe(dir: &Path) -> PathBuf {
    let path = dir.join("probe");
    executable(
        &path,
        "#!/bin/sh\n\
         printf 'probe argv:'\n\
         for a in \"$@\"; do printf ' <%s>' \"$a\"; done\n\
         printf '\\npwd: %s\\n' \"$(pwd -P)\"\n\
         code=0\n\
         while [ \"$#\" -gt 0 ]; do\n\
           case \"$1\" in\n\
             --exit) code=$2; shift ;;\n\
             --err) printf 'probe stderr\\n' >&2 ;;\n\
             --stdin) printf 'stdin: '; cat ;;\n\
             --env) env | LC_ALL=C sort | grep -v -e '^_=' -e '^SHLVL=' ;;\n\
           esac\n\
           shift\n\
         done\n\
         exit \"$code\"\n",
    );
    path
}

/// A runner path the `[hook] runner` rule accepts, or a panic naming the problem: the temp
/// dir of the test host must not need shell quoting.
pub fn runner_path(path: &Path) -> String {
    let text = path.to_str().expect("UTF-8 temp path").to_owned();
    cardinal::config::validate_runner(&text)
        .unwrap_or_else(|err| panic!("the temp dir cannot hold a runner path: {err}"));
    text
}
