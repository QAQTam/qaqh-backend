//! Quarantine zone for features that are NOT part of the agent loop core
//! (compaction engine, session misc actions, dashboard projection).
//!
//! They live inside `qaqh-runtime` for now because they still consume
//! `AgentState` / `RingContext` directly; extraction into standalone crates is
//! blocked on splitting the god-state. Kept behind this module so the loop
//! core (`loop_*`, `turn_lap`, `state`) stays clearly delineated.

pub(crate) mod dashboard;
pub mod engine_compact;
pub(crate) mod engine_misc;
