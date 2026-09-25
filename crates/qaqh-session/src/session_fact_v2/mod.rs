//! Canonical session-fact/v2 contracts.
//!
//! This module is a pure type and validation slice. It does not open session
//! files, acquire writer locks, or mutate runtime state.

mod agent;
mod projection;
mod projection_event;
mod types;
mod validation;

pub use agent::{
    AGENT_PATH_MORPHEUS, AGENT_PATH_ROOT, AgentNamespace, AgentPath, AgentPathError,
    AgentPathSegmentError,
};
pub use projection::{
    END_OF_FACT, MAX_RELIABLE_PROJECTION_INDEX, ProjectionIndex, projection_slots,
};
pub use projection_event::*;
pub use types::*;
pub use validation::ValidationError;
