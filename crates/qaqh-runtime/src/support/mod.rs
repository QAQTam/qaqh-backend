//! Pieces of the retired Ringing-V1 loop that production still needs.
//!
//! Everything else under `src/agent/` was dead: `AgentState` and its engines had
//! zero production references after the thin-loop cutover. These six items are
//! the survivors, deliberately small and dependency-light so they cannot grow
//! back into an executor.

pub mod cancel;
pub mod config;
pub mod ids;
pub mod liveness;
pub mod prompt_env;
pub mod registrars;
