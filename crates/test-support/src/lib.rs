//! Test support for cardinal (`docs/architecture.md` section 13.3): the only way tests start
//! processes and run git. A dev-dependency of `cardinal`, so integration tests and
//! `#[cfg(test)]` unit tests in `src/` use the same API.
//!
//! - [`Scope`]: a root directory owned by the test and the [`Command`] that starts every
//!   child process inside it, with a constructed environment and cleanup that kills the
//!   child's process group (Unix) or job (Windows).
//! - [`fixture`]: git repositories inside a scope, through typed operations only.
//! - [`process`]: guards for processes a test did not start itself, and polling helpers.
//! - [`pty`]: pseudo-terminals for a child's stdin (Unix).

pub mod fixture;
pub mod process;
#[cfg(unix)]
pub mod pty;
mod scope;

pub use scope::{Child, Command, RUN_MARKER, Scope};
