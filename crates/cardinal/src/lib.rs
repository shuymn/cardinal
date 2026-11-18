//! cardinal arbitrates local compute resources for parallel coding agents.
//!
//! The library target exists so integration tests can call modules directly; it is not a
//! published API. The dependency direction between modules is fixed in `docs/architecture.md`
//! (section 2.2) and checked by `tests/layering.rs`.

pub mod act;
pub mod adapter;
pub mod admission;
pub mod classify;
pub mod cli;
pub mod config;
pub mod contract;
pub mod exec;
pub mod explain;
pub mod inbox;
pub mod observe;
pub mod os;
pub mod plan;
pub mod reconcile;
pub mod record;
pub mod run;
pub mod sensor;
pub mod snapshot;
pub mod state;
pub mod testpoint;
