//! The inbox: store, dedup, and refresh (`docs/architecture.md` sections 4.8 and 10.1).
//!
//! Owned by the plan, act, inbox, and explain work unit. The bodies are not written yet;
//! every declared function returns `not_implemented`.

use crate::config::Config;
use crate::contract::{
    Error, Inbox, InboxItem, InboxKind, InboxStatus, InboxSubject, ResolutionOutcome,
};
use crate::state::StateDir;

/// An item to raise. Raising a key that exists updates `seen` and the evidence of an open or
/// escalated item and leaves resolved and dismissed items alone.
#[derive(Clone, Debug, PartialEq)]
pub struct NewItem {
    pub key: String,
    pub kind: InboxKind,
    pub subject: InboxSubject,
    pub evidence: serde_json::Value,
}

/// Raises or updates an item; the one function refresh and (from phase 1) units raise through.
pub fn raise(state: &StateDir, item: &NewItem) -> Result<InboxItem, Error> {
    let _ = (state, item);
    Err(Error::not_implemented("inbox::raise"))
}

/// `--status`: one status, or `all`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusFilter {
    Only(InboxStatus),
    All,
}

/// `cardinal inbox [--status …] [--no-refresh]`. `status` is `None` without `--status`.
pub fn list(
    config: &Config,
    state: &StateDir,
    status: Option<StatusFilter>,
    refresh: bool,
) -> Result<Inbox, Error> {
    let _ = (config, state, status, refresh);
    Err(Error::not_implemented("cardinal inbox"))
}

/// `cardinal inbox resolve KEY --outcome … [--note TEXT]`.
pub fn resolve(
    state: &StateDir,
    key: &str,
    outcome: ResolutionOutcome,
    note: Option<&str>,
) -> Result<InboxItem, Error> {
    let _ = (state, key, outcome, note);
    Err(Error::not_implemented("cardinal inbox resolve"))
}
