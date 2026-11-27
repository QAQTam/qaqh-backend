//! 会话目标（Goal）、待办（Todo）、团队协作（Team）与技能目录（Skills）视图 DTO。

use qaqh_core_vocab::{ContentRef, GoalStatus, SessionId};
use serde::{Deserialize, Serialize};

/// 会话持久目标读模型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalView {
    /// 目标 id。
    pub goal_id: String,
    /// 所属会话 id。
    pub session_id: SessionId,
    /// 目标正文。
    pub objective: String,
    /// 六态状态机。
    pub status: GoalStatus,
    /// 可选 token 预算。
    pub token_budget: Option<u64>,
    /// 累计已用 token。
    pub tokens_used: u64,
    /// 累计运行毫秒。
    pub time_used_ms: u64,
    /// 创建时刻（毫秒）。
    pub created_at_ms: i64,
    /// 更新时刻（毫秒）。
    pub updated_at_ms: i64,
}

/// 团队成员 / 同事状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TeamAgentStatus {
    /// 等待初始化。
    PendingInit,
    /// 执行回合中。
    Running,
    /// 空闲。
    Idle,
    /// 等待用户交互。
    WaitingUser,
    /// 被中断。
    Interrupted,
    /// 已完成。
    Completed,
    /// 出错终结。
    Errored,
    /// 已关闭。
    Shutdown,
    /// 未找到。
    NotFound,
}

/// 团队成员常驻态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TeamAgentResidency {
    /// 已加载在内存。
    Loaded,
    /// 已卸载。
    Unloaded,
}

/// 子代理摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentSummary {
    /// 子会话 id。
    pub session_id: SessionId,
    /// 展示昵称。
    pub nickname: Option<String>,
    /// 活动状态。
    pub status: TeamAgentStatus,
    /// 内存常驻态。
    pub residency: TeamAgentResidency,
}

/// Todo 单条视图。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItemView {
    /// 条目 id。
    pub id: String,
    /// 标题。
    pub title: String,
    /// 描述。
    pub description: String,
    /// 状态词（`pending` / `in_progress` / `completed` / `cancelled`）。
    pub status: String,
    /// 完成证据（如有）。
    pub evidence: Option<String>,
}

/// Todo 计数视图。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoCountsView {
    /// 待办数。
    pub pending: u64,
    /// 进行中数。
    pub in_progress: u64,
    /// 已完成数。
    pub completed: u64,
    /// 已取消数。
    pub cancelled: u64,
    /// 总数。
    pub total: u64,
}

/// 会话待办列表快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoListSnapshot {
    /// 全部条目。
    pub items: Vec<TodoItemView>,
    /// 当前活跃条目 id。
    pub current_id: Option<String>,
    /// 计数摘要。
    pub counts: TodoCountsView,
}

/// 团队成员快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamAgentSnapshot {
    /// 成员会话 id。
    pub agent_id: SessionId,
    /// 代理层级路径。
    pub agent_path: String,
    /// 昵称。
    pub nickname: Option<String>,
    /// 活动状态。
    pub status: TeamAgentStatus,
    /// 常驻态。
    pub residency: TeamAgentResidency,
    /// 父代理路径。
    pub parent_agent_path: Option<String>,
}

/// 团队收件箱摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamInboxSummary {
    /// 消息 id。
    pub message_id: String,
    /// 发送方。
    pub author: String,
    /// 接收方。
    pub recipient: String,
    /// 关联任务 id。
    pub task_id: Option<String>,
    /// 投递类别。
    pub delivery: String,
    /// 创建时刻（毫秒）。
    pub created_at_ms: i64,
}

/// 任务板状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    /// 待认领。
    Open,
    /// 已认领执行中。
    Claimed,
    /// 已完成。
    Completed,
    /// 已取消。
    Cancelled,
}

/// 任务视图。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskView {
    /// 任务 id。
    pub task_id: String,
    /// 标题。
    pub title: String,
    /// 描述引用。
    pub description_ref: Option<ContentRef>,
    /// 当前状态。
    pub state: TaskState,
    /// 当前认领人。
    pub owner: Option<String>,
    /// 认领代际。
    pub claim_epoch: u64,
    /// 依赖任务列表。
    pub depends_on: Vec<String>,
    /// 产物列表。
    pub artifacts: Vec<String>,
    /// 验收标准。
    pub acceptance: Vec<String>,
    /// 写入声明范围。
    pub write_scopes: Vec<String>,
    /// 是否就绪可执行。
    pub ready: bool,
    /// 与进行中任务的写入冲突告警。
    pub write_scope_warnings: Vec<String>,
    /// 结果引用。
    pub result_ref: Option<ContentRef>,
    /// 创建人。
    pub created_by: String,
    /// 创建时刻（毫秒）。
    pub created_at_ms: i64,
    /// 更新时刻（毫秒）。
    pub updated_at_ms: i64,
}

/// 团队任务板快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBoardSnapshot {
    /// 团队 id。
    pub team_id: Option<String>,
    /// 修订序号。
    pub revision: u64,
    /// 最后事实序号。
    pub last_fact_seq: u64,
    /// 任务条目。
    pub tasks: Vec<TaskView>,
}

/// 团队成员与收件箱快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamSnapshot {
    /// 根会话 id。
    pub root_session_id: Option<SessionId>,
    /// 成员列表。
    pub agents: Vec<TeamAgentSnapshot>,
    /// 未读消息。
    pub unread_messages: Vec<TeamInboxSummary>,
    /// 修订序号。
    pub revision: u64,
    /// 最后事实序号。
    pub last_fact_seq: u64,
}

/// 团队全量视图（替代旧 REST `/team`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamOverview {
    /// 查询的会话 id。
    pub session_id: SessionId,
    /// 成员与邮箱快照。
    pub team: TeamSnapshot,
    /// 任务板快照。
    pub tasks: TaskBoardSnapshot,
}

/// 技能目录条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillCatalogEntry {
    /// 技能名称。
    pub name: String,
    /// 描述。
    pub description: String,
    /// 作用域（`project` / `user`）。
    pub scope: String,
    /// 来源标签。
    pub origin: String,
    /// 磁盘路径。
    pub path: String,
}

/// 技能扫描诊断条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillCatalogDiagnostic {
    /// 严重度。
    pub severity: String,
    /// 目标路径。
    pub path: String,
    /// 诊断说明。
    pub message: String,
}

/// 技能目录扫描结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillsCatalogView {
    /// 已发现技能。
    pub skills: Vec<SkillCatalogEntry>,
    /// 发现期诊断。
    pub diagnostics: Vec<SkillCatalogDiagnostic>,
}
