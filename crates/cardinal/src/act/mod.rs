//! Plan execution: claim, snapshot, checks, and descriptor-relative deletion
//! (`docs/architecture.md` sections 9.5 and 9.6).
//!
//! Owned by the plan, act, inbox, and explain work unit. The body is not written yet.

use crate::config::Config;
use crate::contract::{ActResult, Caller, Error};
use crate::state::StateDir;

/// `cardinal act` flags.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActOptions {
    pub dry_run: bool,
    pub owner_approved: bool,
    pub caller: Caller,
}

/// `cardinal act PLAN`: `plan_id` is a unique prefix of at least 4 characters.
pub fn execute(
    config: &Config,
    state: &StateDir,
    plan_id: &str,
    options: &ActOptions,
) -> Result<ActResult, Error> {
    let _ = (config, state, plan_id, options);
    Err(Error::not_implemented("cardinal act"))
}
