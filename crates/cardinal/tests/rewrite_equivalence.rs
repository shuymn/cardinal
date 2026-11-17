//! A rewritten command keeps its meaning (`docs/architecture.md` sections 6.4 and 13.5):
//! each input runs through the real shells (`bash` and `zsh` where installed, `sh` always)
//! once as written and once as the hook rewrote it, and the two runs must agree on stdout,
//! stderr, exit status, the final cwd, and the environment. Claims C5 and C6 (section 16).
//!
//! Until the real runner exists, a fake runner that executes its argv after `--` stands in
//! (`hook_support::fake_runner`); the integration gate repeats these cases with
//! `cardinal run`.

#![cfg(unix)]

mod hook_support;
mod support;

use std::path::PathBuf;
use std::process::Stdio;

use hook_support::{
    PROFILE, Sandbox, configure, executable, fake_runner, host, input, probe, rewritten, run_hook,
    runner_path,
};
use proptest::prelude::{Just, Strategy, prop, prop_assert_eq, prop_oneof, proptest};
use proptest::test_runner::Config as ProptestConfig;
use support::fixture::Fixture;

/// Printed by the outer shell after the command: its status, its cwd (what Claude Code
/// keeps for the next call), and its exported environment.
const TRAILER: &str = "__s=$?; printf '\\n[status] %s\\n[pwd] %s\\n' \"$__s\" \"$(pwd)\"; env | LC_ALL=C sort | grep -v -e '^_=' -e '^SHLVL='";

/// A shell found on `PATH`.
#[derive(Clone, Debug)]
struct Shell {
    name: &'static str,
    path: PathBuf,
    /// The options that keep the shell from reading startup files: `bash --norc
    /// --noprofile` and `zsh -f` (zsh still reads `/etc/zshenv`); `sh` reads none when not
    /// interactive and `ENV` is unset.
    options: &'static [&'static str],
    /// Whether `set -o pipefail` works.
    pipefail: bool,
}

fn find(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// `bash` and `zsh` where installed, `sh` always, checked in `fixture`.
fn shells(fixture: &Fixture) -> Vec<Shell> {
    let options: [(&'static str, &'static [&'static str]); 2] =
        [("bash", &["--norc", "--noprofile"]), ("zsh", &["-f"])];
    let mut found: Vec<Shell> = options
        .into_iter()
        .filter_map(|(name, options)| Some(shell(fixture, name, options, find(name)?)))
        .collect();
    found.push(shell(
        fixture,
        "sh",
        &[],
        find("sh").expect("sh is on PATH"),
    ));
    found
}

fn shell(
    fixture: &Fixture,
    name: &'static str,
    options: &'static [&'static str],
    path: PathBuf,
) -> Shell {
    let pipefail = fixture
        .command(&path, fixture.root())
        .args(options)
        .args(["-c", "set -o pipefail"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    Shell {
        name,
        path,
        options,
        pipefail,
    }
}

/// What one run of a command printed and returned.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Outcome {
    stdout: String,
    stderr: String,
    status: Option<i32>,
}

/// A test bench in a fixture, whose constructed environment the shells get: tools in
/// `bin/`, a fake runner, the shells, and two configurations whose runner path is the fake
/// runner (`present`) or a path that does not exist (`missing`).
struct Bench {
    fixture: Fixture,
    bin: PathBuf,
    log: PathBuf,
    runner: String,
    shells: Vec<Shell>,
    present: Sandbox,
    missing: Sandbox,
}

impl Bench {
    fn new() -> Self {
        let fixture = Fixture::new();
        let root = fixture.root();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let log = root.join("runner.log");
        probe(&bin);
        executable(&bin.join("npx"), "#!/bin/sh\nexec \"$@\"\n");
        let runner = runner_path(&fake_runner(&bin, &log));
        let present = Sandbox::new();
        configure(&present, &host(&runner, ""), &[("generic", PROFILE)]);
        let missing = Sandbox::new();
        let absent = runner_path(&root.join("absent").join("cardinal"));
        configure(&missing, &host(&absent, ""), &[("generic", PROFILE)]);
        let shells = shells(&fixture);
        Self {
            fixture,
            bin,
            log,
            runner,
            shells,
            present,
            missing,
        }
    }

    fn work(&self) -> PathBuf {
        self.fixture.root().join("work")
    }

    /// The command as the hook rewrites it with `dirs`' configuration; unchanged when the
    /// hook prints nothing.
    fn rewrite(&self, dirs: &Sandbox, command: &str) -> String {
        run_hook(hook_support::cmd(dirs), &input(command, &self.work()))
            .map_or_else(|| command.to_owned(), |doc| rewritten(&doc).to_owned())
    }

    /// Runs `setup` and `command` in `shell` in a fresh work dir; returns what it printed and
    /// how many times the fake runner ran.
    fn run(&self, shell: &Shell, setup: &str, command: &str) -> (Outcome, usize) {
        let work = self.work();
        if work.exists() {
            std::fs::remove_dir_all(&work).unwrap();
        }
        std::fs::create_dir_all(work.join("sub")).unwrap();
        for (name, contents) in [
            ("a.txt", "a\n"),
            ("b.txt", "b\n"),
            ("in.txt", "line one\nline two\n"),
            ("sub/c.txt", "c\n"),
        ] {
            std::fs::write(work.join(name), contents).unwrap();
        }
        std::fs::write(&self.log, b"").unwrap();
        let path = std::env::join_paths(
            std::iter::once(self.bin.clone())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        let output = self
            .fixture
            .command(&shell.path, &work)
            .args(shell.options)
            .arg("-c")
            // `/etc/zshenv` may reset PATH; the script sets it again.
            .arg(format!(
                "PATH='{}'; export PATH\n{setup}\n{command}\n{TRAILER}",
                path.to_string_lossy()
            ))
            .env("PATH", &path)
            .env("PWD", &work)
            .env("EXPORTED", "visible")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let calls = std::fs::read_to_string(&self.log).unwrap().lines().count();
        let outcome = Outcome {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            status: output.status.code(),
        };
        (outcome, calls)
    }
}

/// One input: shell setup that stands for the owner's rc (functions, options), the command,
/// how many stages the hook wraps, and how many times the runner runs.
struct Case {
    name: &'static str,
    setup: &'static str,
    command: &'static str,
    wraps: usize,
    calls: usize,
    /// Shells the case applies to; empty for all.
    only: &'static [&'static str],
    /// The case needs `set -o pipefail`, which not every `sh` has.
    pipefail: bool,
}

const fn case(
    name: &'static str,
    setup: &'static str,
    command: &'static str,
    wraps: usize,
    calls: usize,
) -> Case {
    Case {
        name,
        setup,
        command,
        wraps,
        calls,
        only: &[],
        pipefail: false,
    }
}

const CASES: &[Case] = &[
    case("simple", "", "probe --err a 'b c' \"d e\"", 1, 1),
    case(
        "short_circuit",
        "",
        "cd missing && probe one || printf 'fallback\\n'",
        1,
        0,
    ),
    case("persisted_cwd", "", "cd sub && probe two", 1, 1),
    case(
        "documented_example",
        "",
        "cd sub && probe --err x 2>&1 | tail -50 || echo failed",
        1,
        1,
    ),
    Case {
        pipefail: true,
        ..case(
            "pipefail_from_rc",
            "set -o pipefail",
            "probe --exit 3 | cat",
            1,
            1,
        )
    },
    Case {
        pipefail: true,
        ..case(
            "pipefail_inline",
            "",
            "set -o pipefail; probe --exit 4 | cat; echo \"status $?\"",
            1,
            1,
        )
    },
    case(
        "errexit",
        "set -e",
        "probe --err --exit 2; echo after",
        1,
        1,
    ),
    case(
        "errexit_and_list",
        "set -e",
        "probe --exit 2 && echo yes; echo after",
        1,
        1,
    ),
    case(
        "errexit_or_list",
        "set -e",
        "probe --exit 2 || echo recovered; probe --exit 5; echo unreachable",
        2,
        2,
    ),
    case("exit_status", "", "probe --exit 7", 1, 1),
    case(
        "non_exported_variable",
        "X=hidden",
        "probe \"$X\" $X; probe --env",
        2,
        2,
    ),
    case("exported_variable", "export Y=shown", "probe --env", 1, 1),
    case(
        "variable_set_in_command",
        "",
        "Z=inline; probe \"$Z\"; echo \"$Z\"",
        1,
        1,
    ),
    case("globs_and_tilde", "", "probe *.txt sub/* ~ ~/x", 1, 1),
    case(
        "redirect_out",
        "",
        "probe --err out > out.log 2> err.log; cat out.log err.log",
        1,
        1,
    ),
    case(
        "redirect_append",
        "",
        "probe one >> out.log; probe two >> out.log; cat out.log",
        2,
        2,
    ),
    case("redirect_in", "", "probe --stdin < in.txt", 1, 1),
    case(
        "redirect_fds",
        "",
        "probe --err fds 3>fd3.log 2>&1 | cat; cat fd3.log",
        1,
        1,
    ),
    case(
        "redirect_before_command",
        "",
        "2>err.log probe --err pre; cat err.log",
        1,
        1,
    ),
    Case {
        only: &["bash", "zsh"],
        ..case(
            "redirect_both",
            "",
            "probe --err both &>both.log; cat both.log",
            1,
            1,
        )
    },
    case(
        "several_stages",
        "",
        "probe one && probe two; probe three | cat",
        3,
        3,
    ),
    case(
        "two_classified_in_a_pipeline",
        "",
        "probe a | probe --stdin",
        0,
        0,
    ),
    // zsh's MULTIOS sends the output to the file and the pipe; a wrapped stage would lose
    // the pipe to the surrounding `if`.
    case(
        "piped_redirect_stays",
        "",
        "probe out >> out.log | cat; printf 'p\\n' | probe --stdin < in.txt; cat out.log",
        0,
        0,
    ),
    case("sleep_stays", "", "sleep 0 && probe x", 1, 1),
    case("assignment_stays", "", "CI=1 probe x", 0, 0),
    case(
        "background_stays",
        "",
        "probe bg > bg.log & wait; cat bg.log",
        0,
        0,
    ),
    case("shell_state_stays", "", "false; probe $?", 0, 0),
    case(
        "last_argument_stays",
        "",
        "probe first last; probe \"$_\"",
        1,
        1,
    ),
    case("builtin_stays", "", "command probe x", 0, 0),
    case(
        "function_from_rc",
        "greet() { printf 'hi %s\\n' \"$1\"; }",
        "greet you && probe x",
        1,
        1,
    ),
    case("comment", "", "probe a # trailing comment", 1, 1),
    case("multiline", "", "probe one\nprobe two", 2, 2),
    case("env_wrapper", "", "env W=1 probe --env", 1, 1),
    case("shell_c", "", "sh -c 'probe inner'", 1, 1),
    case("tool_wrapper", "", "npx probe wrapped", 1, 1),
    case(
        "quoted_operators",
        "",
        "probe 'a;b' \"c|d\" e\\;f '&&' \\#x",
        1,
        1,
    ),
    case("stderr_only", "", "probe --err quiet 2>/dev/null", 1, 1),
    case("unicode", "", "probe 'é 日本' ü", 1, 1),
    case("refused_command", "", "probe $(printf sub)", 0, 0),
];

/// Runs every case in every shell with `dirs`' rewrite and compares it with the original.
fn check(bench: &Bench, dirs: &Sandbox, expect_calls: bool) {
    let shells = &bench.shells;
    for case in CASES {
        let rewritten = bench.rewrite(dirs, case.command);
        assert_eq!(
            rewritten.matches(" run --spec-b64 ").count(),
            case.wraps,
            "{}: {rewritten}",
            case.name
        );
        for shell in shells.iter().filter(|s| {
            (case.only.is_empty() || case.only.contains(&s.name)) && (!case.pipefail || s.pipefail)
        }) {
            let (original, original_calls) = bench.run(shell, case.setup, case.command);
            let (outcome, calls) = bench.run(shell, case.setup, &rewritten);
            assert_eq!(original_calls, 0);
            assert_eq!(
                outcome, original,
                "{} in {}:\n{rewritten}",
                case.name, shell.name
            );
            let expected = if expect_calls { case.calls } else { 0 };
            assert_eq!(
                calls, expected,
                "{} in {}: runner calls",
                case.name, shell.name
            );
        }
    }
}

/// C6: with a runner in place, every classified stage runs through it and the command
/// keeps its meaning.
#[test]
fn runner_present() {
    let bench = Bench::new();
    check(&bench, &bench.present, true);
}

/// C5: with the runner path missing or not executable, the fallback runs each original
/// stage, and the command keeps its meaning.
#[test]
fn runner_missing() {
    let bench = Bench::new();
    let rewritten = bench.rewrite(&bench.missing, "probe x");
    assert!(
        rewritten.contains("/absent/cardinal ]; then"),
        "{rewritten}"
    );
    check(&bench, &bench.missing, false);

    let bench = Bench::new();
    std::fs::set_permissions(
        &bench.runner,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o644),
    )
    .unwrap();
    check(&bench, &bench.present, false);
}

/// The first stage of a generated pipeline. Only `2>&1` sends anything to stderr, so the
/// concurrent stages of a pipeline never race on it.
fn head() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("probe a"),
        Just("probe --exit 1"),
        Just("probe --exit 0 'x y'"),
        Just("probe --stdin"),
        Just("probe \"$X\" *.txt"),
        Just("npx probe w"),
        Just("true"),
        Just("false"),
        Just("cd sub"),
        Just("cd .."),
        Just("printf 'p\\n'"),
        Just("X=set"),
        Just("echo \"$X\""),
        Just("probe --err e 2>&1"),
        Just("probe out >> out.log"),
        Just("cat out.log 2>&1"),
    ]
}

/// A later stage of a generated pipeline: it reads all of its input, so no earlier stage
/// dies of SIGPIPE at a moment that depends on scheduling.
fn consumer() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("cat"),
        Just("probe --stdin"),
        Just("tail -2"),
        Just("wc -l")
    ]
}

fn pipeline() -> impl Strategy<Value = String> {
    (head(), prop::collection::vec(consumer(), 0..3)).prop_map(|(head, rest)| {
        std::iter::once(head)
            .chain(rest)
            .collect::<Vec<_>>()
            .join(" | ")
    })
}

fn separator() -> impl Strategy<Value = &'static str> {
    prop_oneof![Just(" && "), Just(" || "), Just("; "), Just("\n")]
}

fn setup() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just(""),
        Just("set -e"),
        Just("set -o pipefail"),
        Just("X=outer")
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, failure_persistence: None, ..ProptestConfig::default() })]

    /// Random lists of pipelines of classified and unclassified stages keep their meaning in
    /// every shell.
    #[test]
    fn generated_commands_keep_their_meaning(
        setup in setup(),
        parts in prop::collection::vec((pipeline(), separator()), 1..5),
    ) {
        let mut command = String::new();
        for (i, (pipeline, sep)) in parts.iter().enumerate() {
            command.push_str(pipeline);
            if i + 1 < parts.len() {
                command.push_str(sep);
            }
        }
        let bench = Bench::new();
        let rewritten = bench.rewrite(&bench.present, &command);
        for shell in bench.shells.iter().filter(|s| s.pipefail || !setup.contains("pipefail")) {
            let (original, _) = bench.run(shell, setup, &command);
            let (outcome, _) = bench.run(shell, setup, &rewritten);
            prop_assert_eq!(outcome, original, "{} in {}:\n{}\n{}", command, shell.name, rewritten, setup);
        }
    }
}

/// zsh's MULTIOS copies a stage's redirected stdout into its pipe as well (and reads a
/// redirected stdin after its pipe) only while the redirection and the pipe belong to the
/// same simple command, so the hook leaves such stages unwrapped (section 7.2). The
/// hand-wrapped form shows the difference in zsh and none in the other shells.
#[test]
fn piped_redirection_stays_unwrapped() {
    let bench = Bench::new();
    for command in [
        "probe out >> out.log | cat",
        "printf 'p\\n' | probe --stdin < in.txt",
    ] {
        assert_eq!(bench.rewrite(&bench.present, command), command);
    }
    let stage = "probe out >> out.log";
    let original = format!("{stage} | cat; cat out.log");
    let wrapped = format!(
        "if [ -x {runner} ]; then {runner} run -- {stage}; else {stage}; fi | cat; cat out.log",
        runner = bench.runner
    );
    for shell in &bench.shells {
        let (as_written, _) = bench.run(shell, "", &original);
        let (as_wrapped, _) = bench.run(shell, "", &wrapped);
        if shell.name == "zsh" {
            assert_ne!(
                as_wrapped, as_written,
                "zsh copies into the pipe only as written"
            );
        } else {
            assert_eq!(as_wrapped, as_written, "{}", shell.name);
        }
    }
}
