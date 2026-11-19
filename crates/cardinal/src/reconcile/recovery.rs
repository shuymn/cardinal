//! Step 2 of a pass, after the temp files (`docs/architecture.md` sections 9.7 and 10.3):
//! the recovery of every execution that died, through `act`'s own recovery
//! ([`recover::recover`]), within the pass's reclaim budget, and the listing of
//! `<state>/trash/`, which reports an entry that no execution directory names and ends every
//! `reclaim_stranded` whose trash entry is gone ([`super::raise::stranded_items`]). Nothing
//! here deletes a trash entry that no execution names.

use crate::act::{self, recover};
use crate::config::Config;
use crate::contract::{RecordKind, RecoveredExecution, ScopedError};
use crate::plan;
use crate::state::StateDir;

use super::Budget;
use super::raise::{Trash, TrashEntry};

/// Recovers, in name order, each execution directory through `act`'s recovery under its
/// `act.lock`, while `budget` is left: one with a result has it read whole and its directory
/// synced, at no cost to the budget, one with a claim and no result is finished or undone, and one with neither is left
/// as it is. A recovery under way when the budget runs out finishes, and the next pass takes
/// the rest. Each one finished or undone is returned with its result's status and recorded as
/// an `act` record; a directory that cannot be listed or named, and a recovery that fails,
/// are reported in `errors`.
pub fn recover_executions(
    config: &Config,
    state: &StateDir,
    budget: &mut Budget,
    errors: &mut Vec<ScopedError>,
) -> Vec<RecoveredExecution> {
    let mut recovered = Vec::new();
    let dir = state.executions_dir();
    let listed = match std::fs::read_dir(&dir) {
        Ok(listed) => listed,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return recovered,
        Err(err) => {
            errors.push(scoped(format!("{}: {err}", dir.display())));
            return recovered;
        }
    };
    let mut names = Vec::new();
    for entry in listed {
        match entry.map(|e| e.file_name().into_string()) {
            Ok(Ok(name)) => names.push(name),
            Ok(Err(name)) => errors.push(scoped(format!(
                "{}: not a plan ID",
                dir.join(name).display()
            ))),
            Err(err) => errors.push(scoped(format!("{}: {err}", dir.display()))),
        }
    }
    names.sort();
    let mut names = names.into_iter();
    for plan in names.by_ref() {
        if !budget.left() {
            let waiting = 1 + names.count();
            errors.push(scoped(format!(
                "{plan} and {} more: the pass spent reclaim_budget_s, so their recovery waits \
                 for the next pass",
                waiting - 1
            )));
            break;
        }
        // An execution that had finished, its result read whole and synced, costs nothing:
        // every pass checks every finished one, and they must not use up the budget of the
        // executions that died.
        let recovery = budget.spend_unless(
            || recover::recover(state, &config.dir, &plan, &act::recovery_grant),
            |done| matches!(done, Ok(recover::Recovered::Finished)),
        );
        match recovery {
            Ok(recover::Recovered::Done(result)) => {
                record(config, state, &result);
                recovered.push(RecoveredExecution {
                    plan_id: result.plan_id.clone(),
                    status: result.status,
                });
            }
            Ok(_) => {}
            Err(err) => errors.push(scoped(format!("{plan}: {err}"))),
        }
    }
    recovered
}

/// The entries of `<state>/trash/`, in name order, each with whether an execution directory
/// names it; none when there is no trash directory. A listing that fails is an error; an
/// execution directory that cannot be read leaves its entry's naming unknown, reported in
/// `errors`.
pub fn list_trash(state: &StateDir, errors: &mut Vec<ScopedError>) -> std::io::Result<Trash> {
    let dir = state.trash_dir();
    let listed = match std::fs::read_dir(&dir) {
        Ok(listed) => listed,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Trash::default()),
        Err(err) => return Err(err),
    };
    let mut entries = Vec::new();
    for entry in listed {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let execution = state.executions_dir().join(entry.file_name());
        let named = match std::fs::symlink_metadata(&execution) {
            Ok(_) => Some(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(false),
            Err(err) => {
                errors.push(ScopedError {
                    scope: "trash".into(),
                    message: format!("{}: {err}", execution.display()),
                });
                None
            }
        };
        let (entries_left, bytes_left) = match named {
            Some(false) => recover::left(&path).unzip(),
            _ => (None, None),
        };
        entries.push(TrashEntry {
            name,
            path,
            named,
            entries_left,
            bytes_left,
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Trash { entries })
}

/// The `act` record of a recovery the pass finished, as `act` appends one for its own.
fn record(config: &Config, state: &StateDir, result: &crate::contract::ActResult) {
    let plan = plan::load(state, &result.plan_id).ok();
    let recorder = plan::recorder(config, state);
    let mut record = recorder.record(RecordKind::Act);
    record.plan_id = Some(result.plan_id.clone());
    let data = serde_json::json!({
        "caller": result.caller,
        "kind": plan.as_ref().map(|p| p.kind),
        "target": plan.as_ref().map(|p| p.target.path.clone()),
        "status": result.status,
        "reasons": result.reasons,
        "freed_bytes": result.freed_bytes,
        "skipped_changed": result.skipped_changed,
        "trash": result.trash,
        "recovered": result.recovered,
        "error": result.error,
    });
    record.data = data.as_object().cloned().unwrap_or_default();
    let _ = recorder.append(&record);
}

/// Whether `plan`'s claim is there and cannot be read, which keeps its `reclaim_stranded`
/// open ([`super::raise::stranded_items`]).
pub fn claim_unreadable(state: &StateDir, plan: &str) -> bool {
    matches!(
        recover::read_claim(state, plan),
        recover::ClaimRead::Unreadable(_)
    )
}

fn scoped(message: String) -> ScopedError {
    ScopedError {
        scope: "recover".into(),
        message,
    }
}
