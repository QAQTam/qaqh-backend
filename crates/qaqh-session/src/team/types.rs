//! Canonical TeamFact types for the task board aggregate.

use serde::{Deserialize, Serialize};

use crate::session_fact_v2::{
    AgentNamespace, AgentPath, ContentRef, EventId, FactSchema, LogId, SessionId,
};

use super::error::{TeamError, TeamResult};

pub const TEAM_FACT_SCHEMA: &str = "qaqh.team-fact/v1";
pub const TEAM_FACT_VERSION: u16 = 1;

const MAX_TASK_TITLE_BYTES: usize = 512;
const MAX_TASK_REASON_BYTES: usize = 1024;
const MAX_ACCEPTANCE_ITEMS: usize = 64;
const MAX_ACCEPTANCE_ITEM_BYTES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TeamId(pub SessionId);

impl TeamId {
    pub fn new(session_id: SessionId) -> Self {
        Self(session_id)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Display for TeamId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(pub String);

impl TaskId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn generate() -> Self {
        Self(format!("task_{}", crate::canonical::generate_ulid()))
    }

    pub fn validate(&self) -> TeamResult<()> {
        let Some(ulid) = self.0.strip_prefix("task_") else {
            return Err(TeamError::Validation(format!(
                "task id {:?} must start with task_",
                self.0
            )));
        };
        if !is_ulid(ulid) {
            return Err(TeamError::Validation(format!(
                "task id {:?} has an invalid ULID suffix",
                self.0
            )));
        }
        Ok(())
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamActor {
    pub agent_path: AgentPath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
}

impl TeamActor {
    pub fn new(agent_path: AgentPath, session_id: Option<SessionId>) -> Self {
        Self {
            agent_path,
            session_id,
        }
    }

    pub fn validate(&self) -> TeamResult<()> {
        if self.agent_path.namespace() != AgentNamespace::Root {
            return Err(TeamError::Validation(
                "team actor must belong to the /root namespace".to_string(),
            ));
        }
        if let Some(session_id) = &self.session_id
            && session_id.as_str().is_empty()
        {
            return Err(TeamError::Validation(
                "team actor session_id must not be empty".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Open,
    Claimed,
    Completed,
    Closed,
    Cancelled,
}

impl TaskState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Closed | Self::Cancelled)
    }

    pub fn satisfies_dependency(self) -> bool {
        matches!(self, Self::Completed | Self::Closed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskArtifact {
    pub artifact_ref: ContentRef,
    pub media_type: String,
    pub added_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamCreated {
    pub root_session_id: SessionId,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskCreated {
    pub task_id: TaskId,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_ref: Option<ContentRef>,
    pub created_by: TeamActor,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskClaimed {
    pub task_id: TaskId,
    pub owner: TeamActor,
    pub claim_epoch: u64,
    pub claimed_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskReleased {
    pub task_id: TaskId,
    pub owner: TeamActor,
    pub claim_epoch: u64,
    pub reason: String,
    pub released_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDependencyAdded {
    pub task_id: TaskId,
    pub depends_on: TaskId,
    pub added_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskArtifactAttached {
    pub task_id: TaskId,
    pub artifact_ref: ContentRef,
    pub media_type: String,
    pub added_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskAcceptanceSet {
    pub task_id: TaskId,
    pub acceptance: Vec<String>,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskCompleted {
    pub task_id: TaskId,
    pub owner: TeamActor,
    pub claim_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_ref: Option<ContentRef>,
    pub completed_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskClosed {
    pub task_id: TaskId,
    pub closed_by: TeamActor,
    pub closed_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskCancelled {
    pub task_id: TaskId,
    pub cancelled_by: TeamActor,
    pub reason: String,
    pub cancelled_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum TeamPayload {
    TeamCreated(TeamCreated),
    TaskCreated(TaskCreated),
    TaskClaimed(TaskClaimed),
    TaskReleased(TaskReleased),
    TaskDependencyAdded(TaskDependencyAdded),
    TaskArtifactAttached(TaskArtifactAttached),
    TaskAcceptanceSet(TaskAcceptanceSet),
    TaskCompleted(TaskCompleted),
    TaskClosed(TaskClosed),
    TaskCancelled(TaskCancelled),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamFact {
    pub schema: FactSchema,
    pub team_id: TeamId,
    pub log_id: LogId,
    pub fact_seq: u64,
    pub event_id: EventId,
    pub ts_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<EventId>,
    pub actor: TeamActor,
    pub payload: TeamPayload,
}

impl TeamFact {
    pub fn validate(&self) -> TeamResult<()> {
        if self.schema.name != TEAM_FACT_SCHEMA {
            return Err(TeamError::Validation(format!(
                "team fact schema {:?} is not {TEAM_FACT_SCHEMA}",
                self.schema.name
            )));
        }
        if self.team_id.as_str().is_empty() {
            return Err(TeamError::Validation("team_id must not be empty".into()));
        }
        if self.log_id.as_str().is_empty() {
            return Err(TeamError::Validation("log_id must not be empty".into()));
        }
        if self.fact_seq == 0 {
            return Err(TeamError::Validation(
                "fact_seq must be assigned before validation".into(),
            ));
        }
        if !is_ulid(self.event_id.as_str()) {
            return Err(TeamError::Validation(format!(
                "event_id {:?} is not a ULID",
                self.event_id.as_str()
            )));
        }
        if self.ts_ms <= 0 {
            return Err(TeamError::Validation("ts_ms must be positive".into()));
        }
        self.actor.validate()?;
        self.payload.validate()
    }
}

impl TeamPayload {
    pub fn validate(&self) -> TeamResult<()> {
        match self {
            Self::TeamCreated(payload) => {
                if payload.root_session_id.as_str().is_empty() {
                    return Err(TeamError::Validation(
                        "root_session_id must not be empty".into(),
                    ));
                }
                validate_positive_ms("created_at_ms", payload.created_at_ms)
            }
            Self::TaskCreated(payload) => {
                payload.task_id.validate()?;
                validate_non_empty("title", &payload.title)?;
                validate_byte_limit("title", &payload.title, MAX_TASK_TITLE_BYTES)?;
                if let Some(description_ref) = &payload.description_ref {
                    validate_content_ref("description_ref", description_ref)?;
                }
                payload.created_by.validate()?;
                validate_positive_ms("created_at_ms", payload.created_at_ms)
            }
            Self::TaskClaimed(payload) => {
                payload.task_id.validate()?;
                payload.owner.validate()?;
                if payload.claim_epoch == 0 {
                    return Err(TeamError::Validation("claim_epoch must be positive".into()));
                }
                validate_positive_ms("claimed_at_ms", payload.claimed_at_ms)
            }
            Self::TaskReleased(payload) => {
                payload.task_id.validate()?;
                payload.owner.validate()?;
                if payload.claim_epoch == 0 {
                    return Err(TeamError::Validation("claim_epoch must be positive".into()));
                }
                validate_non_empty("reason", &payload.reason)?;
                validate_byte_limit("reason", &payload.reason, MAX_TASK_REASON_BYTES)?;
                validate_positive_ms("released_at_ms", payload.released_at_ms)
            }
            Self::TaskDependencyAdded(payload) => {
                payload.task_id.validate()?;
                payload.depends_on.validate()?;
                if payload.task_id == payload.depends_on {
                    return Err(TeamError::Validation(
                        "a task cannot depend on itself".into(),
                    ));
                }
                validate_positive_ms("added_at_ms", payload.added_at_ms)
            }
            Self::TaskArtifactAttached(payload) => {
                payload.task_id.validate()?;
                validate_content_ref("artifact_ref", &payload.artifact_ref)?;
                validate_non_empty("media_type", &payload.media_type)?;
                validate_positive_ms("added_at_ms", payload.added_at_ms)
            }
            Self::TaskAcceptanceSet(payload) => {
                payload.task_id.validate()?;
                if payload.acceptance.len() > MAX_ACCEPTANCE_ITEMS {
                    return Err(TeamError::Validation(format!(
                        "acceptance has {} items; maximum is {MAX_ACCEPTANCE_ITEMS}",
                        payload.acceptance.len()
                    )));
                }
                for item in &payload.acceptance {
                    validate_non_empty("acceptance item", item)?;
                    validate_byte_limit("acceptance item", item, MAX_ACCEPTANCE_ITEM_BYTES)?;
                }
                validate_positive_ms("updated_at_ms", payload.updated_at_ms)
            }
            Self::TaskCompleted(payload) => {
                payload.task_id.validate()?;
                payload.owner.validate()?;
                if payload.claim_epoch == 0 {
                    return Err(TeamError::Validation("claim_epoch must be positive".into()));
                }
                if let Some(result_ref) = &payload.result_ref {
                    validate_content_ref("result_ref", result_ref)?;
                }
                validate_positive_ms("completed_at_ms", payload.completed_at_ms)
            }
            Self::TaskClosed(payload) => {
                payload.task_id.validate()?;
                payload.closed_by.validate()?;
                validate_positive_ms("closed_at_ms", payload.closed_at_ms)
            }
            Self::TaskCancelled(payload) => {
                payload.task_id.validate()?;
                payload.cancelled_by.validate()?;
                validate_non_empty("reason", &payload.reason)?;
                validate_byte_limit("reason", &payload.reason, MAX_TASK_REASON_BYTES)?;
                validate_positive_ms("cancelled_at_ms", payload.cancelled_at_ms)
            }
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::TeamCreated(_) => "team_created",
            Self::TaskCreated(_) => "task_created",
            Self::TaskClaimed(_) => "task_claimed",
            Self::TaskReleased(_) => "task_released",
            Self::TaskDependencyAdded(_) => "task_dependency_added",
            Self::TaskArtifactAttached(_) => "task_artifact_attached",
            Self::TaskAcceptanceSet(_) => "task_acceptance_set",
            Self::TaskCompleted(_) => "task_completed",
            Self::TaskClosed(_) => "task_closed",
            Self::TaskCancelled(_) => "task_cancelled",
        }
    }
}

pub fn new_team_schema() -> FactSchema {
    FactSchema {
        name: TEAM_FACT_SCHEMA.to_string(),
        version: TEAM_FACT_VERSION,
        payload_version: TEAM_FACT_VERSION,
    }
}

fn is_ulid(value: &str) -> bool {
    value.len() == 26
        && value
            .bytes()
            .all(|byte| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&byte))
}

fn validate_positive_ms(field: &'static str, value: i64) -> TeamResult<()> {
    if value <= 0 {
        return Err(TeamError::Validation(format!("{field} must be positive")));
    }
    Ok(())
}

fn validate_non_empty(field: &'static str, value: &str) -> TeamResult<()> {
    if value.trim().is_empty() {
        return Err(TeamError::Validation(format!("{field} must not be empty")));
    }
    Ok(())
}

fn validate_byte_limit(field: &'static str, value: &str, max: usize) -> TeamResult<()> {
    if value.len() > max {
        return Err(TeamError::Validation(format!(
            "{field} is {} bytes; maximum is {max}",
            value.len()
        )));
    }
    Ok(())
}

fn validate_content_ref(field: &'static str, value: &ContentRef) -> TeamResult<()> {
    let hash = value.hash().as_str();
    let Some(hex) = hash.strip_prefix("sha256:") else {
        return Err(TeamError::Validation(format!(
            "{field} must start with sha256:"
        )));
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(TeamError::Validation(format!(
            "{field} must contain 64 hex digits after sha256:"
        )));
    }
    Ok(())
}
