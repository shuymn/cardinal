//! The subset tokenizer (`docs/architecture.md` section 7.2): splits a command string into
//! stages with byte spans, or refuses the whole command when a span cannot be delimited with
//! certainty or the outer shell could change what a stage means.
//!
//! Every byte the shell treats specially is ASCII, so the scanner works on bytes and only
//! ever cuts at ASCII positions, which are UTF-8 character boundaries.

use std::ops::Range;

use super::Stage;
use super::words::{
    assigns_unsafe_name, changes_caller, is_assignment, is_precommand, is_refused_keyword,
    names_table_element,
};

/// Why a command is left unrewritten as a whole.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("unbalanced quote")]
    UnbalancedQuote,
    #[error("`{0}` is outside the supported shell subset")]
    Unsupported(&'static str),
    #[error("expansion `{0}` is not a plain `$NAME` or `${{NAME}}`")]
    Expansion(String),
    #[error("keyword `{0}` in command position")]
    Keyword(String),
    #[error("`{0}` can change what later commands mean or trace them")]
    ChangesShell(String),
    #[error("command word `{0}` is not a literal")]
    NotLiteral(String),
    #[error("empty command before `{0}`")]
    EmptyStage(&'static str),
    #[error("the command ends with `{0}`")]
    Dangling(&'static str),
    #[error("redirection without a target")]
    MissingTarget,
}

/// One word as the scanner sees it.
#[derive(Clone, Debug)]
struct Word {
    span: Range<usize>,
    /// The word after quote removal; `None` when it contains an expansion or an unquoted
    /// pattern character.
    value: Option<String>,
    /// The word's text after quote removal without its expansions.
    bare: String,
    /// Expands a parameter whose value the rewrite changes or no two runs share (`$?`,
    /// `$_`, …).
    state: bool,
    /// Contains a newline (inside quotes).
    newline: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    AndIf,
    OrIf,
    Semi,
    Pipe,
    Amp,
    Newline,
}

impl Op {
    fn text(self) -> &'static str {
        match self {
            Self::AndIf => "&&",
            Self::OrIf => "||",
            Self::Semi => ";",
            Self::Pipe => "|",
            Self::Amp => "&",
            Self::Newline => "newline",
        }
    }
}

#[derive(Clone, Debug)]
enum Token {
    Word(Word),
    /// A redirection: optional IO number, operator, and target word.
    Redirect {
        span: Range<usize>,
        target: Word,
        /// The file descriptors it redirects.
        fds: Vec<u32>,
    },
    Op(Op),
}

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

fn is_blank(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

/// Bytes that end an unquoted word.
fn is_meta(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\t' | b'\n' | b'|' | b'&' | b';' | b'<' | b'>' | b'(' | b')'
    )
}

fn is_name_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_name_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Special parameters a plain expansion may name besides a name: positional parameters and
/// the shell's own (`$#`, `$@`, `$?`, …).
fn is_special_parameter(b: u8) -> bool {
    b.is_ascii_digit() || matches!(b, b'#' | b'@' | b'*' | b'?' | b'!' | b'-' | b'$')
}

/// Parameters whose plain expansion keeps a stage unwrapped (section 7.2). The fallback
/// test sets the status (`?`, zsh's `status`), the last argument (`_`), the pipeline
/// statuses (`PIPESTATUS`, `pipestatus`), and, when the runner is missing, the error number
/// (zsh's `ERRNO`); bash's `BASH_COMMAND` and the `-c` string (`BASH_EXECUTION_STRING`,
/// `ZSH_EXECUTION_STRING`) hold command text the rewrite changes. The rest are computed per
/// reference from the process, a clock, or a generator, so no two runs share them and no
/// test can show that a rewrite keeps them.
fn is_state_parameter(name: &str) -> bool {
    matches!(
        name,
        "?" | "_"
            | "status"
            | "PIPESTATUS"
            | "pipestatus"
            | "ERRNO"
            | "BASH_COMMAND"
            | "BASH_EXECUTION_STRING"
            | "ZSH_EXECUTION_STRING"
            | "$"
            | "!"
            | "BASHPID"
            | "RANDOM"
            | "SRANDOM"
            | "SECONDS"
            | "EPOCHSECONDS"
            | "EPOCHREALTIME"
    )
}

/// The length of the parameter at the start of a `${…}` body that is all parameter: a name,
/// digits, or one special parameter. 0 when there is none.
fn plain_parameter_len(body: &[u8]) -> usize {
    match body.first() {
        Some(&b) if is_name_start(b) => body.iter().take_while(|&&b| is_name_char(b)).count(),
        Some(b) if b.is_ascii_digit() => body.iter().take_while(|b| b.is_ascii_digit()).count(),
        Some(&b) if is_special_parameter(b) => 1,
        _ => 0,
    }
}

/// The end of the `${…}` that starts at `start`: just after its closing brace, counting
/// nested braces, or the end of `bytes` when it has none.
fn braced_end(bytes: &[u8], start: usize) -> usize {
    let mut depth = 0_usize;
    for (i, &b) in bytes.iter().enumerate().skip(start + 1) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
    }
    bytes.len()
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
        }
    }

    fn peek(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    /// The character starting at byte `at`, which the caller has seen with [`Self::peek`].
    fn char_at(&self, at: usize) -> char {
        self.src[at..]
            .chars()
            .next()
            .unwrap_or(char::REPLACEMENT_CHARACTER)
    }

    fn tokens(mut self) -> Result<Vec<Token>, Refusal> {
        let mut tokens = Vec::new();
        loop {
            while self.peek(0).is_some_and(is_blank) {
                self.pos += 1;
            }
            let Some(b) = self.peek(0) else { break };
            match b {
                b'#' => {
                    while self.peek(0).is_some_and(|b| b != b'\n') {
                        self.pos += 1;
                    }
                }
                b'\n' => {
                    self.pos += 1;
                    tokens.push(Token::Op(Op::Newline));
                }
                b';' => {
                    if self.peek(1) == Some(b';') {
                        return Err(Refusal::Unsupported(";;"));
                    }
                    self.pos += 1;
                    tokens.push(Token::Op(Op::Semi));
                }
                b'&' => match self.peek(1) {
                    Some(b'&') => {
                        self.pos += 2;
                        tokens.push(Token::Op(Op::AndIf));
                    }
                    Some(b'>') => tokens.push(self.redirect(self.pos)?),
                    _ => {
                        self.pos += 1;
                        tokens.push(Token::Op(Op::Amp));
                    }
                },
                b'|' => match self.peek(1) {
                    Some(b'|') => {
                        self.pos += 2;
                        tokens.push(Token::Op(Op::OrIf));
                    }
                    Some(b'&') => return Err(Refusal::Unsupported("|&")),
                    _ => {
                        self.pos += 1;
                        tokens.push(Token::Op(Op::Pipe));
                    }
                },
                b'<' | b'>' => tokens.push(self.redirect(self.pos)?),
                b'(' => return Err(Refusal::Unsupported("(")),
                b')' => return Err(Refusal::Unsupported(")")),
                _ => {
                    // An IO number: digits immediately followed by `<` or `>`.
                    let digits = self.bytes[self.pos..]
                        .iter()
                        .take_while(|b| b.is_ascii_digit())
                        .count();
                    if digits > 0 && matches!(self.peek(digits), Some(b'<' | b'>')) {
                        let start = self.pos;
                        self.pos += digits;
                        tokens.push(self.redirect(start)?);
                    } else {
                        tokens.push(Token::Word(self.word()?));
                    }
                }
            }
        }
        Ok(tokens)
    }

    /// A redirection starting at `start` (its IO number, if any, is already consumed); the
    /// operator begins at `self.pos`.
    fn redirect(&mut self, start: usize) -> Result<Token, Refusal> {
        let number = if start < self.pos {
            let digits = &self.src[start..self.pos];
            Some(
                digits
                    .parse::<u32>()
                    .map_err(|_| Refusal::Unsupported("file descriptor number"))?,
            )
        } else {
            None
        };
        let fds = match self.peek(0) {
            Some(b'&') => vec![1, 2],
            Some(b'<') => vec![number.unwrap_or(0)],
            _ => vec![number.unwrap_or(1)],
        };
        let op_len = match (self.peek(0), self.peek(1), self.peek(2)) {
            (Some(b'&'), Some(b'>'), Some(b'>')) => 3,
            (Some(b'>' | b'&'), Some(b'>'), _) | (Some(b'>' | b'<'), Some(b'&'), _) => 2,
            (Some(b'<'), Some(b'<'), _) => return Err(Refusal::Unsupported("<<")),
            (Some(b'<'), Some(b'>'), _) => return Err(Refusal::Unsupported("<>")),
            (Some(b'<' | b'>'), Some(b'('), _) => return Err(Refusal::Unsupported("(")),
            (Some(b'>'), Some(b'|'), _) => return Err(Refusal::Unsupported(">|")),
            _ => 1,
        };
        self.pos += op_len;
        // Combinations such as `>>&`, `>&|`, or `>!` are not in the subset.
        if self
            .peek(0)
            .is_some_and(|b| matches!(b, b'<' | b'>' | b'&' | b'|' | b'!' | b'('))
        {
            return Err(Refusal::Unsupported("redirection operator"));
        }
        while self.peek(0).is_some_and(is_blank) {
            self.pos += 1;
        }
        match self.peek(0) {
            Some(b) if !is_meta(b) && b != b'#' => {}
            _ => return Err(Refusal::MissingTarget),
        }
        let target = self.word()?;
        Ok(Token::Redirect {
            span: start..target.span.end,
            target,
            fds,
        })
    }

    fn word(&mut self) -> Result<Word, Refusal> {
        let start = self.pos;
        let mut value = String::new();
        let mut literal = true;
        let mut state = false;
        let mut newline = false;
        while let Some(b) = self.peek(0) {
            if is_meta(b) {
                break;
            }
            match b {
                b'\'' => {
                    let body = self.pos + 1;
                    let end = self.bytes[body..]
                        .iter()
                        .position(|&b| b == b'\'')
                        .ok_or(Refusal::UnbalancedQuote)?
                        + body;
                    let text = &self.src[body..end];
                    newline |= text.contains('\n');
                    value.push_str(text);
                    self.pos = end + 1;
                }
                b'"' => {
                    self.pos += 1;
                    self.double_quoted(&mut value, &mut literal, &mut state, &mut newline)?;
                }
                b'\\' => match self.peek(1) {
                    None => return Err(Refusal::Unsupported("trailing backslash")),
                    Some(b'\n') => return Err(Refusal::Unsupported("line continuation")),
                    Some(_) => {
                        let ch = self.char_at(self.pos + 1);
                        value.push(ch);
                        self.pos += 1 + ch.len_utf8();
                    }
                },
                b'`' => return Err(Refusal::Unsupported("`")),
                b'{' => return Err(Refusal::Unsupported("{")),
                b'}' => return Err(Refusal::Unsupported("}")),
                b'!' => return Err(Refusal::Unsupported("!")),
                b'[' if self.peek(1) == Some(b'[') => return Err(Refusal::Unsupported("[[")),
                b'$' => self.dollar(false, &mut value, &mut literal, &mut state)?,
                _ => {
                    let ch = self.char_at(self.pos);
                    literal &= !self.is_pattern(ch, start);
                    value.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
        Ok(Word {
            span: start..self.pos,
            bare: value.clone(),
            value: literal.then_some(value),
            state,
            newline,
        })
    }

    /// Whether the unquoted `ch` at `self.pos`, in a word that starts at `start`, makes the
    /// word a pathname pattern, which the shell may expand to anything or (with `nullglob`)
    /// to nothing: `*`, `?`, a `[` closed later in the word, and zsh's extended `^`, and `#`
    /// or `~` after the word's start.
    fn is_pattern(&self, ch: char, start: usize) -> bool {
        match ch {
            '*' | '?' | '^' => true,
            '#' | '~' => self.pos > start,
            '[' => self.bytes[self.pos + 1..]
                .iter()
                .take_while(|&&b| !is_meta(b))
                .any(|&b| b == b']'),
            _ => false,
        }
    }

    /// The body of a double-quoted string; `self.pos` is just after the opening quote.
    fn double_quoted(
        &mut self,
        value: &mut String,
        literal: &mut bool,
        state: &mut bool,
        newline: &mut bool,
    ) -> Result<(), Refusal> {
        loop {
            let Some(b) = self.peek(0) else {
                return Err(Refusal::UnbalancedQuote);
            };
            match b {
                b'"' => {
                    self.pos += 1;
                    return Ok(());
                }
                b'\\' => match self.peek(1) {
                    None => return Err(Refusal::UnbalancedQuote),
                    Some(b'\n') => return Err(Refusal::Unsupported("line continuation")),
                    Some(next @ (b'$' | b'`' | b'"' | b'\\')) => {
                        value.push(char::from(next));
                        self.pos += 2;
                    }
                    Some(_) => {
                        value.push('\\');
                        self.pos += 1;
                    }
                },
                b'`' => return Err(Refusal::Unsupported("`")),
                b'$' => self.dollar(true, value, literal, state)?,
                _ => {
                    let ch = self.char_at(self.pos);
                    *newline |= ch == '\n';
                    value.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
    }

    /// A `$` at `self.pos`, unquoted or inside double quotes. Only plain parameter
    /// expansions pass (section 7.2): `$NAME` and `${NAME}`, and the same for positional
    /// and special parameters (`$1`, `${#}`, `$?`); every other expansion refuses the command.
    fn dollar(
        &mut self,
        quoted: bool,
        value: &mut String,
        literal: &mut bool,
        state: &mut bool,
    ) -> Result<(), Refusal> {
        let src = self.src;
        let start = self.pos;
        let name = match self.peek(1) {
            Some(b'(') => return Err(Refusal::Unsupported("$(")),
            Some(b'[') => return Err(Refusal::Unsupported("$[")),
            Some(b'\'') if !quoted => return Err(Refusal::Unsupported("$'")),
            Some(b'{') => {
                let body = start + 2;
                let len = plain_parameter_len(&self.bytes[body..]);
                if len == 0 || self.bytes.get(body + len) != Some(&b'}') {
                    return Err(self.expansion(start, braced_end(self.bytes, start)));
                }
                self.pos = body + len + 1;
                &src[body..body + len]
            }
            Some(b) if is_name_start(b) => {
                self.pos = start + 1 + plain_parameter_len(&self.bytes[start + 1..]);
                // zsh reads letters beyond ASCII as part of a name.
                if self.peek(0).is_some_and(|b| !b.is_ascii()) {
                    return Err(self.expansion(start, self.pos + self.char_at(self.pos).len_utf8()));
                }
                self.unbraced_suffix(start)?;
                &src[start + 1..self.pos]
            }
            Some(b) if is_special_parameter(b) => {
                self.pos = start + 2;
                // zsh's `$#name` is the length of `name`.
                if b == b'#'
                    && self.peek(0).is_some_and(|b| {
                        is_name_char(b) || b == b'{' || is_special_parameter(b) || !b.is_ascii()
                    })
                {
                    return Err(self.expansion(start, self.pos + self.char_at(self.pos).len_utf8()));
                }
                self.unbraced_suffix(start)?;
                &src[start + 1..self.pos]
            }
            // The closing quote of a double-quoted string.
            Some(b'"') if quoted => {
                value.push('$');
                self.pos += 1;
                return Ok(());
            }
            // `$"…"` (a translated string) and zsh's flags such as `$=name` and `$~name`.
            Some(b'"' | b'=' | b'~' | b'^' | b'+') => {
                return Err(self.expansion(start, start + 2));
            }
            _ => {
                value.push('$');
                self.pos += 1;
                return Ok(());
            }
        };
        *literal = false;
        *state |= is_state_parameter(name);
        Ok(())
    }

    /// Refuses what zsh reads after an unbraced parameter that ends at `self.pos`, which
    /// started at `start`: a subscript (`$name[…]`) or a modifier (`$name:h`).
    fn unbraced_suffix(&self, start: usize) -> Result<(), Refusal> {
        match (self.peek(0), self.peek(1)) {
            (Some(b'['), _) => Err(self.expansion(start, self.pos + 1)),
            (Some(b':'), Some(b)) if b.is_ascii_alphabetic() || b == b'&' => {
                Err(self.expansion(start, self.pos + 2))
            }
            _ => Ok(()),
        }
    }

    fn expansion(&self, start: usize, end: usize) -> Refusal {
        Refusal::Expansion(self.src[start..end.min(self.src.len())].to_owned())
    }
}

/// A stage being collected from tokens.
#[derive(Default)]
struct Builder {
    span: Option<Range<usize>>,
    words: Vec<Word>,
    redirected: Vec<u32>,
    state: bool,
    newline: bool,
    piped_in: bool,
}

impl Builder {
    fn extend(&mut self, span: &Range<usize>) {
        self.span = Some(match self.span.take() {
            Some(current) => current.start..span.end,
            None => span.clone(),
        });
    }
}

/// Splits `command` into stages, or refuses the whole command.
pub(super) fn stages(command: &str) -> Result<Vec<Stage>, Refusal> {
    let tokens = Lexer::new(command).tokens()?;
    let mut stages: Vec<Stage> = Vec::new();
    let mut current: Option<Builder> = None;
    let mut pipeline = 0_u32;
    // The first stage of the current and-or list, which `&` makes asynchronous as a whole.
    let mut list_start = 0_usize;
    let mut expect_stage: Option<Op> = None;
    let mut after_pipe = false;

    for token in tokens {
        match token {
            Token::Word(word) => {
                let builder = current.get_or_insert_with(|| Builder {
                    piped_in: after_pipe,
                    ..Builder::default()
                });
                builder.extend(&word.span);
                builder.state |= word.state;
                builder.newline |= word.newline;
                builder.words.push(word);
            }
            Token::Redirect { span, target, fds } => {
                let builder = current.get_or_insert_with(|| Builder {
                    piped_in: after_pipe,
                    ..Builder::default()
                });
                builder.extend(&span);
                builder.redirected.extend(fds);
                builder.state |= target.state;
                builder.newline |= target.newline;
            }
            Token::Op(op) => {
                match current.take() {
                    Some(builder) => {
                        let index = u32::try_from(stages.len()).unwrap_or(u32::MAX);
                        let mut stage = finish(command, builder, index, pipeline)?;
                        stage.piped_out = op == Op::Pipe;
                        stages.push(stage);
                    }
                    None if op == Op::Newline => continue,
                    None => return Err(Refusal::EmptyStage(op.text())),
                }
                expect_stage = None;
                after_pipe = op == Op::Pipe;
                match op {
                    Op::Pipe => expect_stage = Some(op),
                    Op::AndIf | Op::OrIf => {
                        for stage in stages
                            .iter_mut()
                            .rev()
                            .take_while(|s| s.pipeline == pipeline)
                        {
                            stage.continued = true;
                        }
                        expect_stage = Some(op);
                        pipeline += 1;
                    }
                    Op::Amp => {
                        for stage in &mut stages[list_start..] {
                            stage.background = true;
                        }
                        pipeline += 1;
                        list_start = stages.len();
                    }
                    Op::Semi | Op::Newline => {
                        pipeline += 1;
                        list_start = stages.len();
                    }
                }
            }
        }
    }
    if let Some(builder) = current.take() {
        let index = u32::try_from(stages.len()).unwrap_or(u32::MAX);
        stages.push(finish(command, builder, index, pipeline)?);
    } else if let Some(op) = expect_stage {
        return Err(Refusal::Dangling(op.text()));
    }
    Ok(stages)
}

fn finish(
    command: &str,
    mut builder: Builder,
    index: u32,
    pipeline: u32,
) -> Result<Stage, Refusal> {
    builder.redirected.sort_unstable();
    builder.redirected.dedup();
    let span = builder.span.unwrap_or(0..0);
    let assignments = builder
        .words
        .iter()
        .take_while(|w| is_assignment(&command[w.span.clone()]))
        .count();
    if let Some(word) = builder.words[..assignments]
        .iter()
        .map(|w| &command[w.span.clone()])
        .find(|raw| assigns_unsafe_name(raw))
    {
        return Err(Refusal::ChangesShell(word.to_owned()));
    }
    // Arithmetic evaluates words such as zsh's `printf %d` and `shift` arguments, and an
    // element assignment there defines a function or an alias as well.
    if let Some(word) = builder.words.iter().find(|w| names_table_element(&w.bare)) {
        return Err(Refusal::ChangesShell(command[word.span.clone()].to_owned()));
    }
    let command_word = (assignments < builder.words.len()).then_some(assignments);
    if let Some(i) = command_word {
        check_command_position(command, &builder.words[i..])?;
    }
    Ok(Stage {
        index,
        span: span.clone(),
        words: builder
            .words
            .iter()
            .map(|w| command[w.span.clone()].to_owned())
            .collect(),
        values: builder.words.iter().map(|w| w.value.clone()).collect(),
        assignments,
        command_word,
        command_start: command_word.map(|i| builder.words[i].span.start),
        pipeline,
        piped_in: builder.piped_in,
        piped_out: false,
        redirected: builder.redirected,
        background: false,
        continued: false,
        shell_state: builder.state,
        multiline: builder.newline || command[span].contains('\n'),
    })
}

/// Refuses keywords in command position, a command word that is not a literal (an expansion
/// or a pattern can name any command, `eval` included), and builtins that can change what a
/// later stage means or trace it (`eval`, `.`, `alias`, `trap`, `set -x`, …); each after
/// quote removal and behind precommand modifiers (`command "eval" …`).
fn check_command_position(command: &str, words: &[Word]) -> Result<(), Refusal> {
    let first = &command[words[0].span.clone()];
    if is_refused_keyword(first) {
        return Err(Refusal::Keyword(first.to_owned()));
    }
    let mut i = 0;
    while let Some(word) = words.get(i) {
        let raw = &command[word.span.clone()];
        let Some(value) = word.value.as_deref() else {
            return Err(Refusal::NotLiteral(raw.to_owned()));
        };
        let args: Vec<(&str, Option<&str>)> = words[i + 1..]
            .iter()
            .map(|w| (&command[w.span.clone()], w.value.as_deref()))
            .collect();
        if changes_caller(value, &args) {
            return Err(Refusal::ChangesShell(value.to_owned()));
        }
        if !is_precommand(value) {
            break;
        }
        // Skip the modifier's options, then look at the word it runs.
        i += 1;
        while words
            .get(i)
            .and_then(|w| w.value.as_deref())
            .is_some_and(|v| v.starts_with('-') && v != "-")
        {
            i += 1;
        }
    }
    Ok(())
}
