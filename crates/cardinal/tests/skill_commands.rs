//! Every `cardinal …` command in the arbiter skill parses with the real CLI
//! definition and stays inside the arbiter's reach, and the skill handles
//! exactly the results phase 0 `act` returns (architecture §9.5 and §14.3).

use std::collections::BTreeSet;

use cardinal::cli::Cli;
use cardinal::contract::ActStatus;
use clap::Parser;

const SKILL: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../integrations/claude-code/skills/cardinal-arbiter/SKILL.md"
));
const AGENT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../integrations/claude-code/agents/cardinal-arbiter.md"
));
const README: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../integrations/claude-code/README.md"
));

const STATUSES: [ActStatus; 6] = [
    ActStatus::Executed,
    ActStatus::Partial,
    ActStatus::DryRun,
    ActStatus::Rejected,
    ActStatus::Expired,
    ActStatus::AlreadyExecuted,
];

/// Whether phase 0 `act`, which only reports, can return `status`.
fn phase0_returns(status: ActStatus) -> bool {
    match status {
        ActStatus::DryRun | ActStatus::Rejected | ActStatus::Expired => true,
        ActStatus::Executed | ActStatus::Partial | ActStatus::AlreadyExecuted => false,
    }
}

fn name(status: ActStatus) -> String {
    serde_json::to_value(status)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}

/// The text of the `## heading` section of `text`.
fn section<'a>(text: &'a str, heading: &str) -> &'a str {
    let start = text
        .split_once(&format!("\n## {heading}\n"))
        .unwrap_or_else(|| panic!("no section {heading}"))
        .1;
    start.split("\n## ").next().unwrap()
}

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

#[test]
fn act_results_are_what_phase_0_act_returns() {
    let rows: BTreeSet<String> = section(SKILL, "Act results")
        .lines()
        .skip_while(|line| !line.starts_with("| ---"))
        .filter_map(|line| line.strip_prefix("| `")?.split('`').next())
        .map(str::to_owned)
        .collect();
    let returned: BTreeSet<String> = STATUSES
        .into_iter()
        .filter(|s| phase0_returns(*s))
        .map(name)
        .collect();
    assert_eq!(rows, returned, "the act result table");

    for status in STATUSES.into_iter().filter(|s| !phase0_returns(*s)) {
        let code = format!("`{}`", name(status));
        for (file, text) in [("SKILL.md", SKILL), ("agent", AGENT), ("README.md", README)] {
            assert!(
                !text.contains(&code),
                "{file} handles {code}, which phase 0 act never returns"
            );
        }
    }

    let refusals: Vec<String> = STATUSES
        .into_iter()
        .filter(|s| phase0_returns(*s) && s.is_refusal())
        .map(|s| format!("`{}`", name(s)))
        .collect();
    let exit_codes = format!(
        "Exit 0 from `act` means `{}`, exit 3 means {}",
        name(ActStatus::DryRun),
        refusals.join(" or ")
    );
    assert!(SKILL.contains(&exit_codes), "missing: {exit_codes}");
}

#[test]
fn the_report_never_claims_a_reclamation() {
    let sections: Vec<&str> = section(SKILL, "Report")
        .lines()
        .filter_map(|line| line.split_once(". **")?.1.split("**").next())
        .collect();
    assert_eq!(
        sections,
        [
            "Would reclaim",
            "Refused",
            "Owner decisions",
            "Inbox",
            "Errors"
        ]
    );
    for (file, text) in [("SKILL.md", SKILL), ("agent", AGENT), ("README.md", README)] {
        for claim in ["Reclaimed", "partially deleted", "interrupted act"] {
            assert!(!text.contains(claim), "{file} says {claim:?}");
        }
    }
}
