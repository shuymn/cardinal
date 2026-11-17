//! Classification: the shell tokenizer, stage finder, normalizer, rule matcher, and the
//! `Classifier` seam (`docs/architecture.md` section 7).
//!
//! Owned by the hook work unit. The bodies are not written yet; every declared function
//! returns `not_implemented`.

use std::ops::Range;
use std::time::Instant;

use crate::config::Selection;
use crate::contract::{ClassifyRequest, ClassifyResponse, Error};

/// One simple command of a command string, with its byte span in that string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stage {
    /// Position among the command's stages, from 0.
    pub index: u32,
    /// The stage's byte span, the replacement target of a rewrite.
    pub span: Range<usize>,
    /// The stage's words as written (unexpanded).
    pub words: Vec<String>,
}

/// A classifier: the built-in one, and from phase 1 external units speaking the same types
/// as JSON. Called once per stage.
pub trait Classifier {
    fn classify(&self, req: &ClassifyRequest, deadline: Instant) -> ClassifyResponse;
}

/// Splits `command` into stages, or refuses the whole command when a stage span cannot be
/// delimited with certainty (section 7.2).
pub fn find_stages(command: &str) -> Result<Vec<Stage>, Error> {
    let _ = command;
    Err(Error::not_implemented("classify::find_stages"))
}

/// Classifies one stage with the built-in classifier against the rules of `selection`.
pub fn classify(
    req: &ClassifyRequest,
    selection: &Selection<'_>,
    deadline: Instant,
) -> Result<ClassifyResponse, Error> {
    let _ = (req, selection, deadline);
    Err(Error::not_implemented("classify::classify"))
}
