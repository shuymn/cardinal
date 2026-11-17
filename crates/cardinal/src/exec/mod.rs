//! Execution: the local invocation, placement, and the executor contract
//! (`docs/architecture.md` section 5.7).
//!
//! Owned by the runner and admission work unit. The local executor is not written yet; it
//! reports every call as not started.

use std::ffi::OsString;

use crate::contract::{GroupIdentity, ProcessIdentity};

/// A local invocation: the stage's argv as the calling shell expanded it. The cwd and the
/// environment are inherited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub argv: Vec<OsString>,
}

/// Where an invocation runs. Phase 0 always runs locally.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Placement {
    #[default]
    Local,
}

/// The executor failed before anything started; the caller may run the stage itself.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not started: {0}")]
pub struct NotStarted(pub String);

/// How a started invocation ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecOutcome {
    Exited(i32),
    Signaled(i32),
    InfraFailure(String),
}

/// Starts invocations. The start boundary is the same for every executor: `starting` is
/// written before [`Executor::start`], and an `Ok` from it means the stage may have run.
pub trait Executor {
    /// Sets up the process group; runs before the reservation is written.
    fn prepare(&self) -> Result<GroupIdentity, NotStarted>;
    /// Starts the invocation. `Err` guarantees that nothing started.
    fn start(&self, inv: &Invocation) -> Result<Box<dyn Running>, NotStarted>;
}

/// A started invocation.
pub trait Running {
    /// The child, written into the reservation as `running`.
    fn child(&self) -> ProcessIdentity;
    /// Blocks until the child exits; forwards cancellation.
    fn wait(self: Box<Self>) -> ExecOutcome;
}

/// The local executor.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalExecutor;

impl Executor for LocalExecutor {
    fn prepare(&self) -> Result<GroupIdentity, NotStarted> {
        Err(NotStarted("the local executor is not implemented".into()))
    }

    fn start(&self, inv: &Invocation) -> Result<Box<dyn Running>, NotStarted> {
        let _ = inv;
        Err(NotStarted("the local executor is not implemented".into()))
    }
}
