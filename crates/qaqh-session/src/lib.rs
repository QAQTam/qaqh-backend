//! qaqh-session — unified session manager singleton.
//!
//! Follows the same pattern as qaqh-workspace::ToolManager.

#![cfg_attr(not(test), forbid(unsafe_code))]
pub mod actor;
pub mod canonical;
pub mod db;
mod wire;
mod commit_state;
pub mod context_projection;
pub mod grouping;
pub mod manager;
pub mod meta;
pub mod owner;
#[cfg(test)]
mod owner_tests;
pub mod projection;
pub mod session_commit;
pub mod session_fact_v2;
pub mod store;
pub mod team;
pub use wire::{ClientV2ControlState, ClientV2ToolState};
pub use grouping::{WorkspaceEntry, WorkspaceMeta, WorkspaceStore};
pub use manager::SessionManager;

/// Free-function epoch helper.
pub fn now_epoch() -> u64 {
    SessionManager::now_epoch()
}
