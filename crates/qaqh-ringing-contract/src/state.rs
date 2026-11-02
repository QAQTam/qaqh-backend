//! 单一会话状态:一份 SessionState 取代 control/conversation/tool 三份频道快照。
//!
//! 按业务组织字段;执行证明概念(execution_id、request_hash、context_revision、
//! reconciled、args_hash、source_fact_seq、recovery_ref)不进入公共协议。

use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

use crate::fact::{
    ActivityState, ActorRef, AssistantBlockKind, BlockId, ContentRef, ContentValue, InputId,
    InputPurpose, InteractionChoice, InteractionDecision, InteractionId, InteractionKind,
    InterruptReason, SessionId, SubagentTerminalStatus, ToolCallId, ToolError, ToolMetrics,
    TurnError, TurnId, TurnMode, TurnTerminal, UsageInfo,
};

/// 会话当前状态的唯一权威快照。`seq` 是本快照覆盖到的 SSE 水位。
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<DriverState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TurnView {
    pub turn_id: TurnId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_id: Option<InputId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<TurnMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<TurnOutcome>,
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
        actor: ActorRef,
    },
    AssistantBlock {
        block_id: BlockId,
        turn_id: TurnId,
        block_kind: AssistantBlockKind,
        content: ContentValue,
        model: String,
        usage: Option<UsageInfo>,
    },
    ToolCall {
        call_id: ToolCallId,
        turn_id: Option<TurnId>,
    },
    Compaction {
        summary: ContentValue,
    },
}

/// 一次工具调用从参数到结果的单份业务状态。
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
    pub status: ToolCallStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<ContentValue>,
    #[serde(default)]
    pub output_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<ToolMetrics>,
    /// edit 类工具对代码的净变更统计(结构化结果,不压进正文)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_delta: Option<CodeDelta>,
    /// 工具产生的结构化效果(owner 消费,不丢弃)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<ToolEffectInfo>,
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
    SkillActivated { name: String },
    SkillReleased { name: String },
    SubagentSpawned {
        child_session_id: String,
        name: String,
        task_text: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Declared,
    AwaitingPermission,
    Running,
    Finished,
    Failed,
    Cancelled,
    Interrupted,
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
