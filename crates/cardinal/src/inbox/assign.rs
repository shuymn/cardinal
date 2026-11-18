//! Items and arbiter passes (`docs/architecture.md` section 10.5): which items are due, their
//! assignment to a pass, the pass's outcome in each of them, and what `cardinal inbox
//! --assigned` lists and resolves. The launcher and the session's commands share these.
//!
//! An item records the last pass it was assigned to in its `arbiter` entry, written durably
//! under `inbox/.lock`: `{pass, assigned_ms}` when the launcher assigns it, then the pass's
//! `outcome` and `ended_ms` when the pass ends. A pass that `arbiter/last.json` records without
//! an end is running until `timeout_s` after it started; an item whose pass `last.json` no
//! longer names counts as ended `timeout_s` after its `assigned_ms`. So an item never assigned
//! is due at once, and every assigned one is due `retry_s` after its pass ended, however it
//! ended, and never strands.

use std::cmp::Ordering;

use crate::config::{Arbiter, Config};
use crate::contract::{
    ArbiterPass, Error, ErrorCode, Inbox, InboxItem, InboxKind, InboxSchema, InboxStatus,
    ItemArbiter, PassOutcome, ResolutionOutcome,
};
use crate::state::{self, StateDir};

/// The open items due for a pass at `now_ms`, given `last`, the pass `arbiter/last.json`
/// records: never assigned, or `[arbiter] retry_s` past the end of the pass they were
/// assigned to. Items only the owner can act on are never due, and neither is an item of a
/// pass `last` records without an end: the caller records an abandoned pass before it asks.
/// Read under `inbox/.lock`, oldest first.
pub fn due(
    config: &Config,
    state: &StateDir,
    last: Option<&ArbiterPass>,
    now_ms: u64,
) -> Result<Vec<InboxItem>, Error> {
    let arbiter = &config.host.arbiter;
    let items = {
        let _lock = super::lock(state)?;
        super::read_all(state)?
    };
    let mut due: Vec<InboxItem> = items
        .into_iter()
        .map(|(_, item)| item)
        .filter(|item| is_due(item, last, arbiter, now_ms))
        .collect();
    due.sort_by(oldest_first);
    Ok(due)
}

/// Whether `item` is due at `now_ms` (section 10.5).
fn is_due(item: &InboxItem, last: Option<&ArbiterPass>, arbiter: &Arbiter, now_ms: u64) -> bool {
    if item.status != InboxStatus::Open || item.kind.is_owners() {
        return false;
    }
    let Some(assigned) = &item.arbiter else {
        return true;
    };
    pass_end(assigned, last, arbiter)
        .is_some_and(|ended_ms| now_ms >= ended_ms.saturating_add(secs_ms(arbiter.retry_s)))
}

/// When the pass `assigned` names ended, as far as the item and `last.json` tell; `None`
/// while `last.json` records that pass without an end.
fn pass_end(assigned: &ItemArbiter, last: Option<&ArbiterPass>, arbiter: &Arbiter) -> Option<u64> {
    if let Some(ended_ms) = assigned.ended_ms {
        return Some(ended_ms);
    }
    match last {
        Some(last) if last.id == assigned.pass => last.ended_ms,
        // `last.json` no longer names the pass: a later pass replaced it, or the file, which
        // is not fsynced, was lost.
        _ => Some(
            assigned
                .assigned_ms
                .saturating_add(secs_ms(arbiter.timeout_s)),
        ),
    }
}

/// Assigns the open items among `keys` to `pass`, durably, under `inbox/.lock`, and returns
/// the keys it assigned. An item closed since it was found due is left alone.
pub fn assign(
    state: &StateDir,
    pass: &str,
    keys: &[String],
    assigned_ms: u64,
) -> Result<Vec<String>, Error> {
    let _lock = super::lock(state)?;
    let mut assigned = Vec::new();
    for key in keys {
        let (path, existing) = super::find(state, key)?;
        let Some(mut item) = existing else {
            continue;
        };
        if item.status != InboxStatus::Open {
            continue;
        }
        item.arbiter = Some(ItemArbiter {
            pass: pass.to_owned(),
            assigned_ms,
            outcome: None,
            ended_ms: None,
        });
        super::write(state, &path, &item)?;
        assigned.push(item.key);
    }
    Ok(assigned)
}

/// Writes how `pass` ended into each item assigned to it that has no outcome yet, and into no
/// other, under `inbox/.lock`; returns their keys. A completed pass also ends every
/// `arbiter_failing` occurrence (section 10.1): each gets `ended_ms`, and an open or
/// escalated one is resolved by cardinal.
pub fn record_outcome(
    config: &Config,
    state: &StateDir,
    pass: &str,
    outcome: &PassOutcome,
    ended_ms: u64,
) -> Result<Vec<String>, Error> {
    let mut written = Vec::new();
    let mut cleared = Vec::new();
    {
        let _lock = super::lock(state)?;
        for (path, mut item) in super::read_all(state)? {
            let mut changed = false;
            if let Some(assigned) = &mut item.arbiter
                && assigned.pass == pass
                && assigned.outcome.is_none()
            {
                assigned.outcome = Some(outcome.clone());
                assigned.ended_ms = Some(ended_ms);
                written.push(item.key.clone());
                changed = true;
            }
            if *outcome == PassOutcome::Completed
                && item.kind == InboxKind::ArbiterFailing
                && item.ended_ms.is_none()
            {
                item.ended_ms = Some(ended_ms);
                if matches!(item.status, InboxStatus::Open | InboxStatus::Escalated) {
                    super::set_resolution(
                        &mut item,
                        ResolutionOutcome::Resolved,
                        super::BY_CARDINAL,
                        &format!("arbiter pass {pass} completed"),
                        ended_ms,
                    );
                    cleared.push(item.clone());
                }
                changed = true;
            }
            if changed {
                super::write(state, &path, &item)?;
            }
        }
    }
    for item in &cleared {
        super::record_resolution(config, state, item);
    }
    Ok(written)
}

/// `cardinal inbox --assigned`: the open and escalated items assigned to `pass`, the pass
/// `CARDINAL_ARBITER_PASS` names, while it runs; none once it has ended or been recorded
/// `abandoned`. Refused with `not_assigned` when no pass is named, so a session tells a pass
/// the timer started from the owner's request. Lists without the refresh: the `reconcile`
/// pass that started the arbiter ran it just before.
pub fn list_assigned(
    config: &Config,
    state: &StateDir,
    pass: Option<&str>,
) -> Result<Inbox, Error> {
    let pass = named(pass)?;
    let mut items = Vec::new();
    if running(config, state, pass, state::now_ms())? {
        items = super::read_all(state)?
            .into_iter()
            .map(|(_, item)| item)
            .filter(|item| {
                matches!(item.status, InboxStatus::Open | InboxStatus::Escalated)
                    && assigned_to(item, pass)
            })
            .collect();
        items.sort_by(oldest_first);
    }
    Ok(Inbox {
        schema: InboxSchema,
        items,
    })
}

/// `cardinal inbox resolve KEY … --assigned`: answers an open item assigned to the running
/// pass `pass`, once; any other item, and every item once that pass has ended or been
/// recorded `abandoned`, is refused with `not_assigned`, and an item the pass already
/// answered with `invalid_argument`, each left unchanged. The pass and its deadline are
/// checked under `inbox/.lock`, right before the write, so an answer that waited for the
/// lock past the deadline is refused.
pub fn resolve_assigned(
    config: &Config,
    state: &StateDir,
    pass: Option<&str>,
    key: &str,
    outcome: ResolutionOutcome,
    note: Option<&str>,
) -> Result<InboxItem, Error> {
    let pass = named(pass)?;
    let item = {
        let _lock = super::lock(state)?;
        let now = state::now_ms();
        if !running(config, state, pass, now)? {
            return Err(not_assigned(format!(
                "the arbiter pass {pass} is not running"
            )));
        }
        let (path, existing) = super::find(state, key)?;
        // The launcher writes the pass's outcome into its items under this lock before it
        // records the end, so an item read here with no outcome belongs to a running pass.
        let Some(mut item) = existing.filter(|item| assigned_to(item, pass)) else {
            return Err(not_assigned(format!(
                "the inbox item `{key}` is not assigned to the arbiter pass {pass}"
            )));
        };
        // One answer per assigned item: an escalation is not answered again.
        if item.status != InboxStatus::Open {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                format!("the inbox item `{key}` was already answered"),
            ));
        }
        super::set_resolution(
            &mut item,
            outcome,
            super::BY_CLI,
            note.unwrap_or_default(),
            now,
        );
        super::write(state, &path, &item)?;
        item
    };
    super::record_resolution(config, state, &item);
    Ok(item)
}

/// The pass `CARDINAL_ARBITER_PASS` names, or `not_assigned`.
fn named(pass: Option<&str>) -> Result<&str, Error> {
    pass.ok_or_else(|| {
        not_assigned(
            "CARDINAL_ARBITER_PASS names no arbiter pass: this session was not started for one",
        )
    })
}

/// Whether `pass` is the running pass at `now_ms`: `last.json` names it without an end, and
/// `timeout_s` has not passed since it started. The launcher kills its session then, so a
/// session that outlives it (its launcher died, or a process left its group) answers nothing.
/// A `last.json` that cannot be read is an error, never a running pass.
fn running(config: &Config, state: &StateDir, pass: &str, now_ms: u64) -> Result<bool, Error> {
    Ok(read_last(state)?.is_some_and(|last| {
        last.id == pass
            && last.ended_ms.is_none()
            && last.outcome.is_none()
            && now_ms
                < last
                    .started_ms
                    .saturating_add(secs_ms(config.host.arbiter.timeout_s))
    }))
}

/// Whether `item` is assigned to `pass`, which has not written its outcome into it.
fn assigned_to(item: &InboxItem, pass: &str) -> bool {
    item.arbiter
        .as_ref()
        .is_some_and(|assigned| assigned.pass == pass && assigned.outcome.is_none())
}

/// The pass `arbiter/last.json` records: `None` when no pass ever ran, an error when the file
/// exists but cannot be read or parsed. The launcher, its trigger, and `--assigned` read it
/// through this.
pub fn read_last(state: &StateDir) -> Result<Option<ArbiterPass>, Error> {
    let path = state.arbiter_last();
    let unreadable = |err: &dyn std::fmt::Display| {
        Error::new(
            ErrorCode::StateUnavailable,
            format!("{}: {err}", path.display()),
        )
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(unreadable(&err)),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|err| unreadable(&err))
}

fn not_assigned(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::NotAssigned, message)
}

fn oldest_first(a: &InboxItem, b: &InboxItem) -> Ordering {
    (a.created_ms, &a.key).cmp(&(b.created_ms, &b.key))
}

fn secs_ms(seconds: u64) -> u64 {
    seconds.saturating_mul(1000)
}

#[cfg(test)]
mod tests {
    use super::{is_due, pass_end};
    use crate::config::Arbiter;
    use crate::contract::{
        ArbiterPass, ArbiterPassSchema, InboxItem, InboxItemSchema, InboxKind, InboxStatus,
        InboxSubject, ItemArbiter, PassOutcome,
    };

    const ARBITER: Arbiter = Arbiter {
        command: Vec::new(),
        timeout_s: 10,
        min_interval_s: 0,
        retry_s: 100,
    };

    fn item(kind: InboxKind, arbiter: Option<ItemArbiter>) -> InboxItem {
        InboxItem {
            schema: InboxItemSchema,
            key: "k".into(),
            kind,
            status: InboxStatus::Open,
            subject: InboxSubject::default(),
            evidence: serde_json::Map::new(),
            created_ms: 0,
            updated_ms: 0,
            seen: 1,
            ended_ms: None,
            arbiter,
            resolution: None,
        }
    }

    fn assigned(pass: &str, ended_ms: Option<u64>) -> ItemArbiter {
        ItemArbiter {
            pass: pass.into(),
            assigned_ms: 1_000,
            outcome: ended_ms.map(|_| PassOutcome::Completed),
            ended_ms,
        }
    }

    fn last(id: &str, ended_ms: Option<u64>) -> ArbiterPass {
        ArbiterPass {
            schema: ArbiterPassSchema,
            id: id.into(),
            started_ms: 1_000,
            ended_ms,
            outcome: ended_ms.map(|_| PassOutcome::Completed),
            items: vec!["k".into()],
            log: String::new(),
            failures_in_a_row: 0,
        }
    }

    /// An item never assigned is due at once; owners' kinds and closed items never are.
    #[test]
    fn never_assigned_items_are_due() {
        assert!(is_due(
            &item(InboxKind::Reclaimable, None),
            None,
            &ARBITER,
            0
        ));
        for kind in [
            InboxKind::DiskLow,
            InboxKind::DiskCritical,
            InboxKind::ArbiterFailing,
            InboxKind::ReclaimStranded,
        ] {
            assert!(
                !is_due(&item(kind, None), None, &ARBITER, u64::MAX),
                "{kind:?}"
            );
        }
        for status in [
            InboxStatus::Escalated,
            InboxStatus::Resolved,
            InboxStatus::Dismissed,
        ] {
            let closed = InboxItem {
                status,
                ..item(InboxKind::Reclaimable, None)
            };
            assert!(!is_due(&closed, None, &ARBITER, u64::MAX), "{status:?}");
        }
    }

    /// Each way a pass can be known to have ended, and the one running pass.
    #[test]
    fn the_end_of_an_items_pass() {
        // The outcome in the item.
        assert_eq!(
            pass_end(&assigned("p", Some(5_000)), None, &ARBITER),
            Some(5_000)
        );
        // `last.json` records the end the item missed.
        assert_eq!(
            pass_end(
                &assigned("p", None),
                Some(&last("p", Some(7_000))),
                &ARBITER
            ),
            Some(7_000)
        );
        // `last.json` records the pass without an end: running, or not yet recorded
        // abandoned.
        assert_eq!(
            pass_end(&assigned("p", None), Some(&last("p", None)), &ARBITER),
            None
        );
        // `last.json` names another pass, or none: ended `timeout_s` after the assignment.
        for other in [None, Some(last("q", None))] {
            assert_eq!(
                pass_end(&assigned("p", None), other.as_ref(), &ARBITER),
                Some(11_000)
            );
        }
    }

    /// Due exactly `retry_s` after the pass ended, not before.
    #[test]
    fn due_again_after_retry() {
        let left_open = item(InboxKind::Reclaimable, Some(assigned("p", Some(5_000))));
        assert!(!is_due(&left_open, None, &ARBITER, 104_999));
        assert!(is_due(&left_open, None, &ARBITER, 105_000));
        let running = item(InboxKind::Reclaimable, Some(assigned("p", None)));
        assert!(!is_due(
            &running,
            Some(&last("p", None)),
            &ARBITER,
            u64::MAX
        ));
    }
}
