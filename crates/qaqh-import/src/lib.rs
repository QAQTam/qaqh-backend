//! Loss-preserving external transcript capture and normalization.
//!
//! The crate owns source discovery, immutable byte capture, Claude Code
//! normalization, and package verification. It deliberately has no session,
//! runtime, daemon, or provider dependency; callers decide when a verified
//! package becomes a product session.

mod adapters;
mod capture;
mod discover;
mod error;
mod model;
mod package;
mod path_identity;

pub use capture::{capture, capture_with_context, restore_artifact};
pub use discover::{discover, discover_with_context};
pub use error::{ImportError, Result};
pub use model::*;
pub use package::{open_package, prepare, prepare_with_context, verify, verify_with_context};

/// Decode a stored relative source path on the same platform that captured it.
/// The hex encoded path in the package remains authoritative when a package
/// moves between operating systems with different path encodings.
pub fn decode_source_path(path: &EncodedPath) -> Result<std::path::PathBuf> {
    path_identity::decode_relative(path)
}
