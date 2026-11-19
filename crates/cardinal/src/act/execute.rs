//! Executing a granted `reclaim_dir` plan (`docs/architecture.md` section 9.7, "Procedure").
//!
//! An execution takes the plan's `act.lock` and the worktree's in-use lock with one try each
//! (step 1), and recovers a claim it finds there instead of starting another. It evaluates the
//! checks of section 9.5 holding them, then checks the eligibility rules with the target
//! present, bound to the checked worktree (step 2), writes the manifest, and publishes its
//! claim. It moves the target with one no-replace rename into a trash entry it created, right
//! after it pinned the index file (step 3); checks that it moved the planned target from the
//! planned place (step 4), that the moved tree is the manifest's (step 5), the configuration
//! and git's lock again (step 6), and the users of the worktree and the trash entry last (step
//! 7). It then writes its `committed` stage under a temporary name, reads every input of git's
//! classification again, each decided at its own read, and the grant (step 8), and publishes
//! `committed` by a rename: from then on it never restores, and it deletes the trash entry
//! through the deleter. Any failure before that rename restores the target into its original
//! parent, wherever that parent is ([`recover::restore`]).
//!
//! Every git command runs bound to the checked root and git dir ([`BoundGit`]), and a read that
//! is not complete decides nothing: an eligibility rule fails, a target not yet committed is
//! restored (section 9.7, "Reads that fail").

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Read as _};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::{Component, Path, PathBuf};

use super::delete::{self, Deletion, Stop, Watch};
use super::recover::{self, ClaimRead, ExecutionLock, Locked, Published, Recovered, RestoreError};
use super::unix::Target;
use super::{EXECUTION_NOT_GRANTED, Evaluated, Outcome, Run};
use crate::config::Config;
use crate::contract::{
    ActClaim, ActClaimSchema, ActResult, ActStatus, Caller, ChainDir, Check, CheckResult,
    ClaimRepo, ClaimStage, ClaimTarget, ClaimWorktree, EntryPath, EntryType, Error, ErrorCode,
    ManifestEntry, PlanKind, Provenance,
};
use crate::observe::{BoundGit, IgnoreValues, IndexEntry, Measure, WorktreeObservation};
use crate::os::dir::{self, Kind};
use crate::os::mount;
use crate::plan::{self, DataLists, EntryKind, InUseFacts, Inspection, Walk};
use crate::state::{self, FileLock, LockMode, Op, StateDir};
use crate::testpoint;

/// The target is not on the mount of `<state>/trash/` and `<state>/executions/`.
pub const NOT_ON_STATE_MOUNT: &str = "not_on_state_mount";
/// A mount of the target's file system has its root at or below the target (Linux).
pub const BOUND_ELSEWHERE: &str = "bound_elsewhere";
/// An index entry is marked skip-worktree or is a sparse directory.
pub const SPARSE_CHECKOUT: &str = "sparse_checkout";
/// An index entry names the target, a path below it, or a directory above it.
pub const TRACKED_OVERLAP: &str = "tracked_overlap";
/// No rule from outside the target excludes the target itself, or an ignore input is in it.
pub const NOT_DIRECTORY_EXCLUDED: &str = "not_directory_excluded";
/// Something an execution checked changed, or a read of it failed, after the move.
pub const CHANGED_DURING_ACT: &str = "changed_during_act";
/// A write the execution needs failed for lack of space, after the reserve's retry.
pub const NO_SPACE: &str = "no_space";
/// The trash entry, or its `t`, was taken before the move.
pub const TRASH_OCCUPIED: &str = "trash_occupied";
/// Another `act` or recovery holds the plan's `act.lock`.
pub const IN_PROGRESS: &str = "in_progress";
/// The plan's claim cannot be read.
pub const CLAIM_UNREADABLE: &str = "claim_unreadable";
/// The deletion found a process using the trash entry and stopped.
pub const IN_USE_WHILE_DELETING: &str = "in_use_while_deleting";

/// The checks step 6 evaluates again on the manifest (section 9.7).
const CHECKED_AGAIN: [&str; 4] = [
    "regenerable",
    "no_protected",
    "no_unclassifiable",
    "not_managed",
];

/// What `managed_by` reads for a worktree git has locked (section 8.4).
const GIT_LOCK: &str = "git-lock";

/// How many paths a detail names.
const DETAIL_PATHS: usize = 5;

/// Executes `r`'s plan, its caller granted (section 9.7).
pub(super) fn run(r: &Run<'_>) -> Result<ActResult, Error> {
    let state = r.state;
    // 1. Exclude.
    let lock = match take_lock(r)? {
        Ok(lock) => lock,
        Err(result) => return Ok(*result),
    };
    if let Some(result) = finish_earlier(r, &lock) {
        return Ok(result);
    }
    let key = state::worktree_key(&r.worktree());
    let in_use = match state::try_lock(&state.worktree_lock(&key), LockMode::Exclusive) {
        Ok(Some(in_use)) => in_use,
        Ok(None) => {
            return Ok(r.report(Outcome {
                reasons: vec!["not_in_use".into()],
                error: Some("another process holds the worktree's in-use lock".into()),
                ..Outcome::default()
            }));
        }
        Err(err) => {
            return Ok(r.report(Outcome {
                reasons: vec!["not_in_use".into()],
                error: Some(err.to_string()),
                ..Outcome::default()
            }));
        }
    };
    if let Err(err) = testpoint::hit(testpoint::ACT_EXCLUDED) {
        return Ok(r.report(Outcome {
            error: Some(err.to_string()),
            ..Outcome::default()
        }));
    }
    // An executing `act` holds the in-use lock instead of probing it (section 9.2).
    let evaluated = r.evaluate(Ok(false));
    if !evaluated.passed {
        return Ok(r.report(evaluated.outcome));
    }
    Ok(match Execution::start(r, evaluated, lock, in_use) {
        Ok(execution) => execution.go(),
        Err(result) => *result,
    })
}

/// Recovers the claim an earlier execution of `r`'s plan left, for an owner's `act` that
/// does not execute because `[act] execute` does not grant it (section 9.7, "Recovery"):
/// restoring a `claimed` target is the safe direction and needs no grant, while a `committed`
/// one is finished only under the grant recovery checks. `None` when no claim is there,
/// before or under the plan's `act.lock`: the `act` only reports.
pub(super) fn recover_claim(r: &Run<'_>) -> Result<Option<ActResult>, Error> {
    if matches!(recover::read_claim(r.state, &r.plan.id), ClaimRead::Absent) {
        return Ok(None);
    }
    let lock = match take_lock(r)? {
        Ok(lock) => lock,
        Err(result) => return Ok(Some(*result)),
    };
    Ok(finish_earlier(r, &lock))
}

/// Step 1's `act.lock`, or the result of an `act` that cannot take it.
fn take_lock(r: &Run<'_>) -> Result<Result<ExecutionLock, Box<ActResult>>, Error> {
    let plan = &r.plan.id;
    match exclude(r.state, plan) {
        Ok(Some(lock)) => Ok(Ok(lock)),
        Ok(None) => Ok(Err(Box::new(r.report(Outcome::refused(
            ActStatus::Rejected,
            vec![IN_PROGRESS.into()],
        ))))),
        Err(Excluding::NotFound) => Err(Error::new(
            ErrorCode::NotFound,
            format!("plan {plan} was removed while its execution started"),
        )),
        Err(Excluding::Failed(err)) => Ok(Err(Box::new(r.report(write_failed(&err))))),
    }
}

/// Under the plan's `act.lock`: the result of an earlier execution, made durable, or the
/// recovery of a claim without a result, which belongs to an execution that died, instead
/// of starting another; `None` when neither is there.
fn finish_earlier(r: &Run<'_>, lock: &ExecutionLock) -> Option<ActResult> {
    let state = r.state;
    let plan = &r.plan.id;
    match recover::read_result(state, plan) {
        Ok(None) => {}
        Ok(Some(_)) => {
            let synced = state::sync_dir(&state.execution_dir(plan));
            return Some(r.report(Outcome {
                status: ActStatus::AlreadyExecuted,
                error: synced.err().map(|err| err.to_string()),
                ..Outcome::default()
            }));
        }
        Err(err) => {
            return Some(r.report(Outcome {
                status: ActStatus::AlreadyExecuted,
                error: Some(err.to_string()),
                ..Outcome::default()
            }));
        }
    }
    let claimed = match recover::read_claim(state, plan) {
        ClaimRead::Absent => return None,
        ClaimRead::Unreadable(_) => None,
        ClaimRead::Read(claim) => Some(claim.stage),
    };
    let recovered = testpoint::hit(testpoint::RECOVER_LOCKED)
        .map_err(io::Error::other)
        .and_then(|()| recover::recover_held(state, &r.config.dir, plan, &grant, lock));
    Some(report_recovered(r, claimed, recovered))
}

/// The grant recovery checks before it finishes a committed execution: `[act] execute` lists
/// the caller with `reclaim_dir`.
fn grant(config: &Config, caller: &Caller) -> Result<(), String> {
    if config.host.grants(caller, PlanKind::ReclaimDir) {
        Ok(())
    } else {
        Err(format!(
            "act.execute does not grant {caller} reclaim_dir plans"
        ))
    }
}

/// What `act` reports for the recovery of its plan's claim, a `claimed` one or `committed` one
/// as `stage` says (`None` for an unreadable claim).
fn report_recovered(
    r: &Run<'_>,
    stage: Option<ClaimStage>,
    recovered: io::Result<Recovered>,
) -> ActResult {
    let outcome = match recovered {
        Ok(Recovered::Done(result)) => {
            let result = *result;
            r.append(&result, Provenance::default());
            return result;
        }
        Ok(Recovered::Unreadable { error, trash }) => Outcome {
            status: ActStatus::AlreadyExecuted,
            reasons: vec![CLAIM_UNREADABLE.into()],
            trash: trash.map(|t| t.display().to_string()),
            error: Some(error),
            ..Outcome::default()
        },
        Ok(Recovered::Stranded { why, trash }) => {
            let committed = stage == Some(ClaimStage::Committed);
            let reasons = if !committed {
                vec![recover::INTERRUPTED.to_owned()]
            } else if why.starts_with(delete::IN_USE) {
                vec![IN_USE_WHILE_DELETING.to_owned()]
            } else {
                Vec::new()
            };
            Outcome {
                status: if committed {
                    ActStatus::Partial
                } else {
                    ActStatus::Rejected
                },
                reasons,
                trash: Some(trash.display().to_string()),
                error: Some(why),
                recovered: true,
                ..Outcome::default()
            }
        }
        Ok(Recovered::Finished) => Outcome {
            status: ActStatus::AlreadyExecuted,
            ..Outcome::default()
        },
        Ok(Recovered::InProgress) => {
            Outcome::refused(ActStatus::Rejected, vec![IN_PROGRESS.into()])
        }
        Ok(Recovered::Absent | Recovered::NotFound) => Outcome {
            error: Some("the claim found was gone when recovery read it".into()),
            ..Outcome::default()
        },
        Err(err) => Outcome {
            error: Some(format!("recovery: {err}")),
            ..Outcome::default()
        },
    };
    r.report(outcome)
}

/// Why step 1 did not take the plan's `act.lock`.
enum Excluding {
    /// The plan's directory, or the execution directory's lock file, is gone (section 4.3).
    NotFound,
    /// A write failed: the execution directory, the trash directory, or the lock file.
    Failed(io::Error),
}

/// Step 1: makes `executions/<plan>/` and `<state>/trash/` when missing, and takes the plan's
/// `act.lock` with one try, checking it as section 4.3 says; `None` when another holds it.
fn exclude(state: &StateDir, plan: &str) -> Result<Option<ExecutionLock>, Excluding> {
    let reserve = state.reserve_file();
    let make = |path: &Path| {
        state::retry_with_reserve(&reserve, || {
            testpoint::fake_write("execution_dir")?;
            match std::fs::DirBuilder::new().mode(0o700).create(path) {
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
                made => state::logged(Op::Mkdir, path, None, made),
            }
        })
    };
    for dir in [
        state.executions_dir(),
        state.execution_dir(plan),
        state.trash_dir(),
    ] {
        make(&dir).map_err(Excluding::Failed)?;
    }
    let lock = state.execution_lock(plan);
    state::retry_with_reserve(&reserve, || {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock)
    })
    .map_err(Excluding::Failed)?;
    match recover::lock_execution(state, plan).map_err(Excluding::Failed)? {
        Locked::Held(held) => {
            if state.plan_dir(plan).is_dir() {
                Ok(Some(held))
            } else {
                Err(Excluding::NotFound)
            }
        }
        Locked::InProgress => Ok(None),
        Locked::NotFound => Err(Excluding::NotFound),
    }
}

/// The outcome of a write that failed before anything moved: `rejected`, with `no_space` when
/// it failed for lack of space.
fn write_failed(err: &io::Error) -> Outcome {
    Outcome {
        reasons: if state::is_no_space(err) {
            vec![NO_SPACE.into()]
        } else {
            Vec::new()
        },
        error: Some(err.to_string()),
        ..Outcome::default()
    }
}

/// An execution under way: the plan's run, what its evaluation read, and its locks.
struct Execution<'r, 'a> {
    r: &'r Run<'a>,
    outcome: Outcome,
    walk: Walk,
    target: Target,
    observation: WorktreeObservation,
    lock: ExecutionLock,
    in_use: Option<FileLock>,
}

/// Why a step stopped the execution, and how it ends.
enum End {
    /// Before the claim: nothing was written but the execution directory.
    Unclaimed(Box<Outcome>),
    /// After the claim and before the move: the empty trash entry is removed, and the result
    /// written.
    NotMoved(Box<Outcome>),
    /// After the claim, the trash entry found taken: the result is written, and the entry,
    /// not this execution's, stays.
    Occupied(Box<Outcome>),
    /// After the move: the target is restored, and the result written.
    Restore(Box<Outcome>),
}

impl<'r, 'a> Execution<'r, 'a> {
    fn start(
        r: &'r Run<'a>,
        evaluated: Evaluated,
        lock: ExecutionLock,
        in_use: FileLock,
    ) -> Result<Self, Box<ActResult>> {
        let Evaluated {
            outcome,
            snapshot,
            observation,
            ..
        } = evaluated;
        // Every invariant passed, so the snapshot and the observation were read.
        let (Ok(walk), Some(target), Some(observation)) =
            (snapshot.walk, snapshot.opened, observation)
        else {
            return Err(Box::new(r.report(Outcome {
                error: Some("the snapshot or the observation is missing".into()),
                ..outcome
            })));
        };
        Ok(Self {
            r,
            outcome,
            walk,
            target,
            observation,
            lock,
            in_use: Some(in_use),
        })
    }
}

/// What step 2 read that step 8 reads again (section 9.7): the identities of the root and the
/// git dir, the values git resolves, each ignore input, and the index entries reaching the
/// target's parent, by the spelling the index stores.
struct Facts {
    root: (u64, u64),
    git_dir: PathBuf,
    git_dir_id: (u64, u64),
    values: IgnoreValues,
    inputs: Vec<(PathBuf, Reading)>,
    at_parent: BTreeSet<PathBuf>,
}

/// An ignore input as read by the name git opens: missing (`ENOENT`), or the `lstat` of the
/// name and, through it, the file's identity, size, mtime, and a hash of its content.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Reading {
    Missing,
    Present {
        name: Meta,
        content: Option<(Meta, u64)>,
    },
}

/// What a `stat` reads of an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Meta {
    kind: Kind,
    dev: u64,
    ino: u64,
    size: u64,
    mtime_ns: i128,
}

impl Meta {
    fn of(meta: &std::fs::Metadata) -> Self {
        let ty = meta.file_type();
        let kind = if ty.is_file() {
            Kind::File
        } else if ty.is_dir() {
            Kind::Dir
        } else if ty.is_symlink() {
            Kind::Symlink
        } else {
            Kind::Other
        };
        Self {
            kind,
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime_ns: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
        }
    }
}

/// The index file as pinned before the move (section 9.7, "Pin and bound reads").
#[derive(Clone, Debug, PartialEq, Eq)]
struct Pin {
    path: PathBuf,
    meta: Meta,
}

/// What the overlap rule found going down the target's chain (section 9.7, `tracked_overlap`).
#[derive(Debug, Default)]
struct Overlap {
    /// Entries that name the target or a path below it, or end at a directory of the chain.
    found: Vec<PathBuf>,
    /// The entries whose leading components resolved to the chain down to the target's parent,
    /// by the spelling the index stores.
    at_parent: BTreeSet<PathBuf>,
}

impl Execution<'_, '_> {
    fn state(&self) -> &StateDir {
        self.r.state
    }

    fn plan(&self) -> &str {
        &self.r.plan.id
    }

    fn worktree(&self) -> PathBuf {
        self.r.worktree()
    }

    fn entry(&self) -> PathBuf {
        self.state().trash_entry(self.plan())
    }

    fn git_dir(&self) -> Result<PathBuf, String> {
        self.observation
            .worktree
            .git_dir
            .as_ref()
            .map(PathBuf::from)
            .ok_or_else(|| "observation did not read the worktree's git dir".to_owned())
    }

    fn common_dir(&self) -> PathBuf {
        PathBuf::from(&self.observation.repo.common_dir)
    }

    fn go(self) -> ActResult {
        let git_dir = match self.git_dir() {
            Ok(git_dir) => git_dir,
            Err(why) => {
                let outcome = self.failed(Vec::new(), why);
                return self.end(End::Unclaimed(Box::new(outcome)));
            }
        };
        let config = self.r.config;
        let git = BoundGit::new(config, &self.worktree(), &git_dir);
        // 2. Claim.
        let facts = match self.eligibility(&git, &git_dir) {
            Ok(facts) => facts,
            Err((reasons, details)) => {
                let outcome = Outcome {
                    status: ActStatus::DryRun,
                    reasons,
                    error: Some(details),
                    ..self.outcome.clone()
                };
                return self.end(End::Unclaimed(Box::new(outcome)));
            }
        };
        let claim = match self.claim() {
            Ok(claim) => claim,
            Err(end) => return self.end(end),
        };
        // 3. Move.
        let (parent, entry) = match self.make_entry() {
            Ok(opened) => opened,
            Err(end) => return self.end(end),
        };
        if let Err(err) = testpoint::hit(testpoint::ACT_CLAIMED) {
            let outcome = self.failed(Vec::new(), err.to_string());
            return self.end(End::NotMoved(Box::new(outcome)));
        }
        let pin = match self.pin(&git, &git_dir, &facts) {
            Ok(pin) => pin,
            Err(why) => {
                let outcome = self.failed(vec![CHANGED_DURING_ACT.into()], why);
                return self.end(End::NotMoved(Box::new(outcome)));
            }
        };
        match self.move_target(&parent, &entry) {
            Ok(()) => self.moved(&git, &facts, &claim, &pin, &parent, &entry),
            Err(End::Restore(outcome)) => self.restore(&parent, *outcome),
            Err(end) => self.end(end),
        }
    }

    /// Steps 4 to 10, after the target moved.
    fn moved(
        mut self,
        git: &BoundGit<'_>,
        facts: &Facts,
        claim: &ActClaim,
        pin: &Pin,
        parent: &File,
        entry: &File,
    ) -> ActResult {
        // 4. to 7.
        if let Err(end) = self.check_moved(git, entry) {
            return self.restore(parent, *end);
        }
        if let Err(err) = testpoint::hit(testpoint::ACT_CHECKED) {
            let outcome = self.failed(Vec::new(), err.to_string());
            return self.restore(parent, outcome);
        }
        // 8. Commit.
        let committed = ActClaim {
            stage: ClaimStage::Committed,
            updated_ms: state::now_ms(),
            ..claim.clone()
        };
        let mut staged = match recover::stage_claim(self.state(), &committed) {
            Ok(staged) => staged,
            Err(err) => {
                let outcome = write_failed_after(&self.outcome, &err);
                return self.restore(parent, outcome);
            }
        };
        if let Err(err) = testpoint::hit(testpoint::ACT_PUBLISHING) {
            let outcome = self.failed(Vec::new(), err.to_string());
            return self.restore(parent, outcome);
        }
        match self.commit_checks(git, facts, pin) {
            Ok(()) => {}
            Err(Commit::Changed(what)) => {
                let outcome = self.failed(
                    vec![CHANGED_DURING_ACT.into()],
                    format!("changed during act: {what}"),
                );
                return self.restore(parent, outcome);
            }
            Err(Commit::NotGranted(why)) => {
                let outcome = Outcome {
                    status: ActStatus::DryRun,
                    reasons: vec![EXECUTION_NOT_GRANTED.into()],
                    error: Some(why),
                    ..self.outcome.clone()
                };
                return self.restore(parent, outcome);
            }
        }
        let published = recover::publish_staged(
            self.state(),
            self.plan(),
            &mut staged,
            Some(testpoint::ACT_COMMITTED),
        );
        match published {
            Ok(Published::Durable) => {}
            Ok(Published::NotSynced(err)) => {
                // `committed` stands: the execution never restores, and recovery finishes it.
                let outcome = Outcome {
                    status: ActStatus::Partial,
                    trash: Some(self.entry().display().to_string()),
                    error: Some(format!(
                        "committed, and its sync failed; recovery finishes the execution: {err}"
                    )),
                    freed_bytes: None,
                    ..self.outcome.clone()
                };
                recover::stranded(
                    self.state(),
                    &committed,
                    &self.entry(),
                    "committed, and the claim's sync failed",
                    &[],
                );
                return self.without_result(outcome);
            }
            Err(err) => {
                let outcome = write_failed_after(&self.outcome, &err);
                return self.restore(parent, outcome);
            }
        }
        // No job that took the in-use lock started before the commit.
        self.in_use = None;
        // 9. Delete and 10. Finish.
        self.delete(git, &committed)
    }

    /// An outcome `rejected` with `reasons` and `why`, after the evaluation's checks.
    fn failed(&self, reasons: Vec<String>, why: String) -> Outcome {
        Outcome {
            status: ActStatus::Rejected,
            reasons,
            error: Some(why),
            freed_bytes: None,
            ..self.outcome.clone()
        }
    }

    /// Ends the execution as `end` says, writing its result where it published a claim.
    fn end(self, end: End) -> ActResult {
        match end {
            End::Unclaimed(outcome) => self.without_result(*outcome),
            End::NotMoved(mut outcome) => {
                if let Err(err) = recover::remove_empty_entry(self.state(), self.plan()) {
                    let entry = self.entry();
                    if entry.exists() {
                        outcome.trash = Some(entry.display().to_string());
                    }
                    let error = outcome.error.take().unwrap_or_default();
                    outcome.error = Some(format!("{error}; the trash entry stays: {err}"));
                }
                self.finish(*outcome)
            }
            End::Occupied(outcome) => self.finish(*outcome),
            End::Restore(_) => unreachable!("a restore ends through Execution::restore"),
        }
    }

    /// Ends an execution without writing a result: one that published no claim, where nothing
    /// but the execution directory was written, or one whose claim stays for recovery.
    fn without_result(self, outcome: Outcome) -> ActResult {
        let result = self.r.result(&outcome);
        drop(self.in_use);
        drop(self.lock);
        self.r.append(&result, outcome.provenance);
        result
    }

    /// Ends an execution with its result written durably, its locks released, and its record
    /// appended (section 9.7, step 10). A result that cannot be written leaves the claim to
    /// recovery.
    fn finish(self, mut outcome: Outcome) -> ActResult {
        let mut result = self.r.result(&outcome);
        if let Err(err) = recover::write_result(self.state(), &result) {
            let error = outcome
                .error
                .take()
                .map_or_else(String::new, |e| format!("{e}; "));
            outcome.error = Some(format!(
                "{error}the result was not written, and recovery finishes the execution: {err}"
            ));
            result = self.r.result(&outcome);
        }
        drop(self.in_use);
        drop(self.lock);
        self.r.append(&result, outcome.provenance);
        result
    }

    /// Restores the target into its original parent and ends with `outcome`. When the target
    /// cannot go back, it stays in the trash under its `claimed` claim for recovery, with
    /// `reclaim_stranded`, and `rejected` (`no_space`) when space ran out.
    fn restore(self, parent: &File, mut outcome: Outcome) -> ActResult {
        outcome.freed_bytes = None;
        match recover::restore(self.state(), self.plan(), parent, self.target.name()) {
            Ok(()) => self.finish(outcome),
            Err(err) => {
                let entry = self.entry();
                if let RestoreError::Stayed(io) = &err {
                    if state::is_no_space(io) {
                        outcome.status = ActStatus::Rejected;
                        outcome.reasons = vec![NO_SPACE.into()];
                    }
                    if let Ok(claim) = self.current_claim() {
                        let why = format!("the restore failed: {err}");
                        recover::stranded(self.state(), &claim, &entry, &why, &[]);
                    }
                }
                outcome.trash = Some(entry.display().to_string());
                let error = outcome
                    .error
                    .take()
                    .map_or_else(String::new, |e| format!("{e}; "));
                outcome.error = Some(format!("{error}{err}"));
                self.without_result(outcome)
            }
        }
    }

    /// The claim as published, for the evidence of `reclaim_stranded`.
    fn current_claim(&self) -> Result<ActClaim, String> {
        match recover::read_claim(self.state(), self.plan()) {
            ClaimRead::Read(claim) => Ok(*claim),
            ClaimRead::Absent => Err("no claim".into()),
            ClaimRead::Unreadable(why) => Err(why),
        }
    }

    /// Step 2's eligibility rules, with the target present (section 9.7, "Eligibility"): the
    /// reasons of every rule that fails and what each found, or what step 8 reads again.
    fn eligibility(
        &self,
        git: &BoundGit<'_>,
        git_dir: &Path,
    ) -> Result<Facts, (Vec<String>, String)> {
        let mut reasons = Vec::new();
        let mut details = Vec::new();
        let mut fail = |reason: &str, why: String| {
            if !reasons.iter().any(|r| r == reason) {
                reasons.push(reason.to_owned());
            }
            details.push(format!("{reason}: {why}"));
        };
        if let Err(why) = self.on_state_mount() {
            fail(NOT_ON_STATE_MOUNT, why);
        }
        match mount::bound_elsewhere(self.target.parent(), self.target.name()) {
            Ok(binds) if binds.is_empty() => {}
            Ok(binds) => fail(
                BOUND_ELSEWHERE,
                format!("mounted elsewhere at {}", paths(&binds)),
            ),
            Err(why) => fail(BOUND_ELSEWHERE, why),
        }
        let root = self.walk_root_id();
        let mut at_parent = BTreeSet::new();
        match git.index_entries() {
            Ok(entries) => {
                let sparse: Vec<PathBuf> = entries
                    .iter()
                    .filter(|e| e.sparse())
                    .map(|e| e.path.clone())
                    .collect();
                if !sparse.is_empty() {
                    fail(
                        SPARSE_CHECKOUT,
                        format!("skip-worktree or sparse entries: {}", paths(&sparse)),
                    );
                }
                match self.overlap(root, &entries) {
                    Ok(overlap) if overlap.found.is_empty() => at_parent = overlap.at_parent,
                    Ok(overlap) => fail(
                        TRACKED_OVERLAP,
                        format!("tracked: {}", paths(&overlap.found)),
                    ),
                    Err(why) => fail(TRACKED_OVERLAP, why),
                }
            }
            Err(why) => {
                fail(SPARSE_CHECKOUT, why.clone());
                fail(TRACKED_OVERLAP, why);
            }
        }
        let classification = self.classification(git);
        if let Err(why) = &classification {
            fail(NOT_DIRECTORY_EXCLUDED, why.clone());
        }
        let git_dir_id = match std::fs::metadata(git_dir) {
            Ok(meta) => Some((meta.dev(), meta.ino())),
            Err(err) => {
                // Without its git dir no rule decides; the index rules say why.
                fail(
                    TRACKED_OVERLAP,
                    format!("the git dir {}: {err}", git_dir.display()),
                );
                None
            }
        };
        match (classification, git_dir_id) {
            (Ok((values, inputs)), Some(git_dir_id)) if reasons.is_empty() => Ok(Facts {
                root,
                git_dir: git_dir.to_path_buf(),
                git_dir_id,
                values,
                inputs,
                at_parent,
            }),
            _ => Err((reasons, details.join("; "))),
        }
    }

    /// The worktree root's identity as the snapshot opened it.
    fn walk_root_id(&self) -> (u64, u64) {
        let id = self.target.root_id();
        (id.dev, id.ino)
    }

    /// `not_on_state_mount`: the target, `<state>/trash/`, and `<state>/executions/` are on one
    /// mount, each read from its opened directory, the target's from its parent for a file.
    fn on_state_mount(&self) -> Result<(), String> {
        let st = dir::stat_at(self.target.parent(), self.target.name())
            .map_err(|err| format!("the target: {err}"))?;
        let target_mount = if st.kind == Kind::Dir {
            let fd = dir::open_at(self.target.parent(), self.target.name())
                .map_err(|err| format!("the target: {err}"))?
                .ok_or("the target is no longer a directory")?;
            mount::of_dir(&fd)?
        } else {
            mount::of_entry_at(self.target.parent(), self.target.name())?
        };
        let state = self.state();
        for (what, path) in [
            ("<state>/trash/", state.trash_dir()),
            ("<state>/executions/", state.executions_dir()),
        ] {
            let fd = dir::open(&path)
                .map_err(|err| format!("{}: {err}", path.display()))?
                .ok_or_else(|| format!("{} is not a directory", path.display()))?;
            let dev = dir::stat_fd(&fd)
                .map_err(|err| format!("{}: {err}", path.display()))?
                .dev;
            if dev != st.dev || mount::of_dir(&fd)? != target_mount {
                return Err(format!("{what} is on another mount than the target"));
            }
        }
        Ok(())
    }

    /// `tracked_overlap` going down the target's chain from the worktree root, opened again
    /// with the snapshot's identity (section 9.7): each entry whose leading components resolved
    /// to the chain so far has its next component `lstat`ed in the current directory, as the
    /// volume resolves names, and counts when that is the target, or when it ends at a
    /// directory of the chain. At step 8 the target has moved, so no entry resolves to it.
    fn overlap(&self, root: (u64, u64), entries: &[IndexEntry]) -> Result<Overlap, String> {
        let worktree = self.worktree();
        let fd = dir::open(&worktree)
            .map_err(|err| format!("the worktree root: {err}"))?
            .ok_or("the worktree root is not a directory")?;
        let st = dir::stat_fd(&fd).map_err(|err| format!("the worktree root: {err}"))?;
        if (st.dev, st.ino) != root {
            return Err("the worktree root changed".into());
        }
        let mut levels: Vec<(&OsStr, (u64, u64))> = self
            .target
            .chain()
            .iter()
            .map(|(name, dev, ino)| (name.as_os_str(), (*dev, *ino)))
            .collect();
        let target = &self.walk.root;
        levels.push((self.target.name(), (target.dev, target.ino)));
        overlap(fd, &levels, entries)
    }

    /// `not_directory_excluded`: the values git resolves, every ignore input read before and
    /// after `git check-ignore -v`, which must name a rule that is not a negation, and neither
    /// that rule's source nor any other ignore input in the target.
    fn classification(
        &self,
        git: &BoundGit<'_>,
    ) -> Result<(IgnoreValues, Vec<(PathBuf, Reading)>), String> {
        let values = git.ignore_values()?;
        let inputs = self.input_paths(&values)?;
        let before = read_inputs(&inputs)?;
        let rel = self.target.rel();
        let rule = git
            .check_ignore(rel)?
            .ok_or("no rule of git's excludes the target itself")?;
        if rule.negation() {
            return Err(format!(
                "git names the negation {} from {}:{}",
                String::from_utf8_lossy(&rule.pattern),
                rule.source.display(),
                rule.line
            ));
        }
        let after = read_inputs(&inputs)?;
        if before != after {
            return Err("an ignore input changed while git read it".into());
        }
        let worktree = self.worktree();
        let target = worktree.join(rel);
        // git names the source as it opened it, from the root it runs in.
        let source = worktree.join(&rule.source);
        if !inputs.iter().any(|input| same_path(input, &source)) {
            return Err(format!(
                "git names the rule's source {}, which is not an ignore input the execution reads",
                rule.source.display()
            ));
        }
        for path in inputs.iter().chain(std::iter::once(&source)) {
            if inside(path, &target) {
                return Err(format!(
                    "the ignore input {} is in the target",
                    path.display()
                ));
            }
        }
        Ok((values, inputs.into_iter().zip(before).collect()))
    }

    /// The ignore inputs (section 9.7): each `.gitignore` from the worktree root down to the
    /// target's parent, the common dir's `info/exclude`, and the excludes file:
    /// `core.excludesFile`, else `$XDG_CONFIG_HOME/git/ignore` or `~/.config/git/ignore` in
    /// git's environment, which takes both from this process's. A relative path is git's from
    /// the worktree root, where every bound command runs.
    fn input_paths(&self, values: &IgnoreValues) -> Result<Vec<PathBuf>, String> {
        let worktree = self.worktree();
        let mut dir = worktree.clone();
        let mut paths = vec![dir.join(".gitignore")];
        for (name, _, _) in self.target.chain() {
            dir.push(name);
            paths.push(dir.join(".gitignore"));
        }
        paths.push(self.common_dir().join("info").join("exclude"));
        match values.get("core.excludesfile") {
            Some(Some(value)) if !value.is_empty() => {
                paths.push(worktree.join(OsStr::from_bytes(value)));
            }
            Some(_) => return Err("core.excludesFile is set without a path".into()),
            None => {
                let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
                if let Some(config) = var("XDG_CONFIG_HOME") {
                    paths.push(worktree.join(config).join("git").join("ignore"));
                } else if let Some(home) = var("HOME") {
                    paths.push(worktree.join(home).join(".config/git/ignore"));
                }
            }
        }
        Ok(paths)
    }

    /// Writes the manifest from the snapshot and publishes the claim at stage `claimed`, after
    /// `executions/<plan>/` is durable in `executions/` (section 9.7, durable order).
    fn claim(&self) -> Result<ActClaim, End> {
        let unclaimed = |why: String| End::Unclaimed(Box::new(self.failed(Vec::new(), why)));
        let manifest = manifest_of(&self.walk, self.target.rel()).map_err(unclaimed)?;
        let state = self.state();
        let written = recover::write_manifest(state, self.plan(), &manifest)
            .and_then(|()| state::sync_dir(&state.executions_dir()));
        if let Err(err) = written {
            return Err(End::Unclaimed(Box::new(write_failed_after(
                &self.outcome,
                &err,
            ))));
        }
        let claim = self.claim_of(&manifest).map_err(unclaimed)?;
        match recover::publish_claim(state, &claim) {
            Ok(Published::Durable) => Ok(claim),
            // The claim stands, so the result is written; nothing moved.
            Ok(Published::NotSynced(err)) => Err(End::NotMoved(Box::new(
                self.failed(Vec::new(), format!("the claim's sync: {err}")),
            ))),
            Err(err) => Err(End::Unclaimed(Box::new(write_failed_after(
                &self.outcome,
                &err,
            )))),
        }
    }

    /// The claim at stage `claimed` of the target as the snapshot read it.
    fn claim_of(&self, manifest: &[ManifestEntry]) -> Result<ActClaim, String> {
        let utf8 = |name: &OsStr| {
            name.to_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{} is not UTF-8", name.display()))
        };
        let chain = self
            .target
            .chain()
            .iter()
            .map(|(name, dev, ino)| {
                Ok(ChainDir {
                    name: utf8(name)?,
                    dev: *dev,
                    ino: *ino,
                })
            })
            .collect::<Result<_, String>>()?;
        let root = &manifest[0];
        let now = state::now_ms();
        Ok(ActClaim {
            schema: ActClaimSchema,
            plan_id: self.plan().to_owned(),
            caller: self.r.options.caller.to_string(),
            stage: ClaimStage::Claimed,
            created_ms: now,
            updated_ms: now,
            worktree: ClaimWorktree {
                path: self.r.plan.target.worktree.clone(),
                root: self.target.root_id(),
            },
            repo: ClaimRepo {
                common_dir: self.observation.repo.common_dir.clone(),
                // `observed` passed, so the observation read `origin`.
                remote: self.observation.remote.clone().ok().flatten(),
            },
            chain,
            target: ClaimTarget {
                name: utf8(self.target.name())?,
                dev: root.dev,
                ino: root.ino,
                kind: root.kind,
            },
            manifest_entries: manifest.len() as u64,
        })
    }

    /// Creates the trash entry and its hold, each without replacing, and syncs them and
    /// `<state>/trash/`; returns the target's parent and the trash entry, opened.
    fn make_entry(&self) -> Result<(File, File), End> {
        let state = self.state();
        let entry = self.entry();
        let reserve = state.reserve_file();
        let make = |path: &Path| {
            state::retry_with_reserve(&reserve, || {
                let made = std::fs::DirBuilder::new().mode(0o700).create(path);
                state::logged(Op::Mkdir, path, None, made)
            })
        };
        if let Err(err) = make(&entry) {
            let mut outcome = write_failed_after(&self.outcome, &err);
            if err.kind() == io::ErrorKind::AlreadyExists {
                // Not this execution's: it stays, and the result names it.
                outcome.reasons = vec![TRASH_OCCUPIED.into()];
                outcome.error = Some(format!("{} exists", entry.display()));
                // The claim is published, so the result is written; the entry is not removed.
                return Err(End::Occupied(Box::new(outcome)));
            }
            return Err(End::NotMoved(Box::new(outcome)));
        }
        let opened = (|| {
            make(&entry.join("h"))?;
            state::sync_dir(&entry)?;
            state::sync_dir(&state.trash_dir())?;
            let entry_fd = dir::open(&entry)?
                .ok_or_else(|| io::Error::other("the trash entry is not a directory"))?;
            let parent = File::from(self.target.parent().try_clone()?);
            Ok::<_, io::Error>((parent, entry_fd))
        })();
        opened.map_err(|err| End::NotMoved(Box::new(write_failed_after(&self.outcome, &err))))
    }

    /// Pins the index file `git rev-parse --git-path index` names, right before the move,
    /// from an `lstat` that must succeed and name a regular file ([`index_meta`]); a split index, one whose file carries git's `link`
    /// extension, is not read (section 9.7).
    fn pin(&self, git: &BoundGit<'_>, git_dir: &Path, facts: &Facts) -> Result<Pin, String> {
        let path = git.index_path()?;
        if facts.values.truthy("core.splitindex") {
            return Err("the index: core.splitIndex is set".into());
        }
        let meta = index_meta(&path)?;
        let dirs = [git_dir.to_path_buf(), self.common_dir()];
        if let Some(shared) = crate::observe::split_index_file(&path, &dirs)
            .map_err(|why| format!("the index: {why}"))?
        {
            return Err(format!("the index is split: {}", shared.display()));
        }
        Ok(Pin { path, meta })
    }

    /// Renames the target from its parent's descriptor to `t` in the trash entry without
    /// replacing, and syncs the parent and the trash entry; pauses at `act.moved`.
    fn move_target(&self, parent: &File, entry: &File) -> Result<(), End> {
        let state = self.state();
        let from = self.worktree().join(self.target.rel());
        let to = self.entry().join("t");
        let moved = state::retry_with_reserve(&state.reserve_file(), || {
            testpoint::fake_write("move")?;
            let renamed = dir::rename_noreplace(parent, self.target.name(), entry, OsStr::new("t"));
            state::logged(Op::Rename, &from, Some(&to), renamed)
        });
        if let Err(err) = moved {
            let mut outcome = write_failed_after(&self.outcome, &err);
            match err.kind() {
                io::ErrorKind::AlreadyExists => {
                    outcome.reasons = vec![TRASH_OCCUPIED.into()];
                    outcome.trash = Some(self.entry().display().to_string());
                }
                io::ErrorKind::NotFound => {
                    outcome.status = ActStatus::Expired;
                    outcome.reasons = vec!["identity_changed:target".into()];
                }
                _ => {}
            }
            return Err(End::NotMoved(Box::new(outcome)));
        }
        let synced = (|| {
            let parent_path = dir::current_path(parent).unwrap_or_default();
            state::logged(Op::Sync, &parent_path, None, parent.sync_all())?;
            state::logged(Op::Sync, &self.entry(), None, entry.sync_all())?;
            testpoint::hit(testpoint::ACT_MOVED).map_err(io::Error::other)
        })();
        synced.map_err(|err| End::Restore(Box::new(self.failed(Vec::new(), err.to_string()))))
    }

    /// Steps 4 to 7 after the move: the move bound to the planned place, the moved tree the
    /// manifest's, the configuration and git's lock again, and the users last.
    fn check_moved(&mut self, git: &BoundGit<'_>, entry: &File) -> Result<(), Box<Outcome>> {
        // 4. Verify.
        let root = &self.walk.root;
        let t = dir::stat_at(entry, OsStr::new("t")).ok();
        let same = t.is_some_and(|t| {
            (t.dev, t.ino) == (root.dev, root.ino) && same_kind(t.kind, root.kind)
        });
        if !same {
            return Err(Box::new(Outcome {
                status: ActStatus::Expired,
                reasons: vec!["identity_changed:target".into()],
                ..self.failed(Vec::new(), "t is not the snapshot's target".into())
            }));
        }
        if let Err(field) = self.path_leads_through_chain() {
            return Err(Box::new(Outcome {
                status: ActStatus::Expired,
                reasons: vec![format!("identity_changed:{field}")],
                ..self.failed(
                    Vec::new(),
                    "the planned path no longer leads through the chain".into(),
                )
            }));
        }
        // 5. Compare.
        let changed = self.compare(entry);
        if !changed.is_empty() {
            let why = format!("changed since the snapshot: {}", paths(&changed));
            return Err(Box::new(self.failed(vec![CHANGED_DURING_ACT.into()], why)));
        }
        // 6. Check again.
        self.check_again(git)?;
        // 7. In use, last.
        self.in_use_last()
    }

    /// Whether the planned path, walked by name from a fresh open of the worktree root, leads
    /// through the chain the snapshot opened; `Err` names the field that differs.
    fn path_leads_through_chain(&self) -> Result<(), &'static str> {
        let root = dir::open(&self.worktree())
            .ok()
            .flatten()
            .ok_or("root_id")?;
        let st = dir::stat_fd(&root).map_err(|_| "root_id")?;
        if (st.dev, st.ino) != self.walk_root_id() {
            return Err("root_id");
        }
        let mut current = root;
        for (name, dev, ino) in self.target.chain() {
            let next = dir::open_at(&current, name)
                .ok()
                .flatten()
                .ok_or("parent")?;
            let st = dir::stat_fd(&next).map_err(|_| "parent")?;
            if (st.dev, st.ino) != (*dev, *ino) {
                return Err("parent");
            }
            current = next;
        }
        Ok(())
    }

    /// Step 5: the moved tree, walked from `t`'s descriptor as the snapshot walked, against the
    /// manifest: the paths that differ in identity, type, size, or mtime, the root by identity
    /// and type alone; that are missing or new; that are special files; or that are on another
    /// device or mount.
    fn compare(&self, entry: &File) -> Vec<PathBuf> {
        let root = &self.walk.root;
        let mut changed = Vec::new();
        if root.kind != EntryKind::Dir {
            return changed;
        }
        let at = self.entry().join("t");
        let walked = (|| {
            let fd = dir::open_at(entry, OsStr::new("t"))?
                .ok_or_else(|| io::Error::other("t is not a directory"))?;
            let mount = mount::of_dir(&fd).map_err(io::Error::other)?;
            super::unix::walk_moved(&fd.into(), (root.dev, &mount), &at)
        })();
        let Ok((entries, boundaries)) = walked else {
            return vec![PathBuf::new()];
        };
        let target_rel = self.target.rel();
        let mut expected: HashMap<PathBuf, &plan::Entry> = self
            .walk
            .entries
            .iter()
            .filter_map(|e| Some((e.rel.strip_prefix(target_rel).ok()?.to_path_buf(), e)))
            .collect();
        for found in &entries {
            let same = expected.remove(&found.rel).is_some_and(|e| {
                (e.dev, e.ino, e.kind, e.size, e.mtime_ns)
                    == (found.dev, found.ino, found.kind, found.size, found.mtime_ns)
            });
            if !same || found.kind.is_special() {
                changed.push(found.rel.clone());
            }
        }
        changed.extend(boundaries);
        changed.extend(expected.into_keys());
        changed.sort();
        changed.dedup();
        changed
    }

    /// Step 6: the configuration loaded again, git's lock on the worktree and `origin` read
    /// again, and `regenerable`, `no_protected`, `no_unclassifiable`, and `not_managed`
    /// evaluated on the manifest with the profiles `origin` selects now; the checks replace the
    /// evaluation's.
    fn check_again(&mut self, git: &BoundGit<'_>) -> Result<(), Box<Outcome>> {
        let remote = self
            .origin(git)
            .map_err(|why| Box::new(self.failed(vec![CHANGED_DURING_ACT.into()], why)))?;
        let config = Config::load(&self.r.config.dir).map_err(|err| {
            Box::new(self.failed(
                vec![CHANGED_DURING_ACT.into()],
                format!("the configuration does not load: {err}"),
            ))
        })?;
        let locked = self.git_locked().map_err(|why| {
            Box::new(self.failed(
                vec![CHANGED_DURING_ACT.into()],
                format!("git's lock: {why}"),
            ))
        })?;
        let mut observation = self.observation.clone();
        let app = observation
            .worktree
            .managed_by
            .take()
            .filter(|app| app != GIT_LOCK);
        observation.worktree.managed_by = app.or_else(|| locked.then(|| GIT_LOCK.to_owned()));
        let patterns = selected_patterns(&config, &self.observation, remote.as_deref());
        let in_use = InUseFacts {
            lock_held: Ok(false),
            reservations: Err(String::new()),
            processes: Err(String::new()),
            descriptors: None,
        };
        let worktree = self.worktree();
        let target = worktree.join(self.target.rel());
        let inspection = Inspection {
            kind: PlanKind::ReclaimDir,
            worktree: &worktree,
            target: &target,
            observation: Ok(&observation),
            walk: Ok(&self.walk),
            in_use: &in_use,
            patterns: &patterns,
            managed: &config.host.observe.managed,
            squash_merged: false,
        };
        let again = plan::evaluate_only(&inspection, |name| CHECKED_AGAIN.contains(&name));
        self.replace_checks(&again);
        let failed: Vec<String> = again
            .iter()
            .filter(|c| c.result != CheckResult::Pass)
            .map(|c| c.name.clone())
            .collect();
        if failed.is_empty() {
            Ok(())
        } else {
            Err(Box::new(
                self.failed(failed, "checked again after the move".into()),
            ))
        }
    }

    /// `origin` read through `git` again, which must be the URL the observation selected the
    /// profiles with: a URL that changed or a read that failed is an error naming it.
    fn origin(&self, git: &BoundGit<'_>) -> Result<Option<String>, String> {
        origin_again(git, self.observation.remote.as_ref().ok())
    }

    /// Whether git has locked the worktree: its administrative entry holds `locked`. The main
    /// worktree, whose git dir is the common dir, has no such lock.
    fn git_locked(&self) -> Result<bool, String> {
        let git_dir = self.git_dir()?;
        if Path::new(&git_dir) == self.common_dir() {
            return Ok(false);
        }
        let path = git_dir.join("locked");
        match std::fs::symlink_metadata(&path) {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(format!("{}: {err}", path.display())),
        }
    }

    /// Step 7: the reservations naming the worktree, the working directories inside it or the
    /// trash entry, and the descriptor scan of the trash entry; anything found fails
    /// `not_in_use`.
    fn in_use_last(&mut self) -> Result<(), Box<Outcome>> {
        let worktree = self.worktree();
        let key = state::worktree_key(&worktree);
        let entry = self.entry();
        let processes = plan::processes_inside(&worktree).and_then(|mut pids| {
            pids.extend(plan::processes_inside(&entry)?);
            pids.sort_unstable();
            pids.dedup();
            Ok(pids)
        });
        let in_use = InUseFacts {
            lock_held: Ok(false),
            reservations: self.r.reservations(&key),
            processes,
            descriptors: plan::descriptor_scan(PlanKind::ReclaimDir),
        };
        let patterns = DataLists::default();
        let inspection = Inspection {
            kind: PlanKind::ReclaimDir,
            worktree: &worktree,
            target: &entry,
            observation: Ok(&self.observation),
            walk: Ok(&self.walk),
            in_use: &in_use,
            patterns: &patterns,
            managed: &[],
            squash_merged: false,
        };
        let again = plan::evaluate_only(&inspection, |name| name == "not_in_use");
        self.replace_checks(&again);
        if again.iter().all(|c| c.result == CheckResult::Pass) {
            Ok(())
        } else {
            Err(Box::new(self.failed(
                vec!["not_in_use".into()],
                "in use after the move".into(),
            )))
        }
    }

    /// Replaces the outcome's checks of the same names with `checks`.
    fn replace_checks(&mut self, checks: &[Check]) {
        for check in checks {
            if let Some(old) = self
                .outcome
                .checks
                .iter_mut()
                .find(|c| c.name == check.name)
            {
                *old = check.clone();
            }
        }
    }

    /// Step 8's reads, in order (section 9.7): the identities of the root and the git dir and
    /// the pin; the index listed again, which must pass the sparse and overlap rules with the
    /// entries reaching the target's parent those step 2 found there; the values git resolves
    /// and each ignore input; the configuration, for the grant; and the identities and the pin
    /// again, last.
    fn commit_checks(&self, git: &BoundGit<'_>, facts: &Facts, pin: &Pin) -> Result<(), Commit> {
        self.identities(facts, pin)?;
        let entries = git
            .index_entries()
            .map_err(|why| Commit::Changed(format!("the index listing: {why}")))?;
        if let Some(entry) = entries.iter().find(|e| e.sparse()) {
            return Err(Commit::Changed(format!(
                "the index entry {} is skip-worktree or sparse",
                entry.path.display()
            )));
        }
        let overlap = self
            .overlap(facts.root, &entries)
            .map_err(|why| Commit::Changed(format!("the index: {why}")))?;
        if let Some(found) = overlap.found.first() {
            return Err(Commit::Changed(format!(
                "the index entry {}",
                found.display()
            )));
        }
        if overlap.at_parent != facts.at_parent {
            let entry = overlap
                .at_parent
                .symmetric_difference(&facts.at_parent)
                .next()
                .map_or_else(String::new, |p| p.display().to_string());
            return Err(Commit::Changed(format!("the index entry {entry}")));
        }
        let values = git
            .ignore_values()
            .map_err(|why| Commit::Changed(format!("git's configuration: {why}")))?;
        if values != facts.values {
            let key = crate::observe::IGNORE_KEYS
                .iter()
                .find(|key| values.get(key) != facts.values.get(key))
                .copied()
                .unwrap_or("git's configuration");
            return Err(Commit::Changed(key.to_owned()));
        }
        for (path, reading) in &facts.inputs {
            match read_input(path) {
                Ok(now) if now == *reading => {}
                Ok(_) => {
                    return Err(Commit::Changed(format!(
                        "the ignore input {}",
                        path.display()
                    )));
                }
                Err(why) => {
                    return Err(Commit::Changed(format!("the ignore input {why}")));
                }
            }
        }
        self.origin(git).map_err(Commit::Changed)?;
        let config = Config::load(&self.r.config.dir)
            .map_err(|err| Commit::Changed(format!("the configuration does not load: {err}")))?;
        let caller = &self.r.options.caller;
        if !config.host.grants(caller, PlanKind::ReclaimDir) {
            return Err(Commit::NotGranted(format!(
                "act.execute no longer grants {caller} reclaim_dir plans"
            )));
        }
        if let Caller::Unit(name) = caller
            && !config.host.unit_may_act(name)
        {
            return Err(Commit::NotGranted(format!("unit.{name}.act is false")));
        }
        self.identities(facts, pin)
    }

    /// The worktree root's and the git dir's identities as step 2 read them, and the index
    /// file as pinned.
    fn identities(&self, facts: &Facts, pin: &Pin) -> Result<(), Commit> {
        let worktree = self.worktree();
        let root = std::fs::symlink_metadata(&worktree)
            .map_err(|err| Commit::Changed(format!("the worktree root: {err}")))?;
        if (root.dev(), root.ino()) != facts.root {
            return Err(Commit::Changed(format!(
                "the worktree root {}",
                worktree.display()
            )));
        }
        let git_dir = std::fs::metadata(&facts.git_dir)
            .map_err(|err| Commit::Changed(format!("the git dir: {err}")))?;
        if (git_dir.dev(), git_dir.ino()) != facts.git_dir_id {
            return Err(Commit::Changed(format!(
                "the git dir {}",
                facts.git_dir.display()
            )));
        }
        let index = index_meta(&pin.path).map_err(Commit::Changed)?;
        if index != pin.meta {
            return Err(Commit::Changed(format!("the index {}", pin.path.display())));
        }
        Ok(())
    }

    /// The outcome of a committed execution whose deletion left the trash entry to recovery:
    /// `partial`, naming the entry, what it kept, and why.
    fn left_in_trash(&self, why: String, kept: Vec<String>, freed: Option<u64>) -> Outcome {
        Outcome {
            status: ActStatus::Partial,
            skipped_changed: kept,
            trash: Some(self.entry().display().to_string()),
            freed_bytes: freed,
            error: Some(why),
            ..self.outcome.clone()
        }
    }

    /// Steps 9 and 10 after the commit: deletes the trash entry through the deleter, which
    /// also reads `origin` before each batch, and finishes, or leaves the claim to recovery
    /// when the deletion stopped.
    fn delete(self, git: &BoundGit<'_>, claim: &ActClaim) -> ActResult {
        let state = self.state();
        let entry = self.entry();
        let manifest = match manifest_of(&self.walk, self.target.rel()) {
            Ok(manifest) => manifest,
            Err(why) => {
                let outcome = self.left_in_trash(why, Vec::new(), None);
                return self.without_result(outcome);
            }
        };
        let rel = self.target.rel().to_path_buf();
        let measure = Measure::of(&self.walk.fs_type);
        let reserve = state.reserve_file();
        let mut watch = OriginWatch {
            watch: Watch::new(&self.r.config.dir, &claim.repo, &rel, &entry, &manifest),
            git,
            remote: claim.repo.remote.clone(),
        };
        let deletion = Deletion {
            entry: &entry,
            manifest: &manifest,
            target_rel: &rel,
            measure: &measure,
            reserve: &reserve,
            resumed: false,
        };
        let deleted = deletion.run(&mut watch);
        let kept = deleted.kept(&manifest);
        if let Some(stop) = &deleted.stopped {
            let reasons = match stop {
                Stop::Guard(why) if why.starts_with(delete::IN_USE) => {
                    vec![IN_USE_WHILE_DELETING.to_owned()]
                }
                Stop::Write(err) if state::is_no_space(err) => vec![NO_SPACE.to_owned()],
                _ => Vec::new(),
            };
            let why = format!("the deletion stopped: {stop}");
            recover::stranded(state, claim, &entry, &why, &kept);
            let outcome = Outcome {
                reasons,
                ..self.left_in_trash(why, kept, deleted.freed_bytes)
            };
            return self.without_result(outcome);
        }
        let remains = match recover::clean_up(&entry) {
            Ok(remains) => remains,
            Err(err) => {
                let why = format!("the cleanup: {err}");
                recover::stranded(state, claim, &entry, &why, &kept);
                let outcome = self.left_in_trash(why, kept, deleted.freed_bytes);
                return self.without_result(outcome);
            }
        };
        let mut kept = kept;
        if remains && kept.is_empty() {
            // The trash entry holds what the manifest does not name: the target is kept.
            kept.push(String::new());
        }
        if remains {
            recover::stranded(state, claim, &entry, "entries kept", &kept);
        }
        let outcome = Outcome {
            status: if remains || !kept.is_empty() {
                ActStatus::Partial
            } else {
                ActStatus::Executed
            },
            reasons: Vec::new(),
            skipped_changed: kept,
            trash: remains.then(|| entry.display().to_string()),
            freed_bytes: deleted.freed_bytes,
            error: None,
            ..self.outcome.clone()
        };
        self.finish(outcome)
    }
}

/// Why step 8 did not commit.
enum Commit {
    /// An input changed, or a read of it failed: what.
    Changed(String),
    /// The caller's grant was withdrawn, or its unit's gate closed: why.
    NotGranted(String),
}

/// The outcome of a write that failed after the evaluation passed: `rejected`, with `no_space`
/// when it failed for lack of space, the evaluation's checks kept.
fn write_failed_after(evaluated: &Outcome, err: &io::Error) -> Outcome {
    Outcome {
        checks: evaluated.checks.clone(),
        provenance: evaluated.provenance.clone(),
        ..write_failed(err)
    }
}

/// The `lstat` of the index file at `path`, which must succeed and name a regular file: git
/// lists an index it cannot open, a symbolic link that names nothing among them, as empty and
/// exits 0, so only a regular file read by its own name is the index in force (section 9.7).
fn index_meta(path: &Path) -> Result<Meta, String> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|err| format!("the index {}: {err}", path.display()))?;
    if !meta.file_type().is_file() {
        return Err(format!(
            "the index {} is not a regular file",
            path.display()
        ));
    }
    Ok(Meta::of(&meta))
}

/// `origin` read through `git`, which must be `expected`: a URL that changed, or a read that
/// failed, is an error naming it. `expected` is `None` when the observation could not read it.
fn origin_again(
    git: &BoundGit<'_>,
    expected: Option<&Option<String>>,
) -> Result<Option<String>, String> {
    let now = git
        .remote_url()
        .map_err(|why| format!("origin could not be read: {why}"))?;
    // Named, never quoted: a URL may hold credentials.
    if expected == Some(&now) {
        Ok(now)
    } else {
        Err("origin changed".into())
    }
}

/// The deleter's guard of a live execution: [`Watch`], and `origin` read before each batch,
/// which stops the deletion when its URL is no longer the claim's, as the profiles it
/// selects could protect what the batch deletes.
struct OriginWatch<'a> {
    watch: Watch,
    git: &'a BoundGit<'a>,
    remote: Option<String>,
}

impl delete::Guard for OriginWatch<'_> {
    fn before_batch(&mut self) -> Result<(), String> {
        origin_again(self.git, Some(&self.remote))?;
        self.watch.before_batch()
    }

    fn patterns(&self) -> &DataLists {
        self.watch.patterns()
    }
}

/// The patterns of the profiles that the observation's common dir and the `origin` URL
/// `remote` select in `config`, as recovery selects them (section 3.3).
fn selected_patterns(
    config: &Config,
    observation: &WorktreeObservation,
    remote: Option<&str>,
) -> DataLists {
    let common_dir = Path::new(&observation.repo.common_dir);
    DataLists::from_selection(&config.select(|profile| {
        profile.matches_common_dir(common_dir)
            || remote.is_some_and(|url| profile.matches_remote(url))
    }))
}

/// The manifest of the snapshot `walk` of a target at `target_rel`: every entry in walk order,
/// with its path relative to the target.
fn manifest_of(walk: &Walk, target_rel: &Path) -> Result<Vec<ManifestEntry>, String> {
    walk.all()
        .enumerate()
        .map(|(i, entry)| {
            let rel = entry
                .rel
                .strip_prefix(target_rel)
                .map_err(|_| format!("{} is outside the target", entry.rel.display()))?;
            let kind = match entry.kind {
                EntryKind::Dir => EntryType::Dir,
                EntryKind::File => EntryType::File,
                EntryKind::Symlink => EntryType::Symlink,
                other => return Err(format!("{} is a {other:?}", entry.rel.display())),
            };
            let path = match rel.to_str() {
                Some(path) => EntryPath::Utf8(path.to_owned()),
                None => EntryPath::Bytes(rel.as_os_str().as_bytes().to_vec()),
            };
            Ok(ManifestEntry {
                i: i as u64,
                path,
                dev: entry.dev,
                ino: entry.ino,
                kind,
                size: entry.size,
                mtime_ns: i64::try_from(entry.mtime_ns)
                    .map_err(|_| format!("{}: mtime out of range", entry.rel.display()))?,
                nlink: entry.nlink,
            })
        })
        .collect()
}

/// Whether the type `lstat` reads is the walk's.
fn same_kind(kind: Kind, entry: EntryKind) -> bool {
    matches!(
        (kind, entry),
        (Kind::Dir, EntryKind::Dir)
            | (Kind::File, EntryKind::File)
            | (Kind::Symlink, EntryKind::Symlink)
    )
}

/// The overlap rule over `levels`, each name with its identity from the worktree root's child
/// down to the target, starting in the root `root` (section 9.7, `tracked_overlap`). An
/// `lstat` that fails otherwise than with `ENOENT`, or a directory of the chain that is no
/// longer the one opened, is an error.
fn overlap(
    root: File,
    levels: &[(&OsStr, (u64, u64))],
    entries: &[IndexEntry],
) -> Result<Overlap, String> {
    let mut result = Overlap::default();
    let mut candidates: Vec<(&IndexEntry, Vec<&OsStr>)> = entries
        .iter()
        .map(|e| {
            let names = e
                .path
                .components()
                .filter_map(|c| match c {
                    Component::Normal(name) => Some(name),
                    _ => None,
                })
                .collect();
            (e, names)
        })
        .collect();
    let mut dir = root;
    for (depth, (name, id)) in levels.iter().enumerate() {
        let last = depth + 1 == levels.len();
        if last {
            result.at_parent = candidates.iter().map(|(e, _)| e.path.clone()).collect();
        }
        let mut resolved: HashMap<&OsStr, Option<(u64, u64)>> = HashMap::new();
        let mut next = Vec::new();
        for (entry, names) in candidates {
            let Some(component) = names.get(depth).copied() else {
                continue;
            };
            let at = if let Some(at) = resolved.get(component) {
                *at
            } else {
                let at = match dir::stat_at(&dir, component) {
                    Ok(st) => Some((st.dev, st.ino)),
                    Err(err) if err.kind() == io::ErrorKind::NotFound => None,
                    Err(err) => {
                        return Err(format!(
                            "{}: {err}",
                            PathBuf::from_iter(&names[..=depth]).display()
                        ));
                    }
                };
                resolved.insert(component, at);
                at
            };
            if at != Some(*id) {
                continue;
            }
            if last || names.len() == depth + 1 {
                result.found.push(entry.path.clone());
            } else {
                next.push((entry, names));
            }
        }
        candidates = next;
        if !last {
            let opened = dir::open_at(&dir, name)
                .map_err(|err| format!("{}: {err}", name.display()))?
                .ok_or_else(|| format!("{} is no longer a directory", name.display()))?;
            let st = dir::stat_fd(&opened).map_err(|err| format!("{}: {err}", name.display()))?;
            if (st.dev, st.ino) != *id {
                return Err(format!("{} changed", name.display()));
            }
            dir = opened;
        }
    }
    Ok(result)
}

/// Each ignore input of `paths` read by its name ([`read_input`]).
fn read_inputs(paths: &[PathBuf]) -> Result<Vec<Reading>, String> {
    paths.iter().map(|path| read_input(path)).collect()
}

/// An ignore input read by the name git opens: the `lstat` of the name, `Missing` only on
/// `ENOENT`, and, for a file or a link, the file it names with a hash of its content, none
/// for a link that names nothing. Any other error is the read's (section 9.7, "Reads that
/// fail").
fn read_input(path: &Path) -> Result<Reading, String> {
    let named = match std::fs::symlink_metadata(path) {
        Ok(meta) => Meta::of(&meta),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Reading::Missing),
        Err(err) => return Err(format!("{}: {err}", path.display())),
    };
    if !matches!(named.kind, Kind::File | Kind::Symlink) {
        return Ok(Reading::Present {
            name: named,
            content: None,
        });
    }
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(Reading::Present {
                name: named,
                content: None,
            });
        }
        Err(err) => return Err(format!("{}: {err}", path.display())),
    };
    let meta = file
        .metadata()
        .map_err(|err| format!("{}: {err}", path.display()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    Ok(Reading::Present {
        name: named,
        content: Some((Meta::of(&meta), state::fnv1a64(&bytes))),
    })
}

/// Whether `a` and `b` name one file: the same path, or the same path once the volume resolves
/// both.
fn same_path(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
}

/// Whether `path` is the target at `target` or below it, by name, or as the volume resolves
/// the directory that holds it.
fn inside(path: &Path, target: &Path) -> bool {
    if path.starts_with(target) {
        return true;
    }
    let canonical = |p: &Path| std::fs::canonicalize(p).ok();
    let held_in = path.parent().and_then(canonical);
    match (held_in, canonical(target)) {
        (Some(held_in), Some(target)) => held_in.starts_with(target),
        _ => false,
    }
}

/// The first few of `paths`, for a detail.
fn paths(paths: &[PathBuf]) -> String {
    let mut named: Vec<String> = paths
        .iter()
        .take(DETAIL_PATHS)
        .map(|p| p.display().to_string())
        .collect();
    if paths.len() > DETAIL_PATHS {
        named.push(format!("and {} more", paths.len() - DETAIL_PATHS));
    }
    named.join(", ")
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use super::{Reading, overlap, read_input};
    use crate::observe::{BoundGit, IndexEntry};
    use crate::os::child::Output;
    use crate::os::dir;

    /// A fake git that records each run's working directory and arguments and answers
    /// `answer`.
    fn recording<'a>(
        runs: &'a RefCell<Vec<(PathBuf, Vec<OsString>)>>,
        answer: impl Fn(&[OsString]) -> Output + 'a,
    ) -> Box<crate::observe::GitRunner<'a>> {
        Box::new(move |dir: &Path, argv: &[OsString]| {
            runs.borrow_mut().push((dir.to_path_buf(), argv.to_vec()));
            Ok(answer(argv))
        })
    }

    fn output(code: i32, stdout: &[u8], stderr: &[u8]) -> Output {
        Output {
            code: Some(code),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    /// A34: every git command of an execution runs in the checked root, with `--git-dir` and
    /// `--work-tree` naming the git dir and the root the checks read, before the command, so
    /// none discovers a git dir through the root's `.git`.
    #[test]
    fn git_reads_are_bound() {
        let runs = RefCell::new(Vec::new());
        let root = Path::new("/w/a");
        let git_dir = Path::new("/r/.git/worktrees/a");
        let git = BoundGit::with_runner(
            root,
            git_dir,
            recording(&runs, |argv| {
                let command = argv.iter().find(|a| {
                    ["rev-parse", "ls-files", "check-ignore", "config"]
                        .iter()
                        .any(|c| a.as_os_str() == *c)
                });
                match command.and_then(|c| c.to_str()) {
                    Some("rev-parse") => output(0, b"index\n", b""),
                    Some("ls-files") => output(0, b"H src/a.rs\0", b""),
                    Some("check-ignore") => output(0, b".gitignore:1:target/\ttarget\n", b""),
                    _ => output(1, b"", b""),
                }
            }),
        );
        git.index_path().unwrap();
        git.index_entries().unwrap();
        git.check_ignore(Path::new("target")).unwrap();
        git.ignore_values().unwrap();
        drop(git);
        let runs = runs.into_inner();
        assert_eq!(runs.len(), 4);
        for (dir, argv) in runs {
            assert_eq!(dir, root, "{argv:?}");
            let at = |arg: &str| argv.iter().position(|a| a.as_os_str() == arg);
            let c = at("-C").expect("-C");
            assert_eq!(argv[c + 1], root.as_os_str(), "{argv:?}");
            let bound = |prefix: &str, path: &Path| {
                let mut want = OsString::from(prefix);
                want.push(path);
                argv.iter().position(|a| *a == want)
            };
            let git_dir_at = bound("--git-dir=", git_dir).expect("--git-dir");
            let work_tree_at = bound("--work-tree=", root).expect("--work-tree");
            let command = argv
                .iter()
                .position(|a| {
                    ["rev-parse", "ls-files", "check-ignore", "config"]
                        .iter()
                        .any(|c| a.as_os_str() == *c)
                })
                .unwrap();
            assert!(git_dir_at < command && work_tree_at < command, "{argv:?}");
            assert!(
                argv.windows(2)
                    .any(|w| w[0] == "-c" && w[1] == "core.fsmonitor=false"),
                "{argv:?}"
            );
        }
    }

    /// A39: a read that is not complete decides nothing. git exiting 0 with a warning on
    /// stderr, exiting 128, or ending its output without the final NUL is an error of the
    /// read, and so is an ignore input whose `lstat` fails with `EACCES`; only `ENOENT` reads
    /// as a missing input.
    #[test]
    fn incomplete_reads_decide_nothing() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = Path::new("/w/a");
        let git_dir = Path::new("/r/.git/worktrees/a");
        let answers: [(&str, Output); 4] = [
            (
                "a warning",
                output(
                    0,
                    b".gitignore:1:target/\ttarget\n",
                    b"warning: unreadable\n",
                ),
            ),
            ("exit 128", output(128, b"", b"fatal: bad\n")),
            ("no final NUL", output(0, b"H src/a.rs\0H src/b.rs", b"")),
            (
                "a signal",
                Output {
                    code: None,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                },
            ),
        ];
        for (what, answer) in answers {
            let runs = RefCell::new(Vec::new());
            let git = BoundGit::with_runner(
                root,
                git_dir,
                recording(&runs, move |_| Output {
                    code: answer.code,
                    stdout: answer.stdout.clone(),
                    stderr: answer.stderr.clone(),
                }),
            );
            assert!(git.index_entries().is_err(), "{what}: ls-files");
            assert!(
                git.check_ignore(Path::new("target")).is_err(),
                "{what}: check-ignore"
            );
            assert!(git.ignore_values().is_err(), "{what}: config");
            assert!(
                git.index_path().is_err() || what == "no final NUL",
                "{what}"
            );
        }
        // `config` exiting 1 with output, and 0 with none, answer nothing either.
        for answer in [
            output(1, b"core.ignorecase\ntrue\0", b""),
            output(0, b"", b""),
        ] {
            let runs = RefCell::new(Vec::new());
            let git = BoundGit::with_runner(
                root,
                git_dir,
                recording(&runs, move |_| Output {
                    code: answer.code,
                    stdout: answer.stdout.clone(),
                    stderr: answer.stderr.clone(),
                }),
            );
            assert!(git.ignore_values().is_err());
        }

        // An ignore input read by its name: missing only on `ENOENT`.
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            read_input(&tmp.path().join("absent")).unwrap(),
            Reading::Missing
        );
        let closed = tmp.path().join("closed");
        std::fs::create_dir(&closed).unwrap();
        std::fs::write(closed.join(".gitignore"), "target/\n").unwrap();
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
        let denied = read_input(&closed.join(".gitignore"));
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Root reads through any mode; the rule holds for everyone else.
        if rustix::process::geteuid().is_root() {
            return;
        }
        assert!(denied.is_err(), "{denied:?}");
    }

    /// An ignore input's reading changes with its content though its size and mtime stay,
    /// and with its name's `lstat` though the file it names stays.
    #[test]
    fn readings_see_the_content_and_the_name() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join(".gitignore");
        std::fs::write(&input, "target/\n").unwrap();
        let first = read_input(&input).unwrap();
        let mtime = std::fs::metadata(&input).unwrap().modified().unwrap();
        std::fs::write(&input, "targe?/\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&input)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let rewritten = read_input(&input).unwrap();
        assert_ne!(first, rewritten);
        let away = tmp.path().join("away");
        std::fs::rename(&input, &away).unwrap();
        std::os::unix::fs::symlink(&away, &input).unwrap();
        let linked = read_input(&input).unwrap();
        let (
            Reading::Present {
                name: before,
                content: Some(was),
            },
            Reading::Present {
                name: after,
                content: Some(now),
            },
        ) = (&rewritten, &linked)
        else {
            panic!("{rewritten:?} {linked:?}");
        };
        assert_eq!(was, now, "the same file through the link");
        assert_ne!(before, after, "the name is a link now");
    }

    /// The overlap rule on its own inputs: an entry naming the target, one below it, one
    /// ending at a directory of the chain, a sparse directory entry `S dir/` covering the
    /// target, and one beside the target, which only reaches the parent.
    #[test]
    fn the_overlap_rule() {
        use std::os::unix::fs::MetadataExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("pkg/target/x")).unwrap();
        std::fs::write(root.join("pkg/lib.rs"), "").unwrap();
        let id = |rel: &str| {
            let meta = std::fs::symlink_metadata(root.join(rel)).unwrap();
            (meta.dev(), meta.ino())
        };
        let levels = [
            (std::ffi::OsStr::new("pkg"), id("pkg")),
            (std::ffi::OsStr::new("target"), id("pkg/target")),
        ];
        let entry = |tag: u8, path: &str| IndexEntry {
            tag,
            path: PathBuf::from(path),
        };
        let open = || dir::open(root).unwrap().unwrap();
        let beside = [entry(b'H', "pkg/lib.rs"), entry(b'H', "src/main.rs")];
        let found = overlap(open(), &levels, &beside).unwrap();
        assert!(found.found.is_empty(), "{found:?}");
        assert_eq!(
            found.at_parent.into_iter().collect::<Vec<_>>(),
            [PathBuf::from("pkg/lib.rs")]
        );
        for overlapping in [
            entry(b'H', "pkg/target"),
            entry(b'H', "pkg/target/x/y"),
            entry(b'H', "pkg"),
            entry(b'S', "pkg/"),
        ] {
            let path = overlapping.path.clone();
            let found = overlap(open(), &levels, &[overlapping]).unwrap();
            assert_eq!(found.found, std::slice::from_ref(&path), "{path:?}");
        }
        // The target moved: nothing resolves to it, and the parent's entries stay.
        std::fs::rename(root.join("pkg/target"), root.join("moved")).unwrap();
        let found = overlap(open(), &levels, &[entry(b'H', "pkg/target/x")]).unwrap();
        assert!(found.found.is_empty(), "{found:?}");
    }
}
