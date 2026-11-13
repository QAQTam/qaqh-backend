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
    DriverState, ExecSessionView, GoalView, InteractionView, SubagentView, TimelineEntry,
    ToolCallState, TurnOutcome, TurnPhase,
};

/// `skip_serializing_if` 用的假值判定:值为 `false` 时整键缺席。
///
/// additive 字段在不 bump revision 的窗口期必须能「缺席即默认」,否则新客户端
/// 对着旧 daemon 会读到凭空出现的键;与 [`SessionEvent::ToolArgumentsDelta`]
/// 里 `lines_added` 的 `None` 口径同律。
fn is_false(value: &bool) -> bool {
    !*value
}

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
    /// `target_files` 是**内核数出来的**目标文件:只有 `apply_patch` 会填——
    /// 它的参数里没有路径字段,文件只存在于 Codex 补丁文法里,而该文法的唯一
    /// 事实源是补丁引擎;其余工具的目标文件就是参数键本身,客户端按 schema 直读
    /// `args`,内核不重复发明 JSON 键名。缺席 = 内核没认出来,**不是**「不改文件」。
    /// `args_complete` = 参数文本已经吐完且形状合法(JSON 面闭合可 parse /
    /// 原生面见到 `*** End Patch`)。它只说明「这一帧的 `text` 就是最终参数」,
    /// 不授权执行、不代表已声明:声明仍由 `ToolDeclared` 落库并携带权威 `args`。
    ToolArgumentsDelta {
        call_id: ToolCallId,
        turn_id: TurnId,
        tool_name: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lines_added: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lines_removed: Option<u32>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        target_files: Vec<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        args_complete: bool,
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
    /// 撤回(硬截断):`from_turn_id` 起(含)的整段历史已从会话事实源删除——
    /// 不落备份、不可恢复。本帧是**追加**的一条截断事实,已发出的帧不回收
    /// (`SessionEventFrame.seq` 单调与 bootstrap 水位线的口径不得单方改动);
    /// 客户端据此把 `removed_turn_ids` 的回合从本地视图整枝移除(输入、正文、
    /// 工具卡片、交互一并消失),`removed_turn_ids` 是内核算出的权威集合,客户端
    /// 不得自行按顺序推断。断线重连的客户端无需处理本帧:bootstrap 快照里
    /// 本就没有被删的回合。只回历史,**不回副作用**——文件改动、执行会话进程、
    /// 子代理、已通过的审批一律不动(journal 保留原样,需要时按它另行回滚)。
    TimelineTruncated {
        from_turn_id: TurnId,
        removed_turn_ids: Vec<TurnId>,
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
    /// 会话目标变化(ADR-0015):set/状态迁移/记账/清除都经此帧;
    /// `None` = goal 已清除。与 bootstrap `state.goal` 同形状。
    GoalUpdated {
        goal: Option<GoalView>,
    },
    /// ADR-0016：内核 fork 完成的权威广播。纯信息面——客户端只做投影切换
    /// （渲染新会话、标记旧线已封箱），fork 本体不依赖本事件。双发：链根
    /// （封箱线）与后继线各一条，订阅任一流都看得到完整事实。
    HandoffSealed {
        /// 封箱会话（链上本线，已被 handoff 终结）。
        sealed_session_id: String,
        /// 链根 = handoff 工作链锚点（goal 挂靠点）。
        chain_root: String,
        /// 内核 fork 出的后继会话 id。
        successor_session_id: String,
        /// 产生交接的回合（封箱线上的那个回合）。
        turn_id: TurnId,
        /// 模型的一句话工作简述（与后继会话标题同源）。
        work_summary: String,
        /// fork 完成时刻（unix ms）。
        at_ms: u64,
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
            SessionEvent::HandoffSealed { turn_id, .. } => Some(turn_id),
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
