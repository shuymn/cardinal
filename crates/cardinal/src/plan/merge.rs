//! The `merge_evidence` check of `docs/architecture.md` section 9.2: whether HEAD was merged
//! into the base, the one condition a whole-worktree plan has.
//!
//! With `--squash-merged` (section 9.3), the caller's judgment counts as evidence only when
//! the observation of this evaluation finds `content_in_base` `yes`: every file the branch
//! changed is identical in the base. A check that fails while `content_in_base` is `yes` says
//! so, which is how the arbiter finds the plans it may judge.

use crate::contract::{Check, CheckResult, CheckRole, MergeEvidence, Tri};

use super::checks::{Inspection, check};

pub(super) fn merge_evidence(i: &Inspection<'_>) -> Check {
    let condition =
        |result, detail: &str| check("merge_evidence", CheckRole::Condition, result, detail);
    let Ok(observation) = i.observation else {
        return condition(CheckResult::Unknown, "not observed");
    };
    let facts = &observation.worktree.facts;
    match facts.merge_evidence {
        MergeEvidence::BaseAncestor => {
            condition(CheckResult::Pass, "HEAD is an ancestor of the base")
        }
        MergeEvidence::MergedPr => condition(
            CheckResult::Pass,
            "HEAD is the head of a merged pull request",
        ),
        MergeEvidence::None => match (facts.content_in_base, i.squash_merged) {
            (Tri::Yes, true) => condition(
                CheckResult::Pass,
                "judged squash-merged, and every file the branch changed is identical in the base",
            ),
            (Tri::Yes, false) => condition(
                CheckResult::Fail,
                "no evidence that HEAD was merged; content_in_base is yes: every file the branch changed is identical in the base, so a squash merge can be judged (--squash-merged)",
            ),
            (Tri::No, true) => condition(
                CheckResult::Fail,
                "judged squash-merged, but files the branch changed differ in the base",
            ),
            (Tri::Unknown, true) => condition(
                CheckResult::Fail,
                "judged squash-merged, but whether the branch's files are in the base is unknown",
            ),
            (_, false) => condition(CheckResult::Fail, "no evidence that HEAD was merged"),
        },
        MergeEvidence::Unknown => condition(CheckResult::Unknown, "unknown"),
    }
}
