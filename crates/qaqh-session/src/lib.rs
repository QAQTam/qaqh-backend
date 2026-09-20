//! qaqh-session — unified session manager singleton.
//!
//! Follows the same pattern as qaqh-workspace::ToolManager.

pub mod canonical;
pub mod grouping;
pub mod manager;
mod migrate;
pub mod projection;
pub mod session_fact_v2;
pub mod session_meta;
pub mod store;
pub use grouping::{WorkspaceMeta, WorkspaceStore};
pub use manager::{CompactContext, SessionManager};
pub use session_meta::SessionMeta;

/// Free-function seed generator (PR-1-5 / B6): loop crates consume the
/// helpers without naming the [`SessionManager`] type. Delegates to the
/// manager's associated functions — no behaviour change.
pub fn generate_seed() -> String {
    SessionManager::generate_seed()
}

/// Free-function unique-seed generator (BUG-2026-09-13-24): retries until
/// `is_taken` reports the candidate as free. Loop crates use this instead of
/// [`generate_seed`] whenever the seed will be materialized on disk.
pub fn generate_unique_seed(is_taken: impl FnMut(&str) -> bool) -> String {
    SessionManager::generate_unique_seed(is_taken)
}

/// Free-function epoch helper, same rationale as [`generate_seed`].
pub fn now_epoch() -> u64 {
    SessionManager::now_epoch()
}
