//! The tag-name cases of gitoxide's `gix-validate`, run against [`is_ref_name`], which reads
//! a configured base by the rules of `git check-ref-format --allow-onelevel` (section 8.2).
//!
//! Source: the inputs of `gix-validate/tests/validate/tag.rs` in
//! <https://github.com/GitoxideLabs/gitoxide>, commit 3e89f3b56f88f7080d0b42e9619ae4b843ba8b5d,
//! by Sebastian Thiel and the gitoxide contributors, licensed MIT OR Apache-2.0 and used here
//! under MIT. Each input keeps its verdict there; the lone `@`, which the suite accepts as a
//! tag name (`refs/tags/@`), is left out, because git refuses `@` as a whole ref name.
//!
//! The cases are copied under this notice, from the repository's `LICENSE-MIT`, which carries
//! no copyright line of its own:
//!
//! ```text
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

use super::git::is_ref_name;

const VALID: &[&str] = &[
    "你好吗",
    "😅🙌",
    "hello@foo",
    "file.lock.ext",
    "this_{is-fine}_too",
    "this_{@is-fine@}_too",
    "token.other",
    "hello/world",
];

const INVALID: &[&str] = &[
    "this_looks_like_a_@{reflog}",
    "prefix.lock",
    "......",
    "//....///....///",
    "prefix.lock.lock",
    "prefix/",
    "prefix//suffix",
    ".lock",
    "foo.lock/baz.lock/bar",
    "foo.lock/baz.lock/bar.lock",
    "foo.lock.lock/baz.lock.lock/bar.lock.lock",
    "...lock/..lock//lock",
    "with..double-dot",
    "..with-double-dot",
    "with-double-dot..",
    "*suffix",
    "/suffix",
    "prefix*",
    "prefix*suffix",
    "prefix\0suffix",
    "prefix\x07suffix",
    "prefix\x08suffix",
    "prefix\x0bsuffix",
    "prefix\x0csuffix",
    "prefix\x1asuffix",
    "prefix\x1bsuffix",
    "prefix:suffix",
    "prefix?suffix",
    "prefix[suffix",
    r"prefix\suffix",
    "prefix^suffix",
    "prefix~suffix",
    "prefix suffix",
    "prefix\tsuffix",
    "prefix\nsuffix",
    "prefix\rsuffix",
    ".with-dot",
    "with-dot.",
    "",
];

#[test]
fn ref_names_follow_the_gix_validate_cases() {
    for name in VALID {
        assert!(is_ref_name(name), "{name:?} is a ref name");
    }
    for name in INVALID {
        assert!(!is_ref_name(name), "{name:?} is not a ref name");
    }
}
