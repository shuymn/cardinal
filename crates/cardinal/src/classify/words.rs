//! Shell knowledge the tokenizer and the stage rules need: keywords, builtins, and the
//! shape of assignments (`docs/architecture.md` section 7.2). This is POSIX, bash, and zsh
//! knowledge, not tool knowledge; tools are only ever named by profiles.

/// Keywords that refuse the whole command in command position: section 7.2's list plus the
/// bash and zsh keywords that open compound commands (`coproc`, `repeat`, `foreach`, `end`).
const REFUSED_KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "function", "select", "coproc", "repeat", "foreach", "end",
];

/// Precommand modifiers: they run the next word as the command.
const PRECOMMANDS: &[&str] = &[
    "time",
    "command",
    "builtin",
    "exec",
    "noglob",
    "nocorrect",
    "-",
];

/// Builtins of POSIX sh, bash, and zsh, plus the keywords and modifiers above. A stage whose
/// command word is one of these runs inside the calling shell or changes it, so it is never
/// wrapped.
const BUILTINS: &[&str] = &[
    // POSIX special and regular builtins
    ".",
    ":",
    "[",
    "alias",
    "bg",
    "break",
    "cd",
    "command",
    "continue",
    "echo",
    "eval",
    "exec",
    "exit",
    "export",
    "false",
    "fc",
    "fg",
    "getopts",
    "hash",
    "jobs",
    "kill",
    "newgrp",
    "printf",
    "pwd",
    "read",
    "readonly",
    "return",
    "set",
    "shift",
    "test",
    "times",
    "trap",
    "true",
    "type",
    "ulimit",
    "umask",
    "unalias",
    "unset",
    "wait",
    // bash
    "bind",
    "builtin",
    "caller",
    "compgen",
    "complete",
    "compopt",
    "declare",
    "dirs",
    "disown",
    "enable",
    "help",
    "history",
    "let",
    "local",
    "logout",
    "mapfile",
    "popd",
    "pushd",
    "readarray",
    "shopt",
    "source",
    "suspend",
    "typeset",
    // zsh
    "autoload",
    "bindkey",
    "bye",
    "cap",
    "chdir",
    "clone",
    "comparguments",
    "compcall",
    "compctl",
    "compdescribe",
    "compfiles",
    "compgroups",
    "compquote",
    "comptags",
    "comptry",
    "compvalues",
    "disable",
    "emulate",
    "float",
    "functions",
    "getcap",
    "getln",
    "integer",
    "limit",
    "log",
    "noglob",
    "nocorrect",
    "print",
    "private",
    "pushln",
    "r",
    "rehash",
    "sched",
    "setcap",
    "setopt",
    "stat",
    "unfunction",
    "unhash",
    "unlimit",
    "unsetopt",
    "vared",
    "whence",
    "where",
    "which",
    "zcompile",
    "zformat",
    "zle",
    "zmodload",
    "zparseopts",
    "zprof",
    "zpty",
    "zregexparse",
    "zsocket",
    "zstyle",
    "ztcp",
];

/// Builtins that can define, remove, or redirect the commands of the calling shell (functions,
/// aliases, hashed paths, builtins), source code into it, or set traps: a later wrapped stage
/// could then run something else than the same stage unwrapped. `mapfile` and `readarray`
/// (whose `-C` runs a command for each line), `vared`, and `zparseopts` assign to the names
/// their arguments give, in a grammar of options not modeled here.
const CALLER_CHANGING: &[&str] = &[
    "eval",
    ".",
    "source",
    "alias",
    "unalias",
    "unset",
    "functions",
    "unfunction",
    "autoload",
    "enable",
    "disable",
    "hash",
    "unhash",
    "rehash",
    "trap",
    "mapfile",
    "readarray",
    "vared",
    "zparseopts",
];

/// Builtins that declare their name arguments; their `f` option defines, exports, or marks
/// functions, and `n` makes a name refer to another (bash's namerefs).
const TYPESET: &[&str] = &[
    "typeset", "declare", "local", "export", "readonly", "integer", "float", "private",
];

/// bash and zsh arrays whose elements are the shell's functions, aliases, command paths,
/// options, builtins, reserved words, or parameters.
const CALLER_ARRAYS: &[&str] = &[
    "functions",
    "dis_functions",
    "functions_source",
    "dis_functions_source",
    "aliases",
    "dis_aliases",
    "galiases",
    "dis_galiases",
    "saliases",
    "dis_saliases",
    "commands",
    "options",
    "builtins",
    "dis_builtins",
    "reswords",
    "dis_reswords",
    "patchars",
    "dis_patchars",
    "parameters",
    "BASH_ALIASES",
    "BASH_CMDS",
];

/// Options that refuse the command when it sets or unsets them. The first four make the
/// shell print the commands it runs, so the rewrite's fallback test and runner would show up
/// in the output. The rest change how the calling shell finds a command, which the runner,
/// searching `PATH` afresh, does not follow: zsh's `PATH_DIRS` searches `PATH` for a command
/// word with a slash (`tools/probe`), the hashing options (zsh's `HASH_CMDS`, `HASH_DIRS`,
/// `HASH_EXECUTABLES_ONLY`, bash's `hashall` and `checkhash`) decide whether the shell runs
/// a remembered path instead, and `restricted` refuses the runner's path. bash's `keyword`
/// moves a stage's `NAME=value` arguments into the runner's environment.
const REFUSED_OPTIONS: &[&str] = &[
    "xtrace",
    "verbose",
    "functrace",
    "extdebug",
    "pathdirs",
    "hashcmds",
    "hashdirs",
    "hashexecutablesonly",
    "hashall",
    "checkhash",
    "restricted",
    "keyword",
];

/// The letters of [`REFUSED_OPTIONS`] for `set`: `x`, `v`, bash's `T` (`functrace`) and `h`
/// (`hashall`) and `k` (`keyword`), zsh's `Q` (`PATH_DIRS`), and `r` (restricted) in both.
const REFUSED_SET_LETTERS: [char; 7] = ['x', 'v', 'T', 'h', 'k', 'Q', 'r'];

/// The letters of [`REFUSED_OPTIONS`] for zsh's `setopt` and `unsetopt`.
const REFUSED_SETOPT_LETTERS: [char; 4] = ['x', 'v', 'Q', 'r'];

/// Whether `name`, compared as zsh compares option names (case and underscores ignored,
/// without a `no` prefix), is one of [`REFUSED_OPTIONS`].
fn is_refused_option(name: &str) -> bool {
    let name: String = name
        .chars()
        .filter(|c| *c != '_')
        .flat_map(char::to_lowercase)
        .collect();
    let name = name.strip_prefix("no").unwrap_or(&name);
    REFUSED_OPTIONS.contains(&name)
}

fn is_identifier(word: &str) -> bool {
    word.bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && word.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Whether a name argument (`None` when it contains an expansion) can name something other
/// than a plain variable: anything but an identifier, or one of the shells' tables.
fn unsafe_name(name: Option<&str>) -> bool {
    name.is_none_or(|name| !is_identifier(name) || CALLER_ARRAYS.contains(&name))
}

/// Whether running the builtin `name` with `args` (each as written and, without an
/// expansion, after quote removal) can change what a later stage means or make the shell
/// trace commands. A word with an expansion counts as one that does.
pub(super) fn changes_caller(name: &str, args: &[(&str, Option<&str>)]) -> bool {
    if CALLER_CHANGING.contains(&name) {
        return true;
    }
    let values = || args.iter().map(|(_, value)| *value);
    match name {
        _ if TYPESET.contains(&name) => typeset_changes_caller(name, args),
        "printf" | "print" => print_target_unsafe(name == "print", args),
        // zsh's `name?prompt` names the first variable and gives its prompt.
        "read" => values().any(|value| {
            value.is_none_or(|value| {
                !value.starts_with('-')
                    && unsafe_name(Some(value.split_once('?').map_or(value, |(n, _)| n)))
            })
        }),
        "getopts" => args.get(1).is_some_and(|(_, value)| unsafe_name(*value)),
        "let" => values().any(|value| value.is_none_or(|value| value.contains('['))),
        "emulate" => emulate_changes_caller(args),
        "set" => {
            let mut words = values().take_while(|value| *value != Some("--"));
            while let Some(value) = words.next() {
                let Some(value) = value else { return true };
                let Some(letters) = value.strip_prefix(['-', '+']) else {
                    break;
                };
                if letters.contains(REFUSED_SET_LETTERS) {
                    return true;
                }
                // zsh's `set -A name …` assigns an array.
                if letters.contains('A') && words.next().is_some_and(unsafe_name) {
                    return true;
                }
                if letters.contains('o') {
                    match words.next() {
                        Some(Some(option)) if !is_refused_option(option) => {}
                        None => {}
                        Some(_) => return true,
                    }
                }
            }
            false
        }
        "setopt" | "unsetopt" => values().any(|value| {
            value.is_none_or(|value| {
                is_refused_option(value)
                    || value
                        .strip_prefix(['-', '+'])
                        .is_some_and(|letters| letters.contains(REFUSED_SETOPT_LETTERS))
            })
        }),
        "shopt" => values().any(|value| value.is_none_or(is_refused_option)),
        _ => false,
    }
}

/// `typeset` and its relatives: an option `f` (or `n`, except for `export`, where it removes
/// the export), a name argument that is not a plain identifier outside the tables, before or
/// after `=`, options or not, or removing the export of `PATH` (`export -n PATH`, `typeset
/// +x PATH`): the shell would still find commands in its `PATH`, but the runner, which gets
/// only the exported environment, would not.
fn typeset_changes_caller(name: &str, args: &[(&str, Option<&str>)]) -> bool {
    let mut options = true;
    let mut unexports = false;
    let mut names_path = false;
    let unsafe_arg = args.iter().any(|(raw, value)| {
        if options && *value == Some("--") {
            options = false;
            return false;
        }
        if is_assignment(raw) || value.is_some_and(is_assignment) {
            options = false;
            let word = value.unwrap_or(raw);
            names_path |= leading_name(word) == "PATH";
            return assigns_unsafe_name(word);
        }
        let Some(value) = value else { return true };
        if options && let Some(letters) = value.strip_prefix(['-', '+']) {
            unexports |= if value.starts_with('+') {
                letters.contains('x')
            } else {
                name == "export" && letters.contains('n')
            };
            return letters.contains('f') || (name != "export" && letters.contains('n'));
        }
        options = false;
        names_path |= *value == "PATH";
        unsafe_name(Some(value))
    });
    unsafe_arg || (unexports && names_path)
}

/// The name at the start of an assignment `word`: up to its subscript, `+=`, or `=`.
fn leading_name(word: &str) -> &str {
    let len = word
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
        .count();
    &word[..len]
}

/// `printf -v NAME` (and zsh's `print -v NAME`) assigns the output to `NAME`; a word with
/// an expansion where options go could be `-v`. `print` options that take an argument
/// (`-u`, `-f`, `-C`, `-x`, `-X`) take the next word when they end their word.
fn print_target_unsafe(print: bool, args: &[(&str, Option<&str>)]) -> bool {
    let mut i = 0;
    while let Some((_, value)) = args.get(i) {
        let Some(word) = value else {
            return args.get(i + 1).is_some_and(|(_, next)| unsafe_name(*next));
        };
        let Some(letters) = word
            .strip_prefix('-')
            .filter(|l| !l.is_empty() && *l != "-")
        else {
            return false;
        };
        if let Some((_, rest)) = letters.split_once('v') {
            return if rest.is_empty() {
                args.get(i + 1).is_some_and(|(_, next)| unsafe_name(*next))
            } else {
                unsafe_name(Some(rest))
            };
        }
        i += 1;
        if print && letters.ends_with(['u', 'f', 'C', 'x', 'X']) {
            i += 1;
        }
    }
    false
}

/// zsh's `emulate [-lLR] [MODE [FLAGS…]]`: its flags set options as the shell's own do
/// (`-x` traces), and `-c CODE` runs code, so anything after the mode refuses the command.
fn emulate_changes_caller(args: &[(&str, Option<&str>)]) -> bool {
    let mut words = args.iter().map(|(_, value)| *value);
    let mut next = words.next();
    while let Some(Some(word)) = next {
        match word.strip_prefix('-') {
            Some(letters)
                if !letters.is_empty() && letters.bytes().all(|b| b"lLR".contains(&b)) =>
            {
                next = words.next();
            }
            _ => break,
        }
    }
    match next {
        None => false,
        Some(Some(mode)) if !mode.starts_with(['-', '+']) => words.next().is_some(),
        Some(_) => true,
    }
}

/// Whether the assignment `word` (as written or after quote removal) sets an element of an
/// array, whose subscript is arithmetic that can assign anything, or one of the shells'
/// tables of functions, aliases, command paths, or options.
pub(super) fn assigns_unsafe_name(word: &str) -> bool {
    let name = leading_name(word);
    word[name.len()..].starts_with('[') || CALLER_ARRAYS.contains(&name)
}

/// Whether `text` (a word after quote removal, without its expansions) names an element of
/// one of the shells' tables (`functions[`), which arithmetic can assign.
pub(super) fn names_table_element(text: &str) -> bool {
    CALLER_ARRAYS.iter().any(|table| {
        text.match_indices(table).any(|(at, _)| {
            text[at + table.len()..].starts_with('[')
                && !text[..at]
                    .bytes()
                    .next_back()
                    .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
    })
}

/// Whether `word` (as written) is a keyword that refuses the whole command in command position.
pub(super) fn is_refused_keyword(word: &str) -> bool {
    REFUSED_KEYWORDS.contains(&word)
}

/// Whether `word` (after quote removal) is a precommand modifier.
pub(super) fn is_precommand(word: &str) -> bool {
    PRECOMMANDS.contains(&word)
}

/// Whether a stage with command word `word` (after quote removal) must stay unwrapped because
/// the shell runs it itself: a builtin, keyword, or precommand modifier.
pub(super) fn is_shell_command(word: &str) -> bool {
    BUILTINS.contains(&word)
        || PRECOMMANDS.contains(&word)
        || REFUSED_KEYWORDS.contains(&word)
        || word == "[["
}

/// Whether `word` (as written) is an assignment: `NAME=…`, `NAME+=…`, or `NAME[…]=…`.
pub(super) fn is_assignment(word: &str) -> bool {
    let bytes = word.as_bytes();
    if !bytes
        .first()
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
    {
        return false;
    }
    let mut i = bytes
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
        .count();
    if bytes.get(i) == Some(&b'[') {
        match bytes[i..].iter().position(|&b| b == b']') {
            Some(close) => i += close + 1,
            None => return false,
        }
    }
    if bytes.get(i) == Some(&b'+') {
        i += 1;
    }
    bytes.get(i) == Some(&b'=')
}

#[cfg(test)]
mod tests {
    use super::is_assignment;

    #[test]
    fn assignments() {
        for word in ["A=1", "_x=", "A_1+=x", "arr[2]=v", "A='q r'"] {
            assert!(is_assignment(word), "{word}");
        }
        for word in [
            "=x", "1A=2", "A", "'A'=1", "A[1=2", "a-b=1", "--x=1", "A+x=1",
        ] {
            assert!(!is_assignment(word), "{word}");
        }
    }
}
