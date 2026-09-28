//! qaqh-session — unified session manager singleton.
//!
//! Follows the same pattern as qaqh-workspace::ToolManager.

pub mod actor;
pub mod canonical;
pub mod grouping;
pub mod manager;
pub mod projection;
pub mod session_fact_v2;
pub mod store;
pub mod team;
pub use grouping::{WorkspaceMeta, WorkspaceStore};
pub use manager::SessionManager;

/// Free-function epoch helper.
pub fn now_epoch() -> u64 {
    SessionManager::now_epoch()
}
