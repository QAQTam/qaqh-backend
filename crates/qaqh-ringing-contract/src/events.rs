//! 单条会话 SSE 流的类型化事件词表。
//!
//! 一条流、一个游标(`epoch:seq`);消息、工具、交互、执行会话用同一套
//! 事件表达,不创建第二条进程 SSE(spec §6.1)。事件只表达业务状态迁移;
//! 内部调度、授权检查与诊断数据不进入公共协议。

use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

use crate::fact::{
    ActivityState, AssistantBlockKind, BlockId, ContentValue, ExecSessionId, InputId,
    InteractionDecision, InteractionId, ToolCallId, ToolCallOutcome, ToolMetrics, TurnId,
};
use crate::state::{
    DriverState, ExecSessionView, InteractionView, SubagentView, TimelineEntry, ToolCallState,
    TurnOutcome, TurnPhase,
};

/// 普通工具(无执行会话)实时输出流标识;无输出的工具不产生 output.delta 事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// 执行会话输出块来源(spec §7.1):PTY 使用 terminal,不能假称还可区分两条流。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "snake_case")]
pub enum ExecOutputSource {
    Stdout,
    Stderr,
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "snake_case")]
pub enum InteractionEndStatus {
    Resolved,
    Expired,
    Cancelled,
}

/// 会话内业务变化。exec 事件与调用事件同流同水位;客户端不得靠事件顺序
/// 推断关联,关联只按 call_id/turn_id/exec_session_id(spec §6.1)。
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
    },
    /// 回合非终态 phase 迁移(waiting_user/cancelling/running);终态由
    /// TurnEnded 表达,不发 phase=null 事件。
    TurnPhaseChanged {
        turn_id: TurnId,
        phase: TurnPhase,
    },
    /// 回合终态:finished 或 interrupted 由 outcome 区分,不发两类事件。
    TurnEnded {
        turn_id: TurnId,
        outcome: TurnOutcome,
    },
    /// 参数生成进度,仅用于展示;不代表工具已执行或参数已合法。
    /// `tool_name` 是模型侧声明的工具名;`text` 是累计参数前缀(替换语义)。
    /// `lines_added`/`lines_removed` 是从**已到达的参数文本**数出来的累计行差
    /// 估算:与 `text` 同帧、同替换语义,所以丢帧自愈、重帧幂等。口径是参数文本
    /// (`edit` 数 `old_string`/`new_string`,`write` 数 `content`,
    /// `apply_patch` 数补丁的 `+`/`-` 行),**不等于**实际改动行数——`replace_all`
    /// 会成倍、命中失败会归零,而这些在参数吐完之前不可知。
    /// 只对写工具出数:`None` = 该工具没有行数概念(read/grep/exec…)或估算已
    /// 冻结,客户端不得用 `?? 0` 把「不知道」渲染成「零变化」。
    /// 终态一到权威值立刻接管(`ToolFinished.code_delta` 是真 diff 口径)。
    ToolArgumentsDelta {
        call_id: ToolCallId,
        turn_id: TurnId,
        tool_name: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lines_added: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lines_removed: Option<u32>,
    },
    /// 模型声明工具调用:declared 行已落库;`args` 是最终参数(裸 JSON 对象
    /// 或 null)。实时订阅者据此建立与 bootstrap `tools` 视图一致的工具身份。
    ToolDeclared {
        call_id: ToolCallId,
        turn_id: TurnId,
        tool_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<serde_json::Value>,
    },
    /// 非终态工具调用状态迁移:running 的首次迁移就是执行开始,等待与取消
    /// 也经此表达(spec §6.1;替代旧 tool_started)。
    ToolStateChanged {
        call_id: ToolCallId,
        turn_id: TurnId,
        state: ToolCallState,
    },
    /// 工具执行输出尾窗片段(无执行会话的普通工具);按需区分 stdout/stderr。
    ToolOutputDelta {
        call_id: ToolCallId,
        stream: OutputStream,
        text: String,
    },
    /// 工具调用终态:outcome 类型层面不可携带 running;yielded 是成功返回。
    ToolFinished {
        call_id: ToolCallId,
        outcome: ToolCallOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exec_session_id: Option<ExecSessionId>,
        output: Option<ContentValue>,
        output_truncated: bool,
        metrics: Option<ToolMetrics>,
        /// edit 类工具的结构化结果。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code_delta: Option<crate::state::CodeDelta>,
        /// 工具产生的结构化效果(owner 消费后随终态发布)。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        effects: Vec<crate::state::ToolEffectInfo>,
    },
    /// 执行会话建立;先于该会话任何输出。
    ExecSessionStarted {
        session: ExecSessionView,
    },
    /// 执行会话非终态迁移(含 stopping)。
    ExecSessionStateChanged {
        session: ExecSessionView,
    },
    /// 执行会话输出增量;追加语义,offset 控制去重(spec §6.3)。
    ExecSessionOutputDelta {
        exec_session_id: ExecSessionId,
        start_offset: u64,
        end_offset: u64,
        source: ExecOutputSource,
        text: String,
    },
    /// 执行会话终态;每个会话一次,含权威输出引用与完整性信息。
    /// 之前必须完成已接受输出的发布;之后不再发布该会话的输出。
    ExecSessionFinished {
        session: ExecSessionView,
    },
    TimelineAppended {
        entry: TimelineEntry,
    },
    /// 模型文本流式片段;同一 block_id 的后续事件替换已展示内容。
    /// `block_kind` 区分 reasoning 与 answer,与 sealed AssistantBlock 同词表。
    MessageDelta {
        turn_id: TurnId,
        block_id: BlockId,
        block_kind: AssistantBlockKind,
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
    /// driver 席位变化(claim/release 生效);`None` = 席位空。与 bootstrap
    /// `state.driver` 同形状。
    DriverChanged {
        driver: Option<DriverState>,
    },
}

impl SessionEvent {
    /// 事件影响的回合(用于前端局部刷新;无关联回合为 None)。
    pub fn turn_id(&self) -> Option<&TurnId> {
        match self {
            SessionEvent::ActivityChanged {
                current_turn_id, ..
            } => current_turn_id.as_ref(),
            SessionEvent::TurnStarted { turn_id, .. }
            | SessionEvent::TurnEnded { turn_id, .. }
            | SessionEvent::TurnPhaseChanged { turn_id, .. }
            | SessionEvent::MessageDelta { turn_id, .. }
            | SessionEvent::ToolDeclared { turn_id, .. }
            | SessionEvent::ToolStateChanged { turn_id, .. } => Some(turn_id),
            SessionEvent::ToolArgumentsDelta { turn_id, .. } => Some(turn_id),
            _ => None,
        }
    }
}

/// SSE 帧载荷:`seq` 是 epoch 内 0 基单调序号(第 s 个事件帧的 seq = s)。
/// bootstrap `SessionState.seq` 是"下一个将分配的 seq"——快照已覆盖全部
/// `seq < state.seq` 的帧;客户端跳过 `seq < state.seq`,处理 `seq >= state.seq`
/// (相等边界必须处理)。与 `SessionState::seq` 的文档同源,不得单方改动。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct SessionEventFrame {
    pub seq: u64,
    #[serde(flatten)]
    pub event: SessionEvent,
}
