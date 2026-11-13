//! team/board 线上快照与 delta 词表(自 qaqh-session 投影文件迁入)。
#[cfg(feature = "ts")]
use ts_rs::TS;

use serde::{Deserialize, Serialize};

use super::board_types::{BoardId, BoardSubscriptionTarget, ChannelId, PostId, ThreadId};
use super::task_types::{TaskArtifact, TaskCreated, TaskId, TaskState, TeamActor, TeamId};
use crate::fact::{
    AgentPath, ContentRef, InterAgentActor, InterAgentDelivery, MessageId, SessionId,
};

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamAgentStatus {
    PendingInit,
    Running,
    WaitingUser,
    Interrupted,
    Completed,
    Errored,
    Shutdown,
    NotFound,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamAgentResidency {
    Loaded,
    Unloaded,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamAgentSnapshot {
    pub agent_id: SessionId,
    pub agent_path: AgentPath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub status: TeamAgentStatus,
    pub residency: TeamAgentResidency,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_path: Option<AgentPath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_task_id: Option<String>,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamInboxSummary {
    pub message_id: MessageId,
    pub author: InterAgentActor,
    pub recipient: InterAgentActor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub delivery: InterAgentDelivery,
    pub created_at_ms: i64,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamTaskArtifact {
    pub content_ref: ContentRef,
    pub media_type: String,
    pub added_at_ms: i64,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamTaskSnapshot {
    pub task_id: String,
    pub title: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<AgentPath>,
    pub claim_epoch: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<TeamTaskArtifact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_ref: Option<ContentRef>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamBoardChannel {
    pub channel_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub created_by: AgentPath,
    pub created_at_ms: i64,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamBoardThread {
    pub thread_id: String,
    pub channel_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub created_by: AgentPath,
    pub created_at_ms: i64,
    pub post_count: u64,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamBoardPost {
    pub post_id: String,
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub author: AgentPath,
    pub body: String,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TeamBoardSubscriptionTarget {
    Channel { channel_id: String },
    Thread { thread_id: String },
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamBoardSubscription {
    pub target: TeamBoardSubscriptionTarget,
    pub subscriber: AgentPath,
    pub subscribed: bool,
    pub updated_at_ms: i64,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamBoardSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board_id: Option<SessionId>,
    pub revision: u64,
    pub last_fact_seq: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<TeamBoardChannel>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub threads: Vec<TeamBoardThread>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub posts: Vec<TeamBoardPost>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscriptions: Vec<TeamBoardSubscription>,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum TeamDelta {
    AgentJoined {
        revision: u64,
        agent: Box<TeamAgentSnapshot>,
    },
    AgentStatusChanged {
        revision: u64,
        agent_id: SessionId,
        status: TeamAgentStatus,
    },
    AgentResidencyChanged {
        revision: u64,
        agent_id: SessionId,
        residency: TeamAgentResidency,
    },
    AgentMessageQueued {
        revision: u64,
        message: Box<TeamInboxSummary>,
    },
    AgentMessageDelivered {
        revision: u64,
        message_id: MessageId,
    },
    AgentInterrupted {
        revision: u64,
        agent_id: SessionId,
    },
    AgentCompleted {
        revision: u64,
        agent_id: SessionId,
        status: TeamAgentStatus,
    },
    TaskChanged {
        revision: u64,
        task: Box<TeamTaskSnapshot>,
    },
    BoardChanged {
        revision: u64,
        board: Box<TeamBoardSnapshot>,
    },
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBoardSnapshot {
    pub team_id: Option<TeamId>,
    pub revision: u64,
    pub last_fact_seq: u64,
    pub tasks: Vec<TaskView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum TaskBoardDelta {
    TeamCreated {
        revision: u64,
        team_id: TeamId,
    },
    TaskCreated {
        revision: u64,
        task: Box<TaskView>,
    },
    TaskClaimed {
        revision: u64,
        task_id: TaskId,
        owner: TeamActor,
        claim_epoch: u64,
        claimed_at_ms: i64,
    },
    TaskReleased {
        revision: u64,
        task_id: TaskId,
        previous_owner: TeamActor,
        claim_epoch: u64,
        released_at_ms: i64,
    },
    TaskDependencyAdded {
        revision: u64,
        task_id: TaskId,
        depends_on: TaskId,
        added_at_ms: i64,
    },
    TaskArtifactAttached {
        revision: u64,
        task_id: TaskId,
        artifact: TaskArtifact,
    },
    TaskAcceptanceSet {
        revision: u64,
        task_id: TaskId,
        acceptance: Vec<String>,
        updated_at_ms: i64,
    },
    TaskCompleted {
        revision: u64,
        task_id: TaskId,
        result_ref: Option<crate::fact::ContentRef>,
        completed_at_ms: i64,
    },
    TaskClosed {
        revision: u64,
        task_id: TaskId,
        closed_at_ms: i64,
    },
    TaskCancelled {
        revision: u64,
        task_id: TaskId,
        reason: String,
        cancelled_at_ms: i64,
    },
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskView {
    pub task_id: TaskId,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_ref: Option<crate::fact::ContentRef>,
    pub state: TaskState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<TeamActor>,
    pub claim_epoch: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<TaskId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<TaskArtifact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_ref: Option<crate::fact::ContentRef>,
    pub created_by: TeamActor,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardSnapshot {
    pub board_id: Option<BoardId>,
    pub revision: u64,
    pub last_fact_seq: u64,
    pub channels: Vec<BoardChannelView>,
    pub threads: Vec<BoardThreadView>,
    pub posts: Vec<BoardPostView>,
    pub subscriptions: Vec<BoardSubscriptionView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardChannelView {
    pub channel_id: ChannelId,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub created_by: TeamActor,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardThreadView {
    pub thread_id: ThreadId,
    pub channel_id: ChannelId,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    pub created_by: TeamActor,
    pub created_at_ms: i64,
    pub post_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardPostView {
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
pub struct BoardSubscriptionView {
    pub target: BoardSubscriptionTarget,
    pub subscriber: TeamActor,
    pub subscribed: bool,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum BoardDelta {
    BoardCreated {
        revision: u64,
        board_id: BoardId,
    },
    ChannelCreated {
        revision: u64,
        channel: Box<BoardChannelView>,
    },
    ThreadCreated {
        revision: u64,
        thread: Box<BoardThreadView>,
    },
    PostCreated {
        revision: u64,
        post: Box<BoardPostView>,
    },
    SubscriptionChanged {
        revision: u64,
        subscription: Box<BoardSubscriptionView>,
    },
}

/// 团队 roster 与 inbox 快照。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TeamSnapshot {
    pub root_session_id: Option<SessionId>,
    pub agents: Vec<TeamAgentSnapshot>,
    pub unread_messages: Vec<TeamInboxSummary>,
    pub revision: u64,
    pub last_fact_seq: u64,
}

impl TaskView {
    pub fn from_created(payload: &TaskCreated, ts_ms: i64) -> Self {
        Self {
            task_id: payload.task_id.clone(),
            title: payload.title.clone(),
            description_ref: payload.description_ref.clone(),
            state: TaskState::Open,
            owner: None,
            claim_epoch: 0,
            depends_on: Vec::new(),
            artifacts: Vec::new(),
            acceptance: Vec::new(),
            result_ref: None,
            created_by: payload.created_by.clone(),
            created_at_ms: payload.created_at_ms,
            updated_at_ms: ts_ms,
        }
    }
}
