//! The first release's `act` is report-only (A10, C14; `docs/architecture.md` sections 9.3
//! and 9.5): for both plan kinds, with and without owner approval and `--dry-run`, and for
//! every caller, a repeated `act` reports, a plan whose checks pass as `dry_run` with
//! `execution_not_designed`, and leaves the target's contents, the worktree's `.git`, Git's
//! administrative data for the worktree, and the plan unclaimed and unchanged.
//!
//! Each `act` is the `cardinal` binary, which observes the fixture through git and the fake
//! `gh` of `tests/plan_support`.

mod observe_common;
mod plan_support;
mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cardinal::contract::{ActResult, ActStatus, PlanKind};
use plan_support::{Setup, act_result, write};

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
        let main = plan_support::listing(&s.main);
        let name = wt.file_name().unwrap();
        assert!(
            main.contains_key(&Path::new(".git/worktrees").join(name).join("gitdir")),
            "the worktree's administrative data is part of the comparison"
        );
        Self {
            worktree: plan_support::listing(wt),
            main,
            registered: s.fx.worktree_list(&s.main),
            plan: std::fs::read(s.state.plan_file(plan)).unwrap(),
            claimed: s.state.execution_claim(plan).exists(),
            result: s.state.execution_result(plan).exists(),
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

/// `cardinal act PLAN ARGS…`, after checking that the plan targets the fixture.
fn act(s: &Setup, plan: &str, args: &[&str]) -> ActResult {
    plan_support::assert_plans_inside(s.fx.root(), &s.state, plan);
    let output = s
        .cardinal()
        .args(["act", plan])
        .args(args)
        .output()
        .unwrap();
    act_result(&output).expect("an act result")
}

/// The callers, as `act` arguments and as the result names them: the default caller, the
/// arbiter by name, the owner, and a unit whose `act` is true.
const CALLERS: [(&[&str], &str); 4] = [
    (&[], "arbiter"),
    (&["--caller", "arbiter"], "arbiter"),
    (&["--caller", "owner"], "owner"),
    (&["--caller", "unit:disk-reclaim"], "unit:disk-reclaim"),
];

/// A plan of each kind, one that needs approval (gh stops reporting the merged PR before it is
/// made) and ones that do not, acted on by every caller, without and with `--dry-run`, and by
/// the owner with `--owner-approved` too (any other caller giving it is a usage error,
/// `cli::owner_approved_needs_the_owner`): each run reports, and nothing changes.
#[test]
fn repeated_act_changes_nothing() {
    let s = Setup::with_host("[unit.disk-reclaim]\nact = true\n");
    let cases = [
        (PlanKind::ReclaimDir, false),
        (PlanKind::ReclaimWorktree, false),
        (PlanKind::ReclaimWorktree, true),
    ];
    for (n, (kind, needs_approval)) in cases.into_iter().enumerate() {
        for owner_approved in [false, true] {
            let name = format!("case-{n}-{owner_approved}");
            let wt = s.worktree(&name);
            write(&wt, "node_modules/pkg/index.js", "x");
            if needs_approval {
                s.unset_merged(&s.head(&wt));
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
                (ActStatus::DryRun, vec!["execution_not_designed".to_owned()])
            };
            for (caller, named) in CALLERS {
                if owner_approved && named != "owner" {
                    continue;
                }
                for dry_run in [false, true] {
                    let mut args = caller.to_vec();
                    if owner_approved {
                        args.push("--owner-approved");
                    }
                    if dry_run {
                        args.push("--dry-run");
                    }
                    let result = act(&s, &plan.id, &args);
                    assert_eq!(result.caller, named, "{case}");
                    assert_eq!(
                        (result.status, result.reasons),
                        expected,
                        "{case}, {caller:?}"
                    );
                    before.assert_unchanged(&s, &wt, &plan.id, &case);
                }
            }
        }
    }
}

/// A10 through the binary: a worktree that passes every check (an upstream, merged per gh,
/// clean, nothing unpushed) reports `dry_run` with `execution_not_designed` for every caller,
/// the owner with approval included, with and without `--dry-run`, and its contents, its Git
/// linkage, and Git's administrative data for it are as they were before.
#[test]
fn execution_not_designed_for_every_caller() {
    let s = Setup::with_host("[unit.disk-reclaim]\nact = true\n");
    let wt = s.worktree("passes");
    write(&wt, "node_modules/pkg/index.js", "x");
    let plan = s.plan(PlanKind::ReclaimWorktree, &wt);
    assert!(
        plan.checks
            .iter()
            .all(|c| c.result == cardinal::contract::CheckResult::Pass),
        "{:#?}",
        plan.checks
    );
    let before = Untouched::read(&s, &wt, &plan.id);
    for (args, named) in [
        (&[][..], "arbiter"),
        (&["--caller", "owner"][..], "owner"),
        (&["--caller", "owner", "--owner-approved"][..], "owner"),
        (&["--caller", "unit:disk-reclaim"][..], "unit:disk-reclaim"),
    ] {
        for dry_run in [&[][..], &["--dry-run"][..]] {
            let args = [args, dry_run].concat();
            let result = act(&s, &plan.id, &args);
            assert_eq!(result.caller, named);
            assert_eq!(result.status, ActStatus::DryRun, "{args:?}: {result:#?}");
            assert_eq!(result.reasons, ["execution_not_designed"], "{args:?}");
            assert!(result.freed_bytes.unwrap() > 0, "{args:?}: {result:#?}");
            before.assert_unchanged(&s, &wt, &plan.id, named);
        }
    }
}
