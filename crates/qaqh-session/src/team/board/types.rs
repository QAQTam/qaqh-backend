//! Canonical BoardFact types for the team message board aggregate.

use serde::{Deserialize, Serialize};

use crate::session_fact_v2::{EventId, FactSchema, LogId, SessionId};

use crate::team::types::{TaskId, TeamActor};
use crate::team::{TeamError, TeamResult};

pub const BOARD_FACT_SCHEMA: &str = "qaqh.board-fact/v1";
pub const BOARD_FACT_VERSION: u16 = 1;

pub const MAX_BOARD_CHANNELS: usize = 64;
pub const MAX_BOARD_THREADS: usize = 512;
pub const MAX_BOARD_POSTS: usize = 4096;
pub const MAX_BOARD_SUBSCRIPTIONS: usize = 2048;

const MAX_CHANNEL_NAME_BYTES: usize = 64;
const MAX_CHANNEL_TOPIC_BYTES: usize = 1024;
const MAX_THREAD_TITLE_BYTES: usize = 512;
const MAX_POST_BODY_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BoardId(pub SessionId);

impl BoardId {
    pub fn new(session_id: SessionId) -> Self {
        Self(session_id)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Display for BoardId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

macro_rules! board_id_type {
    ($name:ident, $prefix:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn generate() -> Self {
                Self(format!("{}{}", $prefix, crate::canonical::generate_ulid()))
            }

            pub fn validate(&self) -> TeamResult<()> {
                let Some(ulid) = self.0.strip_prefix($prefix) else {
                    return Err(TeamError::Validation(format!(
                        "{} id {:?} must start with {}",
                        stringify!($name),
                        self.0,
                        $prefix
                    )));
                };
                if !is_ulid(ulid) {
                    return Err(TeamError::Validation(format!(
                        "{} id {:?} has an invalid ULID suffix",
                        stringify!($name),
                        self.0
                    )));
                }
                Ok(())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

board_id_type!(ChannelId, "chan_");
board_id_type!(ThreadId, "thread_");
board_id_type!(PostId, "post_");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BoardSubscriptionTarget {
    Channel { channel_id: ChannelId },
    Thread { thread_id: ThreadId },
}

impl BoardSubscriptionTarget {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Channel { .. } => "channel",
            Self::Thread { .. } => "thread",
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Channel { channel_id } => channel_id.as_str(),
            Self::Thread { thread_id } => thread_id.as_str(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardCreated {
    pub root_session_id: SessionId,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelCreated {
    pub channel_id: ChannelId,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub created_by: TeamActor,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadCreated {
    pub thread_id: ThreadId,
    pub channel_id: ChannelId,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    pub created_by: TeamActor,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostCreated {
    pub post_id: PostId,
    pub thread_id: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    pub author: TeamActor,
    pub body: String,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<PostId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionChanged {
    pub target: BoardSubscriptionTarget,
    pub subscriber: TeamActor,
    pub subscribed: bool,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum BoardPayload {
    BoardCreated(BoardCreated),
    ChannelCreated(ChannelCreated),
    ThreadCreated(ThreadCreated),
    PostCreated(PostCreated),
    SubscriptionChanged(SubscriptionChanged),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardFact {
    pub schema: FactSchema,
    pub board_id: BoardId,
    pub log_id: LogId,
    pub fact_seq: u64,
    pub event_id: EventId,
    pub ts_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<EventId>,
    pub actor: TeamActor,
    pub payload: BoardPayload,
}

impl BoardFact {
    pub fn validate(&self) -> TeamResult<()> {
        if self.schema.name != BOARD_FACT_SCHEMA {
            return Err(TeamError::Validation(format!(
                "board fact schema {:?} is not {BOARD_FACT_SCHEMA}",
                self.schema.name
            )));
        }
        if self.board_id.as_str().is_empty() {
            return Err(TeamError::Validation("board_id must not be empty".into()));
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

impl BoardPayload {
    pub fn validate(&self) -> TeamResult<()> {
        match self {
            Self::BoardCreated(payload) => {
                if payload.root_session_id.as_str().is_empty() {
                    return Err(TeamError::Validation(
                        "root_session_id must not be empty".into(),
                    ));
                }
                validate_positive_ms("created_at_ms", payload.created_at_ms)
            }
            Self::ChannelCreated(payload) => {
                payload.channel_id.validate()?;
                validate_channel_name(&payload.name)?;
                if let Some(topic) = &payload.topic {
                    validate_byte_limit("channel topic", topic, MAX_CHANNEL_TOPIC_BYTES)?;
                }
                payload.created_by.validate()?;
                validate_positive_ms("created_at_ms", payload.created_at_ms)
            }
            Self::ThreadCreated(payload) => {
                payload.thread_id.validate()?;
                payload.channel_id.validate()?;
                validate_non_empty("thread title", &payload.title)?;
                validate_byte_limit("thread title", &payload.title, MAX_THREAD_TITLE_BYTES)?;
                if let Some(task_id) = &payload.task_id {
                    task_id.validate()?;
                }
                payload.created_by.validate()?;
                validate_positive_ms("created_at_ms", payload.created_at_ms)
            }
            Self::PostCreated(payload) => {
                payload.post_id.validate()?;
                payload.thread_id.validate()?;
                if let Some(task_id) = &payload.task_id {
                    task_id.validate()?;
                }
                payload.author.validate()?;
                validate_non_empty("post body", &payload.body)?;
                validate_byte_limit("post body", &payload.body, MAX_POST_BODY_BYTES)?;
                validate_positive_ms("created_at_ms", payload.created_at_ms)?;
                if let Some(reply_to) = &payload.reply_to {
                    reply_to.validate()?;
                }
                Ok(())
            }
            Self::SubscriptionChanged(payload) => {
                match &payload.target {
                    BoardSubscriptionTarget::Channel { channel_id } => channel_id.validate()?,
                    BoardSubscriptionTarget::Thread { thread_id } => thread_id.validate()?,
                }
                payload.subscriber.validate()?;
                validate_positive_ms("updated_at_ms", payload.updated_at_ms)
            }
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::BoardCreated(_) => "board_created",
            Self::ChannelCreated(_) => "channel_created",
            Self::ThreadCreated(_) => "thread_created",
            Self::PostCreated(_) => "post_created",
            Self::SubscriptionChanged(_) => "subscription_changed",
        }
    }
}

pub fn new_board_schema() -> FactSchema {
    FactSchema {
        name: BOARD_FACT_SCHEMA.to_string(),
        version: BOARD_FACT_VERSION,
        payload_version: BOARD_FACT_VERSION,
    }
}

fn is_ulid(value: &str) -> bool {
    value.len() == 26
        && value
            .bytes()
            .all(|byte| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&byte))
}

fn validate_channel_name(value: &str) -> TeamResult<()> {
    let valid = !value.is_empty()
        && value.len() <= MAX_CHANNEL_NAME_BYTES
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        });
    if !valid {
        return Err(TeamError::Validation(format!(
            "channel name {value:?} must match [a-z0-9][a-z0-9_-]{{0,63}}"
        )));
    }
    Ok(())
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
