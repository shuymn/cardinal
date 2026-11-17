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

/// Whether `word` (as written) is a keyword that refuses the whole command in command position.
pub(super) fn is_refused_keyword(word: &str) -> bool {
    REFUSED_KEYWORDS.contains(&word)
}

/// Whether `word` (as written) is a precommand modifier.
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
