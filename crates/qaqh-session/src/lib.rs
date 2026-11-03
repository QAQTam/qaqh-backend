//! qaqh-session — unified session manager singleton.
//!
//! Follows the same pattern as qaqh-workspace::ToolManager.

#![cfg_attr(not(test), forbid(unsafe_code))]
pub mod db;
pub mod grouping;
pub mod manager;
pub mod owner;
#[cfg(test)]
mod owner_tests;
pub use manager::SessionManager;

/// Free-function epoch helper.
pub fn now_epoch() -> u64 {
    SessionManager::now_epoch()
}
