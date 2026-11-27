//! Session 域强类型 RPC、首屏快照与历史时间线分页 DTO。

use qaqh_core_vocab::{
    BlockId, ContentRef, GoalStatus, InputId, ModelSelection, SessionId, SettingOverride,
    ToolCallId, ToolMode, TurnId, TurnPhase, TurnTerminal, UsageInfo, WorkspaceId,
};
use serde::{Deserialize, Serialize};

use crate::stream::{
    CompactionSummary, ExecSessionSnapshot, ImageRefView, InteractionView, ToolCallView,
};
use crate::team::{
    GoalView, SkillsCatalogView, SubagentSummary, TeamOverview, TodoListSnapshot,
};

/// 会话工具设置覆盖项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolsSettingPatch {
    /// 切到精简核心模式（`Core`）。
    Core,
    /// 切到标准全量模式（`Standard`）。
    Standard,
    /// 切到自定义白名单模式（`Custom`）。
    Custom {
        /// 白名单工具名称列表。
        tools: Vec<String>,
    },
}

/// 会话级设置增量补丁（由 `expected_revision` 乐观锁保护）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSettingsPatch {
    /// 模型选择覆写（`None` = 不修改）。
    pub model_selection: Option<SettingOverride<ModelSelection>>,
    /// 工具模式与白名单补丁（`None` = 不修改）。
    pub tools: Option<ToolsSettingPatch>,
    /// AgentTeam 协作总开关（`None` = 不修改）。
    pub team_mode: Option<bool>,
}

/// 会话域请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionRequest {
    /// 新建会话（含模型绑定、自定义白名单与交接来源）。
    Create {
        /// 可选标题。
        title: Option<String>,
        /// 工作目录（`None` = 默认工作区）。
        cwd: Option<String>,
        /// 初始工具模式（`None` = `Standard`）。
        tool_mode: Option<ToolMode>,
        /// 初始模型绑定。
        model_selection: Option<ModelSelection>,
        /// 仅 `Custom` 模式时的白名单工具名列表。
        custom_tools: Option<Vec<String>>,
        /// 若由交接分叉创建，来源会话 id。
        handoff_from: Option<SessionId>,
    },
    /// 首屏快照（`tail_blocks`/`tail_turns` 控制尾部窗口；向上翻页走 [`SessionRequest::TranscriptPage`]）。
    GetSessionSnapshot {
        /// 会话 id。
        session_id: SessionId,
        /// 尾部块窗口上限。
        tail_blocks: u32,
        /// 尾部回合窗口上限。
        tail_turns: u32,
    },
    /// 向上翻页。
    ///
    /// `timeline_epoch` 由快照给出；失配一律 `StaleCursor`（客户端必须重取快照，
    /// 不允许"尽量拼一下"——拼出来的时间线是错位的假历史）。
    TranscriptPage {
        /// 会话 id。
        session_id: SessionId,
        /// 早于该 `ord` 的块（`0` = 最新）。
        before_ord: u64,
        /// 本页块数上限。
        limit: u32,
        /// 时间线代际。
        timeline_epoch: u64,
    },
    /// 列出会话。
    List {
        /// 是否包含已归档会话。
        include_archived: bool,
    },
    /// 重命名。
    Rename {
        /// 会话 id。
        session_id: SessionId,
        /// 新标题。
        title: String,
    },
    /// 归档会话。
    Archive {
        /// 会话 id。
        session_id: SessionId,
    },
    /// 取消归档会话。
    Unarchive {
        /// 会话 id。
        session_id: SessionId,
    },
    /// 修改会话级设置（模型/工具/团队模式），带乐观锁版本号。
    PatchSettings {
        /// 会话 id。
        session_id: SessionId,
        /// 补丁负载。
        patch: SessionSettingsPatch,
        /// 期望当前设置修订号。
        expected_revision: u64,
    },
    /// 从指定回合起截断并回滚时间线（触发 `timeline_epoch` 递增）。
    Truncate {
        /// 会话 id。
        session_id: SessionId,
        /// 截断起点回合 id。
        turn_id: TurnId,
    },
    /// 设置或更新持久目标。
    SetGoal {
        /// 会话 id。
        session_id: SessionId,
        /// 目标文本。
        objective: String,
        /// 目标状态（`None` = 保持/默认 `Active`）。
        status: Option<GoalStatus>,
        /// token 预算上限（`None` = 不限）。
        token_budget: Option<u64>,
    },
    /// 清除会话持久目标。
    ClearGoal {
        /// 会话 id。
        session_id: SessionId,
    },
    /// 读取当前会话 `todo` 快照。
    GetTodos {
        /// 会话 id。
        session_id: SessionId,
    },
    /// 读取当前会话 `team` 协作与任务板快照。
    GetTeam {
        /// 会话 id。
        session_id: SessionId,
    },
    /// 按可选工作区目录扫描可用技能目录。
    GetSkillsCatalog {
        /// 目标工作区目录（`None` = daemon 当前目录/用户级）。
        cwd: Option<String>,
    },
    /// 删除。
    Delete {
        /// 会话 id。
        session_id: SessionId,
    },
    /// 切换工具模式。
    SetToolMode {
        /// 会话 id。
        session_id: SessionId,
        /// 目标模式（三档，压测档解析不到这里）。
        mode: ToolMode,
    },
    /// 附着会话事件流（打开 `SessionEvents` 逻辑流的控制面入口）。
    Attach {
        /// 会话 id。
        session_id: SessionId,
        /// 已收到的最后序号。
        since_seq: u64,
    },
    /// 让出/取得驱动权（多端联动）。
    SwitchDriver {
        /// 会话 id。
        session_id: SessionId,
        /// 目标客户端实例 id（`None` = 让出给自己之外的下一个申请者）。
        target_client_instance_id: Option<String>,
    },
}

/// 时间线输入块发起者类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockActor {
    /// 人类用户。
    User,
    /// 外部 API。
    Api,
    /// 系统内部注入。
    System,
    /// 代理自身/同事代理。
    Agent,
    /// 子代理。
    Subagent,
}

/// 用户/系统输入块投递意图。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputPurpose {
    /// 触发新回合。
    TriggerTurn,
    /// 仅入队。
    QueueOnly,
    /// 运行中转向。
    Steer,
    /// 运行中插话。
    Interject,
}

/// 跨代理通信消息卡片元数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterAgentView {
    /// 消息 id。
    pub message_id: String,
    /// 根会话 id。
    pub root_session_id: SessionId,
    /// 发送者签名。
    pub author: String,
    /// 主接收者签名。
    pub recipient: String,
    /// 其余抄送接收者。
    pub other_recipients: Vec<String>,
    /// 关联任务板任务 id。
    pub task_id: Option<String>,
    /// 回复目标消息 id。
    pub reply_to: Option<String>,
    /// 创建时刻（毫秒）。
    pub created_at_ms: u64,
}

/// 子代理终态种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubagentTerminalKind {
    /// 正常完成。
    Completed,
    /// 失败。
    Failed,
    /// 被取消。
    Cancelled,
    /// 超时。
    TimedOut,
}

/// 随子代理终态注入携带的结构化通知。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentTerminalView {
    /// 子会话 id。
    pub child_session_id: SessionId,
    /// 父级派生工具调用 id。
    pub parent_call_id: ToolCallId,
    /// 终态种类。
    pub terminal: SubagentTerminalKind,
}

/// 时间线块。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockView {
    /// 块 id（与 `AnswerDelta` / `ReasoningDelta` / `BlockFinal` 对齐）。
    pub block_id: BlockId,
    /// 所属回合 id（会话级横幅为 `None`）。
    pub turn_id: Option<TurnId>,
    /// 时间线内的定序号（跨页唯一）。
    pub ord: u64,
    /// 块种类。
    pub kind: BlockKind,
    /// 内容引用（大内容走 blob/窗口）。
    pub content: ContentRef,
    /// 关联的工具调用（**跨页归属规则**：同 `tool_call_id` 的块在任一页都带同一 id）。
    pub tool_call_id: Option<ToolCallId>,
    /// 产生该块的时间（毫秒）。
    pub created_at_ms: u64,
    /// 发起者类别（`User` 块携带）。
    pub actor: Option<BlockActor>,
    /// 输入投递意图（`User` 块携带）。
    pub purpose: Option<InputPurpose>,
    /// 生成该助手块的模型名（`Answer`/`Reasoning` 块携带）。
    pub model: Option<String>,
    /// 块级 token 用量（遥测口径）。
    pub usage: Option<UsageInfo>,
    /// 内核实测产出时长（毫秒；吐字速率分母）。
    pub generated_ms: Option<u64>,
    /// 块附带图片（内容寻址 blob，禁止 base64 内联）。
    pub images: Vec<ImageRefView>,
    /// 块附带文本附件引用。
    pub attachments: Vec<ContentRef>,
    /// 跨代理消息元数据（如有）。
    pub inter_agent: Option<InterAgentView>,
    /// 子代理终态通知元数据（如有）。
    pub subagent_terminal: Option<SubagentTerminalView>,
}

/// 块种类。
///
/// 压缩摘要由 [`SessionSnapshot::pinned_compaction`] 单独承载，不混入时间线；
/// `System` 只承载系统提示/横幅。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockKind {
    /// 用户输入。
    User,
    /// 模型答案。
    Answer,
    /// 模型推理。
    Reasoning,
    /// 工具调用。
    ToolCall,
    /// 工具结果。
    ToolResult,
    /// 系统提示（横幅等；压缩摘要单独走 `pinned_compaction`）。
    System,
}

/// 回合摘要（首屏 `turns` 窗口）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnSummary {
    /// 回合 id。
    pub turn_id: TurnId,
    /// 触发该回合的用户输入 id（如有）。
    pub input_id: Option<InputId>,
    /// 回合定序号。
    pub ordinal: u64,
    /// 非终态子阶段（已有 `terminal` 时为 `None`）。
    pub phase: Option<TurnPhase>,
    /// 终态（未结束为 `None`）。
    pub terminal: Option<TurnTerminal>,
    /// 回合汇总用量（遥测兜底）。
    pub usage: Option<UsageInfo>,
    /// 开始时刻（毫秒）。
    pub started_at_ms: u64,
}

/// 首屏快照（开箱即渲染：一次带齐块、工具调用、未决交互、执行会话与驱动权）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// 会话 id。
    pub session_id: SessionId,
    /// 标题。
    pub title: Option<String>,
    /// 所属工作区 id（如已归组）。
    pub workspace_id: Option<WorkspaceId>,
    /// 会话工作目录。
    pub cwd: Option<String>,
    /// 是否已归档。
    pub archived: bool,
    /// 会话设置乐观锁修订号。
    pub settings_revision: u64,
    /// 会话显式模型绑定（未覆写为 `None`）。
    pub model_selection: Option<ModelSelection>,
    /// 当前活动状态（V3 仅四态，不存在"未运行/未加载"态）。
    pub activity: qaqh_core_vocab::ActivityState,
    /// 时间线代际（翻页校验用）。
    pub timeline_epoch: u64,
    /// 事件流当前序号（客户端从此续订）。
    pub seq: u64,
    /// 回合窗口。
    pub turns: Vec<TurnSummary>,
    /// 尾部块窗口（`ord` 升序）。
    pub tail_blocks: Vec<BlockView>,
    /// 尾部窗口里出现的工具调用（一次给齐，前端不必二次请求）。
    pub tool_calls: Vec<ToolCallView>,
    /// 当前等待用户处置的交互（审批 / Ask）。
    pub pending_interactions: Vec<InteractionView>,
    /// 关联的 exec 执行会话快照。
    pub exec_sessions: Vec<ExecSessionSnapshot>,
    /// 子代理/同事摘要列表。
    pub subagents: Vec<SubagentSummary>,
    /// 会话持久目标（无目标为 `None`）。
    pub goal: Option<GoalView>,
    /// 当前驱动权客户端实例 id（无驱动端为 `None`）。
    pub driver_client_instance_id: Option<String>,
    /// 当前驱动权定序序号。
    pub driver_ordinal: u64,
    /// **独立于 `turns` 窗口**的固定压缩摘要。
    pub pinned_compaction: Option<CompactionSummary>,
    /// 当前工具模式。
    pub tool_mode: ToolMode,
    /// `Custom` 工具模式下的自定义工具白名单。
    pub custom_tools: Vec<String>,
    /// 若由交接产生，来源会话 id。
    pub handoff_from: Option<SessionId>,
    /// 创建时刻（毫秒）。
    pub created_at_ms: u64,
}

/// 翻页结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptPage {
    /// 本页块（`ord` 升序）。
    pub blocks: Vec<BlockView>,
    /// 本页关联的工具调用（跨页工具归属：同 id 同视图）。
    pub tool_calls: Vec<ToolCallView>,
    /// 下一页的 `before_ord`（`None` = 到头了）。
    pub next_before_ord: Option<u64>,
    /// 时间线代际（与请求一致）。
    pub timeline_epoch: u64,
}

/// 会话摘要（列表与设置刷新共用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// 会话 id。
    pub session_id: SessionId,
    /// 标题。
    pub title: Option<String>,
    /// 所属工作区 id。
    pub workspace_id: Option<WorkspaceId>,
    /// 会话工作目录。
    pub cwd: Option<String>,
    /// 是否已归档。
    pub archived: bool,
    /// 当前模型选择。
    pub model_selection: Option<ModelSelection>,
    /// 当前工具模式。
    pub tool_mode: ToolMode,
    /// `Custom` 工具模式下的自定义工具白名单。
    pub custom_tools: Vec<String>,
    /// 设置乐观锁修订号。
    pub settings_revision: u64,
    /// 若由交接产生，来源会话 id。
    pub handoff_from: Option<SessionId>,
    /// 活动状态（V3 语义下无 `NotRunning`/"未加载"态，前端直接按四态渲染）。
    pub activity: qaqh_core_vocab::ActivityState,
    /// 创建时刻（毫秒）。
    pub created_at_ms: u64,
    /// 最后活动时刻（毫秒）。
    pub updated_at_ms: u64,
}

/// 会话域回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionResponse {
    /// 已新建会话。
    Created(SessionSummary),
    /// 首屏快照。
    Snapshot(Box<SessionSnapshot>),
    /// 翻页结果。
    Page(Box<TranscriptPage>),
    /// 会话列表。
    Sessions(Vec<SessionSummary>),
    /// 会话最新摘要/设置回读。
    Summary(SessionSummary),
    /// Todo 列表快照。
    Todos(TodoListSnapshot),
    /// 团队协作与任务板快照。
    Team(Box<TeamOverview>),
    /// 技能目录快照。
    Skills(SkillsCatalogView),
    /// 状态性操作已受理（真正的事件/驱动权广播走 `SessionEvents` 流）。
    Ack,
}
