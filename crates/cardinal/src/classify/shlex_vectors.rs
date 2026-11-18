//! The word-splitting cases of the `shlex` crate, run against the subset tokenizer
//! (section 7.2).
//!
//! Source: `SPLIT_TEST_ITEMS` in `src/lib.rs` of <https://github.com/comex/rust-shlex>, commit
//! 653936e120cdd0a8d1b4f0837e2cce0f347da4c0, Copyright (c) 2015 Nicholas Allegra (comex),
//! licensed MIT OR Apache-2.0 and used here under MIT.
//!
//! The cases are copied under this notice, from the crate's `LICENSE-MIT`:
//!
//! ```text
//! The MIT License (MIT)
//!
//! Copyright (c) 2015 Nicholas Allegra (comex).
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in
//! all copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
//! THE SOFTWARE.
//! ```

use super::{Refusal, tokenize};

/// `(input, the words shlex splits it into, or None where shlex refuses it)`.
const SPLIT_TEST_ITEMS: &[(&str, Option<&[&str]>)] = &[
    ("foo$baz", Some(&["foo$baz"])),
    ("foo baz", Some(&["foo", "baz"])),
    ("foo\"bar\"baz", Some(&["foobarbaz"])),
    ("foo \"bar\"baz", Some(&["foo", "barbaz"])),
    ("   foo \nbar", Some(&["foo", "bar"])),
    ("foo\\\nbar", Some(&["foobar"])),
    ("\"foo\\\nbar\"", Some(&["foobar"])),
    ("'baz\\$b'", Some(&["baz\\$b"])),
    ("'baz\\''", None),
    ("\\", None),
    ("\"\\", None),
    ("'\\", None),
    ("\"", None),
    ("'", None),
    ("foo #bar\nbaz", Some(&["foo", "baz"])),
    ("foo #bar", Some(&["foo"])),
    ("foo#bar", Some(&["foo#bar"])),
    ("foo\"#bar", None),
    ("'\\n'", Some(&["\\n"])),
    ("'\\\\n'", Some(&["\\\\n"])),
];

/// The refusals allowed of a case shlex splits, each for what section 7.2 leaves unrewritten
/// on purpose: a command word that is not a literal (an expansion, or a character zsh can read
/// as a pattern) and a line continuation. A refusal of any other case fails.
fn allowed_refusal(input: &str) -> Option<Refusal> {
    match input {
        "foo$baz" => Some(Refusal::NotLiteral("foo$baz".to_owned())),
        "foo#bar" => Some(Refusal::NotLiteral("foo#bar".to_owned())),
        "foo\\\nbar" | "\"foo\\\nbar\"" => Some(Refusal::Unsupported("line continuation")),
        _ => None,
    }
}

/// The tokenizer reads each case as shlex does or refuses it as [`allowed_refusal`] allows.
/// shlex reads a newline as a blank where the shell ends a command, so words are compared
/// across stages.
#[test]
fn the_tokenizer_splits_as_shlex_or_refuses() {
    for &(input, expected) in SPLIT_TEST_ITEMS {
        match (expected, tokenize(input)) {
            (None, Ok(stages)) => {
                panic!("{input:?}: shlex refuses it, the tokenizer read {stages:?}")
            }
            (None, Err(_)) => {}
            (Some(words), Ok(stages)) => {
                let values: Vec<Option<String>> =
                    stages.into_iter().flat_map(|stage| stage.values).collect();
                let expected: Vec<Option<String>> =
                    words.iter().map(|word| Some((*word).to_owned())).collect();
                assert_eq!(values, expected, "{input:?}");
            }
            (Some(_), Err(refusal)) => assert_eq!(
                Some(refusal),
                allowed_refusal(input),
                "{input:?}: shlex splits it, the tokenizer refuses it"
            ),
        }
    }
}
