//! Shared test helpers. Each integration test crate declares `mod support;` and uses the
//! parts it needs. Processes start and git runs only through `cardinal_test_support`
//! (`docs/architecture.md` section 13.3); these modules build on it.
//!
//! - [`helper`]: helper processes started by re-running the test binary.
//! - [`cardinal`]: the `cardinal` binary with its own state and config directories.
//! - [`testpoints`]: driving the pause points of `cardinal::testpoint`.

#![allow(
    dead_code,
    reason = "each test crate uses a different subset of the helpers"
)]

pub mod cardinal;
pub mod helper;
pub mod testpoints;
