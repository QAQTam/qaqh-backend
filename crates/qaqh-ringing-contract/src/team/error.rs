//! Team task board canonical aggregate errors.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum TeamError {
    #[error("team io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("team json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("team identity mismatch: expected {expected}, got {actual}")]
    IdentityMismatch { expected: String, actual: String },
    #[error("team commit mismatch: {0}")]
    CommitMismatch(String),
    #[error("team validation error: {0}")]
    Validation(String),
    #[error("team task not found: {0}")]
    TaskNotFound(String),
    #[error("team task already exists: {0}")]
    TaskAlreadyExists(String),
    #[error("team log is empty; TeamCreated must be the first fact")]
    MissingTeamCreated,
    #[error("team log id mismatch: expected {expected}, got {actual}")]
    LogIdMismatch { expected: String, actual: String },
}

pub type TeamResult<T> = Result<T, TeamError>;
