//! Items and arbiter passes (`docs/architecture.md` section 10.5): which items are due, their
//! assignment to a pass, the pass's outcome in each of them, and what `cardinal inbox
//! --assigned` lists and resolves. The launcher and the session's commands share these.

use crate::config::Config;
use crate::contract::{Error, Inbox, InboxItem, PassOutcome, ResolutionOutcome};
use crate::state::StateDir;

/// The open items due for a pass at `now_ms`: never assigned, or `[arbiter] retry_s` past
/// the end of the pass they were assigned to. Items only the owner can act on are never
/// due. Read under `inbox/.lock`.
pub fn due(_config: &Config, _state: &StateDir, _now_ms: u64) -> Result<Vec<InboxItem>, Error> {
    Err(Error::not_implemented("the arbiter's due items"))
}

/// Assigns the items `keys` to `pass`, durably, under `inbox/.lock`.
pub fn assign(
    _state: &StateDir,
    _pass: &str,
    _keys: &[String],
    _assigned_ms: u64,
) -> Result<(), Error> {
    Err(Error::not_implemented("assigning items to an arbiter pass"))
}

/// Writes how `pass` ended into each item assigned to it, and into no other.
pub fn record_outcome(
    _state: &StateDir,
    _pass: &str,
    _outcome: &PassOutcome,
    _ended_ms: u64,
) -> Result<(), Error> {
    Err(Error::not_implemented(
        "recording an arbiter pass's outcome",
    ))
}

/// `cardinal inbox --assigned`: the open and escalated items assigned to `pass`, the pass
/// `CARDINAL_ARBITER_PASS` names (`None` when it is unset).
pub fn list_assigned(
    _config: &Config,
    _state: &StateDir,
    _pass: Option<&str>,
) -> Result<Inbox, Error> {
    Err(Error::not_implemented("inbox --assigned"))
}

/// `cardinal inbox resolve KEY … --assigned`: resolves an item assigned to the running pass
/// `pass`; any other item, and every item once that pass has ended or been recorded
/// `abandoned`, is refused with `not_assigned` and left unchanged.
pub fn resolve_assigned(
    _config: &Config,
    _state: &StateDir,
    _pass: Option<&str>,
    _key: &str,
    _outcome: ResolutionOutcome,
    _note: Option<&str>,
) -> Result<InboxItem, Error> {
    Err(Error::not_implemented("inbox resolve --assigned"))
}
