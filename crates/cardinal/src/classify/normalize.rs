//! Normalization (`docs/architecture.md` section 7.3): the argv a stage is classified by.
//! It decides the class only; the rewrite always uses the stage's own words.

use super::tokenize;
use super::words::is_assignment;

/// How many wrappers a stage may nest before normalization gives up.
const MAX_UNWRAP: usize = 16;

/// One word of a stage: its text as written and, when it contains no expansion, its value.
#[derive(Clone, Copy, Debug)]
pub(super) struct Arg<'a> {
    pub raw: &'a str,
    pub value: Option<&'a str>,
}

impl<'a> Arg<'a> {
    /// The value, or the text as written for a word with an expansion, which then only
    /// matches a pattern's `*`.
    fn text(self) -> &'a str {
        self.value.unwrap_or(self.raw)
    }

    fn literal(self) -> Option<&'a str> {
        self.value
    }
}

/// The base name of a command word: `./node_modules/.bin/vitest` → `vitest`.
pub(super) fn base_name(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// The candidate argvs of a stage, in the order classification tries them. A stage has one
/// candidate, except `sh -c STRING`, which has one per simple command in `STRING`. No
/// candidate means the stage cannot be classified.
pub(super) fn candidates(args: &[Arg<'_>], wrappers: &[Vec<String>]) -> Vec<Vec<String>> {
    candidates_at(args, wrappers, 0)
}

fn candidates_at(args: &[Arg<'_>], wrappers: &[Vec<String>], depth: usize) -> Vec<Vec<String>> {
    // 1. Leading assignments.
    let mut rest = args;
    while rest.first().is_some_and(|a| is_assignment(a.raw)) {
        rest = &rest[1..];
    }
    for _ in 0..MAX_UNWRAP {
        let Some(first) = rest.first() else {
            return Vec::new();
        };
        let Some(word) = first.literal() else {
            return Vec::new();
        };
        let base = base_name(word);
        // 2. Shell-level wrappers.
        if base == "env" {
            match unwrap_env(&rest[1..]) {
                Some(inner) => rest = inner,
                None => return Vec::new(),
            }
            continue;
        }
        if base == "nice" {
            match unwrap_nice(&rest[1..]) {
                Some(inner) => rest = inner,
                None => return Vec::new(),
            }
            continue;
        }
        // 3. `sh -c STRING`, one level deep.
        if depth == 0 && matches!(base, "sh" | "bash" | "zsh" | "dash") {
            match shell_script(&rest[1..]) {
                Script::None => {}
                Script::Opaque => return Vec::new(),
                Script::Literal(script) => {
                    let Ok(stages) = tokenize::stages(script) else {
                        return Vec::new();
                    };
                    return stages
                        .iter()
                        .flat_map(|stage| candidates_at(&stage.args(), wrappers, depth + 1))
                        .collect();
                }
            }
        }
        // 4. Tool wrappers from the profile.
        if let Some(inner) = unwrap_tool(rest, wrappers) {
            rest = inner;
            continue;
        }
        break;
    }
    if rest.is_empty() || rest[0].literal().is_none() {
        return Vec::new();
    }
    // 5. Base name of argv[0], and no `+toolchain` after cargo.
    let mut normalized: Vec<String> = rest.iter().map(|a| a.text().to_owned()).collect();
    let base = base_name(&normalized[0]).to_owned();
    normalized[0] = base;
    if normalized[0] == "cargo" && normalized.get(1).is_some_and(|w| w.starts_with('+')) {
        normalized.remove(1);
    }
    vec![normalized]
}

/// `env` with assignments and `-u NAME` only.
fn unwrap_env<'s, 'a>(mut rest: &'s [Arg<'a>]) -> Option<&'s [Arg<'a>]> {
    loop {
        let first = rest.first()?;
        if is_assignment(first.raw) {
            rest = &rest[1..];
            continue;
        }
        let word = first.literal()?;
        if word == "-u" {
            rest.get(1)?.literal()?;
            rest = &rest[2..];
        } else if word.starts_with('-') {
            return None;
        } else {
            return Some(rest);
        }
    }
}

/// `nice` with an optional `-n N`.
fn unwrap_nice<'s, 'a>(rest: &'s [Arg<'a>]) -> Option<&'s [Arg<'a>]> {
    let first = rest.first()?.literal()?;
    if first == "-n" {
        rest.get(1)?.literal()?;
        Some(&rest[2..])
    } else if first.starts_with('-') {
        None
    } else {
        Some(rest)
    }
}

/// The script argument of a shell invocation.
enum Script<'a> {
    /// Not `-c STRING` with options from `-c`, `-e`, `-l`: the shell is classified as itself.
    None,
    /// `-c` with a script that contains an expansion.
    Opaque,
    Literal(&'a str),
}

/// The script of `sh -c STRING` with options from `-c`, `-e`, `-l`, clustered or not.
fn shell_script<'a>(rest: &[Arg<'a>]) -> Script<'a> {
    let mut command = false;
    let mut i = 0;
    while let Some(arg) = rest.get(i) {
        let Some(flags) = arg.literal().and_then(|word| word.strip_prefix('-')) else {
            break;
        };
        if flags.is_empty() || !flags.bytes().all(|b| matches!(b, b'c' | b'e' | b'l')) {
            return Script::None;
        }
        command |= flags.contains('c');
        i += 1;
    }
    match rest.get(i) {
        Some(arg) if command => arg.literal().map_or(Script::Opaque, Script::Literal),
        _ => Script::None,
    }
}

/// Drops the first matching wrapper's words, then words starting with `-` until the first
/// that does not.
fn unwrap_tool<'s, 'a>(rest: &'s [Arg<'a>], wrappers: &[Vec<String>]) -> Option<&'s [Arg<'a>]> {
    let wrapper = wrappers.iter().find(|wrapper| {
        wrapper.len() < rest.len()
            && wrapper.iter().zip(rest).enumerate().all(|(i, (w, arg))| {
                arg.literal().is_some_and(|value| {
                    let value = if i == 0 { base_name(value) } else { value };
                    value == w
                })
            })
    })?;
    let mut inner = &rest[wrapper.len()..];
    while inner
        .first()
        .is_some_and(|a| a.literal().is_some_and(|w| w.starts_with('-')))
    {
        inner = &inner[1..];
    }
    Some(inner)
}
