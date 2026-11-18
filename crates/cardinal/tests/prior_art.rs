//! `docs/prior-art.md` records, per module and per piece of the test harness, the prior art
//! compared, the decision, and the checks borrowed. This test keeps it complete: every module
//! that `tests/layering.rs` knows has a row, every row names a module or a harness path that
//! exists, every script in `scripts/` and every crate but `cardinal` is named by a harness row,
//! and every row has a kind, a decision with its reason, and borrowed checks: the existing test
//! file or suite directory that reuses another project's checks, or an exception (`none found:`
//! or `not ported:`) with at least one link upstream and a reason. Every repository path a
//! borrowed-checks cell names outside a link exists.
//!
//! The document is read as GitHub Flavored Markdown by `pulldown-cmark`, so a table whose
//! delimiter row is invalid is no table. The module list comes from the `TABLE` of
//! `tests/layering.rs`, which its own tests tie to the source tree, plus `cli`. A module that
//! only holds others (`adapter`, which holds `adapter::claude`) is covered by their rows.

use std::collections::BTreeSet;
use std::ops::Range;
use std::path::{Path, PathBuf};

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

const DOC: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/prior-art.md"
));
const LAYERING: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/layering.rs"));

const MODULES: &str = "Modules";
const HARNESS: &str = "Test harness";
const COLUMNS: [&str; 5] = [
    "Kind",
    "Mechanisms",
    "Prior art compared",
    "Decision",
    "Borrowed checks",
];
const KIND: usize = 1;
const MECHANISMS: usize = 2;
const PRIOR_ART: usize = 3;
const DECISION: usize = 4;
const BORROWED: usize = 5;

/// The words an exception's reason has at least, besides its links.
const REASON_WORDS: usize = 3;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The modules of `tests/layering.rs`: the first string of each tuple of its `TABLE`, and `cli`,
/// without the modules that only hold others.
fn layering_modules(layering: &str) -> BTreeSet<String> {
    let table = layering
        .split_once("const TABLE")
        .and_then(|(_, rest)| rest.split_once("\n];"))
        .expect("tests/layering.rs has a `const TABLE` ending in `];`")
        .0;
    let mut keys = BTreeSet::from(["cli".to_owned()]);
    let mut rest = table;
    while let Some(open) = rest.find('(') {
        rest = rest[open + 1..].trim_start();
        if let Some(quoted) = rest.strip_prefix('"') {
            let end = quoted.find('"').expect("a closed string");
            keys.insert(quoted[..end].to_owned());
        }
    }
    let parents: BTreeSet<String> = keys
        .iter()
        .filter(|key| {
            keys.iter()
                .any(|other| other.starts_with(&format!("{key}::")))
        })
        .cloned()
        .collect();
    keys.difference(&parents).cloned().collect()
}

/// A table cell as Markdown reads it.
#[derive(Debug, Default)]
struct Cell {
    /// The cell's text, each code span in backticks and each link as its text.
    text: String,
    /// The text outside links.
    outside: String,
    /// The code spans outside links.
    codes: Vec<String>,
    /// The destinations of the cell's links.
    links: Vec<String>,
    /// The cell's content in the document.
    range: Range<usize>,
}

/// A table row: its line in the document, its source, and its cells.
#[derive(Debug)]
struct Row {
    line: usize,
    range: Range<usize>,
    cells: Vec<Cell>,
}

impl Row {
    /// The name in the first cell's first code span.
    fn name(&self) -> Option<&str> {
        self.cells.first()?.codes.first().map(String::as_str)
    }
}

/// A table under a second-level heading.
#[derive(Debug)]
struct Table {
    heading: String,
    header: Vec<String>,
    /// The header row's source.
    header_range: Range<usize>,
    rows: Vec<Row>,
}

/// Every table of `doc` with the second-level heading above it.
fn tables(doc: &str) -> Vec<Table> {
    let line_of = |offset: usize| doc[..offset].matches('\n').count() + 1;
    let mut tables: Vec<Table> = Vec::new();
    let mut heading: Option<String> = None;
    let mut in_heading = false;
    let mut row: Option<Row> = None;
    let mut cell: Option<Cell> = None;
    let mut in_link = false;
    for (event, range) in Parser::new_ext(doc, Options::ENABLE_TABLES).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading {
                level: HeadingLevel::H2,
                ..
            }) => {
                in_heading = true;
                heading = Some(String::new());
            }
            Event::End(TagEnd::Heading(HeadingLevel::H2)) => in_heading = false,
            Event::Start(Tag::Table(_)) => tables.push(Table {
                heading: heading.clone().unwrap_or_default(),
                header: Vec::new(),
                header_range: range.clone(),
                rows: Vec::new(),
            }),
            Event::Start(Tag::TableHead) => {
                tables.last_mut().unwrap().header_range = range;
            }
            Event::Start(Tag::TableRow) => {
                row = Some(Row {
                    line: line_of(range.start),
                    range,
                    cells: Vec::new(),
                });
            }
            Event::End(TagEnd::TableRow) => {
                tables.last_mut().unwrap().rows.extend(row.take());
            }
            Event::Start(Tag::TableCell) => {
                cell = Some(Cell {
                    range,
                    ..Cell::default()
                });
            }
            Event::End(TagEnd::TableCell) => {
                let cell = cell.take().unwrap();
                match row.as_mut() {
                    Some(row) => row.cells.push(cell),
                    None => tables.last_mut().unwrap().header.push(cell.text),
                }
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                in_link = true;
                if let Some(cell) = cell.as_mut() {
                    cell.links.push(dest_url.into_string());
                }
            }
            Event::End(TagEnd::Link) => in_link = false,
            Event::Text(text) => {
                if in_heading {
                    heading.as_mut().unwrap().push_str(&text);
                }
                if let Some(cell) = cell.as_mut() {
                    cell.text.push_str(&text);
                    if !in_link {
                        cell.outside.push_str(&text);
                    }
                }
            }
            Event::Code(code) => {
                if let Some(cell) = cell.as_mut() {
                    cell.text.extend(["`", &code, "`"]);
                    if !in_link {
                        cell.outside.push_str(&code);
                        cell.codes.push(code.into_string());
                    }
                }
            }
            _ => {}
        }
    }
    tables
}

/// The table under `## {heading}` with the first column `first`, after checking its header.
fn table<'t>(
    tables: &'t [Table],
    heading: &str,
    first: &str,
    problems: &mut Vec<String>,
) -> &'t [Row] {
    let Some(table) = tables.iter().find(|t| t.heading == heading) else {
        problems.push(format!("no table under `## {heading}`"));
        return &[];
    };
    let columns: Vec<&str> = std::iter::once(first).chain(COLUMNS).collect();
    if table.header != columns {
        problems.push(format!(
            "`## {heading}`: the columns are {:?}, not {columns:?}",
            table.header
        ));
        return &[];
    }
    &table.rows
}

/// A path to a test file (one with a `#[test]`) or a directory holding one.
fn is_test(path: &Path) -> bool {
    if path.is_dir() {
        std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .any(|entry| entry.is_ok_and(|e| is_test(&e.path())))
    } else {
        std::fs::read_to_string(path).is_ok_and(|text| text.contains("#[test]"))
    }
}

/// Every problem with the row's kind, decision, and borrowed checks.
fn check_cells(row: &Row, root: &Path, problems: &mut Vec<String>) {
    let at = format!("line {}", row.line);
    if row.cells.len() != COLUMNS.len() + 1 {
        problems.push(format!("{at}: {} cells, not 6", row.cells.len()));
        return;
    }
    let kind = row.cells[KIND].text.as_str();
    if kind != "core" && kind != "generic" {
        problems.push(format!("{at}: Kind is {kind:?}, not core or generic"));
    }
    for column in [MECHANISMS, PRIOR_ART] {
        if row.cells[column].text.trim().is_empty() {
            problems.push(format!("{at}: {} is empty", COLUMNS[column - 1]));
        }
    }
    let decision = &row.cells[DECISION].text;
    let decided = ["adopt", "adapt", "own"].into_iter().any(|d| {
        decision
            .strip_prefix(&format!("`{d}`: "))
            .is_some_and(|reason| !reason.trim().is_empty())
    });
    if !decided {
        problems.push(format!(
            "{at}: Decision {decision:?} is not `adopt`, `adapt`, or `own` followed by `: ` \
             and a reason"
        ));
    }
    check_borrowed(&row.cells[BORROWED], &at, root, problems);
}

/// A borrowed-checks cell starts with the code span of an existing test file or suite
/// directory, or is an exception: `none found:` or `not ported:`, at least one link, and a
/// reason of [`REASON_WORDS`] words or more outside the links. Every code span outside links
/// that holds a `/` is a repository path and exists.
fn check_borrowed(cell: &Cell, at: &str, root: &Path, problems: &mut Vec<String>) {
    let text = cell.text.trim();
    let test_path = cell
        .codes
        .first()
        .filter(|path| text.starts_with(&format!("`{path}`")));
    let exception = ["none found:", "not ported:"]
        .into_iter()
        .find_map(|prefix| cell.outside.trim().strip_prefix(prefix));
    match (test_path, exception) {
        (Some(path), _) => {
            if !is_test(&root.join(path)) {
                problems.push(format!(
                    "{at}: Borrowed checks starts with `{path}`, which is not a test file or \
                     a directory holding one"
                ));
            }
        }
        (None, Some(reason)) => {
            if !cell.links.iter().any(|l| l.starts_with("https://")) {
                problems.push(format!(
                    "{at}: the exception {text:?} links to nothing upstream"
                ));
            }
            if reason.split_whitespace().count() < REASON_WORDS {
                problems.push(format!("{at}: the exception {text:?} gives no reason"));
            }
        }
        (None, None) => problems.push(format!(
            "{at}: Borrowed checks {text:?} is neither a test path nor an exception \
             (`none found:` or `not ported:`)"
        )),
    }
    for path in cell.codes.iter().filter(|code| code.contains('/')) {
        if !root.join(path).exists() {
            problems.push(format!(
                "{at}: Borrowed checks names `{path}`, which does not exist"
            ));
        }
    }
}

/// The harness pieces a harness row must name: every entry of `scripts/` and every crate but
/// `cardinal`, hidden entries (`.DS_Store`) left out.
fn harness_paths(root: &Path) -> BTreeSet<String> {
    let entries = |dir: &str| -> Vec<String> {
        std::fs::read_dir(root.join(dir))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| !name.starts_with('.'))
            .map(|name| format!("{dir}/{name}"))
            .collect()
    };
    entries("scripts")
        .into_iter()
        .chain(
            entries("crates")
                .into_iter()
                .filter(|c| c != "crates/cardinal"),
        )
        .collect()
}

/// Every problem with `doc` against the modules `modules`, the repository at `root`, and the
/// harness pieces `harness`.
fn problems(
    doc: &str,
    modules: &BTreeSet<String>,
    harness: &BTreeSet<String>,
    root: &Path,
) -> Vec<String> {
    let tables = tables(doc);
    let mut problems = Vec::new();
    let mut named = BTreeSet::new();
    for row in table(&tables, MODULES, "Module", &mut problems) {
        match row.name() {
            Some(name) if !modules.contains(name) => problems.push(format!(
                "line {}: `{name}` is not a module of tests/layering.rs",
                row.line
            )),
            Some(name) if !named.insert(name.to_owned()) => {
                problems.push(format!("line {}: a second row for `{name}`", row.line));
            }
            Some(_) => {}
            None => problems.push(format!("line {}: no module in a code span", row.line)),
        }
        check_cells(row, root, &mut problems);
    }
    for module in modules.difference(&named) {
        problems.push(format!("no row for module `{module}`"));
    }
    let mut pieces = BTreeSet::new();
    for row in table(&tables, HARNESS, "Piece", &mut problems) {
        let paths = row.cells.first().map_or_default(|c| c.codes.clone());
        if paths.is_empty() {
            problems.push(format!("line {}: no harness path in a code span", row.line));
        }
        for path in paths {
            if !root.join(&path).exists() {
                problems.push(format!(
                    "line {}: `{path}` does not exist in the repository",
                    row.line
                ));
            }
            pieces.insert(path);
        }
        check_cells(row, root, &mut problems);
    }
    for piece in harness.difference(&pieces) {
        problems.push(format!("no harness row names `{piece}`"));
    }
    problems
}

fn real_problems(doc: &str) -> Vec<String> {
    let root = repo_root();
    problems(
        doc,
        &layering_modules(LAYERING),
        &harness_paths(&root),
        &root,
    )
}

#[test]
fn every_module_and_harness_piece_has_a_complete_row() {
    let problems = real_problems(DOC);
    assert!(
        problems.is_empty(),
        "docs/prior-art.md:\n{}",
        problems.join("\n")
    );
}

#[test]
fn modules_come_from_the_layering_table() {
    let modules = layering_modules(LAYERING);
    for module in ["contract", "state", "adapter::claude", "reconcile", "cli"] {
        assert!(modules.contains(module), "{module} in {modules:?}");
    }
    assert!(!modules.contains("adapter"), "{modules:?}");
}

// Each check fails when the document breaks it: the real document with one row, found by its
// parsed name, removed or with one cell replaced.

/// The real document with the row named `name` in the table under `heading` removed
/// (`None`) or with cell `column` replaced by `text`.
fn edited(heading: &str, name: &str, cell: Option<(usize, &str)>) -> String {
    let tables = tables(DOC);
    let row = tables
        .iter()
        .filter(|t| t.heading == heading)
        .flat_map(|t| &t.rows)
        .find(|row| row.name() == Some(name))
        .unwrap_or_else(|| panic!("no row named `{name}` under `## {heading}`"));
    let mut doc = DOC.to_owned();
    if let Some((column, text)) = cell {
        doc.replace_range(row.cells[column].range.clone(), text);
    } else {
        let start = DOC[..row.range.start].rfind('\n').map_or(0, |i| i + 1);
        let end = DOC[start..].find('\n').map_or(DOC.len(), |i| start + i + 1);
        doc.replace_range(start..end, "");
    }
    doc
}

fn assert_problem(doc: &str, expected: &str) {
    let problems = real_problems(doc);
    assert!(
        problems.iter().any(|p| p.contains(expected)),
        "expected {expected:?}, got {problems:?}"
    );
}

#[test]
fn a_module_without_a_row_fails() {
    assert_eq!(
        real_problems(&edited(MODULES, "state", None)),
        ["no row for module `state`"]
    );
}

#[test]
fn a_row_for_a_module_that_does_not_exist_fails() {
    assert_problem(
        &edited(MODULES, "state", Some((0, "`statestore`"))),
        "`statestore` is not a module",
    );
}

#[test]
fn a_harness_piece_without_a_row_fails() {
    assert_problem(
        &edited(HARNESS, "scripts/repo-guard", None),
        "no harness row names `scripts/repo-guard`",
    );
}

#[test]
fn a_harness_row_for_a_path_that_does_not_exist_fails() {
    assert_problem(
        &edited(HARNESS, "scripts/repo-guard", Some((0, "`scripts/guard`"))),
        "`scripts/guard` does not exist",
    );
}

#[test]
fn a_row_without_a_kind_fails() {
    assert_problem(&edited(MODULES, "record", Some((KIND, ""))), "Kind is \"\"");
}

#[test]
fn a_row_without_a_decision_fails() {
    assert_problem(
        &edited(MODULES, "record", Some((DECISION, ""))),
        "Decision \"\" is not",
    );
    assert_problem(
        &edited(MODULES, "record", Some((DECISION, "`adapt`:"))),
        "Decision \"`adapt`:\" is not",
    );
}

#[test]
fn a_row_without_borrowed_checks_fails() {
    for blank in ["", "—", "TBD"] {
        assert_problem(
            &edited(MODULES, "record", Some((BORROWED, blank))),
            "is neither a test path nor an exception",
        );
    }
}

#[test]
fn an_exception_without_a_link_or_a_reason_fails() {
    for (cell, expected) in [
        ("none found: placeholder", "links to nothing upstream"),
        ("not ported: x", "links to nothing upstream"),
        ("not ported: x", "gives no reason"),
        (
            "none found: [the suite](https://example.com/suite)",
            "gives no reason",
        ),
        (
            "none found: [the suite](ftp://example.com/suite) is for another protocol",
            "links to nothing upstream",
        ),
    ] {
        assert_problem(&edited(MODULES, "record", Some((BORROWED, cell))), expected);
    }
}

#[test]
fn a_borrowed_path_that_is_no_test_fails() {
    assert_problem(
        &edited(MODULES, "state", Some((BORROWED, "`docs/design.md`"))),
        "starts with `docs/design.md`, which is not a test file",
    );
}

#[test]
fn a_borrowed_path_that_does_not_exist_fails() {
    for path in [
        "crates/cardinal/src/state/missing.rs",
        "crates/cardinal/tests/missing.rs",
        "tests/records.rs",
    ] {
        assert_problem(
            &edited(MODULES, "state", Some((BORROWED, &format!("`{path}`")))),
            &format!("names `{path}`, which does not exist"),
        );
    }
    let exception = "none found: [the suite](https://example.com/suite) covers one writer; \
                     `crates/cardinal/tests/gone.rs` holds cases of its own";
    assert_problem(
        &edited(MODULES, "record", Some((BORROWED, exception))),
        "names `crates/cardinal/tests/gone.rs`, which does not exist",
    );
}

#[test]
fn a_table_with_an_invalid_delimiter_row_is_no_table() {
    let tables = tables(DOC);
    let header = &tables
        .iter()
        .find(|t| t.heading == MODULES)
        .unwrap()
        .header_range;
    let start = header.start + DOC[header.start..].find('\n').unwrap() + 1;
    let end = start + DOC[start..].find('\n').unwrap();
    assert!(
        DOC[start..end].starts_with("| ---"),
        "{:?}",
        &DOC[start..end]
    );
    let mut doc = DOC.to_owned();
    doc.replace_range(start..end, "| --- | --- |");
    assert_problem(&doc, "no table under `## Modules`");
}
