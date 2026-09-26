//! Team task board canonical aggregate.
//!
//! The team aggregate is a separate append-only log keyed by root session id;
//! it is intentionally not part of any session's canonical log. See
//! `docs/current/spec/2026-09-26-team-task-board.md`.

mod error;
mod projection;
mod store;
mod types;

pub use error::{TeamError, TeamResult};
pub use projection::{TaskBoardDelta, TaskBoardProjection, TaskBoardSnapshot, TaskView};
pub use store::{
    TEAM_COMMIT_FILE, TEAM_EVENTS_FILE, TEAM_IDENTITY_FILE, TEAM_IDENTITY_SCHEMA, TEAM_LOCK_FILE,
    TeamAppendOutcome, TeamStore,
};
pub use types::{
    TEAM_FACT_SCHEMA, TEAM_FACT_VERSION, TaskAcceptanceSet, TaskArtifact, TaskArtifactAttached,
    TaskCancelled, TaskClaimed, TaskClosed, TaskCompleted, TaskCreated, TaskDependencyAdded,
    TaskId, TaskReleased, TaskState, TeamActor, TeamCreated, TeamFact, TeamId, TeamPayload,
    new_team_schema,
};
