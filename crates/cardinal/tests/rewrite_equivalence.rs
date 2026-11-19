//! A rewritten command keeps its meaning (`docs/architecture.md` sections 6.4 and 13.5):
//! each input runs through the real shells (`bash` and `zsh` where installed, `sh` always)
//! once as written and once as the hook rewrote it, and the two runs must agree on stdout,
//! stderr, exit status, the final cwd, and the environment. Claims C5 and C6 (section 16).
//!
//! A wrapped stage runs through the real runner, `cardinal run` with a state and config dir of
//! the test's own, and the records it appends count the jobs it started.

#![cfg(unix)]

mod hook_support;
mod support;

use std::path::{Path, PathBuf};
use std::process::Stdio;

use cardinal_test_support::Scope;
use hook_support::{
    PROFILE, configure, executable, host, input, probe, real_runner, rewritten, run_hook,
    runner_path, started_jobs,
};
use proptest::prelude::{Just, Strategy, prop, prop_assert_eq, prop_oneof};
use proptest::test_runner::{Config as ProptestConfig, TestRunner, contextualize_config};
use support::cardinal::Dirs;

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

/// `bash` and `zsh` where installed, `sh` always, checked in `scope`.
fn shells(scope: &Scope) -> Vec<Shell> {
    let options: [(&'static str, &'static [&'static str]); 2] =
        [("bash", &["--norc", "--noprofile"]), ("zsh", &["-f"])];
    let mut found: Vec<Shell> = options
        .into_iter()
        .filter_map(|(name, options)| Some(shell(scope, name, options, find(name)?)))
        .collect();
    found.push(shell(scope, "sh", &[], find("sh").expect("sh is on PATH")));
    found
}

fn shell(
    scope: &Scope,
    name: &'static str,
    options: &'static [&'static str],
    path: PathBuf,
) -> Shell {
    let pipefail = scope
        .command(&path, scope.root())
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

/// A test bench in a scope, whose constructed environment the shells get: tools in `bin/`,
/// the real runner as `bin/cardinal`, the shells, and two configurations whose runner path is
/// that runner (`present`) or a path that does not exist (`missing`).
struct Bench {
    scope: Scope,
    bin: PathBuf,
    runner: String,
    shells: Vec<Shell>,
    present: Dirs,
    missing: Dirs,
}

impl Bench {
    fn new() -> Self {
        let scope = Scope::new();
        let root = scope.root();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        probe(&bin);
        // Found by zsh's `PATH_DIRS` search for `tools/probe`.
        std::fs::create_dir_all(bin.join("tools")).unwrap();
        probe(&bin.join("tools"));
        executable(&bin.join("npx"), "#!/bin/sh\nexec \"$@\"\n");
        let runner = runner_path(&real_runner(&bin));
        let present = Dirs::new();
        configure(&present, &host(&runner, ""), &[("generic", PROFILE)]);
        let missing = Dirs::new();
        let absent = runner_path(&root.join("absent").join("cardinal"));
        configure(&missing, &host(&absent, ""), &[("generic", PROFILE)]);
        let shells = shells(&scope);
        Self {
            scope,
            bin,
            runner,
            shells,
            present,
            missing,
        }
    }

    /// Everything a command can change: generated commands `cd ..` at most four times from
    /// the work dir, so they stay inside it.
    fn play(&self) -> PathBuf {
        self.scope.path("play")
    }

    fn work(&self) -> PathBuf {
        self.play().join("1/2/3/4/work")
    }

    /// The command as the hook rewrites it with `dirs`' configuration; unchanged when the
    /// hook prints nothing.
    fn rewrite(&self, dirs: &Dirs, command: &str) -> String {
        run_hook(dirs.cmd(), &input(command, &self.work()))
            .map_or_else(|| command.to_owned(), |doc| rewritten(&doc).to_owned())
    }

    /// Removes everything a run changed, the work dir included.
    fn clear(&self) {
        let play = self.play();
        if play.exists() {
            std::fs::remove_dir_all(&play).unwrap();
        }
    }

    /// Runs `setup` and `command` in `shell` in a fresh work dir with `dirs`' state and config
    /// dir exported; returns what it printed and how many jobs the runner started.
    fn run(&self, shell: &Shell, dirs: &Dirs, setup: &str, command: &str) -> (Outcome, usize) {
        self.clear();
        let work = self.work();
        std::fs::create_dir_all(work.join("sub")).unwrap();
        for (name, contents) in [
            ("a.txt", "a\n"),
            ("b.txt", "b\n"),
            ("in.txt", "line one\nline two\n"),
            ("sub/c.txt", "c\n"),
        ] {
            std::fs::write(work.join(name), contents).unwrap();
        }
        let jobs_before = started_jobs(dirs.state()).len();
        let path = std::env::join_paths(
            std::iter::once(self.bin.clone())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        let mut command_line = self.scope.command(&shell.path, &work);
        if cfg!(target_os = "macos") {
            // CoreFoundation, which `cardinal` links (through sysinfo), sets this variable to
            // the user's text encoding before `main`, so every stage the runner starts carries
            // it. The agent's shell inherits the same value from the agent, which links
            // CoreFoundation as well, and so do these shells, from this process.
            let encoding = std::env::var_os("__CF_USER_TEXT_ENCODING")
                .expect("CoreFoundation sets __CF_USER_TEXT_ENCODING in this process too");
            command_line.env("__CF_USER_TEXT_ENCODING", encoding);
        }
        let output = command_line
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
            .env("CARDINAL_STATE_DIR", dirs.state())
            .env("CARDINAL_CONFIG_DIR", dirs.config())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let calls = started_jobs(dirs.state()).len() - jobs_before;
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
        "cd missing && probe one 2>&1 | cat || printf 'fallback\\n'",
        1,
        0,
    ),
    case("persisted_cwd", "", "cd sub && probe two", 1, 1),
    // A write above the work dir is reset between the two runs as well.
    case(
        "parent_dir_write",
        "",
        "cd ..; probe out >> out.log && cat out.log 2>&1",
        0,
        0,
    ),
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
    // zsh ignores errexit for the rest of an and-or list after an `if` that ran in the
    // calling shell, so a stage the list goes on after stays unwrapped (section 7.2).
    case(
        "errexit_after_and",
        "set -e",
        "probe --exit 0 'x y' && false",
        0,
        0,
    ),
    case(
        "errexit_after_or",
        "set -e",
        "probe --exit 3 || false; echo unreachable",
        0,
        0,
    ),
    case(
        "errexit_after_last_in_pipeline",
        "set -e",
        "printf 'x\\n' | probe --stdin && false",
        0,
        0,
    ),
    // A stage that feeds a pipe runs in a subshell, which keeps the flag to itself.
    case(
        "errexit_after_piped_stage",
        "set -e",
        "probe --exit 0 | cat && false; echo unreachable",
        1,
        1,
    ),
    // With `pipefail`, an earlier stage's status decides `errexit` for the last stage's
    // pipeline, which zsh no longer applies after the `if`.
    Case {
        pipefail: true,
        ..case(
            "errexit_pipefail_last_in_pipeline",
            "set -eo pipefail",
            "cat absent.log 2>&1 | probe --stdin; echo unreachable",
            0,
            0,
        )
    },
    Case {
        pipefail: true,
        ..case(
            "errexit_pipefail",
            "set -eo pipefail",
            "probe --exit 3 | cat || false; echo unreachable",
            1,
            1,
        )
    },
    case(
        "errexit_and_list",
        "set -e",
        "probe --exit 2 && echo yes; echo after",
        0,
        0,
    ),
    case(
        "errexit_or_list",
        "set -e",
        "probe --exit 2 || echo recovered; probe --exit 5; echo unreachable",
        1,
        1,
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
        "probe one | cat && probe two; probe three | cat",
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
    // The fallback test sets `$?` to 0 before the runner and to 1 before the fallback.
    case(
        "nested_status_stays",
        "",
        "false; probe \"${UNSET:-$?}\"; true; probe \"${UNSET:-${?}}\"",
        0,
        0,
    ),
    case("zsh_status_stays", "", "false; probe \"$status\"", 0, 0),
    case(
        "nested_last_argument_stays",
        "",
        "probe first last; probe \"${UNSET:-$_}\"",
        0,
        0,
    ),
    case(
        "pipeline_status_stays",
        "",
        "true | false; probe \"${PIPESTATUS[*]}${pipestatus[*]}\"",
        0,
        0,
    ),
    case("current_command_stays", "", "probe \"$BASH_COMMAND\"", 0, 0),
    // A literal integer subscript is plain syntax: the pipeline before it is still wrapped,
    // and the stage that reads the statuses stays unwrapped. sh (dash) has no arrays.
    Case {
        only: &["bash", "zsh"],
        ..case(
            "pipeline_status_element",
            "",
            "probe --exit 3 | cat; probe \"${pipestatus[1]}\" \"$pipestatus[1]\" \"${PIPESTATUS[0]}\"",
            1,
            1,
        )
    },
    case(
        "pipeline_status_element_stays",
        "",
        "false; probe \"${pipestatus[1]}\"",
        0,
        0,
    ),
    // The `-c` string holds the whole command, which the rewrite of any stage changes.
    case(
        "bash_execution_string",
        "",
        "echo \"$BASH_EXECUTION_STRING\" | wc -c; probe x",
        0,
        0,
    ),
    case(
        "zsh_execution_string",
        "",
        "echo \"$ZSH_EXECUTION_STRING\" | wc -c; probe x",
        0,
        0,
    ),
    // An operator can evaluate arithmetic that reads the fallback test's status (section
    // 7.2), so every expansion but a plain parameter refuses the command.
    case(
        "nested_arithmetic_pipestatus",
        "",
        "false; probe \"${UNSET:-$[PIPESTATUS[0]]}\"",
        0,
        0,
    ),
    case(
        "nested_arithmetic_status",
        "",
        "false; probe \"${UNSET:-$[status]}\"",
        0,
        0,
    ),
    case(
        "substring_offset_pipestatus",
        "text=abcdef",
        "false; probe \"${text:PIPESTATUS[0]}\"",
        0,
        0,
    ),
    // Plain parameters that keep their values inside the `if` still wrap.
    case(
        "plain_parameters",
        "",
        "set -- 'a b' c; probe \"$@\" $# $1 \"$*\" \"$-\" \"${2}\" \"$HOME:/x\"\nprobe $LINENO",
        2,
        2,
    ),
    case(
        "last_argument_stays",
        "",
        "probe first last; probe \"$_\"",
        1,
        1,
    ),
    case("builtin_stays", "", "command probe x", 0, 0),
    // Without a match the pattern vanishes and `cd sub` runs in the calling shell; wrapped,
    // the runner would run it as a program.
    Case {
        only: &["bash"],
        ..case(
            "nullglob_command_word",
            "shopt -s nullglob",
            "./absent*/probe cd sub; pwd",
            0,
            0,
        )
    },
    Case {
        only: &["zsh"],
        ..case(
            "zsh_nullglob_command_word",
            "setopt nullglob",
            "./absent*/probe cd sub; pwd",
            0,
            0,
        )
    },
    case(
        "function_from_rc",
        "greet() { printf 'hi %s\\n' \"$1\"; }",
        "greet you && probe x",
        1,
        1,
    ),
    // Definitions, traps, and tracing in the calling shell would see or change what a later
    // wrapped stage runs.
    case(
        "quoted_eval",
        "",
        "\"eval\" 'probe() { cd sub; }'; probe x; pwd",
        0,
        0,
    ),
    case(
        "escaped_eval",
        "",
        "\\eval 'probe() { cd sub; }'; probe x; pwd",
        0,
        0,
    ),
    case(
        "sourced_function",
        "printf 'probe() { cd sub; }\\n' > defs.sh",
        ". ./defs.sh; probe x; pwd",
        0,
        0,
    ),
    // Each ends its trap or tracing, which would otherwise race through the trailer's
    // pipeline.
    case(
        "debug_trap",
        "",
        "trap 'echo DEBUG' DEBUG; probe x; trap - DEBUG",
        0,
        0,
    ),
    case("xtrace", "", "set -x; probe x; set +x", 0, 0),
    Case {
        only: &["zsh"],
        ..case(
            "zsh_xtrace",
            "",
            "setopt xtrace; probe x; unsetopt xtrace",
            0,
            0,
        )
    },
    // Expansions of an earlier stage that write the function or command-path table, which
    // the shell consults for `probe` and the runner does not.
    case(
        "default_assigns_function",
        "",
        ": ${functions[probe]:=cd sub}; probe x; pwd",
        0,
        0,
    ),
    case(
        "arithmetic_assigns_function",
        "",
        ": \"${U:-$[functions[probe]=1]}\"; probe x",
        0,
        0,
    ),
    case(
        "subscript_assigns_function",
        "arr=(a b c)",
        ": \"${arr[functions[probe]=0]}\"; probe x",
        0,
        0,
    ),
    case(
        "unbraced_subscript_assigns_function",
        "arr=(a b c)",
        ": $arr[functions[probe]=0]; probe x",
        0,
        0,
    ),
    case(
        "default_assigns_hashed_path",
        "",
        ": \"${BASH_CMDS[probe]:=/bin/echo}\"; probe x",
        0,
        0,
    ),
    // Builtins of an earlier stage that write the same tables.
    case(
        "printf_assigns_function",
        "",
        "printf -v 'functions[probe]' 'cd sub'; probe x; pwd",
        0,
        0,
    ),
    case(
        "printf_assigns_hashed_path",
        "",
        "printf -v 'BASH_CMDS[probe]' /bin/echo; probe x",
        0,
        0,
    ),
    case(
        "read_assigns_function",
        "printf 'cd sub\\n' > defs.txt",
        "read 'functions[probe]' < defs.txt; probe x; pwd",
        0,
        0,
    ),
    case(
        "emulate_defines_function",
        "",
        "emulate zsh -c 'probe() { cd sub; }'; probe x; pwd",
        0,
        0,
    ),
    case(
        "set_assigns_functions",
        "",
        "set -A functions probe 'cd sub'; probe x; pwd",
        0,
        0,
    ),
    case(
        "printf_arithmetic_assigns_function",
        "",
        "printf '%d\\n' 'functions[probe]=0'; probe x",
        0,
        0,
    ),
    // The runner gets only the exported environment, without `PATH`; the shell still finds
    // `probe` in its own.
    case("unexported_path", "", "export -n PATH; probe x", 0, 0),
    case(
        "typeset_unexported_path",
        "",
        "typeset +x PATH; probe x",
        0,
        0,
    ),
    case(
        "declare_unexported_path",
        "",
        "declare +x PATH; probe x",
        0,
        0,
    ),
    // zsh searches `PATH` for `tools/probe`; the runner looks in the cwd.
    case("path_dirs", "", "setopt PATH_DIRS; tools/probe x", 0, 0),
    // Arguments of other commands are text, not assignments.
    case(
        "table_element_text",
        "",
        "probe -F 'functions[probe]=0' x",
        1,
        1,
    ),
    // A leading `~` expands to `$HOME`, here `eval`.
    case(
        "tilde_command_word",
        "",
        "HOME=eval; ~ 'probe() { cd sub; }'; probe x; pwd",
        0,
        0,
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
fn check(bench: &Bench, dirs: &Dirs, expect_calls: bool) {
    check_after(bench, dirs, expect_calls, || {});
}

/// [`check`], with `change` applied after the hook rewrote every case and before any runs.
fn check_after(bench: &Bench, dirs: &Dirs, expect_calls: bool, change: impl FnOnce()) {
    let shells = &bench.shells;
    let rewrites: Vec<String> = CASES
        .iter()
        .map(|case| bench.rewrite(dirs, case.command))
        .collect();
    change();
    for (case, rewritten) in CASES.iter().zip(rewrites) {
        for shell in shells.iter().filter(|s| {
            (case.only.is_empty() || case.only.contains(&s.name)) && (!case.pipefail || s.pipefail)
        }) {
            let (original, original_calls) = bench.run(shell, dirs, case.setup, case.command);
            let (outcome, calls) = bench.run(shell, dirs, case.setup, &rewritten);
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
        // After the runs, so that a rewrite that changes the meaning shows how.
        assert_eq!(
            rewritten.matches(" run --spec-b64 ").count(),
            case.wraps,
            "{}: {rewritten}",
            case.name
        );
    }
}

/// C6: with a runner in place, every classified stage runs through it and the command
/// keeps its meaning.
#[test]
fn runner_present() {
    let bench = Bench::new();
    check(&bench, &bench.present, true);
}

/// C5: with the runner path missing, not executable, or a directory, the fallback runs each
/// original stage, and the command keeps its meaning.
#[test]
fn runner_missing() {
    let bench = Bench::new();
    let rewritten = bench.rewrite(&bench.missing, "probe x");
    assert!(
        rewritten.contains("/absent/cardinal ]; then"),
        "{rewritten}"
    );
    check(&bench, &bench.missing, false);

    // A regular file without execute permission in place of the link; a script that would
    // change every outcome if it ran.
    let bench = Bench::new();
    std::fs::remove_file(&bench.runner).unwrap();
    executable(Path::new(&bench.runner), "#!/bin/sh\nexit 99\n");
    std::fs::set_permissions(
        &bench.runner,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o644),
    )
    .unwrap();
    check(&bench, &bench.present, false);

    // A directory in its place passes `-x` but not `-f`; the hook, which refuses a directory
    // runner, rewrote while the runner was the binary.
    let bench = Bench::new();
    check_after(&bench, &bench.present, false, || {
        std::fs::remove_file(&bench.runner).unwrap();
        std::fs::create_dir(&bench.runner).unwrap();
        std::fs::set_permissions(
            &bench.runner,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();
    });
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
        Just("set -e"),
        Just("strict"),
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

/// The owner's rc: `strict`, a function that turns `errexit` on, is defined in each setup
/// except the first.
fn setup() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just(""),
        Just("strict() { set -e; }"),
        Just("strict() { set -e; }\nset -e"),
        Just("strict() { set -e; }\nset -o errexit"),
        Just("strict() { set -e; }\nset -o pipefail"),
        Just("strict() { set -e; }\nset -eo pipefail"),
        Just("strict() { set -e; }\nX=outer")
    ]
}

/// Random lists of pipelines of classified and unclassified stages keep their meaning in
/// every shell. One bench serves every case, as it serves the fixed cases, because the tools in
/// `bin/` are new files, which macOS checks at their first start for seconds on a loaded host;
/// each case starts without a work dir, as on a fresh bench, and each run in a fresh one. The
/// configuration is the `proptest!` macro's: the environment's overrides and the test's name.
#[test]
fn generated_commands_keep_their_meaning() {
    let bench = Bench::new();
    let mut config = contextualize_config(ProptestConfig {
        cases: 48,
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    config.test_name = Some(concat!(
        module_path!(),
        "::",
        "generated_commands_keep_their_meaning"
    ));
    let mut runner = TestRunner::new(config);
    let inputs = (
        setup(),
        prop::collection::vec((pipeline(), separator()), 1..5),
    );
    let result = runner.run(&inputs, |(setup, parts)| {
        let mut command = String::new();
        for (i, (pipeline, sep)) in parts.iter().enumerate() {
            command.push_str(pipeline);
            if i + 1 < parts.len() {
                command.push_str(sep);
            }
        }
        bench.clear();
        let rewritten = bench.rewrite(&bench.present, &command);
        for shell in bench
            .shells
            .iter()
            .filter(|s| s.pipefail || !setup.contains("pipefail"))
        {
            let (original, _) = bench.run(shell, &bench.present, setup, &command);
            let (outcome, _) = bench.run(shell, &bench.present, setup, &rewritten);
            prop_assert_eq!(
                outcome,
                original,
                "{} in {}:\n{}\n{}",
                command,
                shell.name,
                rewritten,
                setup
            );
        }
        Ok(())
    });
    if let Err(err) = result {
        panic!("{err}");
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
        "if [ -f {runner} ] && [ -x {runner} ]; then {runner} run -- {stage}; else {stage}; fi | cat; cat out.log",
        runner = bench.runner
    );
    for shell in &bench.shells {
        let (as_written, _) = bench.run(shell, &bench.present, "", &original);
        let (as_wrapped, calls) = bench.run(shell, &bench.present, "", &wrapped);
        assert_eq!(calls, 1, "{}: the runner ran the stage", shell.name);
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
