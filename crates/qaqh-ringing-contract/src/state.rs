//! 单一会话状态:一份 SessionState 取代 control/conversation/tool 三份频道快照。
//!
//! 按业务组织字段;执行证明概念(execution_id、request_hash、context_revision、
//! reconciled、args_hash、source_fact_seq、recovery_ref)不进入公共协议。

use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

use crate::commands::{ImageBlock, InterAgentEnvelope, SubagentTerminalNotification};
use crate::fact::{
    ActivityState, ActorRef, AssistantBlockKind, BlockId, ContentRef, ContentValue, ExecSessionId,
    InputId, InputPurpose, InteractionChoice, InteractionDecision, InteractionId, InteractionKind,
    InterruptReason, SessionId, SubagentTerminalStatus, ToolCallId, ToolCallOutcome, ToolMetrics,
    TurnError, TurnId, TurnMode, TurnTerminal, UsageInfo,
};

/// 会话当前状态的唯一权威快照。`seq` 是"下一个将分配的事件序号"(exclusive
/// 水位):快照已覆盖全部 `seq < state.seq` 的 SSE 帧;客户端跳过
/// `seq < state.seq`、处理 `seq >= state.seq`(相等边界必须处理)。与
/// `SessionEventFrame` 的文档同源。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct SessionState {
    pub session_id: Option<SessionId>,
    pub seq: u64,
    pub activity: ActivityState,
    pub current_turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub turns: Vec<TurnView>,
    #[serde(default)]
    pub timeline: Vec<TimelineEntry>,
    #[serde(default)]
    pub tools: Vec<ToolCallView>,
    #[serde(default)]
    pub interactions: Vec<InteractionView>,
    #[serde(default)]
    pub subagents: Vec<SubagentView>,
    /// 执行会话读模型:全部非终态会话 + 最近 32 个终态摘要;终态正文经
    /// ContentValue 引用按需读取(spec §6.3)。
    #[serde(default)]
    pub exec_sessions: Vec<ExecSessionView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<DriverState>,
}

/// `GET /ringing/v2/sessions/{id}/bootstrap` 的响应:一次 HTTP 拿到
/// 单份 `SessionState` 快照 + 同步游标所需的服务器 epoch。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct SessionBootstrap {
    pub schema: String,
    pub version: u32,
    /// 读模型/事件词表契约代际(`SESSION_CONTRACT_REVISION`);客户端严格校验。
    pub session_contract_revision: String,
    pub server_epoch: String,
    pub session_id: String,
    pub state: SessionState,
}

/// 回合活动状态:由活动回合的 phase 直接派生的统一词表(spec §4.4)。
/// 失败/中断记录在回合 outcome,不能用活动状态替代历史结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum SessionActivityState {
    Idle,
    Running,
    WaitingUser,
    Cancelling,
}

/// Typed `session.activity` and `/activity` result entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct SessionActivity {
    #[serde(rename = "session_id")]
    pub session_id: String,
    pub state: SessionActivityState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub seq: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TurnView {
    pub turn_id: TurnId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_id: Option<InputId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<TurnMode>,
    /// 非终态 phase;有 outcome 时必须为 None。等待迁移靠事件,不靠前端定时器。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<TurnPhase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<TurnOutcome>,
}

/// 回合非终态 phase。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    Running,
    WaitingUser,
    Cancelling,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum TurnOutcome {
    Finished {
        terminal: TurnTerminal,
        usage: Option<UsageInfo>,
        error: Option<TurnError>,
    },
    Interrupted {
        reason: InterruptReason,
    },
}

/// timeline 条目:消息、模型输出块、工具调用引用与压缩摘要。
/// 工具调用以 `call_id` 引用 `tools` 列表,不重复携带调用状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum TimelineEntry {
    Input {
        block_id: BlockId,
        turn_id: Option<TurnId>,
        purpose: InputPurpose,
        content: ContentValue,
        attachments: Vec<ContentRef>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageBlock>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inter_agent: Option<InterAgentEnvelope>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subagent_terminal: Option<SubagentTerminalNotification>,
        actor: ActorRef,
    },
    AssistantBlock {
        block_id: BlockId,
        turn_id: TurnId,
        block_kind: AssistantBlockKind,
        content: ContentValue,
        model: String,
        usage: Option<UsageInfo>,
        /// Provider 专属续传状态(Responses API encrypted reasoning 等),
        /// 下一轮原样回传;客户端不解读。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        opaque: Vec<serde_json::Value>,
    },
    ToolCall {
        call_id: ToolCallId,
        turn_id: Option<TurnId>,
    },
    /// 工具结果进入对话流(回传模型的 tool 消息与前端 transcript 同源)。
    ToolResult {
        call_id: ToolCallId,
        turn_id: Option<TurnId>,
        content: ContentValue,
    },
    Compaction {
        summary: ContentValue,
    },
}

/// 一次工具调用从参数到结果的单份业务状态。
///
/// 状态权威是带标签的 `state` 联合;独立的 status/error 权威字段已删除,
/// 终态事实在 `state.finished.outcome` 单点表达。关联执行会话用显式
/// `exec_session_id`(仅 yielded 的调用携带)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ToolCallView {
    pub call_id: ToolCallId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// 工具原始参数(裸 JSON 对象)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
    pub state: ToolCallState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_session_id: Option<ExecSessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<ContentValue>,
    #[serde(default)]
    pub output_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<ToolMetrics>,
    /// edit 类工具对代码的净变更统计(结构化结果,不压进正文)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_delta: Option<CodeDelta>,
    /// 工具产生的结构化效果(owner 消费,不丢弃)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<ToolEffectInfo>,
}

/// On-demand transcript slice for one persisted turn. `SessionBootstrap` stays
/// bounded to its existing read path; clients use this view to restore an
/// origin turn that has left the renderer's pagination window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct SessionTurnSnapshot {
    pub turn: TurnView,
    pub timeline: Vec<TimelineEntry>,
    pub tools: Vec<ToolCallView>,
}

/// 工具调用的带标签状态联合(spec §4.2)。finished 是调用已返回,不等于
/// 关联进程已退出;终态 outcome 不可改写,仅允许同一事实的幂等重放。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolCallState {
    Declared,
    /// 统一覆盖 permission、Ask、plan 等;具体种类由关联 InteractionView 表达。
    AwaitingInteraction {
        interaction_id: String,
    },
    Running,
    Cancelling,
    Finished {
        outcome: ToolCallOutcome,
    },
}

impl ToolCallState {
    /// 非终态判定(terminal = finished;finished 无后续业务迁移)。
    pub fn is_terminal(&self) -> bool {
        matches!(self, ToolCallState::Finished { .. })
    }
}

/// 工具对代码的净变更统计。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct CodeDelta {
    pub lines_added: usize,
    pub lines_removed: usize,
    pub files_created: usize,
    pub files_deleted: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// 工具产生的结构化效果的契约词表。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolEffectInfo {
    SkillActivated {
        name: String,
    },
    SkillReleased {
        name: String,
    },
    SubagentSpawned {
        child_session_id: String,
        name: String,
        task_text: String,
    },
}

/// 一次可续读、可输入、可停止的执行会话的公共读模型(spec §6.1)。
///
/// 输入/权限身份不由正文携带;客户端不得以 command/cwd 的相似性配对会话。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ExecSessionView {
    pub id: ExecSessionId,
    pub owner_session_id: SessionId,
    pub origin_turn_id: TurnId,
    pub origin_call_id: ToolCallId,
    pub state: ExecSessionState,
    pub tty: bool,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub started_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<i64>,
    /// 捕获流下一个 offset;客户端按 offset 去重输出块。
    pub next_offset: u64,
    /// UTF-8 边界上的连续尾窗(≤16 KiB)。
    pub output_preview: String,
    /// 尾窗起始 offset。
    pub preview_start_offset: u64,
    /// 捕获是否完整(EOF 已收尾);与进程 outcome 独立。
    pub output_complete: bool,
    pub capture_omitted_bytes: u64,
    pub stream_dropped_bytes: u64,
    /// 终态权威输出(ContentValue 引用);非终态为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<ContentValue>,
}

/// 执行会话状态机(spec §4.3)。后台不是一种生命状态:调用 yielded、
/// 会话 running 即构成后台继续执行。终态不可回退。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecSessionState {
    Running,
    /// 已请求停止;实际退出与有界输出收尾完成后才进入 finished。
    Stopping {
        reason: ExecStopReason,
    },
    Finished {
        outcome: ExecSessionOutcome,
    },
}

impl ExecSessionState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, ExecSessionState::Finished { .. })
    }
}

/// 发起停止的原因;发起者与停止原因分开记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "snake_case")]
pub enum ExecStopReason {
    Interrupt,
    Terminate,
    TurnCancel,
    Timeout,
    Shutdown,
}

/// 执行会话终态结果。正常退出码非零用 exited{exit_code != 0} 记录事实;
/// 不为取消/超时伪造退出码,已知的 OS 退出码可作为附加事实保存。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecSessionOutcome {
    Exited {
        exit_code: i32,
    },
    Cancelled {
        reason: ExecCancelReason,
        exit_code: Option<i32>,
    },
    TimedOut {
        exit_code: Option<i32>,
    },
    Interrupted {
        reason: ExecInterruptReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "snake_case")]
pub enum ExecCancelReason {
    Interrupt,
    Terminate,
    TurnCancel,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "snake_case")]
pub enum ExecInterruptReason {
    DaemonRestart,
    LostExecution,
    UpgradeStateUnknown,
}

/// permission / ask / plan 统一的交互视图,按 interaction_id 应答。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct InteractionView {
    pub interaction_id: InteractionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<ToolCallId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    pub kind: InteractionKind,
    pub request: ContentValue,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<InteractionChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<InteractionResolution>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct InteractionResolution {
    pub decision: InteractionDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<ContentValue>,
    pub resolved_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct SubagentView {
    pub child_session_id: SessionId,
    pub parent_call_id: ToolCallId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SubagentTerminalStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ContentValue>,
}

/// driver 席位:holder 为 daemon 认证的 client_session_id,epoch 单调递增。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct DriverState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    pub epoch: u64,
}
