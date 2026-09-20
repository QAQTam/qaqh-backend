//! Canonical session-fact/v2 contracts.
//!
//! This module is a pure type and validation slice. It does not open session
//! files, acquire writer locks, or mutate runtime state.

mod projection;
mod types;
mod validation;

pub use projection::{
    END_OF_FACT, MAX_RELIABLE_PROJECTION_INDEX, ProjectionIndex, projection_slots,
};
pub use types::*;
pub use validation::ValidationError;
