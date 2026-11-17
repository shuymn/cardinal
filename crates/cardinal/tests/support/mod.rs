//! Shared test helpers. Each integration test crate declares `mod support;` and uses the
//! parts it needs.
//!
//! - [`fixture`]: git fixtures, the only way tests run git (`docs/architecture.md` section 13.3).
//! - [`helper`]: helper processes started by re-running the test binary, and guards that kill
//!   what a test started.
//! - [`cardinal`]: the `cardinal` binary with its own state and config directories.
//! - [`testpoints`]: driving the pause points of `cardinal::testpoint`.

#![allow(
    dead_code,
    reason = "each test crate uses a different subset of the helpers"
)]

pub mod cardinal;
pub mod fixture;
pub mod helper;
pub mod testpoints;
