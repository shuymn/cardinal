//! Starting the arbiter (`docs/architecture.md` section 10.5): the trigger a pass runs at
//! step 7, and `cardinal arbiter run`, the launcher that records a pass, assigns its due
//! items, and runs `[arbiter] command` under `arbiter/.lock` with a deadline.

use crate::config::Config;
use crate::contract::{ArbiterPass, ArbiterStart, Error};
use crate::state::StateDir;

/// Step 7: starts `cardinal arbiter run` detached when an item is due and `min_interval_s`
/// has passed since the last pass started.
pub fn trigger(_config: &Config, _state: &StateDir) -> Result<ArbiterStart, Error> {
    Err(Error::not_implemented("reconcile: the arbiter trigger"))
}

/// `cardinal arbiter run`: one bounded arbiter pass, or the running one when
/// `arbiter/.lock` is held.
pub fn run(_config: &Config, _state: &StateDir) -> Result<ArbiterPass, Error> {
    Err(Error::not_implemented("arbiter run"))
}
