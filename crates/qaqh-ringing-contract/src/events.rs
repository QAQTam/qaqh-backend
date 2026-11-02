//! 单条会话 SSE 流的类型化事件词表。
//!
//! 一条流、一个游标(`epoch:seq`);消息、工具、交互用同一套事件表达,
//! 不再有 control/conversation/tool 频道区分。

use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

use crate::fact::{
    ActivityState, BlockId, ContentValue, InputId, InteractionDecision, InteractionId,
    ToolCallId, ToolError, ToolMetrics, TurnId, TurnMode,
};
use crate::state::{
    InteractionView, SubagentView, TimelineEntry, ToolCallStatus, TurnOutcome,
};

/// 工具实时输出流标识;无输出的工具不产生 output.delta 事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "snake_case")]
pub enum InteractionEndStatus {
    Resolved,
    Expired,
    Cancelled,
}

/// 会话内业务变化。事件只表达业务状态迁移;内部调度、授权检查与
/// 诊断数据不进入公共协议。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    ActivityChanged {
        activity: ActivityState,
        current_turn_id: Option<TurnId>,
    },
    TurnStarted {
        turn_id: TurnId,
        input_id: Option<InputId>,
        mode: Option<TurnMode>,
    },
    /// 回合终态:finished 或 interrupted 由 outcome 区分,不发两类事件。
    TurnEnded {
        turn_id: TurnId,
        outcome: TurnOutcome,
    },
    /// 参数生成进度,仅用于展示;不代表工具已执行或参数已合法。
    ToolArgumentsDelta {
        call_id: ToolCallId,
        turn_id: TurnId,
        text: String,
    },
    /// 后端进入工具实际执行阶段;不代表执行成功。
    ToolStarted {
        call_id: ToolCallId,
    },
    /// 工具执行输出尾窗片段;按需区分 stdout/stderr。
    ToolOutputDelta {
        call_id: ToolCallId,
        stream: OutputStream,
        text: String,
    },
    ToolFinished {
        call_id: ToolCallId,
        status: ToolCallStatus,
        output: Option<ContentValue>,
        output_truncated: bool,
        error: Option<ToolError>,
        metrics: Option<ToolMetrics>,
    },
    TimelineAppended {
        entry: TimelineEntry,
    },
    /// 模型文本流式片段;同一 block_id 的后续事件替换已展示内容。
    MessageDelta {
        turn_id: TurnId,
        block_id: BlockId,
        text: String,
    },
    InteractionRequested {
        interaction: InteractionView,
    },
    InteractionResolved {
        interaction_id: InteractionId,
        decision: Option<InteractionDecision>,
        status: InteractionEndStatus,
    },
    SubagentUpdated {
        subagent: SubagentView,
    },
    SessionMetaChanged {
        title: Option<String>,
    },
}

impl SessionEvent {
    /// 事件影响的回合(用于前端局部刷新;无关联回合为 None)。
    pub fn turn_id(&self) -> Option<&TurnId> {
        match self {
            SessionEvent::ActivityChanged { current_turn_id, .. } => current_turn_id.as_ref(),
            SessionEvent::TurnStarted { turn_id, .. } | SessionEvent::TurnEnded { turn_id, .. }
            | SessionEvent::MessageDelta { turn_id, .. } => Some(turn_id),
            SessionEvent::ToolArgumentsDelta { turn_id, .. } => Some(turn_id),
            _ => None,
        }
    }
}

/// SSE 帧载荷:`seq` 是 epoch 内单调序号,同时是状态快照水位。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct SessionEventFrame {
    pub seq: u64,
    #[serde(flatten)]
    pub event: SessionEvent,
}
