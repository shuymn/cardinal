//! The runner: start boundary, fail-open, wait loop, child supervision, and release
//! (`docs/architecture.md` section 5).
//!
//! Owned by the runner and admission work unit. The body is not written yet.

use std::ffi::OsString;
use std::process::ExitCode;

use crate::contract::Error;

/// Runs one stage: `cardinal run [--spec-b64 SPEC] -- COMMAND [ARG]...`. Returns the stage's
/// exit status (section 5.6).
pub fn run(spec_b64: Option<&str>, command: &[OsString]) -> Result<ExitCode, Error> {
    let _ = (spec_b64, command);
    Err(Error::not_implemented("cardinal run"))
}
