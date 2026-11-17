//! Explanation documents (`docs/architecture.md` section 10.2).
//!
//! Owned by the plan, act, inbox, and explain work unit. The body is not written yet.

use std::path::PathBuf;

use crate::config::Config;
use crate::contract::{Error, Explain};
use crate::state::StateDir;

/// What to explain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subject {
    Job(String),
    Plan(String),
    Inbox(String),
    /// A classification and rewrite trace without running anything.
    Command {
        cwd: Option<PathBuf>,
        command: String,
    },
}

/// `cardinal explain`.
pub fn explain(config: &Config, state: &StateDir, subject: &Subject) -> Result<Explain, Error> {
    let _ = (config, state, subject);
    Err(Error::not_implemented("cardinal explain"))
}
