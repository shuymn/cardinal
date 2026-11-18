//! Arbiter skill v2 (`docs/architecture.md` section 14.3): every `cardinal …` command in the
//! skill parses with the real CLI definition and stays inside the arbiter's reach; the text a
//! pass the timer started follows runs only commands whose output is bounded (section 10.5):
//! no bare `cardinal plan`, no `cardinal explain`, `cardinal inventory` only with `--worktree`,
//! and the inbox only with `--assigned --brief`; it checks the subject of every kind of item
//! about a worktree and answers every item, deferring the ones it leaves open; the owner's
//! request reads the whole inbox and answers without `--assigned`; and the skill's tables cover
//! exactly the `act` statuses of section 11.3 and the inbox kinds of section 10.1.

use std::collections::BTreeSet;

use cardinal::cli::Cli;
use cardinal::contract::{ActStatus, InboxKind};
use clap::Parser as _;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

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

/// Every status of `cardinal.act-result.v1`, in the order of [`status_index`].
const STATUSES: [ActStatus; 6] = [
    ActStatus::DryRun,
    ActStatus::Executed,
    ActStatus::Partial,
    ActStatus::Rejected,
    ActStatus::Expired,
    ActStatus::AlreadyExecuted,
];

/// `status`'s place in [`STATUSES`]. A status added to the contract fails to compile here
/// until it gets a place, and then fails [`lists_are_complete`] until [`STATUSES`] has it.
fn status_index(status: ActStatus) -> usize {
    match status {
        ActStatus::DryRun => 0,
        ActStatus::Executed => 1,
        ActStatus::Partial => 2,
        ActStatus::Rejected => 3,
        ActStatus::Expired => 4,
        ActStatus::AlreadyExecuted => 5,
    }
}

/// Every inbox kind of `cardinal.inbox-item.v1`, in the order of [`kind_index`].
const KINDS: [InboxKind; 11] = [
    InboxKind::RecoveryNeeded,
    InboxKind::OutlivedProcesses,
    InboxKind::Reclaimable,
    InboxKind::MergeUndecided,
    InboxKind::ClosedPrDirty,
    InboxKind::ManagedMerged,
    InboxKind::UnclassifiableData,
    InboxKind::DiskLow,
    InboxKind::DiskCritical,
    InboxKind::ArbiterFailing,
    InboxKind::ReclaimStranded,
];

/// Whether an item of `kind` is about a worktree: a pass checks the worktree still is as the item
/// says before it answers. The others are about a job, a session, a volume, a pass, or a trash
/// entry, which `cardinal explain` reads afresh.
fn about_a_worktree(kind: InboxKind) -> bool {
    match kind {
        InboxKind::Reclaimable
        | InboxKind::MergeUndecided
        | InboxKind::ClosedPrDirty
        | InboxKind::ManagedMerged
        | InboxKind::UnclassifiableData => true,
        InboxKind::RecoveryNeeded
        | InboxKind::OutlivedProcesses
        | InboxKind::DiskLow
        | InboxKind::DiskCritical
        | InboxKind::ArbiterFailing
        | InboxKind::ReclaimStranded => false,
    }
}

/// `kind`'s place in [`KINDS`], as [`status_index`] for statuses.
fn kind_index(kind: InboxKind) -> usize {
    match kind {
        InboxKind::RecoveryNeeded => 0,
        InboxKind::OutlivedProcesses => 1,
        InboxKind::Reclaimable => 2,
        InboxKind::MergeUndecided => 3,
        InboxKind::ClosedPrDirty => 4,
        InboxKind::ManagedMerged => 5,
        InboxKind::UnclassifiableData => 6,
        InboxKind::DiskLow => 7,
        InboxKind::DiskCritical => 8,
        InboxKind::ArbiterFailing => 9,
        InboxKind::ReclaimStranded => 10,
    }
}

fn name(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}

/// The Markdown of `text` under its `## heading`, up to the next level-2 heading.
fn section<'a>(text: &'a str, heading: &str) -> &'a str {
    let (mut title, mut start) = (None::<String>, None);
    for (event, range) in Parser::new(text).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading {
                level: HeadingLevel::H2,
                ..
            }) => {
                if let Some(start) = start {
                    return &text[start..range.start];
                }
                title = Some(String::new());
            }
            Event::Text(part) | Event::Code(part) => {
                if let Some(title) = &mut title {
                    title.push_str(&part);
                }
            }
            Event::End(TagEnd::Heading(HeadingLevel::H2)) => {
                let read = title.take();
                if read.as_deref() == Some(heading) {
                    start = Some(range.end);
                }
            }
            _ => {}
        }
    }
    start.map_or_else(|| panic!("no section {heading}"), |start| &text[start..])
}

/// The code span each body row of the first table in `text` starts with.
fn table_keys(text: &str) -> BTreeSet<String> {
    let (mut keys, mut in_body, mut at_row_start) = (BTreeSet::new(), false, false);
    for event in Parser::new_ext(text, Options::ENABLE_TABLES) {
        match event {
            Event::End(TagEnd::TableHead) => in_body = true,
            Event::End(TagEnd::Table) => break,
            Event::Start(Tag::TableRow) => at_row_start = in_body,
            Event::Start(Tag::TableCell) => {}
            Event::Code(code) if at_row_start => {
                keys.insert(code.into_string());
                at_row_start = false;
            }
            _ => at_row_start = false,
        }
    }
    keys
}

/// A list item as Markdown reads it.
#[derive(Debug, Default)]
struct Item {
    /// 1 for an item of a top-level list, 2 for one nested in it, and so on.
    depth: usize,
    ordered: bool,
    /// The bold text the item starts with.
    lead: Option<String>,
    /// The item's text, its nested items' included, with code spans in backticks.
    text: String,
}

/// Every list item of `text`, each before the items nested in it.
fn list_items(text: &str) -> Vec<Item> {
    fn add(items: &mut [Item], open: &[usize], in_lead: bool, part: &str) {
        if let (true, Some(&innermost)) = (in_lead, open.last()) {
            items[innermost].lead.get_or_insert_default().push_str(part);
        }
        for &i in open {
            items[i].text.push_str(part);
        }
    }

    let (mut items, mut lists, mut open) = (Vec::<Item>::new(), Vec::new(), Vec::new());
    let mut in_lead = false;
    for event in Parser::new(text) {
        match event {
            Event::Start(Tag::List(first)) => lists.push(first.is_some()),
            Event::End(TagEnd::List(_)) => {
                lists.pop();
            }
            Event::Start(Tag::Item) => {
                open.push(items.len());
                items.push(Item {
                    depth: lists.len(),
                    ordered: lists.last() == Some(&true),
                    ..Item::default()
                });
            }
            Event::End(TagEnd::Item) => {
                open.pop();
            }
            Event::Start(Tag::Strong) => {
                if let Some(&i) = open.last()
                    && items[i].text.is_empty()
                    && items[i].lead.is_none()
                {
                    items[i].lead = Some(String::new());
                    in_lead = true;
                }
            }
            Event::End(TagEnd::Strong) => in_lead = false,
            Event::Text(part) => add(&mut items, &open, in_lead, &part),
            Event::Code(code) => add(&mut items, &open, in_lead, &format!("`{code}`")),
            Event::SoftBreak | Event::HardBreak => {
                for &i in &open {
                    items[i].text.push(' ');
                }
            }
            _ => {}
        }
    }
    items
}

/// Subcommands the arbiter may run: the default `[arbiter] command`'s allow list.
const ALLOWED: [&str; 5] = ["inventory", "inbox", "explain", "plan", "act"];

/// Commands from code block lines and code spans that start with `cardinal `.
fn skill_commands(text: &str) -> Vec<String> {
    let (mut commands, mut in_block) = (Vec::new(), false);
    for event in Parser::new_ext(text, Options::ENABLE_TABLES) {
        match event {
            Event::Start(Tag::CodeBlock(_)) => in_block = true,
            Event::End(TagEnd::CodeBlock) => in_block = false,
            Event::Text(block) if in_block => commands.extend(
                block
                    .lines()
                    .map(str::trim)
                    .filter(|line| line.starts_with("cardinal "))
                    .map(str::to_owned),
            ),
            Event::Code(span) if span.starts_with("cardinal ") => commands.push(span.into_string()),
            _ => {}
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

/// The value of `flag` in `words`, if given.
fn flag_value<'a>(words: &'a [String], flag: &str) -> Option<&'a str> {
    words
        .windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].as_str())
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
            assert_eq!(
                flag_value(&words, "--caller"),
                Some("arbiter"),
                "act as anyone but the arbiter: {command}"
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

/// What in `skill` would take a pass the timer started beyond its items or past the bound on
/// its commands' output. Such a pass follows every section but the owner's request, so the
/// shared sections count too: a plan other than `cardinal plan reclaim-worktree`, an inbox
/// command without `--assigned` and `--brief`, an inventory without `--worktree`, and any
/// `cardinal explain`.
fn timer_violations(skill: &str) -> Vec<String> {
    let timer = skill.replacen(section(skill, "The owner's request"), "", 1);
    let mut violations = Vec::new();
    for command in skill_commands(&timer) {
        let words = shell_words(&command);
        let has = |flag: &str| words.iter().any(|w| w == flag);
        match words[1].as_str() {
            "plan" if words.get(2).map(String::as_str) != Some("reclaim-worktree") => {
                violations.push(format!("a plan of another kind: {command}"));
            }
            "inbox" if !has("--assigned") || !has("--brief") => {
                violations.push(format!("the inbox without --assigned --brief: {command}"));
            }
            "inventory" if !has("--worktree") || words.len() != 4 => {
                violations.push(format!("an inventory of more than a worktree: {command}"));
            }
            "explain" => violations.push(format!("an unbounded explanation: {command}")),
            _ => {}
        }
    }
    violations
}

/// A pass the timer started works on its assigned items only, through commands whose output is
/// bounded: it plans just their worktrees, never with a bare `cardinal plan` or a
/// `reclaim_dir` plan, reads other worktree items' facts with `cardinal inventory --worktree`,
/// explains nothing, and lists and answers items only with `--assigned --brief`, in its own
/// steps and in the sections it shares with the owner's request.
#[test]
fn a_timer_pass_stays_on_its_items() {
    assert_eq!(timer_violations(SKILL), Vec::<String>::new());
    let steps = skill_commands(section(SKILL, "A pass the timer started"));
    for expected in [
        "cardinal plan reclaim-worktree ",
        "cardinal inventory --worktree ",
    ] {
        assert!(
            steps.iter().any(|command| command.starts_with(expected)),
            "a timer pass runs `{expected}PATH`"
        );
    }
    // The pass's first command tells a timer pass from the owner's request.
    assert!(
        section(SKILL, "Which pass this is")
            .contains("Run `cardinal inbox --assigned --brief` first."),
        "the skill starts by listing the pass's items"
    );
    // A command added to a shared section is in the pass's reach.
    for added in [
        "`cardinal plan`",
        "`cardinal plan reclaim-dir PATH`",
        "`cardinal inbox`",
        "`cardinal inbox --assigned`",
        "`cardinal inbox resolve KEY --outcome resolved --assigned`",
        "`cardinal inventory`",
        "`cardinal inventory --sizes`",
        "`cardinal explain inbox KEY`",
    ] {
        let skill = SKILL.replacen("\n## Plans\n", &format!("\n## Plans\n\nRun {added}.\n"), 1);
        assert!(
            !timer_violations(&skill).is_empty(),
            "{added} in the Plans section"
        );
    }
}

/// A pass the timer started answers every item it checked, deferring the ones it leaves open,
/// because a pass that answers none of its open items counts as failed (section 10.5).
#[test]
fn a_timer_pass_answers_every_item() {
    let steps = section(SKILL, "A pass the timer started");
    let outcomes: BTreeSet<String> = skill_commands(steps)
        .iter()
        .map(|command| shell_words(command))
        .filter(|words| words.get(2).map(String::as_str) == Some("resolve"))
        .filter_map(|words| flag_value(&words, "--outcome").map(str::to_owned))
        .collect();
    assert_eq!(
        outcomes,
        BTreeSet::from(["deferred", "escalated", "resolved"].map(String::from))
    );
    assert!(
        steps.contains("Answer every `open` item once"),
        "every item is answered"
    );
    assert!(
        steps.contains("answered none of its open items counts as failed"),
        "the skill says why"
    );
    // What the brief view cut is reported as cut, never as the whole.
    for phrase in [
        "never quote a preview as the whole text",
        "names the entries shown, says how many were omitted",
    ] {
        assert!(steps.contains(phrase), "{phrase}");
    }
}

/// Before it answers an item about a worktree, a pass checks, for every such kind, that the
/// worktree still is as the item says, leaving one it cannot confirm open, and the owner's
/// request does as a timer pass does.
#[test]
fn the_subject_of_every_worktree_item_is_checked() {
    let items = list_items(section(SKILL, "A pass the timer started"));
    let step = items
        .iter()
        .position(|item| item.depth == 1 && item.lead.as_deref() == Some("Subject."))
        .expect("a subject step");
    let checks: Vec<&str> = items[step + 1..]
        .iter()
        .take_while(|item| item.depth > 1)
        .map(|item| item.text.as_str())
        .collect();
    for kind in KINDS.into_iter().filter(|kind| about_a_worktree(*kind)) {
        let listed = format!("`{}`", name(kind));
        assert!(
            checks.iter().any(|check| check.contains(&listed)),
            "the subject step checks {kind:?}"
        );
    }
    // An `unclassifiable_data` item's entries are compared as a set, whatever their number.
    let unclassifiable = checks
        .iter()
        .find(|check| check.starts_with("`unclassifiable_data`"))
        .expect("an unclassifiable_data check");
    assert!(
        unclassifiable.contains("`unclassifiable_set`")
            && unclassifiable.contains("`evidence.set`"),
        "the set is compared: {unclassifiable}"
    );
    assert!(
        section(SKILL, "The owner's request").contains("checking its subject as there"),
        "the owner's request checks subjects too"
    );
}

/// The owner's request reads the whole inbox and the inventory, plans every target, and
/// resolves without `--assigned`; both cases apply decisions to open items only, so an
/// escalation is not answered again.
#[test]
fn the_owners_request_reads_everything() {
    let owner = section(SKILL, "The owner's request");
    let commands: Vec<Vec<String>> = skill_commands(owner)
        .iter()
        .map(|command| shell_words(command))
        .collect();
    let has = |expected: &[&str]| {
        commands.iter().any(|words| {
            words
                .iter()
                .map(String::as_str)
                .eq(expected.iter().copied())
        })
    };
    for bare in [
        ["cardinal", "inbox"],
        ["cardinal", "inventory"],
        ["cardinal", "plan"],
    ] {
        assert!(has(&bare), "the owner's request runs `{}`", bare.join(" "));
    }
    let resolves: Vec<&Vec<String>> = commands
        .iter()
        .filter(|words| words.get(2).map(String::as_str) == Some("resolve"))
        .collect();
    assert!(!resolves.is_empty(), "the owner's request resolves items");
    for words in resolves {
        assert!(
            !words.iter().any(|w| w == "--assigned"),
            "the owner's request names no pass: {}",
            words.join(" ")
        );
    }
    for (heading, rule) in [
        (
            "A pass the timer started",
            "Answer every `open` item once, with the inbox table's decision",
        ),
        (
            "The owner's request",
            "Apply the inbox table's decisions to the `open` items",
        ),
    ] {
        assert!(
            section(SKILL, heading).contains(rule),
            "{heading}: decisions apply to open items"
        );
    }
}

/// The statuses and kinds are the contract's, every one of them.
#[test]
fn lists_are_complete() {
    for (i, status) in STATUSES.into_iter().enumerate() {
        assert_eq!(status_index(status), i, "{status:?}");
    }
    for (i, kind) in KINDS.into_iter().enumerate() {
        assert_eq!(kind_index(kind), i, "{kind:?}");
    }
}

#[test]
fn the_tables_cover_every_status_and_kind() {
    let statuses: BTreeSet<String> = STATUSES.into_iter().map(name).collect();
    assert_eq!(
        table_keys(section(SKILL, "Act results")),
        statuses,
        "the act result table"
    );
    let kinds: BTreeSet<String> = KINDS.into_iter().map(name).collect();
    assert_eq!(
        table_keys(section(SKILL, "Inbox items")),
        kinds,
        "the inbox table"
    );

    let codes = |refusal: bool| -> Vec<String> {
        STATUSES
            .into_iter()
            .filter(|s| s.is_refusal() == refusal)
            .map(|s| format!("`{}`", name(s)))
            .collect()
    };
    let list = |items: Vec<String>| match items.as_slice() {
        [one] => one.clone(),
        [rest @ .., last] => format!("{}, or {last}", rest.join(", ")),
        [] => panic!("no statuses"),
    };
    let exit_codes = format!(
        "Exit 0 from `act` means {}, exit 3 means {}",
        list(codes(false)),
        list(codes(true))
    );
    assert!(SKILL.contains(&exit_codes), "missing: {exit_codes}");
}

#[test]
fn the_report_never_claims_a_reclamation() {
    let sections: Vec<String> = list_items(section(SKILL, "Report"))
        .into_iter()
        .filter(|item| item.depth == 1 && item.ordered)
        .filter_map(|item| item.lead)
        .collect();
    assert_eq!(
        sections,
        [
            "Reclaimable",
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
fn list_items_read_nesting_and_leads() {
    let text = "1. **One.** Run `x`:\n   - `a`: first\n     line\n2. **Two**: done\n\n- plain\n";
    let items = list_items(text);
    let read: Vec<(usize, bool, Option<&str>, &str)> = items
        .iter()
        .map(|i| (i.depth, i.ordered, i.lead.as_deref(), i.text.as_str()))
        .collect();
    assert_eq!(
        read,
        [
            (1, true, Some("One."), "One. Run `x`:`a`: first line"),
            (2, false, None, "`a`: first line"),
            (1, true, Some("Two"), "Two: done"),
            (1, false, None, "plain"),
        ]
    );
}

#[test]
fn table_keys_read_the_first_code_span_of_each_row() {
    let text = "intro `x`\n\n| Kind | Decision |\n| --- | --- |\n| `a`, `b` | `c` |\n| `d` | e |\n\nafter\n| `f` | g |\n";
    assert_eq!(
        table_keys(text),
        BTreeSet::from(["a".to_owned(), "d".to_owned()])
    );
}
