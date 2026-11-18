//! The read-only web UI (`docs/architecture.md` section 10.6): `cardinal.snapshot.v1` and the
//! self-contained page `ui/index.html` that embeds it, written by every `reconcile` pass and
//! by `cardinal ui` from what the state directory holds, with no git, gh, or network call.

use std::path::PathBuf;

use crate::config::Config;
use crate::contract::{Error, Snapshot};
use crate::state::StateDir;

/// The snapshot of this host's state: disk states, the last full observation,
/// reservations, open and escalated inbox items, recent records, and the last arbiter pass.
pub fn build(_config: &Config, _state: &StateDir) -> Result<Snapshot, Error> {
    Err(Error::not_implemented("the UI snapshot"))
}

/// Writes `ui/snapshot.json` and `ui/index.html` and returns the page's path.
pub fn write_page(_config: &Config, _state: &StateDir) -> Result<PathBuf, Error> {
    Err(Error::not_implemented("the UI page"))
}
