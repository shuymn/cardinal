//! The classification trace of a command, for `explain command` (`docs/architecture.md`
//! section 10.2): per stage, the values [`decide`](super::decide) produced and
//! [`rewrite`](super::rewrite) acts on, with the profile rule and pattern behind a wrap of
//! the built-in classifier.

use std::path::PathBuf;

use super::{Skip, StageOutcome, first_match};
use crate::config::Selection;
use crate::contract::{ClassifyDecision, ClassifyResponse};

/// The profile rule that classified a stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleMatch {
    /// The profile's name (its file stem) and file.
    pub profile: String,
    pub file: PathBuf,
    /// `<profile>/<id>`
    pub rule: String,
    /// The rule's pattern that matched the normalized argv, as written in the profile.
    pub pattern: String,
}

/// How one stage was classified, and whether the rewrite wraps it.
#[derive(Clone, Debug, PartialEq)]
pub struct StageTrace {
    /// The stage's index, as in its run spec.
    pub index: u32,
    /// The stage as written.
    pub stage: String,
    /// The normalized argv the stage was classified by; `None` when the stage was not
    /// offered to a classifier.
    pub argv: Option<Vec<String>>,
    /// The classifier's answer: decision, class, rule, resources, and reason.
    pub response: Option<ClassifyResponse>,
    /// The profile rule and pattern behind the answer; `None` unless the built-in
    /// classifier wrapped the stage.
    pub matched: Option<RuleMatch>,
    /// Why the stage stays unwrapped; `None` exactly when the rewrite wraps it.
    pub skip: Option<Skip>,
}

/// The trace of `outcomes`, which [`decide`](super::decide) produced for `command` under
/// `selection`. Given the outcomes passed to [`rewrite`](super::rewrite), it describes that
/// rewrite: the stages with no `skip` are the ones wrapped.
#[must_use]
pub fn trace(
    command: &str,
    outcomes: &[StageOutcome],
    selection: &Selection<'_>,
) -> Vec<StageTrace> {
    outcomes
        .iter()
        .map(|outcome| StageTrace {
            index: outcome.stage.index,
            stage: command[outcome.stage.span.clone()].to_owned(),
            argv: outcome.argv.clone(),
            response: outcome.response.clone(),
            matched: matched(outcome, selection),
            skip: outcome.skip,
        })
        .collect()
}

/// The built-in classifier's match for a wrapped stage, found again with the same lookup;
/// `None` when another classifier named the rule.
fn matched(outcome: &StageOutcome, selection: &Selection<'_>) -> Option<RuleMatch> {
    let response = outcome.response.as_ref()?;
    if response.decision != ClassifyDecision::Wrap {
        return None;
    }
    let (profile, rule, pattern) = first_match(selection, outcome.argv.as_deref()?)?;
    (response.rule.as_deref() == Some(rule.qualified_id.as_str())).then(|| RuleMatch {
        profile: profile.name.clone(),
        file: profile.file.clone(),
        rule: rule.qualified_id.clone(),
        pattern: pattern.text.clone(),
    })
}
