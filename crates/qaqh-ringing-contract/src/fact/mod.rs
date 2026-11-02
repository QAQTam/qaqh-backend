//! Canonical session-fact/v2 词表(自 qaqh-session 迁入:纯类型与校验,无存储依赖)。

pub mod agent;
pub mod content;
pub mod slots;
pub mod types;
pub mod validation;

pub use agent::{
    AGENT_PATH_MORPHEUS, AGENT_PATH_ROOT, AgentMetadata, AgentNamespace, AgentPath, AgentPathError,
    AgentPathSegmentError,
};
pub use content::ContentValue;
pub use slots::*;
pub use types::*;
pub use validation::{ValidationError, is_ulid_text};
