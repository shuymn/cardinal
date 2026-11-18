//! Module layering (`docs/architecture.md` section 2.2). Every module of `src/` may reference
//! only the modules the table allows, transitively: a module may use whatever the modules it
//! may use may use. On top of the table, `testpoint` (a leaf with no dependencies) is open to
//! every module above the foundation, because its pause points sit in `run`, `act`,
//! `reconcile`, and the hook. Platform crates stay where the table puts them, and `admission`
//! uses neither `std::net` nor `std::process`.
//!
//! The scanner reads `crate::…` and `super::…` paths (including grouped `use` trees) from the
//! source with comments and string literals removed. A glob import of the crate root and an
//! alias of `crate` would bring modules into scope without such a path, so both count as a
//! reference to the whole crate, which no module may use; other glob imports are denied by
//! `clippy::wildcard_imports`. The scanner guards against accidental imports, not against
//! code written to slip past it; the runtime tests in `tests/admission_isolation.rs` are the
//! real check for C17 and C18.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The "May use" column of section 2.2, internal modules only.
const TABLE: &[(&str, &[&str])] = &[
    ("contract", &[]),
    ("config", &["contract"]),
    ("state", &["contract"]),
    ("record", &["state", "contract"]),
    ("testpoint", &[]),
    ("os", &[]),
    ("sensor", &["os", "contract"]),
    ("exec", &["os", "contract"]),
    (
        "admission",
        &["state", "config", "os", "sensor", "record", "contract"],
    ),
    (
        "run",
        &[
            "admission",
            "exec",
            "record",
            "state",
            "config",
            "os",
            "testpoint",
        ],
    ),
    ("classify", &["config", "contract"]),
    ("adapter", &["adapter::claude"]),
    (
        "adapter::claude",
        &["classify", "config", "record", "state", "os"],
    ),
    (
        "observe",
        &["config", "state", "admission", "os", "contract"],
    ),
    (
        "plan",
        &["observe", "admission", "state", "record", "contract"],
    ),
    (
        "act",
        &[
            "plan",
            "observe",
            "admission",
            "inbox",
            "state",
            "record",
            "contract",
            "testpoint",
        ],
    ),
    (
        "inbox",
        &["admission", "observe", "state", "record", "contract"],
    ),
    (
        "reconcile",
        &[
            "act",
            "plan",
            "inbox",
            "observe",
            "sensor",
            "snapshot",
            "admission",
            "record",
            "state",
            "config",
            "os",
            "contract",
        ],
    ),
    ("snapshot", &["inbox", "record", "state", "contract"]),
    (
        "explain",
        &[
            "adapter::claude",
            "classify",
            "record",
            "plan",
            "inbox",
            "admission",
            "state",
            "contract",
        ],
    ),
];

/// `contract`, `state`, `config`, `testpoint`, and `os` import nothing above them.
const FOUNDATION: &[&str] = &["contract", "state", "config", "testpoint", "os"];

/// The admission path: it never reaches observation, the inbox, plans, `act`, `reconcile`, or
/// the web UI's snapshot (C17, C18).
const ADMISSION_PATH: &[&str] = &["admission", "run", "exec"];

/// External crates that only some modules may use.
const PLATFORM_CRATES: &[(&str, &[&str])] = &[
    ("sysinfo", &["os"]),
    ("signal_hook", &["os"]),
    ("rustix", &["os", "act"]),
    ("windows_sys", &["os"]),
    ("libc", &["os"]),
    ("objc2_core_foundation", &["os"]),
];

/// Parts of `std` a module may not use: admission neither talks to the network nor starts
/// processes (gh and git live in `observe`).
const STD_BANS: &[(&str, &[&str])] = &[("admission", &["net", "process"])];

/// The segment that stands for the root a path starts from (`crate`, `std`) itself:
/// `use crate::*`, `use crate as c`.
const ROOT: &str = "<root>";

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// The allowed internal dependencies of each module: the table's transitive closure, plus
/// `testpoint` for modules above the foundation. `cli` may use everything.
fn allowed() -> BTreeMap<&'static str, BTreeSet<&'static str>> {
    let direct: BTreeMap<&str, &[&str]> = TABLE.iter().copied().collect();
    let mut allowed = BTreeMap::new();
    for (module, _) in TABLE {
        let mut seen = BTreeSet::new();
        let mut stack: Vec<&str> = direct[module].to_vec();
        while let Some(next) = stack.pop() {
            if seen.insert(next) {
                stack.extend(direct.get(next).copied().unwrap_or_default());
            }
        }
        if !FOUNDATION.contains(module) {
            seen.insert("testpoint");
        }
        allowed.insert(*module, seen);
    }
    allowed
}

fn module_keys() -> BTreeSet<&'static str> {
    TABLE.iter().map(|(m, _)| *m).chain(["cli"]).collect()
}

/// A reference from one source file.
#[derive(Debug, PartialEq, Eq)]
struct Reference {
    file: String,
    from: String,
    to: String,
}

impl std::fmt::Display for Reference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {} -> {}", self.file, self.from, self.to)
    }
}

/// The module path of a source file relative to `src/`: `admission/mod.rs` is
/// `["admission"]`, `adapter/claude/hook.rs` is `["adapter", "claude", "hook"]`.
fn module_path(rel: &Path) -> Vec<String> {
    let mut parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let file = parts.pop().unwrap();
    let stem = file.strip_suffix(".rs").unwrap().to_owned();
    if stem != "mod" {
        parts.push(stem);
    }
    parts
}

/// The layering key of a module path: `adapter::claude` for anything below it, else the
/// top-level module.
fn key_of(path: &[String]) -> Option<String> {
    match path {
        [] => None,
        [first, second, ..] if first == "adapter" && second == "claude" => {
            Some("adapter::claude".into())
        }
        [first, ..] => Some(first.clone()),
    }
}

/// Every `.rs` file below `src/` except the crate roots.
fn source_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let src = src_dir();
    let mut files = Vec::new();
    walk(&src, &mut files);
    files.retain(|f| f != &src.join("lib.rs") && f != &src.join("main.rs"));
    files.sort();
    files
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    Ident(String),
    PathSep,
    Open,
    Close,
    Comma,
    Other,
}

/// Tokenizes Rust source, dropping comments, string, byte-string, raw-string, and character
/// literals, and lifetimes.
fn tokenize(src: &str) -> Vec<Tok> {
    let b = src.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    let ident_start = |c: u8| c.is_ascii_alphabetic() || c == b'_';
    let ident_char = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i..].starts_with(b"/*") {
            let mut depth = 0;
            while i < b.len() {
                if b[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if b[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        } else if c == b'"' || (c == b'b' && b.get(i + 1) == Some(&b'"')) {
            i += if c == b'b' { 2 } else { 1 };
            while i < b.len() && b[i] != b'"' {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i += 1;
            toks.push(Tok::Other);
        } else if let Some(len) = raw_string_len(&b[i..]) {
            i += len;
            toks.push(Tok::Other);
        } else if c == b'\'' {
            // A character literal ('x', '\n', '\u{..}', b'x' handled by the ident branch) or a
            // lifetime ('a): a lifetime has no closing quote right after its identifier.
            if b.get(i + 1) == Some(&b'\\') {
                i += 2;
                while i < b.len() && b[i] != b'\'' {
                    i += 1;
                }
                i += 1;
            } else if b.get(i + 2) == Some(&b'\'') {
                i += 3;
            } else {
                let len = src[i + 1..].chars().next().map_or(1, char::len_utf8);
                if src.as_bytes().get(i + 1 + len) == Some(&b'\'') {
                    i += 2 + len;
                } else {
                    i += 1;
                    while i < b.len() && ident_char(b[i]) {
                        i += 1;
                    }
                }
            }
            toks.push(Tok::Other);
        } else if ident_start(c) {
            let start = i;
            while i < b.len() && ident_char(b[i]) {
                i += 1;
            }
            let word = &src[start..i];
            if (word == "b" || word == "br" || word == "r")
                && i < b.len()
                && (b[i] == b'\'' || b[i] == b'"' || b[i] == b'#')
            {
                // A byte or raw literal prefix: let the literal branches consume the rest.
                if word == "b" && b[i] == b'\'' {
                    i += 1;
                    while i < b.len() && b[i] != b'\'' {
                        i += if b[i] == b'\\' { 2 } else { 1 };
                    }
                    i += 1;
                    toks.push(Tok::Other);
                } else if let Some(len) = raw_string_len(&b[start..]) {
                    i = start + len;
                    toks.push(Tok::Other);
                } else {
                    toks.push(Tok::Ident(word.to_owned()));
                }
            } else {
                toks.push(Tok::Ident(word.to_owned()));
            }
        } else if b[i..].starts_with(b"::") {
            toks.push(Tok::PathSep);
            i += 2;
        } else {
            toks.push(match c {
                b'{' => Tok::Open,
                b'}' => Tok::Close,
                b',' => Tok::Comma,
                _ => Tok::Other,
            });
            i += 1;
        }
    }
    toks
}

/// The length of a raw (byte) string literal at the start of `b`, if one starts there.
fn raw_string_len(b: &[u8]) -> Option<usize> {
    let mut i = if b.starts_with(b"br") {
        2
    } else if b.starts_with(b"r") {
        1
    } else {
        return None;
    };
    let hashes = b[i..].iter().take_while(|&&c| c == b'#').count();
    i += hashes;
    if b.get(i) != Some(&b'"') {
        return None;
    }
    i += 1;
    let close: Vec<u8> = std::iter::once(b'"')
        .chain(std::iter::repeat_n(b'#', hashes))
        .collect();
    let end = b[i..]
        .windows(close.len())
        .position(|w| w == close.as_slice())?;
    Some(i + end + close.len())
}

/// Parses the path segments after a `::` at `toks[i]`: a plain path yields one list of
/// segments; a group `{a, b::c}` yields one per item (nested groups too).
fn paths_after(toks: &[Tok], i: usize, prefix: &[String], out: &mut Vec<Vec<String>>) {
    let mut segments = prefix.to_vec();
    let mut i = i;
    loop {
        match toks.get(i) {
            Some(Tok::Ident(name)) if name != "self" || segments.is_empty() => {
                segments.push(name.clone());
                if toks.get(i + 1) == Some(&Tok::PathSep) {
                    i += 2;
                    continue;
                }
                out.push(segments);
                return;
            }
            Some(Tok::Open) => {
                let mut depth = 0;
                let mut item_start = i + 1;
                let mut j = i;
                while j < toks.len() {
                    match toks[j] {
                        Tok::Open => depth += 1,
                        Tok::Close => {
                            depth -= 1;
                            if depth == 0 {
                                paths_after(toks, item_start, &segments, out);
                                return;
                            }
                        }
                        Tok::Comma if depth == 1 => {
                            paths_after(toks, item_start, &segments, out);
                            item_start = j + 1;
                        }
                        _ => {}
                    }
                    j += 1;
                }
                return;
            }
            _ => {
                // `self`, `*`, or the end of a group item; with no segments, the path names
                // the root it started from (`crate::*`).
                if segments.is_empty() {
                    segments.push(ROOT.into());
                }
                out.push(segments);
                return;
            }
        }
    }
}

/// Every module path (from the crate root) a file references through `crate::` or `super::`,
/// given the file's own module path. Inline `mod name { … }` blocks deepen `super`.
fn references(src: &str, file_path: &[String]) -> Vec<Vec<String>> {
    let toks = tokenize(src);
    let mut out = Vec::new();
    let mut inline: Vec<(String, usize)> = Vec::new();
    let mut depth = 0usize;
    let mut i = 0;
    while i < toks.len() {
        match &toks[i] {
            Tok::Open => depth += 1,
            Tok::Close => {
                depth = depth.saturating_sub(1);
                while inline.last().is_some_and(|(_, d)| *d > depth) {
                    inline.pop();
                }
            }
            Tok::Ident(word) if word == "mod" => {
                if let (Some(Tok::Ident(name)), Some(Tok::Open)) =
                    (toks.get(i + 1), toks.get(i + 2))
                {
                    inline.push((name.clone(), depth + 1));
                }
            }
            Tok::Ident(word) if word == "crate" && toks.get(i + 1) == Some(&Tok::PathSep) => {
                paths_after(&toks, i + 2, &[], &mut out);
            }
            Tok::Ident(word) if word == "crate" && toks.get(i + 1) == Some(&ident("as")) => {
                out.push(vec![ROOT.into()]);
            }
            Tok::Ident(word)
                if word == "super"
                    && toks.get(i + 1) == Some(&Tok::PathSep)
                    && (i == 0 || toks[i - 1] != Tok::PathSep) =>
            {
                let mut base: Vec<String> = file_path
                    .iter()
                    .cloned()
                    .chain(inline.iter().map(|(name, _)| name.clone()))
                    .collect();
                let mut j = i;
                while toks.get(j) == Some(&Tok::Ident("super".into()))
                    && toks.get(j + 1) == Some(&Tok::PathSep)
                {
                    base.pop();
                    j += 2;
                }
                paths_after(&toks, j, &base, &mut out);
                i = j;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

fn ident(word: &str) -> Tok {
    Tok::Ident(word.into())
}

/// Every `crate::`/`super::` reference in the file at `rel` that leaves its own module, as
/// layering keys.
fn file_references(rel: &Path, src: &str) -> Vec<Reference> {
    let path = module_path(rel);
    let from = key_of(&path).unwrap();
    references(src, &path)
        .into_iter()
        .filter_map(|target| key_of(&target))
        .filter(|to| *to != from)
        .map(|to| Reference {
            file: rel.display().to_string(),
            from: from.clone(),
            to,
        })
        .collect()
}

/// Whether `toks[i]` is the external crate `name` at the start of a path: `name` or
/// `::name`, but not `crate::name` or `os::name`.
fn is_crate(toks: &[Tok], i: usize, name: &str) -> bool {
    let inner = i >= 2 && toks[i - 1] == Tok::PathSep && matches!(toks[i - 2], Tok::Ident(_));
    toks[i] == ident(name) && !inner
}

/// Every layering violation in the file at `rel` (relative to `src/`) with source `src`:
/// internal references the table does not allow, platform crates outside their modules,
/// and banned parts of `std`.
fn check_source(rel: &Path, src: &str) -> Vec<String> {
    let file = rel.display();
    let from = key_of(&module_path(rel)).unwrap();
    let allowed = allowed();
    let keys = module_keys();
    let mut violations: Vec<String> = file_references(rel, src)
        .into_iter()
        .filter(|r| r.from != "cli")
        .filter(|r| {
            !keys.contains(r.to.as_str()) || !allowed[r.from.as_str()].contains(r.to.as_str())
        })
        .map(|r| r.to_string())
        .collect();
    let toks = tokenize(src);
    let banned_std = STD_BANS
        .iter()
        .find(|(module, _)| *module == from)
        .map_or(&[][..], |(_, parts)| *parts);
    for i in 0..toks.len() {
        for (name, modules) in PLATFORM_CRATES {
            if is_crate(&toks, i, name)
                && toks.get(i + 1) == Some(&Tok::PathSep)
                && !modules.contains(&from.as_str())
            {
                violations.push(format!("{file}: {from} uses {name}"));
            }
        }
        if banned_std.is_empty() || !is_crate(&toks, i, "std") {
            continue;
        }
        let mut paths = Vec::new();
        match toks.get(i + 1) {
            Some(Tok::PathSep) => paths_after(&toks, i + 2, &[], &mut paths),
            Some(tok) if *tok == ident("as") => paths.push(vec![ROOT.into()]),
            _ => {}
        }
        for path in paths {
            let part = &path[0];
            if part == ROOT || banned_std.contains(&part.as_str()) {
                violations.push(format!("{file}: {from} uses std::{part}"));
            }
        }
    }
    violations
}

#[test]
fn every_source_file_belongs_to_a_known_module() {
    let keys = module_keys();
    for file in source_files() {
        let rel = file.strip_prefix(src_dir()).unwrap().to_path_buf();
        let key = key_of(&module_path(&rel)).unwrap();
        assert!(
            keys.contains(key.as_str()),
            "{} is in unknown module {key}",
            rel.display()
        );
    }
}

#[test]
fn sources_follow_the_layering() {
    let src = src_dir();
    let violations: Vec<String> = source_files()
        .iter()
        .flat_map(|file| {
            let text = std::fs::read_to_string(file).unwrap();
            check_source(file.strip_prefix(&src).unwrap(), &text)
        })
        .collect();
    assert!(
        violations.is_empty(),
        "layering violations:\n{}",
        violations.join("\n")
    );
}

// The sources follow the table (`sources_follow_the_layering`); these check that the table
// itself keeps the rules of section 2.2.

#[test]
fn admission_has_no_observe_dependency() {
    for module in ADMISSION_PATH {
        for banned in ["observe", "plan", "act", "reconcile", "snapshot"] {
            assert!(
                !allowed()[module].contains(banned),
                "the table lets {module} use {banned}"
            );
        }
    }
}

#[test]
fn admission_has_no_inbox_dependency() {
    for module in ADMISSION_PATH {
        assert!(
            !allowed()[module].contains("inbox"),
            "the table lets {module} use inbox"
        );
    }
}

#[test]
fn foundation_modules_import_nothing_above_them() {
    let allowed = allowed();
    for module in FOUNDATION {
        assert!(
            allowed[module].iter().all(|dep| *dep == "contract"),
            "the table lets {module} use {:?}",
            allowed[module]
        );
    }
}

#[test]
fn checker_rejects_forbidden_sources() {
    let cases = [
        (
            "use crate::*; fn f() { observe::scan(); }",
            "admission -> <root>",
        ),
        ("use super::*;", "admission -> <root>"),
        (
            "use crate as root; fn f() { root::inbox::raise(); }",
            "admission -> <root>",
        ),
        ("use crate::observe as o;", "admission -> observe"),
        ("use crate::{state, inbox::raise};", "admission -> inbox"),
        ("use std::net::TcpStream;", "uses std::net"),
        ("use std::{time::Duration, process};", "uses std::process"),
        (
            "fn f() { ::std::process::Command::new(\"gh\"); }",
            "uses std::process",
        ),
        (
            "use std as s; fn f() { s::net::TcpStream::connect(\"x\"); }",
            "uses std::<root>",
        ),
        ("fn f() { sysinfo::System::new(); }", "uses sysinfo"),
    ];
    for (src, expected) in cases {
        let violations = check_source(Path::new("admission/mod.rs"), src);
        assert!(
            violations.iter().any(|v| v.contains(expected)),
            "{src}: expected {expected}, got {violations:?}"
        );
    }
}

#[test]
fn checker_accepts_allowed_sources() {
    let src = r"
        use crate::{state::StateDir, record::Recorder, contract};
        use super::store;
        use std::time::Duration;
        fn f() { crate::os::process_table(); let _ = os_process::x; }
        mod tests { use super::*; }
    ";
    assert_eq!(
        check_source(Path::new("admission/budget.rs"), src),
        Vec::<String>::new()
    );
    let run = "use std::process::Command; use crate::testpoint;";
    assert_eq!(
        check_source(Path::new("run/mod.rs"), run),
        Vec::<String>::new()
    );
}

fn refs_of(src: &str, path: &[&str]) -> Vec<String> {
    let path: Vec<String> = path.iter().map(|s| (*s).to_owned()).collect();
    references(src, &path)
        .into_iter()
        .filter_map(|target| key_of(&target))
        .collect()
}

#[test]
fn scanner_reads_paths_groups_and_super() {
    let src = r"
        use crate::{state::{self, StateDir}, observe, adapter::claude::hook};
        use crate::inbox::raise as r;
        fn f() { crate::plan::create(); let _ = super::super::act::execute; }
        mod tests { use super::*; use super::super::config::Config; }
    ";
    assert_eq!(
        refs_of(src, &["admission", "store"]),
        [
            "state",
            "state",
            "observe",
            "adapter::claude",
            "inbox",
            "plan",
            "act",
            "admission",
            "admission"
        ]
    );
}

#[test]
fn scanner_ignores_comments_strings_and_lifetimes() {
    let src = r##"
        // use crate::observe;
        /* crate::inbox /* nested crate::plan */ crate::act */
        const A: &str = "crate::observe::x";
        const B: &str = r#"crate::inbox"#;
        const C: &[u8] = b"crate::plan";
        const D: char = '"';
        const E: u8 = b'"';
        fn g<'a>(x: &'a str) -> &'a str { x }
        use crate::state::StateDir;
    "##;
    assert_eq!(refs_of(src, &["admission"]), ["state"]);
}

#[test]
fn scanner_resolves_super_from_a_top_level_mod_rs() {
    let src = "use super::observe::inventory; mod tests { use super::*; }";
    assert_eq!(refs_of(src, &["admission"]), ["observe", "admission"]);
}
