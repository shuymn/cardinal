use std::path::Path;
use std::time::{Duration, Instant};

use proptest::prelude::{Just, Strategy, prop, prop_assert, prop_assert_eq, prop_oneof, proptest};
use proptest::test_runner::Config as ProptestConfig;

use super::{
    BuiltIn, CallIds, Classifier, Context, Refusal, Rewritten, Skip, Stage, StageOutcome,
    StageTrace, decide, normalize, rewrite, rules_could_match, tokenize, trace,
};
use crate::config::{Profile, Selection};
use crate::contract::{
    ClassifyDecision, ClassifyRequest, ClassifyResponse, ClassifyResponseSchema, ClassifyTool,
    ToolName,
};

fn stages(command: &str) -> Vec<Stage> {
    tokenize(command).unwrap_or_else(|err| panic!("{command:?} refused: {err}"))
}

fn spans(command: &str) -> Vec<&str> {
    stages(command)
        .iter()
        .map(|s| &command[s.span.clone()])
        .collect()
}

fn profile(name: &str, text: &str) -> Profile {
    let path = format!("{name}.toml");
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("repos").join(&path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, text).unwrap();
    let mut config =
        crate::config::Config::load_with(dir.path(), 4, Some(Path::new("/h"))).unwrap();
    config.profiles.remove(0)
}

const GENERIC: &str = r#"
[classify]
wrappers = [["npx"], ["pnpm", "exec"], ["mise", "exec", "--"]]
skip = ["shadowed"]
[[rule]]
id = "test"
commands = ["pnpm test", "vitest", "cargo test", "cargo * run"]
except = ["--watch"]
class = "test"
resources = { cpu = 4 }
[[rule]]
id = "build"
commands = ["cargo build", "shadowed", "make"]
class = "build"
"#;

fn outcomes(command: &str, profiles: &[Profile]) -> Vec<StageOutcome> {
    let selection = Selection::new(profiles, |_| false);
    let tool = ClassifyTool {
        name: ToolName::Bash,
        timeout_ms: 120_000,
        background: false,
    };
    let ctx = Context {
        command,
        cwd: "/w",
        repo: None,
        tool: &tool,
    };
    decide(
        stages(command),
        &ctx,
        &selection,
        &BuiltIn {
            selection: &selection,
        },
        Instant::now() + Duration::from_secs(10),
    )
}

fn skips(command: &str) -> Vec<Option<Skip>> {
    let profiles = [profile("generic", GENERIC)];
    outcomes(command, &profiles)
        .into_iter()
        .map(|o| o.skip)
        .collect()
}

#[test]
fn stages_of_the_documented_example() {
    let command = "cd packages/app && pnpm test 2>&1 | tail -50 || echo failed";
    let found = stages(command);
    assert_eq!(
        spans(command),
        [
            "cd packages/app",
            "pnpm test 2>&1",
            "tail -50",
            "echo failed"
        ]
    );
    assert_eq!(found[1].words, ["pnpm", "test"]);
    assert_eq!(
        found.iter().map(|s| s.pipeline).collect::<Vec<_>>(),
        [0, 1, 1, 2]
    );
    assert_eq!(found[1].command_start, Some(command.find("pnpm").unwrap()));
    assert!(found.iter().all(|s| !s.background && !s.shell_state));
}

#[test]
fn words_values_and_redirections() {
    let command = r#"2>err A=1 B+=x tool 'a b' "c $HOME" d\ e >out <in 3>&1 &>both"#;
    let stage = &stages(command)[0];
    assert_eq!(&command[stage.span.clone()], command);
    assert_eq!(
        stage.words,
        ["A=1", "B+=x", "tool", "'a b'", "\"c $HOME\"", "d\\ e"]
    );
    assert_eq!(
        stage.values,
        [
            Some("A=1".into()),
            Some("B+=x".into()),
            Some("tool".into()),
            Some("a b".into()),
            None,
            Some("d e".into())
        ]
    );
    assert_eq!(stage.assignments, 2);
    assert_eq!(stage.command_word, Some(2));
    assert_eq!(stage.redirected, [0, 1, 2, 3]);
    assert!(!stage.piped_in && !stage.piped_out);
    assert_eq!(stage.command_start, Some(command.find("tool").unwrap()));
}

#[test]
fn comments_blank_lines_and_trailing_separators() {
    assert_eq!(
        spans("a 1 # c && b\n\n  b 2 ;\nc&&\n d #e\n"),
        ["a 1", "b 2", "c", "d"]
    );
    assert_eq!(spans("c a#b"), ["c a#b"]);
    assert_eq!(spans("a;#c"), ["a"]);
    // Found by `any_input_gives_well_formed_stages`: an escaped blank ends the span.
    assert_eq!(spans("t a\\  # c"), ["t a\\ "]);
    assert_eq!(spans("> f"), ["> f"]);
    assert_eq!(stages("> f")[0].command_word, None);
}

#[test]
fn background_covers_the_whole_and_or_list() {
    let found = stages("a && b | c & d; e &");
    assert_eq!(
        found.iter().map(|s| s.background).collect::<Vec<_>>(),
        [true, true, true, false, true]
    );
}

#[test]
fn continued_marks_each_stage_of_a_pipeline_before_and_or() {
    let found = stages("a && b | c ||\n d; e | f & g");
    assert_eq!(
        found.iter().map(|s| s.continued).collect::<Vec<_>>(),
        [true, true, true, false, false, false, false]
    );
}

#[test]
fn shell_state_and_newlines_are_flagged() {
    for command in [
        "t $?",
        "t ${?}",
        "t \"$_\"",
        "t ${_}",
        "t $!",
        "t $$",
        "t $pipestatus",
        "t \"$PIPESTATUS\"",
        "t \"$status\"",
        "t ${status}",
        "t $BASH_COMMAND",
        "t \"$BASH_EXECUTION_STRING\" $ZSH_EXECUTION_STRING",
        "t $ERRNO",
        // A literal integer subscript is plain; the parameter still decides.
        "t ${PIPESTATUS[0]}",
        "t \"$pipestatus[1]\"",
        "t ${pipestatus[-1]}",
        "t $RANDOM",
        "t ${SECONDS}",
        "t $EPOCHREALTIME",
        "t > \"$?\"",
    ] {
        assert!(stages(command)[0].shell_state, "{command}");
    }
    for command in [
        "t $_x",
        "t ${x}",
        "t $#",
        "t ${#}",
        "t $1",
        "t ${10}",
        "t \"$@\" $*",
        "t $-",
        "t $LINENO",
        "t '$?'",
        "t \\$?",
        "t $statusx",
        "t $HOME:/x \"$PATH:$HOME\"",
        "t ${arr[0]} $x[1] \"${a[12]}\" $a[-1]",
    ] {
        assert!(!stages(command)[0].shell_state, "{command}");
    }
    assert!(stages("t 'a\nb'")[0].multiline);
    assert!(stages("t \"a\nb\"")[0].multiline);
    assert!(!stages("t a\nt b")[0].multiline);
}

/// Every expansion but a plain parameter refuses the whole command, in any stage, quoted or
/// not, nested or not: operators can read the fallback test's state
/// (`${UNSET:-$[PIPESTATUS[0]]}`) or assign in the calling shell
/// (`${functions[probe]:=cd sub}`), as can zsh's subscripts and modifiers on an unbraced
/// parameter.
#[test]
fn non_plain_expansions_refuse_the_command() {
    for command in [
        "false; probe \"${UNSET:-$[PIPESTATUS[0]]}\"",
        "false; probe \"${UNSET:-$[status]}\"",
        "false; probe \"${text:PIPESTATUS[0]}\"",
        ": ${functions[probe]:=cd sub}; probe x",
        ": \"${U:-$[functions[probe]=1]}\"; probe x",
        "echo \"${arr[i=3]}\" && make",
        "echo $arr[i=3] && make",
        "make \"$x[i]\"",
        "make ${x[i]}",
        "make ${x[1,2]} $x[1,2]",
        "make ${x[1]:-y}",
        "make ${x[+1]}",
        "make ${x[1a]}",
        "make ${x[]}",
        "make $x[-]",
        "make ${1[2]}",
        "make $x[1][2]",
        "make $x[1]:h",
        "make ${#x[1]}",
        "make $x:h",
        "make \"$x:u\"",
        "make $@[1] $1:t",
        "make $#name",
        "make $#1",
        "make ${#name}",
        "make ${x}${y:-z}",
        "make ${!name}",
        "make ${(e)x}",
        "make ${x@P}",
        "make \"${arr[@]}\"",
        "make ${HOME:-/tmp}",
        "make ${x/a/b}",
        "make ${a",
        "make $\"x\"",
        "make $=x",
        "make \"$~x\"",
        "make $^x $+x",
        "make $x\u{e9}",
        "X=${Y:-1} make",
        "make > \"${UNSET:-$?}\"",
    ] {
        assert!(
            matches!(tokenize(command), Err(Refusal::Expansion(_))),
            "{command:?}: {:?}",
            tokenize(command)
        );
    }
    assert_eq!(
        tokenize("make \"${UNSET:-${x}}\" y"),
        Err(Refusal::Expansion("${UNSET:-${x}}".into()))
    );
}

#[test]
fn refused_commands() {
    for command in [
        "cat <<EOF\nx\nEOF",
        "cat <<< x",
        "echo $(date)",
        "echo `date`",
        "echo \"$(date)\"",
        "echo $((1+2))",
        "diff <(a) <(b)",
        "tee >(a)",
        "(cd x && make)",
        "{ make; }",
        "echo {a,b}",
        "[[ -f x ]] && make",
        "! make",
        "make !",
        "if true; then make; fi",
        "while true; do make; done",
        "for x in a; do make; done",
        "case x in a) make;; esac",
        "f() { make; }",
        "function f { make; }",
        "eval make",
        "command eval make",
        "builtin -x alias ll=ls",
        "alias x=y",
        "echo 'unbalanced",
        "echo \"unbalanced",
        "echo ${a",
        "echo ${a:-\"x\"}",
        "echo $\"x\"",
        "echo $'a\\'b'",
        "echo $[1+2]",
        "make \\\n test",
        "make trailing\\",
        "a |& b",
        "a ;; b",
        "a >| f",
        "a <> f",
        "a >> & f",
        "a >",
        "a > # c",
        "a &&",
        "a |",
        "&& a",
        "; a",
        "a ; ; b",
        "a & ; b",
        "a && && b",
        "a | | b",
        "coproc make",
        "repeat 3 make",
    ] {
        assert!(tokenize(command).is_err(), "{command:?} was accepted");
    }
    assert_eq!(tokenize("make x | |"), Err(Refusal::EmptyStage("|")));
    // A command word with an expansion or a pattern can name any command, `eval` too, or
    // vanish (`nullglob`).
    for command in [
        "$TOOL test",
        "\"$X\" test",
        "make && ${CC}cc x",
        "./absent*/probe cd sub",
        "pro?be x",
        "x[ab] y",
        "^probe x",
        "a~b x",
        "command $x",
        "nice -n 5 x && command -p \"$y\"",
        // A leading `~` expands to `$HOME`, `$PWD` (`~+`), or `$OLDPWD` (`~-`).
        "~ x",
        "~/bin/probe x",
        "~+ x",
        "command ~- x",
    ] {
        assert!(
            matches!(tokenize(command), Err(Refusal::NotLiteral(_))),
            "{command:?}: {:?}",
            tokenize(command)
        );
    }
}

#[test]
fn accepted_commands() {
    for command in [
        "make && make test || echo failed; echo done",
        "a | b | c",
        "a &&\n b",
        "a |\n b",
        "echo \"a\\\"b\" 'c\\' d\\'e",
        "echo \"${HOME}/x\" ${a}b $a/y",
        "echo \"!\" '!' \\!",
        "echo \"$\" $ \"a$\"",
        "x=1; echo $x",
        "a 2>&1 >/dev/null 1>&2 0<in",
        "a >&2 <&0",
        "a &>f &>>g",
        "echo [x] x[1] 'if' \"then\"",
        "\"if\" true",
        "'pro*be' x \\? \"[a]\"",
        "[ -f x ] && probe *.txt [ab] ~/x a~b ^c ~",
        "'~/bin/probe' x",
        "echo if then fi done",
        "echo é 日本 ü",
        "",
        "   ",
        "# just a comment",
        "a &",
    ] {
        assert!(
            tokenize(command).is_ok(),
            "{command:?}: {:?}",
            tokenize(command)
        );
    }
}

fn normalized(command: &str, wrappers: &[&[&str]]) -> Vec<Vec<String>> {
    let wrappers: Vec<Vec<String>> = wrappers
        .iter()
        .map(|w| w.iter().map(|s| (*s).to_owned()).collect())
        .collect();
    normalize(&stages(command)[0], &wrappers)
}

#[test]
fn normalization_examples() {
    let wrappers: &[&[&str]] = &[&["npx"], &["pnpm", "exec"], &["mise", "exec", "--"]];
    for command in [
        "vitest",
        "pnpm exec vitest",
        "npx vitest",
        "npx --yes vitest",
        "env X=1 vitest",
        "env -u X Y=2 vitest",
        "nice vitest",
        "nice -n 5 vitest",
        "bash -c 'vitest'",
        "sh -ec vitest",
        "zsh -l -c \"vitest\"",
        "/usr/bin/env X=1 ./node_modules/.bin/vitest",
        "A=1 vitest",
        "mise exec -- vitest",
        "env nice npx vitest",
    ] {
        assert_eq!(normalized(command, wrappers), [["vitest"]], "{command}");
    }
    assert_eq!(normalized("cargo +nightly test", &[]), [["cargo", "test"]]);
    assert_eq!(
        normalized("cargo test \"$X\"", &[]),
        [["cargo", "test", "\"$X\""]]
    );
    assert_eq!(
        normalized("bash -lc 'cd x && cargo test'", &[]),
        [vec!["cd", "x"], vec!["cargo", "test"]]
    );
    assert_eq!(normalized("bash script.sh", &[]), [["bash", "script.sh"]]);
    assert_eq!(
        normalized("bash -x -c make", &[]),
        [["bash", "-x", "-c", "make"]]
    );
    for unclassifiable in [
        "env -i vitest",
        "env --",
        "nice -5 vitest",
        "bash -c \"$CMD\"",
        "bash -c 'echo $(x)'",
        "bash -c 'bash -c vitest'",
        "A=1",
    ] {
        let found = normalized(unclassifiable, wrappers);
        assert!(
            found.is_empty() || found == [["bash", "-c", "vitest"]],
            "{unclassifiable}: {found:?}"
        );
    }
    assert_eq!(normalized("npx", &[&["npx"]]), [["npx"]]);
}

#[test]
fn rule_matching_and_precedence() {
    let app = profile(
        "app",
        "[match]\nremotes = [\"github.com/o/app\"]\n[[rule]]\nid = \"t\"\ncommands = [\"cargo test\"]\nexcept = [\"--watch\"]\nclass = \"app-test\"",
    );
    let generic = profile("generic", GENERIC);
    let profiles = [app, generic];
    let selection = Selection::new(&profiles, |_| true);
    let tool = ClassifyTool {
        name: ToolName::Bash,
        timeout_ms: 1,
        background: false,
    };
    let command = "cargo test && cargo x run && cargo test --watch && make";
    let found = decide(
        stages(command),
        &Context {
            command,
            cwd: "/w",
            repo: None,
            tool: &tool,
        },
        &selection,
        &BuiltIn {
            selection: &selection,
        },
        Instant::now() + Duration::from_secs(10),
    );
    let rules: Vec<Option<&str>> = found
        .iter()
        .map(|o| o.response.as_ref().and_then(|r| r.rule.as_deref()))
        .collect();
    assert_eq!(
        rules,
        [
            Some("app/t"),
            Some("generic/test"),
            None,
            Some("generic/build")
        ]
    );
    assert_eq!(
        found[0].response.as_ref().unwrap().class.as_deref(),
        Some("app-test")
    );
    assert_eq!(
        found[2].skip,
        Some(Skip::Unclassified),
        "except cancels the match"
    );
    assert!(found[3].response.as_ref().unwrap().resources.is_none());

    let expired = BuiltIn {
        selection: &selection,
    };
    let req = crate::contract::ClassifyRequest {
        schema: crate::contract::ClassifyRequestSchema,
        command: "make".into(),
        stage: "make".into(),
        argv: vec!["make".into()],
        cwd: "/w".into(),
        repo: None,
        tool,
    };
    let response = super::Classifier::classify(&expired, &req, Instant::now());
    assert_eq!(response.decision, ClassifyDecision::Pass);
}

#[test]
fn unwrapped_stages() {
    use Skip::{
        Assignments, Background, Errexit, Multiline, NoCommand, PipedRedirect, SharedPipeline,
        ShellCommand, ShellState, Skipped, Sleep, Unclassified,
    };
    let cases: &[(&str, &[Option<Skip>])] = &[
        ("pnpm test", &[None]),
        (
            "cd x && pnpm test 2>&1 | tail",
            &[Some(ShellCommand), None, Some(Unclassified)],
        ),
        ("sleep 1 && make", &[Some(Sleep), None]),
        ("/bin/sleep 1", &[Some(Sleep)]),
        (
            "command make; exec make; time make; noglob make",
            &[Some(ShellCommand); 4],
        ),
        ("\"cd\" x", &[Some(ShellCommand)]),
        ("shadowed", &[Some(Skipped)]),
        ("npx shadowed", &[Some(Skipped)]),
        ("make $?", &[Some(ShellState)]),
        ("CI=1 make", &[Some(Assignments)]),
        ("make &", &[Some(Background)]),
        (
            "make && vitest & wait",
            &[Some(Background), Some(Background), Some(ShellCommand)],
        ),
        ("make 'a\nb'", &[Some(Multiline)]),
        ("make | vitest", &[Some(SharedPipeline), Some(Errexit)]),
        (
            "make | CI=1 vitest",
            &[Some(SharedPipeline), Some(Assignments)],
        ),
        (
            "make | tail && vitest | tail",
            &[None, Some(Unclassified), None, Some(Unclassified)],
        ),
        ("X=1", &[Some(NoCommand)]),
        ("> out", &[Some(NoCommand)]),
        ("npx vitest --watch", &[Some(Unclassified)]),
        (
            "make > f | tail",
            &[Some(PipedRedirect), Some(Unclassified)],
        ),
        (
            "make &> f | tail",
            &[Some(PipedRedirect), Some(Unclassified)],
        ),
        (
            "make >&2 | tail",
            &[Some(PipedRedirect), Some(Unclassified)],
        ),
        (
            "tail | make < f",
            &[Some(Unclassified), Some(PipedRedirect)],
        ),
        (
            "tail |\n make 0<f",
            &[Some(Unclassified), Some(PipedRedirect)],
        ),
        ("make 2>&1 3>f | tail", &[None, Some(Unclassified)]),
        ("make < f | tail", &[None, Some(Unclassified)]),
        ("tail | make > f", &[Some(Unclassified), Some(Errexit)]),
        ("tail && make > f", &[Some(Unclassified), None]),
        // Once an `if` has run in the calling shell, zsh ignores `errexit` for the rest of its
        // and-or list and for the status `pipefail` takes from earlier stages; a stage that
        // feeds a pipe runs in a subshell.
        ("make && echo ok", &[Some(Errexit), Some(ShellCommand)]),
        (
            "make ||\n echo failed",
            &[Some(Errexit), Some(ShellCommand)],
        ),
        (
            "tail | make && echo ok",
            &[Some(Unclassified), Some(Errexit), Some(ShellCommand)],
        ),
        ("tail | make", &[Some(Unclassified), Some(Errexit)]),
        (
            "make | tail || echo failed",
            &[None, Some(Unclassified), Some(ShellCommand)],
        ),
        ("echo a && make; make", &[Some(ShellCommand), None, None]),
        // A literal integer subscript leaves the decision to the parameter.
        (
            "make | tail; make \"${PIPESTATUS[0]}\" $pipestatus[1]",
            &[None, Some(Unclassified), Some(ShellState)],
        ),
        ("make ${arr[0]} $x[-1]", &[None]),
    ];
    for (command, expected) in cases {
        assert_eq!(skips(command), *expected, "{command}");
    }
    let found = &outcomes("2>e npx vitest", &[profile("generic", GENERIC)])[0];
    assert_eq!(found.argv.as_deref(), Some(&["vitest".to_owned()][..]));
    assert!(found.wrap());
}

#[test]
fn rules_could_match_needs_a_matching_rule() {
    let profiles = [profile("generic", GENERIC)];
    assert!(rules_could_match(&stages("cd x && npx vitest"), &profiles));
    assert!(!rules_could_match(&stages("cd x && ls | tail"), &profiles));
    assert!(!rules_could_match(&stages("echo make"), &profiles));
    // A wrapper of another profile counts too.
    let other = profile("other", "[classify]\nwrappers = [[\"bunx\"]]");
    assert!(rules_could_match(
        &stages("bunx vitest"),
        &[profiles[0].clone(), other]
    ));
}

// Property tests. Failures are not persisted (the test run must not write into the
// repository); a counterexample is reported in the test output and kept as a unit test.

fn config() -> ProptestConfig {
    ProptestConfig {
        cases: 512,
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

/// A word part: its text and its value after quote removal (`None` for an expansion).
fn part() -> impl Strategy<Value = (String, Option<String>)> {
    prop_oneof![
        "[A-Za-z0-9._/,:@%+=-]{1,6}".prop_map(|s| (s.clone(), Some(s))),
        "[a-z]{0,2}[*?][a-z]{0,2}".prop_map(|s| (s, None)),
        "[^']{0,8}".prop_map(|s| (format!("'{s}'"), Some(s))),
        "[^\"\\\\$`]{0,8}".prop_map(|s| (format!("\"{s}\""), Some(s))),
        "[ ;|&<>(){}#!'\"$`\\\\a]".prop_map(|c| (format!("\\{c}"), Some(c))),
        "[A-Za-z_][A-Za-z0-9_]{0,4}".prop_map(|n| (format!("${{{n}}}"), None)),
        "[A-Za-z_][A-Za-z0-9_]{0,4}".prop_map(|n| (format!("\"${n}\""), None)),
        "[A-Za-z_][A-Za-z0-9_]{0,4}".prop_map(|n| (format!("\"${{{n}}}x\""), None)),
    ]
}

fn word() -> impl Strategy<Value = (String, Option<String>)> {
    prop::collection::vec(part(), 1..4).prop_map(|parts| {
        let text: String = parts.iter().map(|(t, _)| t.as_str()).collect();
        let value = parts
            .iter()
            .map(|(_, v)| v.clone())
            .collect::<Option<String>>();
        (text, value)
    })
}

fn redirect() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(">out".to_owned()),
        Just(">> out".to_owned()),
        Just("2>&1".to_owned()),
        Just("< in".to_owned()),
        Just("2> err".to_owned()),
        Just("&> both".to_owned()),
        Just(">&2".to_owned()),
        Just("3>'f g'".to_owned()),
    ]
}

/// A generated stage: its text, its words as written, and their values.
#[derive(Clone, Debug)]
struct GenStage {
    text: String,
    words: Vec<String>,
    values: Vec<Option<String>>,
    assignments: usize,
}

fn stage() -> impl Strategy<Value = GenStage> {
    (
        prop::collection::vec("[A-Z]{1,3}=[a-z0-9]{0,3}", 0..2),
        "t_[a-z0-9]{0,4}",
        prop::collection::vec(
            prop_oneof![
                3 => word().prop_map(Ok),
                1 => redirect().prop_map(Err),
            ],
            0..5,
        ),
    )
        .prop_map(|(assignments, command, rest)| {
            let mut text = Vec::new();
            let mut words = Vec::new();
            let mut values = Vec::new();
            for a in &assignments {
                text.push(a.clone());
                words.push(a.clone());
                values.push(Some(a.clone()));
            }
            text.push(command.clone());
            words.push(command.clone());
            values.push(Some(command));
            for item in rest {
                match item {
                    Ok((t, v)) => {
                        text.push(t.clone());
                        words.push(t);
                        values.push(v);
                    }
                    Err(r) => text.push(r),
                }
            }
            GenStage {
                text: text.join(" "),
                words,
                values,
                assignments: assignments.len(),
            }
        })
}

fn separator() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just(" && "),
        Just("&&"),
        Just(" || "),
        Just("||"),
        Just(" ; "),
        Just(";"),
        Just(" | "),
        Just("|"),
        Just("\n"),
        Just(" &\n"),
        Just(" & "),
        Just(" # comment\n"),
        Just(" &&\n "),
    ]
}

fn command() -> impl Strategy<Value = (String, Vec<GenStage>, Vec<&'static str>)> {
    (
        prop::collection::vec((stage(), separator()), 1..5),
        prop_oneof![Just(""), Just(" "), Just(";"), Just(" &"), Just(" # end")],
    )
        .prop_map(|(items, end)| {
            let mut text = String::new();
            let mut stages = Vec::new();
            let mut seps = Vec::new();
            let last = items.len() - 1;
            for (i, (stage, sep)) in items.into_iter().enumerate() {
                text.push_str(&stage.text);
                stages.push(stage);
                if i == last {
                    text.push_str(end);
                    seps.push(end);
                } else {
                    text.push_str(sep);
                    seps.push(sep);
                }
            }
            (text, stages, seps)
        })
}

fn is_async(sep: &str) -> bool {
    sep.trim_start().starts_with('&') && !sep.trim_start().starts_with("&&")
}

proptest! {
    #![proptest_config(config())]

    /// Arbitrary input never panics, and accepted input yields well-formed spans.
    #[test]
    fn any_input_gives_well_formed_stages(input in "\\PC{0,40}|[ a-z;|&<>()$`'\"\\\\#!{}\n]{0,40}") {
        if let Ok(found) = tokenize(&input) {
            let mut end = 0;
            for (i, stage) in found.iter().enumerate() {
                prop_assert_eq!(stage.index as usize, i);
                prop_assert!(stage.span.start >= end && stage.span.end <= input.len());
                prop_assert!(input.is_char_boundary(stage.span.start));
                prop_assert!(input.is_char_boundary(stage.span.end));
                let text = &input[stage.span.clone()];
                // A span starts at a word or redirection and ends with one; it ends in a
                // blank only when the blank is escaped.
                prop_assert!(!text.is_empty() && !text.starts_with([' ', '\t', '\n']));
                if text.ends_with([' ', '\t']) {
                    prop_assert!(text[..text.len() - 1].ends_with('\\'));
                }
                prop_assert_eq!(stage.words.len(), stage.values.len());
                if let Some(start) = stage.command_start {
                    prop_assert!(stage.span.contains(&start));
                }
                end = stage.span.end;
            }
        }
    }

    /// Generated commands of the subset split into exactly the generated stages, with
    /// their words, values, pipelines, and asynchronous lists.
    #[test]
    fn generated_commands_round_trip((text, generated, seps) in command()) {
        let found = tokenize(&text).map_err(|e| proptest::test_runner::TestCaseError::fail(format!("{text:?}: {e}")))?;
        prop_assert_eq!(found.len(), generated.len(), "{:?}", text);
        let mut pipeline = 0;
        let mut list_start = 0;
        let mut background = vec![false; generated.len()];
        for (i, (stage, expected)) in found.iter().zip(&generated).enumerate() {
            prop_assert_eq!(&text[stage.span.clone()], expected.text.as_str());
            prop_assert_eq!(&stage.words, &expected.words);
            prop_assert_eq!(&stage.values, &expected.values);
            prop_assert_eq!(stage.assignments, expected.assignments);
            prop_assert_eq!(stage.command_word, Some(expected.assignments));
            prop_assert_eq!(stage.pipeline, pipeline, "{:?}", text);
            let sep = seps[i].trim_start_matches(' ');
            if is_async(seps[i]) {
                for flag in &mut background[list_start..=i] {
                    *flag = true;
                }
            }
            if !sep.starts_with('|') || sep.starts_with("||") {
                pipeline += 1;
            }
            if is_async(seps[i]) || sep.starts_with(';') || sep.starts_with('\n') || sep.starts_with('#') {
                list_start = i + 1;
            }
        }
        let flags: Vec<bool> = found.iter().map(|s| s.background).collect();
        prop_assert_eq!(flags, background, "{:?}", text);
    }

    /// Putting a construct outside the subset in unquoted position refuses the whole
    /// command.
    #[test]
    fn unsupported_constructs_refuse_the_command(
        (text, generated, _) in command(),
        construct in prop_oneof![
            Just("$(x)"), Just("`x`"), Just("(x)"), Just("{x}"), Just("!x"), Just("<<E"),
            Just("<(x)"), Just("[[x"), Just("'open"), Just("\"open"), Just("$'x'"), Just("|&"),
        ],
        at in 0_usize..8,
    ) {
        let found = tokenize(&text).map_err(|e| proptest::test_runner::TestCaseError::fail(e.to_string()))?;
        // An opening quote would pair with a later quote, so it goes after the last stage.
        let stage = if construct.contains(['\'', '"']) { found.len() - 1 } else { at % generated.len() };
        let insert = found[stage].span.end;
        let broken = format!("{} {construct}{}", &text[..insert], &text[insert..]);
        prop_assert!(tokenize(&broken).is_err(), "{:?} was accepted", broken);
    }
}

#[test]
fn rewrite_wraps_the_decided_stages_with_their_specs() {
    use super::{CallIds, Rewritten, rewrite};
    use crate::contract::RunSpec;

    let profiles = [profile("generic", GENERIC)];
    let tool = ClassifyTool {
        name: ToolName::Bash,
        timeout_ms: 7000,
        background: true,
    };
    let default = [("cpu".to_owned(), std::num::NonZeroU64::new(2).unwrap())]
        .into_iter()
        .collect();
    let ids = CallIds {
        session_id: Some("s"),
        agent_id: None,
        tool_use_id: Some("t"),
    };
    let rewrite_of = |command: &str| {
        let ctx = Context {
            command,
            cwd: "/w",
            repo: None,
            tool: &tool,
        };
        rewrite(&ctx, &outcomes(command, &profiles), "/r", &default, &ids)
    };

    let Rewritten::Command(text) = rewrite_of("cd x && npx vitest run | tail; make") else {
        panic!("nothing wrapped");
    };
    let specs: Vec<RunSpec> = text
        .split("--spec-b64 ")
        .skip(1)
        .map(|rest| RunSpec::decode_b64(rest.split(' ').next().unwrap()).unwrap())
        .collect();
    assert!(
        text.starts_with("cd x && if [ -f /r ] && [ -x /r ]; then /r run --spec-b64 "),
        "{text}"
    );
    assert!(
        text.contains(
            " -- npx vitest run; else npx vitest run; fi | tail; if [ -f /r ] && [ -x /r ]"
        ),
        "{text}"
    );
    assert_eq!(specs.len(), 2);
    assert_eq!(
        (
            specs[0].stage_index,
            specs[0].stage.as_str(),
            specs[0].rule.as_deref()
        ),
        (1, "npx vitest run", Some("generic/test"))
    );
    assert_eq!(specs[0].resources.get("cpu").map(|n| n.get()), Some(4));
    assert_eq!(
        specs[1].resources.get("cpu").map(|n| n.get()),
        Some(2),
        "default resources"
    );
    assert!(specs.iter().all(|s| s.timeout_ms == 7000 && s.background));
    assert!(
        specs
            .iter()
            .all(|s| s.session_id.as_deref() == Some("s") && s.agent_id.is_none())
    );

    assert_eq!(
        rewrite_of("cd x && CI=1 make | tail"),
        Rewritten::Unchanged(vec![
            Skip::ShellCommand,
            Skip::Assignments,
            Skip::Unclassified
        ])
    );
    assert_eq!(rewrite_of(""), Rewritten::Unchanged(vec![]));
}

const APP_TEST: &str = "[match]\nremotes = [\"github.com/o/app\"]\n[[rule]]\nid = \"t\"\ncommands = [\"cargo build\", \"cargo * test\", \"cargo test\"]\nclass = \"app-test\"";

const TRACED: &str = "cd x && npx cargo +nightly test --all | tail -3; make; CI=1 vitest; sleep 1";

fn trace_tool() -> ClassifyTool {
    ClassifyTool {
        name: ToolName::Bash,
        timeout_ms: 1000,
        background: false,
    }
}

/// The outcomes of `command` under the app and generic profiles, both selected, and their
/// trace; `classifier` replaces the built-in one.
fn traced(
    command: &str,
    classifier: Option<&dyn Classifier>,
) -> (Vec<StageOutcome>, Vec<StageTrace>) {
    let profiles = [profile("app", APP_TEST), profile("generic", GENERIC)];
    let selection = Selection::new(&profiles, |_| true);
    let tool = trace_tool();
    let ctx = Context {
        command,
        cwd: "/w",
        repo: None,
        tool: &tool,
    };
    let built_in = BuiltIn {
        selection: &selection,
    };
    let decided = decide(
        stages(command),
        &ctx,
        &selection,
        classifier.unwrap_or(&built_in),
        Instant::now() + Duration::from_secs(10),
    );
    let found = trace(command, &decided, &selection);
    (decided, found)
}

#[test]
fn trace_reports_argv_rule_pattern_and_skip() {
    let (_, found) = traced(TRACED, None);
    let summary: Vec<_> = found
        .iter()
        .map(|t| {
            (
                t.index,
                t.stage.as_str(),
                t.argv.as_ref().map(|argv| argv.join(" ")),
                t.matched
                    .as_ref()
                    .map(|m| (m.profile.as_str(), m.rule.as_str(), m.pattern.as_str())),
                t.skip,
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (0, "cd x", None, None, Some(Skip::ShellCommand)),
            (
                1,
                "npx cargo +nightly test --all",
                Some("cargo test --all".into()),
                Some(("app", "app/t", "cargo test")),
                None
            ),
            (
                2,
                "tail -3",
                Some("tail -3".into()),
                None,
                Some(Skip::Unclassified)
            ),
            (
                3,
                "make",
                Some("make".into()),
                Some(("generic", "generic/build", "make")),
                None
            ),
            (
                4,
                "CI=1 vitest",
                Some("vitest".into()),
                Some(("generic", "generic/test", "vitest")),
                Some(Skip::Assignments)
            ),
            (5, "sleep 1", None, None, Some(Skip::Sleep)),
        ]
    );
    assert!(
        found[1]
            .matched
            .as_ref()
            .unwrap()
            .file
            .ends_with("app.toml")
    );
    assert_eq!(
        found[1].response.as_ref().unwrap().class.as_deref(),
        Some("app-test")
    );
}

/// The stages the trace leaves unskipped are the ones the rewrite wraps, by the same rule.
#[test]
fn trace_unskipped_stages_are_the_wrapped_ones() {
    use crate::contract::{Resources, RunSpec};

    let (decided, found) = traced(TRACED, None);
    let tool = trace_tool();
    let ctx = Context {
        command: TRACED,
        cwd: "/w",
        repo: None,
        tool: &tool,
    };
    let ids = CallIds {
        session_id: None,
        agent_id: None,
        tool_use_id: None,
    };
    let Rewritten::Command(text) = rewrite(&ctx, &decided, "/r", &Resources::default(), &ids)
    else {
        panic!("nothing wrapped");
    };
    let wrapped: Vec<(u32, Option<String>)> = text
        .split("--spec-b64 ")
        .skip(1)
        .map(|rest| RunSpec::decode_b64(rest.split(' ').next().unwrap()).unwrap())
        .map(|spec| (spec.stage_index, spec.rule))
        .collect();
    let unskipped: Vec<(u32, Option<String>)> = found
        .iter()
        .filter(|t| t.skip.is_none())
        .map(|t| (t.index, t.matched.as_ref().map(|m| m.rule.clone())))
        .collect();
    assert_eq!(wrapped, unskipped);
}

/// A classifier that wraps everything under a rule no profile holds.
struct Unit;

impl Classifier for Unit {
    fn classify(&self, _: &ClassifyRequest, _: Instant) -> ClassifyResponse {
        ClassifyResponse {
            schema: ClassifyResponseSchema,
            decision: ClassifyDecision::Wrap,
            class: Some("unit".into()),
            rule: Some("unit/x".into()),
            resources: None,
            reason: "unit".into(),
        }
    }
}

/// A rule named by another classifier has no profile pattern behind it.
#[test]
fn trace_of_another_classifier_has_no_profile_rule() {
    let (_, found) = traced("make", Some(&Unit));
    assert_eq!(found[0].matched, None);
    assert_eq!(
        found[0].response.as_ref().unwrap().rule.as_deref(),
        Some("unit/x")
    );
    assert_eq!(found[0].skip, None);
}

/// The name of a table element in an argument of a command other than a builtin is text, so
/// the stage is classified and wrapped as any other.
#[test]
fn table_element_text_in_arguments_still_wraps() {
    let profiles = [profile(
        "generic",
        "[[rule]]\nid = \"g\"\ncommands = [\"grep\"]\nclass = \"c\"\n",
    )];
    let found = outcomes("grep -F 'functions[' code.txt", &profiles);
    assert_eq!(found.len(), 1);
    assert!(found[0].wrap(), "{found:?}");
}

/// Pattern characters that are quoted or escaped leave the command word a literal.
#[test]
fn quoted_pattern_characters_still_wrap() {
    let profiles = [profile(
        "generic",
        "[[rule]]\nid = \"p\"\ncommands = [\"pro*be\"]\nclass = \"c\"\n",
    )];
    let found = outcomes("'pro*be' x; \"pro*be\" y; pro\\*be z", &profiles);
    assert_eq!(found.len(), 3);
    assert!(found.iter().all(StageOutcome::wrap), "{found:?}");
}

/// Builtins that can define, remove, or redirect commands in the calling shell, source code
/// into it, set traps, or trace it refuse the whole command, after quote removal and behind
/// precommand modifiers; the same names as arguments, and harmless options, do not.
#[test]
fn caller_changing_builtins_refuse_the_command() {
    for command in [
        "eval make",
        "\"eval\" 'probe() { cd sub; }'; probe",
        "\\eval x",
        "'ev'al x",
        "command \"eval\" x",
        "builtin -x \\alias ll=ls",
        "noglob eval x",
        "- eval x",
        "make && . ./defs.sh",
        "source defs",
        "alias x=y",
        "unalias x",
        "unset -f probe",
        "unset X",
        "functions -c a b",
        "unfunction probe",
        "autoload -U compinit",
        "enable -f ./lib.so x",
        "disable probe",
        "hash -p ./x probe",
        "unhash probe",
        "typeset -f probe",
        "declare -fx probe",
        "export -f probe",
        "readonly -f probe",
        "local -f probe",
        "declare \"$opts\" x",
        "typeset 'functions[probe]=cd sub'",
        "functions[probe]='cd sub'",
        "aliases[probe]=x make",
        "BASH_ALIASES[probe]='cd sub'",
        "BASH_CMDS[probe]=/x",
        "commands[probe]=/x",
        "options[xtrace]=on",
        "trap 'echo D' DEBUG",
        "trap",
        "set -x",
        "set -xv",
        "set -euxo pipefail",
        "set +x",
        "set -T",
        "set -o xtrace",
        "set -eo verbose",
        "set -o functrace",
        "set -o \"$opt\"",
        "set $opts",
        "setopt xtrace",
        "setopt XTRACE",
        "setopt x_trace",
        "unsetopt noxtrace",
        "setopt -x",
        "setopt \"$opt\"",
        "shopt -s extdebug",
    ] {
        assert!(
            matches!(tokenize(command), Err(Refusal::ChangesShell(_))),
            "{command:?}: {:?}",
            tokenize(command)
        );
    }
    for command in [
        "set -e",
        "set -euo pipefail",
        "set -o pipefail",
        "set -- -x \"$@\"",
        "setopt nullglob",
        "unsetopt nomatch",
        "shopt -s nullglob",
        "export FOO=$x PATH=\"$PATH:/x\"",
        "declare -a arr",
        "typeset -i n=1",
        "local x=1",
        "readonly X=1",
        "echo eval trap source",
        "probe trap 'set -x'",
        "X=1 make",
    ] {
        assert!(
            tokenize(command).is_ok(),
            "{command:?}: {:?}",
            tokenize(command)
        );
    }
}

/// Builtins, assignments, and words that can write the shells' tables of functions, aliases,
/// command paths, or options, or a name that is not a plain identifier (a subscript is
/// arithmetic, which can assign anything), refuse the whole command; the same builtins with
/// plain names, and table names that are not elements, do not.
#[test]
fn table_writers_refuse_the_command() {
    for command in [
        "printf -v 'functions[probe]' 'cd sub'",
        "printf -vfunctions x",
        "printf \"$fmt\" \"$name\" x",
        "print -v 'functions[probe]' x",
        "print -u 2 -v \"$n\" x",
        "read 'functions[probe]' < defs",
        "read -A functions",
        "read \"$name\"",
        "getopts ab 'aliases[x]'",
        "emulate zsh -c 'probe() { cd sub; }'",
        "emulate -c x",
        "emulate sh -o xtrace",
        "emulate \"$mode\"",
        "set -A functions probe 'cd sub'",
        "typeset -- 'functions[probe]=cd sub'",
        "typeset 'x[1]'",
        "declare 'x[1]'=v",
        "local functions",
        "declare -n r='BASH_ALIASES[probe]'",
        "let 'a[1]=2'",
        "let \"$expr\"",
        "mapfile -C cb lines < f",
        "readarray lines < f",
        "vared x",
        "zparseopts -A opts a",
        "arr[i]=x make",
        "arr[functions[probe]=1]=x",
        "printf '%d' 'functions[probe]=0'",
        "shift 'functions[probe]=0'",
        "integer n=\"aliases[x]\"",
        "echo a && return BASH_ALIASES[x]=1",
        // An integer variable's value is arithmetic.
        "x='functions[probe]=0'",
        "n='functions[probe]=0' make",
        "typeset -i n='functions[probe]=0'",
        // So are numeric arguments of zsh's builtins, behind modifiers too.
        "[ 'functions[probe]=0' -eq 0 ]",
        "exit 'functions[probe]=0'",
        "ulimit 'functions[probe]=0'",
        "command printf %d 'functions[probe]=0'",
        "builtin shift 'aliases[x]=0'",
    ] {
        assert!(
            matches!(tokenize(command), Err(Refusal::ChangesShell(_))),
            "{command:?}: {:?}",
            tokenize(command)
        );
    }
    for command in [
        "printf '%s\\n' \"$x\" \"$y\"",
        "printf -v out '%s' x",
        "printf -- -v x",
        "print -rn -- x",
        "print -u2 -v out x",
        "read -r line",
        "read -r 'line?Prompt: '",
        "getopts ab: opt",
        "emulate",
        "emulate -L zsh",
        "emulate -LR sh",
        "set -A arr a b",
        "export -n FOO",
        "let i++",
        "echo functions commands my_options[1] subcommands[2]",
        // Other commands get their arguments as text.
        "grep -F 'functions[' code.txt",
        "make 'functions[probe]=0' && rg 'BASH_CMDS[x]'",
        "command grep 'aliases[x]' f",
        "nice -n 5 make CFLAGS='functions[x]'",
    ] {
        assert!(
            tokenize(command).is_ok(),
            "{command:?}: {:?}",
            tokenize(command)
        );
    }
}
