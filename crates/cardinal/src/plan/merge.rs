//! The `merge_evidence` check of `docs/architecture.md` section 9.2: whether HEAD was merged
//! into the base, the one condition a whole-worktree plan has.

use crate::contract::{Check, CheckResult, CheckRole, MergeEvidence};

use super::checks::{Inspection, check};

pub(super) fn merge_evidence(i: &Inspection<'_>) -> Check {
    let condition =
        |result, detail: &str| check("merge_evidence", CheckRole::Condition, result, detail);
    match i.observation.map(|o| o.worktree.facts.merge_evidence) {
        Err(_) => condition(CheckResult::Unknown, "not observed"),
        Ok(MergeEvidence::BaseAncestor) => {
            condition(CheckResult::Pass, "HEAD is an ancestor of the base")
        }
        Ok(MergeEvidence::MergedPr) => condition(
            CheckResult::Pass,
            "HEAD is the head of a merged pull request",
        ),
        Ok(MergeEvidence::None) => condition(CheckResult::Fail, "no evidence that HEAD was merged"),
        Ok(MergeEvidence::Unknown) => condition(CheckResult::Unknown, "unknown"),
    }
}
