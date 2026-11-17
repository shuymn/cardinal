//! The subset tokenizer (`docs/architecture.md` section 7.2): splits a command string into
//! stages with byte spans, or refuses the whole command when a span cannot be delimited with
//! certainty or the outer shell could change what a stage means.
//!
//! Every byte the shell treats specially is ASCII, so the scanner works on bytes and only
//! ever cuts at ASCII positions, which are UTF-8 character boundaries.

use std::ops::Range;

use super::Stage;
use super::words::{
    assigns_caller_array, changes_caller, is_assignment, is_precommand, is_refused_keyword,
};

/// Why a command is left unrewritten as a whole.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("unbalanced quote")]
    UnbalancedQuote,
    #[error("`{0}` is outside the supported shell subset")]
    Unsupported(&'static str),
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
    /// Expands shell state the fallback test could change (`$?`, `$_`, …).
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

/// Names whose expansion reads state that the rewrite's fallback test changes (section 7.2):
/// the last argument, zsh's exit status, the pipeline statuses, and bash's current command.
fn is_state_name(name: &str) -> bool {
    matches!(
        name,
        "_" | "status" | "PIPESTATUS" | "pipestatus" | "BASH_COMMAND"
    )
}

/// The parameter name at the start of `text`.
fn name_prefix(text: &str) -> &str {
    &text[..text.bytes().take_while(|&b| is_name_char(b)).count()]
}

/// Whether the parameter at the start of a `${…}` body is shell state: `?`, `!`, `-`, `$`,
/// or a state name, after zsh's `=`, `~`, `^`, `+` prefixes and a `#` (length). zsh flags
/// (`${(e)…}`, `${(%)…}`) and bash indirection (`${!name}`) can reach any parameter, so they
/// count as state too.
fn parameter_state(body: &str) -> bool {
    let rest = body.trim_start_matches(['=', '~', '^', '+']);
    match rest.bytes().next() {
        Some(b'(' | b'!' | b'?' | b'-' | b'$') => true,
        Some(b'#') => {
            let after = &rest[1..];
            matches!(after.bytes().next(), Some(b'?' | b'!' | b'-' | b'$'))
                || is_state_name(name_prefix(after))
        }
        _ => is_state_name(name_prefix(rest)),
    }
}

/// Whether the body of `${…}` reads shell state, at its parameter or in any expansion nested
/// in its operands (`${UNSET:-$?}`), or expands a value as a prompt (`${x@P}`), which can.
fn braced_state(body: &str) -> bool {
    if parameter_state(body) || body.contains("@P") {
        return true;
    }
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'$' => {
                let rest = &body[i + 1..];
                let state = match rest.bytes().next() {
                    Some(b'{') => parameter_state(&rest[1..]),
                    Some(b'?' | b'!' | b'-' | b'$') => true,
                    _ => is_state_name(name_prefix(rest)),
                };
                if state {
                    return true;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    false
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

    /// A `$` at `self.pos`, unquoted or inside double quotes.
    fn dollar(
        &mut self,
        quoted: bool,
        value: &mut String,
        literal: &mut bool,
        state: &mut bool,
    ) -> Result<(), Refusal> {
        match self.peek(1) {
            Some(b'(') => Err(Refusal::Unsupported("$(")),
            Some(b'[') => Err(Refusal::Unsupported("$[")),
            Some(b'{') => {
                *literal = false;
                self.pos += 2;
                let body = self.pos;
                let mut depth = 1;
                loop {
                    match self.peek(0) {
                        None => return Err(Refusal::UnbalancedQuote),
                        Some(b'}') => {
                            depth -= 1;
                            self.pos += 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        Some(b'$') if self.peek(1) == Some(b'{') => {
                            depth += 1;
                            self.pos += 2;
                        }
                        Some(b'$') if self.peek(1) == Some(b'(') => {
                            return Err(Refusal::Unsupported("$("));
                        }
                        Some(b'\'' | b'"') => {
                            return Err(Refusal::Unsupported("quote inside ${…}"));
                        }
                        Some(b'`') => return Err(Refusal::Unsupported("`")),
                        Some(b'{') => return Err(Refusal::Unsupported("{")),
                        Some(b'\n') => return Err(Refusal::Unsupported("newline inside ${…}")),
                        Some(b'\\') => match self.peek(1) {
                            None | Some(b'\n') => {
                                return Err(Refusal::Unsupported("backslash inside ${…}"));
                            }
                            Some(_) => {
                                let ch = self.char_at(self.pos + 1);
                                self.pos += 1 + ch.len_utf8();
                            }
                        },
                        Some(_) => {
                            let ch = self.char_at(self.pos);
                            self.pos += ch.len_utf8();
                        }
                    }
                }
                if braced_state(&self.src[body..self.pos - 1]) {
                    *state = true;
                }
                Ok(())
            }
            Some(b'\'') if !quoted => Err(Refusal::Unsupported("$'")),
            Some(b'"') if !quoted => {
                // `$"…"`: a locale-translated string; the quote is scanned next.
                *literal = false;
                self.pos += 1;
                Ok(())
            }
            Some(b'?' | b'!' | b'-' | b'$') => {
                *literal = false;
                *state = true;
                self.pos += 2;
                Ok(())
            }
            Some(b'#' | b'@' | b'*' | b'0'..=b'9') => {
                *literal = false;
                self.pos += 2;
                Ok(())
            }
            Some(b) if is_name_start(b) => {
                *literal = false;
                let start = self.pos + 1;
                let name = name_prefix(&self.src[start..]);
                if is_state_name(name) {
                    *state = true;
                }
                self.pos = start + name.len();
                Ok(())
            }
            // zsh expansion flags such as `$=name` and `$~name`.
            Some(b'=' | b'~' | b'^' | b'+') => {
                *literal = false;
                self.pos += 1;
                Ok(())
            }
            _ => {
                value.push('$');
                self.pos += 1;
                Ok(())
            }
        }
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
        .find(|raw| assigns_caller_array(raw))
    {
        return Err(Refusal::ChangesShell(word.to_owned()));
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
