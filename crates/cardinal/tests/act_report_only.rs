//! Phase-0 `act` is report-only (A10, C14; `docs/architecture.md` section 9.5): for both plan
//! kinds, with and without owner approval, and for every caller, a repeated `act` reports and
//! leaves the target's contents, the worktree's `.git`, Git's administrative data for the
//! worktree, and the plan unclaimed and unchanged.
//!
//! Each `act` runs in its own process: the helper, which observes through the fake, and the
//! `cardinal` binary itself, which observes the fixture through git.

mod fakes;
mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cardinal::act::ActOptions;
use cardinal::contract::{ActStatus, Caller, IgnoredClass, MergeEvidence, PlanKind};
use fakes::{Setup, write};

#[test]
fn helper() {
    fakes::run_act_if_requested();
}

/// Everything `act` must leave as it was.
struct Untouched {
    /// The worktree, its `.git` file and the target included.
    worktree: BTreeMap<PathBuf, String>,
    /// The main worktree, its `.git` with `worktrees/<name>` included.
    main: BTreeMap<PathBuf, String>,
    /// `git worktree list --porcelain`.
    registered: String,
    plan: Vec<u8>,
    claimed: bool,
    result: bool,
}

impl Untouched {
    fn read(s: &Setup, wt: &Path, plan: &str) -> Self {
        let main = fakes::listing(&s.main);
        let name = wt.file_name().unwrap();
        assert!(
            main.contains_key(&Path::new(".git/worktrees").join(name).join("gitdir")),
            "the worktree's administrative data is part of the comparison"
        );
        Self {
            worktree: fakes::listing(wt),
            main,
            registered: s.fx.worktree_list(&s.main),
            plan: std::fs::read(s.state.plan_file(plan)).unwrap(),
            claimed: s.state.plan_claim(plan).exists(),
            result: s.state.plan_result(plan).exists(),
        }
    }

    /// Reads everything again and panics with what differs.
    fn assert_unchanged(&self, s: &Setup, wt: &Path, plan: &str, case: &str) {
        let now = Self::read(s, wt, plan);
        assert_eq!(now.claimed, self.claimed, "{case}: the plan's claim");
        assert_eq!(now.result, self.result, "{case}: the plan's result");
        assert!(now.plan == self.plan, "{case}: the plan file changed");
        assert_eq!(
            now.registered, self.registered,
            "{case}: registered worktrees"
        );
        for (name, before, after) in [
            ("worktree", &self.worktree, &now.worktree),
            ("main worktree", &self.main, &now.main),
        ] {
            let changed: Vec<&PathBuf> = before
                .iter()
                .filter(|(path, entry)| after.get(*path) != Some(entry))
                .map(|(path, _)| path)
                .chain(after.keys().filter(|path| !before.contains_key(*path)))
                .collect();
            assert!(
                changed.is_empty(),
                "{case}: changed in the {name}: {changed:?}"
            );
        }
    }
}

/// Runs `act` in a helper process and returns its status and reasons.
fn act_in_helper(s: &Setup, plan: &str, options: &ActOptions) -> (ActStatus, Vec<String>) {
    let out = s.fx.path(&format!("result-{plan}.json"));
    let status = s.act_command(plan, options, &out).status().unwrap();
    assert!(status.success(), "the act helper: {status}");
    let result = fakes::read_result(&out);
    assert_eq!(result.caller, options.caller.to_string());
    (result.status, result.reasons)
}

#[test]
fn repeated_act_changes_nothing() {
    let s = Setup::with_host(Some("[unit.disk-reclaim]\nact = true\n"));
    let cases = [
        (PlanKind::ReclaimDir, false),
        (PlanKind::ReclaimWorktree, false),
        (PlanKind::ReclaimWorktree, true),
    ];
    for (n, (kind, needs_approval)) in cases.into_iter().enumerate() {
        for owner_approved in [false, true] {
            let name = format!("case-{n}-{owner_approved}");
            let wt = s.worktree(&name, &[("node_modules", IgnoredClass::Regenerable)]);
            write(&wt, "node_modules/pkg/index.js", "x");
            if needs_approval {
                s.fake
                    .update(&wt, |w| w.facts.merge_evidence = MergeEvidence::None);
            }
            let target = match kind {
                PlanKind::ReclaimWorktree => wt.clone(),
                PlanKind::ReclaimDir => wt.join("node_modules"),
            };
            let plan = s.plan(kind, &target);
            assert!(plan.invariants_ok, "{:#?}", plan.checks);
            let before = Untouched::read(&s, &wt, &plan.id);
            assert!(!before.claimed && !before.result);
            let case =
                format!("{kind:?}, approval needed {needs_approval}, approved {owner_approved}");

            let expected = if needs_approval && !owner_approved {
                (
                    ActStatus::Rejected,
                    vec!["owner_approval_required".to_owned()],
                )
            } else {
                (ActStatus::DryRun, Vec::new())
            };
            let callers = [
                Caller::Arbiter,
                Caller::Arbiter,
                Caller::Owner,
                Caller::Unit("disk-reclaim".into()),
            ];
            for caller in callers {
                let options = ActOptions {
                    dry_run: false,
                    owner_approved,
                    caller,
                };
                assert_eq!(act_in_helper(&s, &plan.id, &options), expected, "{case}");
                before.assert_unchanged(&s, &wt, &plan.id, &case);
            }

            // The binary observes the fixture itself, through git. The branch was pushed to a
            // path, not to a remote, so the worktree has no upstream and counts as unpushed:
            // a worktree plan is refused for both, whatever the approval, and a directory plan
            // reports as it does through the fake.
            let (code, status) = match kind {
                PlanKind::ReclaimDir => (0, "dry_run"),
                PlanKind::ReclaimWorktree => (3, "rejected"),
            };
            fakes::assert_plans_inside(s.fx.root(), &s.state, &plan.id);
            for _ in 0..2 {
                let mut cardinal = s.cardinal();
                cardinal.args(["act", &plan.id]);
                if owner_approved {
                    cardinal.arg("--owner-approved");
                }
                let assert = cardinal.assert().code(code);
                let doc: serde_json::Value =
                    serde_json::from_slice(&assert.get_output().stdout).unwrap();
                assert_eq!(doc["status"], status, "{case}: {doc:#}");
                if kind == PlanKind::ReclaimWorktree {
                    for check in ["no_unpushed", "has_upstream"] {
                        assert!(
                            doc["reasons"]
                                .as_array()
                                .is_some_and(|reasons| reasons.iter().any(|r| r == check)),
                            "{case}: refused for {check}: {doc:#}"
                        );
                    }
                }
                before.assert_unchanged(&s, &wt, &plan.id, &case);
            }
        }
    }
}
