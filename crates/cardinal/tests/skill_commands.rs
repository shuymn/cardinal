//! Every `cardinal …` command in the arbiter skill parses with the real CLI
//! definition and stays inside the arbiter's reach (architecture §14.3).

use std::collections::BTreeSet;

use cardinal::cli::Cli;
use clap::Parser;

const SKILL: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../integrations/claude-code/skills/cardinal-arbiter/SKILL.md"
));

/// Subcommands the arbiter may run.
const ALLOWED: [&str; 5] = ["inventory", "inbox", "explain", "plan", "act"];

/// Commands from fenced code lines and inline code spans that start with `cardinal `.
fn skill_commands(text: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            if trimmed.starts_with("cardinal ") {
                commands.push(trimmed.to_owned());
            }
            continue;
        }
        for (i, span) in line.split('`').enumerate() {
            if i % 2 == 1 && span.starts_with("cardinal ") {
                commands.push(span.to_owned());
            }
        }
    }
    commands
}

/// Splits a command into words with POSIX quoting. Anything a shell would
/// expand or treat as an operator is rejected, so a command only passes when
/// its words are exactly what the shell would hand to `cardinal`.
fn shell_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => panic!("unbalanced single quote in {command:?}"),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some(c @ ('\\' | '$' | '`')) => {
                            panic!("unsupported {c:?} inside double quotes in {command:?}")
                        }
                        Some(c) => word.push(c),
                        None => panic!("unbalanced double quote in {command:?}"),
                    }
                }
            }
            '\\' | '$' | '`' | '|' | '&' | ';' | '<' | '>' | '(' | ')' | '*' | '?' | '[' | '#'
            | '~' => panic!("unsupported shell character {c:?} in {command:?}"),
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

#[test]
fn skill_commands_parse_with_the_cli() {
    let commands = skill_commands(SKILL);
    let mut used = BTreeSet::new();
    for command in &commands {
        let words = shell_words(command);
        if let Err(err) = Cli::try_parse_from(&words) {
            panic!("skill command does not parse: {command}\n{err}");
        }
        let sub = words.get(1).map_or("", String::as_str);
        assert!(
            ALLOWED.contains(&sub),
            "skill runs `cardinal {sub}`, outside the arbiter's commands: {command}"
        );
        assert!(
            !words.iter().any(|w| w.starts_with("--owner-approved")),
            "skill passes --owner-approved: {command}"
        );
        if sub == "act" {
            assert!(
                words.windows(2).any(|w| w == ["--caller", "arbiter"]),
                "act without --caller arbiter: {command}"
            );
        }
        used.insert(sub.to_owned());
    }
    let missing: Vec<_> = ALLOWED.iter().filter(|s| !used.contains(**s)).collect();
    assert!(
        missing.is_empty(),
        "no skill command found for {missing:?}; extraction found {commands:?}"
    );
}

#[test]
fn extraction_finds_fenced_and_inline_commands() {
    let text = "Run `cardinal inbox` first.\n\n```sh\ncardinal act PLAN --caller arbiter\n```\n\n`cardinal.error.v1` is not a command.\n";
    assert_eq!(
        skill_commands(text),
        ["cardinal inbox", "cardinal act PLAN --caller arbiter"]
    );
}

#[test]
fn shell_words_follow_quoting() {
    assert_eq!(
        shell_words(r#"cardinal inbox resolve K --note "two words" --x 'a b'"#),
        [
            "cardinal",
            "inbox",
            "resolve",
            "K",
            "--note",
            "two words",
            "--x",
            "a b"
        ]
    );
}
