//! Helpers shared by the hook tests: configuration, hook input, running the hook, and the
//! fake runner and probe tools that execute rewritten commands (`docs/architecture.md`
//! section 13.5). A test crate declares `mod support;` and `mod hook_support;`.
//!
//! Every child a hook test starts gets a constructed environment and a work dir inside a
//! fixture ([`Sandbox`]), never the environment or the directory the tests run in.

#![allow(
    dead_code,
    reason = "each test crate uses a different subset of the helpers"
)]

use std::ffi::OsStr;
use std::ops::Deref;
use std::path::{Path, PathBuf};

use cardinal::contract::RunSpec;
use serde_json::{Value, json};

use crate::support::cardinal::Dirs;
use crate::support::fixture::Fixture;

/// cardinal's state and config dirs for a test, and a fixture whose constructed environment
/// (`docs/architecture.md` section 13.3) and work dir every child of the test gets: nothing
/// of the environment the tests run in reaches a child but `PATH` (and what Windows needs to
/// start a program), so no `GIT_*`, `CLAUDE_*`, `CARDINAL_TEST_*`, or Bash timeout variable
/// does; `HOME` and the git configuration are the fixture's; and the child starts in the
/// fixture, so a hook that falls back to its own cwd never looks at the repository the tests
/// run in.
pub struct Sandbox {
    dirs: Dirs,
    fixture: Fixture,
    work: PathBuf,
}

impl Sandbox {
    pub fn new() -> Self {
        let fixture = Fixture::new();
        let work = fixture.root().join("work");
        std::fs::create_dir_all(&work).expect("create the work dir");
        Self {
            dirs: Dirs::new(),
            fixture,
            work,
        }
    }

    pub fn fixture(&self) -> &Fixture {
        &self.fixture
    }

    /// The directory children start in, inside the fixture root.
    pub fn work(&self) -> &Path {
        &self.work
    }

    /// `program` started in the work dir with the constructed environment and the cardinal
    /// dirs.
    pub fn command(&self, program: impl AsRef<OsStr>) -> std::process::Command {
        let mut command = self.fixture.command(program, &self.work);
        command
            .env("CARDINAL_STATE_DIR", self.dirs.state())
            .env("CARDINAL_CONFIG_DIR", self.dirs.config());
        command
    }

    /// `cardinal`, started as [`Sandbox::command`] starts a program. Shadows [`Dirs::cmd`],
    /// which inherits the environment.
    pub fn cmd(&self) -> assert_cmd::Command {
        assert_cmd::Command::from_std(self.command(env!("CARGO_BIN_EXE_cardinal")))
    }
}

impl Deref for Sandbox {
    type Target = Dirs;

    fn deref(&self) -> &Dirs {
        &self.dirs
    }
}

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

/// `host.toml` with `runner`, a generous deadline (tests start many processes at once), and
/// `extra` appended.
pub fn host(runner: &str, extra: &str) -> String {
    format!("[budget]\ncpu = 8\n[hook]\nrunner = \"{runner}\"\ndeadline_ms = 10000\n{extra}")
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

/// Runs `cardinal hook claude pre-tool-use` with `input` on stdin. Asserts exit 0 and at
/// most one line of output; returns the parsed output, `None` when it printed nothing.
pub fn run_hook(mut command: assert_cmd::Command, input: &Value) -> Option<Value> {
    let output = command
        .args(["hook", "claude", "pre-tool-use"])
        .write_stdin(serde_json::to_vec(input).expect("serialize the input"))
        .output()
        .expect("run the hook");
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

/// A fake runner at `dir/fake-cardinal` that stands in for `cardinal run` until the real
/// runner exists: it appends its argv to `log`, then executes the argv after `--`, as the
/// runner does, in the same process.
pub fn fake_runner(dir: &Path, log: &Path) -> PathBuf {
    let path = dir.join("fake-cardinal");
    executable(
        &path,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = -- ]; then shift; break; fi\n  shift\ndone\nexec \"$@\"\n",
            log.display()
        ),
    );
    path
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

/// `cardinal` for `sandbox` ([`Sandbox::cmd`]); no Bash timeout variable is set, so specs get
/// the documented defaults unless a test sets one.
pub fn cmd(sandbox: &Sandbox) -> assert_cmd::Command {
    sandbox.cmd()
}
