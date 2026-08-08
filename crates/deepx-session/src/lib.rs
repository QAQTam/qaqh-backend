//! deepx-session — unified session manager singleton.
//!
//! Follows the same pattern as deepx-workspace::ToolManager.

pub mod manager;
mod migrate;
pub mod session_meta;
pub mod store;
pub use manager::{CompactContext, SessionManager};
pub use session_meta::SessionMeta;
