//! team/任务板线上快照(自 qaqh-session 投影文件迁入)。ADR-0033:留言板词表
//! 整体退役,通信只走 canonical mailbox。
#[cfg(feature = "ts")]
use ts_rs::TS;

use serde::{Deserialize, Serialize};

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
    /// 无活动回合(ADR-0033:投影说真话,Idle 不再洗成 Running)。
    Idle,
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
    /// 同事名(spawn 时的 agent_name;ADR-0033:名字必须诚实上册)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    pub status: TeamAgentStatus,
    pub residency: TeamAgentResidency,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_path: Option<AgentPath>,
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


#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBoardSnapshot {
    pub team_id: Option<TeamId>,
    pub revision: u64,
    pub last_fact_seq: u64,
    pub tasks: Vec<TaskView>,
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
    /// advisory 文件触及范围(ADR-0033 切片D additive,dsh 同律)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_scopes: Vec<String>,
    /// 派生值:open 且全部依赖达标(读取侧投影,不入库)。
    #[serde(default)]
    pub ready: bool,
    /// 与进行中任务 write_scopes 前缀相交警告(advisory,绝不阻止)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_scope_warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_ref: Option<crate::fact::ContentRef>,
    pub created_by: TeamActor,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
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
            write_scopes: Vec::new(),
            ready: false,
            write_scope_warnings: Vec::new(),
            result_ref: None,
            created_by: payload.created_by.clone(),
            created_at_ms: payload.created_at_ms,
            updated_at_ms: ts_ms,
        }
    }
}
