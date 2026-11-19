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
//! - [`bin_exe`]: the path of a binary under test.

use std::path::PathBuf;

pub mod fixture;
#[cfg(unix)]
mod marker;
pub mod process;
#[cfg(unix)]
pub mod pty;
mod scope;

#[cfg(target_os = "linux")]
pub use scope::TMPFS_VAR;
pub use scope::{Child, Command, RUN_MARKER, Scope};
#[cfg(target_os = "macos")]
pub use scope::{IMAGE_VAR, Image};

/// The path of the package's binary `name` that cargo-nextest gives an integration test at
/// run time, `NEXTEST_BIN_EXE_<name>`: in a run from an archive (`scripts/test`) the binary
/// extracted with the tests. Cargo's compile-time `CARGO_BIN_EXE_<name>` names the build tree
/// instead, where a build during the run replaces the binary under test.
///
/// # Panics
///
/// When the variable is unset: the tests run under cargo-nextest only.
#[must_use]
pub fn bin_exe(name: &str) -> PathBuf {
    let var = format!("NEXTEST_BIN_EXE_{name}");
    std::env::var_os(&var).map_or_else(
        || panic!("{var} is unset: run the tests with cargo-nextest (scripts/test)"),
        PathBuf::from,
    )
}
