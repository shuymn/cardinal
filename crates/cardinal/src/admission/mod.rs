//! Admission: budget math, the reservation store, liveness and the recovery pass, and the
//! worktree in-use query (`docs/architecture.md` sections 4.4–4.6).
//!
//! Owned by the runner and admission work unit. The bodies are not written yet; every
//! declared function returns `not_implemented`.

use crate::config::HostConfig;
use crate::contract::{Error, InUse, ReservationView};
use crate::state::StateDir;

/// What one recovery pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Jobs whose stale reservations were deleted.
    pub released: Vec<String>,
    /// Jobs whose verified groups were sent SIGTERM or SIGKILL.
    pub signalled: Vec<String>,
    /// The reservations left after the pass, with their derived state.
    pub reservations: Vec<ReservationView>,
}

/// Every reservation with its derived state, group members, and verification. Read-only: it
/// never deletes or signals anything (`inventory` uses it).
pub fn list_reservations(state: &StateDir) -> Result<Vec<ReservationView>, Error> {
    let _ = state;
    Err(Error::not_implemented("admission::list_reservations"))
}

/// Whether a live or recovery-needed reservation names the worktree with key `worktree_key`,
/// and which jobs do. `act` calls it under the admission lock while it holds the worktree's
/// in-use lock exclusively.
pub fn in_use(state: &StateDir, worktree_key: &str) -> Result<InUse, Error> {
    let _ = (state, worktree_key);
    Err(Error::not_implemented("admission::in_use"))
}

/// Runs the recovery pass of section 4.4 under the admission lock (taken with
/// `admission.lock_timeout_ms`). `cardinal inbox` runs it before raising items.
pub fn recovery_pass(state: &StateDir, host: &HostConfig) -> Result<RecoveryReport, Error> {
    let _ = (state, host);
    Err(Error::not_implemented("admission::recovery_pass"))
}
