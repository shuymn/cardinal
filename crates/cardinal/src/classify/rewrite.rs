//! The rewritten command (`docs/architecture.md` section 6.4): each wrapped stage's span is
//! replaced, in place, by `if [ -x R ]; then S'; else S; fi`, where `S'` is the stage with
//! `R run --spec-b64 <spec> --` before its command word. Every other byte stays. The form is
//! shell syntax, not agent knowledge: the hook and `explain command` produce it the same way.

use std::ops::Range;

use super::{Context, Skip, StageOutcome};
use crate::contract::{Resources, RunSpec, RunSpecSchema};

/// The IDs of the agent call a rewrite is for; each run spec carries them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallIds<'a> {
    pub session_id: Option<&'a str>,
    pub agent_id: Option<&'a str>,
    pub tool_use_id: Option<&'a str>,
}

/// What [`rewrite`] made of a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rewritten {
    /// At least one stage is wrapped: the rewritten command.
    Command(String),
    /// No stage is wrapped: why, for each stage in order.
    Unchanged(Vec<Skip>),
}

/// Rewrites the stages of `ctx.command` that `outcomes` (from [`super::decide`]) wrap. Each
/// wrapped stage gets a `cardinal.run-spec.v1` with its classification, its rule's
/// resources (else `default_resources`), the tool's timeout and background flag, and `ids`.
/// `runner` must satisfy the `[hook] runner` rule (`config::Hook::runner_path` returns only
/// such paths), so it needs no quoting.
#[must_use]
pub fn rewrite(
    ctx: &Context<'_>,
    outcomes: &[StageOutcome],
    runner: &str,
    default_resources: &Resources,
    ids: &CallIds<'_>,
) -> Rewritten {
    let wraps: Vec<Wrap> = outcomes
        .iter()
        .filter(|outcome| outcome.wrap())
        .filter_map(|outcome| {
            let response = outcome.response.as_ref()?;
            let stage = &outcome.stage;
            let spec = RunSpec {
                schema: RunSpecSchema,
                class: response.class.clone(),
                rule: response.rule.clone(),
                resources: outcome.resources(default_resources),
                timeout_ms: ctx.tool.timeout_ms,
                background: ctx.tool.background,
                stage_index: stage.index,
                stage: ctx.command[stage.span.clone()].to_owned(),
                session_id: ids.session_id.map(str::to_owned),
                agent_id: ids.agent_id.map(str::to_owned),
                tool_use_id: ids.tool_use_id.map(str::to_owned),
            };
            Some(Wrap {
                span: stage.span.clone(),
                command_start: stage.command_start?,
                spec: spec.encode_b64(),
            })
        })
        .collect();
    if wraps.is_empty() {
        Rewritten::Unchanged(outcomes.iter().filter_map(|o| o.skip).collect())
    } else {
        Rewritten::Command(splice(ctx.command, runner, &wraps))
    }
}

/// One stage to wrap.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Wrap {
    /// The stage's span in the command.
    span: Range<usize>,
    /// Where the command word starts, inside `span`.
    command_start: usize,
    /// The unpadded base64url run spec.
    spec: String,
}

/// Replaces each wrap's span; spans must not overlap. They are replaced last to first so
/// earlier offsets stay valid.
fn splice(command: &str, runner: &str, wraps: &[Wrap]) -> String {
    let mut ordered: Vec<&Wrap> = wraps.iter().collect();
    ordered.sort_by_key(|w| std::cmp::Reverse(w.span.start));
    let mut out = command.to_owned();
    for wrap in ordered {
        out.replace_range(wrap.span.clone(), &replacement(command, runner, wrap));
    }
    out
}

fn replacement(command: &str, runner: &str, wrap: &Wrap) -> String {
    let stage = &command[wrap.span.clone()];
    let (before, from_command) = stage.split_at(wrap.command_start - wrap.span.start);
    format!(
        "if [ -x {runner} ]; then {before}{runner} run --spec-b64 {spec} -- {from_command}; else {stage}; fi",
        spec = wrap.spec
    )
}

#[cfg(test)]
mod tests {
    use proptest::prelude::{prop, prop_assert_eq, proptest};
    use proptest::test_runner::Config;

    use super::super::tokenize;
    use super::{Wrap, replacement, splice};

    fn wraps_for(command: &str, spec: &str) -> Vec<Wrap> {
        tokenize(command)
            .unwrap()
            .into_iter()
            .filter_map(|stage| {
                Some(Wrap {
                    span: stage.span.clone(),
                    command_start: stage.command_start?,
                    spec: format!("{spec}{}", stage.index),
                })
            })
            .collect()
    }

    #[test]
    fn documented_example() {
        let command = "cd packages/app && pnpm test 2>&1 | tail -50 || echo failed";
        let start = command.find("pnpm").unwrap();
        let wrap = Wrap {
            span: start..start + "pnpm test 2>&1".len(),
            command_start: start,
            spec: "eyJzY2hlbWEiOi".into(),
        };
        assert_eq!(
            splice(command, "/usr/local/bin/cardinal", &[wrap]),
            "cd packages/app && if [ -x /usr/local/bin/cardinal ]; then /usr/local/bin/cardinal run --spec-b64 eyJzY2hlbWEiOi -- pnpm test 2>&1; else pnpm test 2>&1; fi | tail -50 || echo failed"
        );
    }

    #[test]
    fn redirections_before_the_command_word_stay_in_front_of_the_runner() {
        let command = "2>/dev/null make x";
        let wraps = wraps_for(command, "S");
        assert_eq!(
            splice(command, "/r", &wraps),
            "if [ -x /r ]; then 2>/dev/null /r run --spec-b64 S0 -- make x; else 2>/dev/null make x; fi"
        );
    }

    #[test]
    fn several_stages_are_each_wrapped() {
        let command = "a 1 && b|c;d # comment";
        assert_eq!(
            splice(command, "/r", &wraps_for(command, "S")),
            "if [ -x /r ]; then /r run --spec-b64 S0 -- a 1; else a 1; fi && \
             if [ -x /r ]; then /r run --spec-b64 S1 -- b; else b; fi|\
             if [ -x /r ]; then /r run --spec-b64 S2 -- c; else c; fi;\
             if [ -x /r ]; then /r run --spec-b64 S3 -- d; else d; fi # comment"
        );
    }

    proptest! {
        #![proptest_config(Config { cases: 256, failure_persistence: None, ..Config::default() })]

        /// Rewriting any subset of stages keeps every byte outside their spans: the result
        /// equals the original with each chosen span replaced, built front to back.
        #[test]
        fn bytes_outside_spans_are_kept(
            parts in prop::collection::vec(("[a-z]{1,4}( [a-z'\" ]{0,6})?", "( && | \\|\\| |; |\\||\n| # c\n)"), 1..6),
            chosen in prop::collection::vec(proptest::bool::ANY, 6),
        ) {
            let command: String = parts.iter().flat_map(|(s, sep)| [s.as_str(), sep.as_str()]).collect();
            let Ok(stages) = tokenize(&command) else { return Ok(()) };
            let wraps: Vec<Wrap> = wraps_for(&command, "S")
                .into_iter()
                .zip(&chosen)
                .filter_map(|(w, keep)| keep.then_some(w))
                .collect();
            let mut expected = String::new();
            let mut last = 0;
            for wrap in &wraps {
                expected.push_str(&command[last..wrap.span.start]);
                expected.push_str(&replacement(&command, "/r", wrap));
                last = wrap.span.end;
            }
            expected.push_str(&command[last..]);
            prop_assert_eq!(splice(&command, "/r", &wraps), expected);
            prop_assert_eq!(stages.len() >= wraps.len(), true);
        }
    }
}
