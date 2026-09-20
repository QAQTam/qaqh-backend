//! Canonical session-fact/v2 contracts.
//!
//! This module is a pure type and validation slice. It does not open session
//! files, acquire writer locks, or mutate runtime state.

mod types;
mod validation;

pub use types::*;
pub use validation::ValidationError;
