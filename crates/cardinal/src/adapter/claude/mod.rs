//! The Claude Code adapter: hook input and output, stage rewriting, and the session-end
//! snapshot (`docs/architecture.md` section 6).
//!
//! Owned by the hook work unit. The bodies are not written yet; every declared function
//! returns `not_implemented`. The CLI exits 0 for every hook, whatever these return, and
//! writes the output of `pre_tool_use` to stdout only when it returns `Ok`.

use std::io::{Read, Write};

use crate::contract::Error;

/// `cardinal hook claude pre-tool-use`: reads the hook input from `input` and writes the hook
/// output (or nothing) to `output`.
pub fn pre_tool_use(input: &mut dyn Read, output: &mut dyn Write) -> Result<(), Error> {
    let _ = (input, output);
    Err(Error::not_implemented("cardinal hook claude pre-tool-use"))
}

/// `cardinal hook claude subagent-stop`: records the end; never stops processes.
pub fn subagent_stop(input: &mut dyn Read) -> Result<(), Error> {
    let _ = input;
    Err(Error::not_implemented("cardinal hook claude subagent-stop"))
}

/// `cardinal hook claude session-end`: records the end and writes the session snapshot;
/// never stops processes.
pub fn session_end(input: &mut dyn Read) -> Result<(), Error> {
    let _ = input;
    Err(Error::not_implemented("cardinal hook claude session-end"))
}
