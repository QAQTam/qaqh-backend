//! deepx-session — unified session manager singleton.
//!
//! Follows the same pattern as deepx-workspace::ToolManager.

pub mod manager;
mod migrate;
pub mod mirror;
pub mod session_meta;
pub mod store;
pub use manager::{CompactContext, SessionManager};
pub use mirror::{MirrorManifest, MirrorSnapshot};
pub use session_meta::SessionMeta;

/// Whether this build contains the optional Turso session mirror.
///
/// The public capability check keeps configuration and UI contracts stable
/// while the backend is temporarily compiled out of production builds.
pub const fn turso_backend_available() -> bool {
    cfg!(feature = "turso-backend")
}
