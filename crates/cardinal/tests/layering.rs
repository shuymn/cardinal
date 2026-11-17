//! Module layering (`docs/architecture.md` section 2.2). Every module of `src/` may reference
//! only the modules the table allows, transitively: a module may use whatever the modules it
//! may use may use. On top of the table, `testpoint` (a leaf with no dependencies) is open to
//! every module above the foundation, because its pause points sit in `run`, `act`, and the
//! hook. Platform crates stay where the table puts them.
//!
//! The scanner reads `crate::…` and `super::…` paths (including grouped `use` trees) from the
//! source with comments and string literals removed.

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
    ("sensor", &["contract"]),
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
        "explain",
        &["classify", "record", "plan", "inbox", "state", "contract"],
    ),
];

/// `contract`, `state`, `config`, `testpoint`, and `os` import nothing above them.
const FOUNDATION: &[&str] = &["contract", "state", "config", "testpoint", "os"];

/// The admission path: it never reaches observation, the inbox, plans, or `act` (C17, C18).
const ADMISSION_PATH: &[&str] = &["admission", "run", "exec"];

/// External crates that only some modules may use.
const PLATFORM_CRATES: &[(&str, &[&str])] = &[
    ("sysinfo", &["os"]),
    ("signal_hook", &["os"]),
    ("rustix", &["os", "act"]),
];

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
                // `self`, `*`, or the end of a group item.
                if !segments.is_empty() {
                    out.push(segments);
                }
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

/// Every `crate::`/`super::` reference in `src/` that leaves its own module, as layering keys.
fn module_references() -> Vec<Reference> {
    let src = src_dir();
    let mut refs = Vec::new();
    for file in source_files() {
        let rel = file.strip_prefix(&src).unwrap();
        let path = module_path(rel);
        let from = key_of(&path).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        for target in references(&text, &path) {
            let Some(to) = key_of(&target) else { continue };
            if to != from {
                refs.push(Reference {
                    file: rel.display().to_string(),
                    from: from.clone(),
                    to,
                });
            }
        }
    }
    refs
}

/// Every use of a platform crate, as (file, module key, crate).
fn crate_uses() -> Vec<(String, String, &'static str)> {
    let src = src_dir();
    let mut uses = Vec::new();
    for file in source_files() {
        let rel = file.strip_prefix(&src).unwrap();
        let from = key_of(&module_path(rel)).unwrap();
        let toks = tokenize(&std::fs::read_to_string(&file).unwrap());
        for (i, tok) in toks.iter().enumerate() {
            for (name, _) in PLATFORM_CRATES {
                // `rustix::…` or `::rustix::…`, but not `crate::rustix` or `os::rustix`.
                let inner =
                    i >= 2 && toks[i - 1] == Tok::PathSep && matches!(toks[i - 2], Tok::Ident(_));
                if *tok == Tok::Ident((*name).into())
                    && toks.get(i + 1) == Some(&Tok::PathSep)
                    && !inner
                {
                    uses.push((rel.display().to_string(), from.clone(), *name));
                }
            }
        }
    }
    uses
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
fn module_dependencies_follow_the_table() {
    let allowed = allowed();
    let keys = module_keys();
    let violations: Vec<String> = module_references()
        .into_iter()
        .filter(|r| r.from != "cli")
        .filter(|r| {
            !keys.contains(r.to.as_str()) || !allowed[r.from.as_str()].contains(r.to.as_str())
        })
        .map(|r| r.to_string())
        .collect();
    assert!(
        violations.is_empty(),
        "forbidden module references:\n{}",
        violations.join("\n")
    );
}

#[test]
fn admission_has_no_observe_dependency() {
    let violations: Vec<String> = module_references()
        .into_iter()
        .filter(|r| ADMISSION_PATH.contains(&r.from.as_str()))
        .filter(|r| ["observe", "plan", "act"].contains(&r.to.as_str()))
        .map(|r| r.to_string())
        .collect();
    assert!(
        violations.is_empty(),
        "the admission path reaches observation:\n{}",
        violations.join("\n")
    );
    for module in ADMISSION_PATH {
        for banned in ["observe", "plan", "act"] {
            assert!(
                !allowed()[module].contains(banned),
                "the table lets {module} use {banned}"
            );
        }
    }
}

#[test]
fn admission_has_no_inbox_dependency() {
    let violations: Vec<String> = module_references()
        .into_iter()
        .filter(|r| ADMISSION_PATH.contains(&r.from.as_str()) && r.to == "inbox")
        .map(|r| r.to_string())
        .collect();
    assert!(
        violations.is_empty(),
        "the admission path reaches the inbox:\n{}",
        violations.join("\n")
    );
    for module in ADMISSION_PATH {
        assert!(
            !allowed()[module].contains("inbox"),
            "the table lets {module} use inbox"
        );
    }
}

#[test]
fn foundation_modules_import_nothing_above_them() {
    let violations: Vec<String> = module_references()
        .into_iter()
        .filter(|r| FOUNDATION.contains(&r.from.as_str()) && r.to != "contract")
        .map(|r| r.to_string())
        .collect();
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn platform_crates_stay_in_their_modules() {
    let violations: Vec<String> = crate_uses()
        .into_iter()
        .filter(|(_, from, name)| {
            PLATFORM_CRATES.iter().any(|(crate_name, modules)| {
                crate_name == name && !modules.contains(&from.as_str())
            })
        })
        .map(|(file, from, name)| format!("{file}: {from} uses {name}"))
        .collect();
    assert!(violations.is_empty(), "{}", violations.join("\n"));
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
