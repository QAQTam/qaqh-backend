//! 四类逻辑流与多端联动原语（`ADR-0042` 铁律 4）。
//!
//! ## 因果独立域分流，**不按投递分级拆流**
//!
//! 四条流的分界是**因果**：控制面（一问一答）、会话事件、Shell PTY 字节、
//! Blob 传输。`Reliable` / `Replaceable` 是**同一流内的帧属性**，不是拆流依据——
//! 按分级拆流会让"终态必须先于尾增量 flush"这条定序不变量失去物理载体
//! （两条流之间没有顺序保证）。
//!
//! 所以：**同会话死守单流单 `seq`**，`Replaceable` 帧由后端合帧/节流，
//! 但 `seq` 仍在这条流上单调递增；`Reliable` 帧发送前必须先 flush 已合并的增量。

use bytes::Bytes;
use qaqh_core_vocab::{
    ActivityState, ContentHash, ContentRef, ExecSessionId, InteractionId, SessionId, ToolCallId,
    TurnId, TurnTerminal, UsageInfo,
};
use serde::{Deserialize, Serialize};

/// 四条逻辑流（多路复用连接上的流身份）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamKind {
    /// 控制面：一问一答，全部 `Reliable`。
    Control,
    /// 会话事件：前端状态唯一真相源；`since_seq` 支持重连尾追。
    SessionEvents {
        /// 会话 id。
        session_id: SessionId,
        /// 客户端已收到的最后序号（`0` = 从头）。
        since_seq: u64,
    },
    /// Shell PTY：原始字节直喂终端模拟器（零转义加工）。
    ShellPty {
        /// shell id。
        shell_id: String,
        /// 客户端已有的字节偏移（服务端从此处续传）。
        from_offset: u64,
    },
    /// Blob 传输：大输出/图片按窗口分块，天然可背压。
    BlobTransfer,
}

/// 投递分级（**帧属性**，不是流属性）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeliveryClass {
    /// 必达：终态、审批、错误。发送前必须先 flush 已合并的增量。
    Reliable,
    /// 可替换：允许后端高频合帧/节流（答案增量、exec 输出增量）。
    Replaceable,
}

/// 帧公共头（四条流共用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameMeta {
    /// 单流内单调递增序号（同会话单流单 seq）。
    pub seq: u64,
    /// 服务端产生该帧的时刻（毫秒 Unix；**服务端时钟是唯一时间权威**）。
    pub server_ts_ms: u64,
    /// 投递分级。
    pub delivery: DeliveryClass,
}

/// 会话事件帧。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEventFrame {
    /// 帧头。
    pub meta: FrameMeta,
    /// 事件体。
    pub event: SessionEvent,
}

/// Shell PTY 输出帧（原始字节，**不做 UTF-8 假设**）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellOutputFrame {
    /// 帧头。
    pub meta: FrameMeta,
    /// 起始偏移（字节）。
    pub start_offset: u64,
    /// 结束偏移（字节）。
    pub end_offset: u64,
    /// 原始字节。
    pub bytes: Bytes,
}

/// Blob 分片帧。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobChunkFrame {
    /// 帧头。
    pub meta: FrameMeta,
    /// 内容哈希（内容寻址）。
    pub hash: ContentHash,
    /// 分片在整体中的偏移。
    pub offset: u64,
    /// 分片字节。
    pub bytes: Bytes,
    /// 是否最后一片。
    pub last: bool,
}

/// 工具调用状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolCallStatus {
    /// 已声明、尚未执行。
    Pending,
    /// 执行中。
    Running,
    /// 成功。
    Succeeded,
    /// 失败。
    Failed,
    /// 取消。
    Cancelled,
    /// 结果不确定（已产生副作用，结论未知）。
    Indeterminate,
}

/// 工具调用视图（跨页工具归属的唯一键是 `tool_call_id`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallView {
    /// 调用 id。
    pub tool_call_id: ToolCallId,
    /// 工具名。
    pub tool_name: String,
    /// 状态。
    pub status: ToolCallStatus,
    /// 开始时刻（毫秒）。
    pub started_at_ms: u64,
    /// 结束时刻（毫秒；未结束为 `None`）。
    pub finished_at_ms: Option<u64>,
    /// 结果摘要（全文走 [`ContentRef`]）。
    pub summary: Option<ContentRef>,
}

/// 交互请求视图（审批 / Ask）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionView {
    /// 交互 id。
    pub interaction_id: InteractionId,
    /// 交互种类。
    pub kind: InteractionKind,
    /// 标题。
    pub title: String,
    /// 正文（审批理由 / Ask 问题）。
    pub body: String,
    /// 选项（Ask；审批为空）。
    pub options: Vec<String>,
    /// 到期时刻（毫秒；`None` = 不超时）。
    pub expires_at_ms: Option<u64>,
}

/// 交互种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InteractionKind {
    /// 权限审批。
    Permission,
    /// 自由问答。
    Ask,
}

/// 交互处置结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InteractionResolution {
    /// 批准。
    Approved,
    /// 拒绝（带理由）。
    Denied {
        /// 理由。
        reason: String,
    },
    /// 已回答。
    Answered {
        /// 回答文本。
        answer: String,
    },
    /// 超时/取消。
    Abandoned,
}

/// 执行会话视图（V3 形状：对外只给引用与水位）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecSessionSnapshot {
    /// 执行会话 id。
    pub exec_session_id: ExecSessionId,
    /// 命令行。
    pub command: String,
    /// 是否 PTY。
    pub tty: bool,
    /// 是否已结束。
    pub finished: bool,
    /// 退出码。
    pub exit_code: Option<i32>,
    /// 模型/客户端可见水位（字节）。
    pub next_offset: u64,
    /// 全文引用（大输出走 blob；权威全文另在 append-only 日志里）。
    pub output: Option<ContentRef>,
}

/// 需重同步的原因（客户端据此决定是"从头拉快照"还是"只拉尾巴"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResyncReason {
    /// 游标跌出回放尾覆盖范围。
    CursorGap,
    /// 时间线代际变更（重写/压缩）。
    EpochMismatch,
}

/// 会话事件（前端状态唯一真相源）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionEvent {
    /// 活动状态变更。
    ActivityChanged {
        /// 会话 id。
        session_id: SessionId,
        /// 新活动状态。
        activity: ActivityState,
        /// 当前回合（空闲时为 `None`）。
        current_turn_id: Option<TurnId>,
    },
    /// 回合开始。
    TurnStarted {
        /// 会话 id。
        session_id: SessionId,
        /// 回合 id。
        turn_id: TurnId,
        /// 开始时刻（毫秒）。
        started_at_ms: u64,
    },
    /// 回合结束（终态一旦成立不可回退）。
    TurnFinished {
        /// 会话 id。
        session_id: SessionId,
        /// 回合 id。
        turn_id: TurnId,
        /// 终态。
        terminal: TurnTerminal,
        /// 用量。
        usage: UsageInfo,
    },
    /// 答案增量（`Replaceable`）。
    AnswerDelta {
        /// 会话 id。
        session_id: SessionId,
        /// 块 id。
        block_id: qaqh_core_vocab::BlockId,
        /// 文本增量。
        text: String,
    },
    /// 推理增量（`Replaceable`）。
    ReasoningDelta {
        /// 会话 id。
        session_id: SessionId,
        /// 块 id。
        block_id: qaqh_core_vocab::BlockId,
        /// 文本增量。
        text: String,
    },
    /// 块定稿（全文以引用给出）。
    BlockFinal {
        /// 会话 id。
        session_id: SessionId,
        /// 块 id。
        block_id: qaqh_core_vocab::BlockId,
        /// 全文引用。
        content: ContentRef,
    },
    /// 工具调用状态变更。
    ToolCallUpdated {
        /// 会话 id。
        session_id: SessionId,
        /// 调用视图。
        call: ToolCallView,
    },
    /// 交互请求。
    InteractionRequested {
        /// 会话 id。
        session_id: SessionId,
        /// 交互视图。
        interaction: InteractionView,
    },
    /// 交互处置。
    InteractionResolved {
        /// 会话 id。
        session_id: SessionId,
        /// 交互 id。
        interaction_id: InteractionId,
        /// 处置结果。
        resolution: InteractionResolution,
    },
    /// 执行会话状态变更。
    ExecSessionUpdated {
        /// 会话 id。
        session_id: SessionId,
        /// 执行会话视图。
        exec: ExecSessionSnapshot,
    },
    /// 执行输出增量（`Replaceable`；泄洪后 `spilled=true` 且不再有后续增量）。
    ExecOutputDelta {
        /// 会话 id。
        session_id: SessionId,
        /// 执行会话 id。
        exec_session_id: ExecSessionId,
        /// 起始偏移。
        start_offset: u64,
        /// 结束偏移。
        end_offset: u64,
        /// 文本。
        text: String,
        /// 是否已泄洪（ADR-0035：模型面被闸住，客户端面照旧）。
        spilled: bool,
    },
    /// **多端联动核心**：会话驱动权变更（单点定序，见 [`DriverChanged`]）。
    DriverChanged {
        /// 会话 id。
        session_id: SessionId,
        /// 取得驱动权的客户端实例 id。
        driver_client_instance_id: String,
        /// 驱动权序号（单调递增；旧序号的重放会被丢弃）。
        ordinal: u64,
    },
    /// 固定压缩摘要变更（首屏与翻页共享同一份）。
    PinnedCompactionChanged {
        /// 会话 id。
        session_id: SessionId,
        /// 摘要（解除固定时为 `None`）。
        summary: Option<CompactionSummary>,
    },
    /// 工具目录变更（前端据此重取 `ToolCatalog`）。
    CatalogChanged {
        /// 会话 id。
        session_id: SessionId,
    },
    /// 会话被删除。
    SessionRemoved {
        /// 会话 id。
        session_id: SessionId,
    },
    /// 需要重同步（两类真缺口才走这里，不是常规回退路径）。
    ResyncRequired {
        /// 会话 id。
        session_id: SessionId,
        /// 原因。
        reason: ResyncReason,
        /// 当前时间线代际。
        timeline_epoch: u64,
    },
}

/// 固定压缩摘要（**独立于 `turns` 窗口**：首屏与向上翻页都带着它）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionSummary {
    /// 压缩发生的时刻（毫秒）。
    pub pinned_at_ms: u64,
    /// 被压缩的块数。
    pub blocks_compacted: u64,
    /// 摘要正文。
    pub summary: String,
}

impl SessionEvent {
    /// 事件所属会话。
    pub fn session_id(&self) -> &SessionId {
        match self {
            Self::ActivityChanged { session_id, .. }
            | Self::TurnStarted { session_id, .. }
            | Self::TurnFinished { session_id, .. }
            | Self::AnswerDelta { session_id, .. }
            | Self::ReasoningDelta { session_id, .. }
            | Self::BlockFinal { session_id, .. }
            | Self::ToolCallUpdated { session_id, .. }
            | Self::InteractionRequested { session_id, .. }
            | Self::InteractionResolved { session_id, .. }
            | Self::ExecSessionUpdated { session_id, .. }
            | Self::ExecOutputDelta { session_id, .. }
            | Self::DriverChanged { session_id, .. }
            | Self::PinnedCompactionChanged { session_id, .. }
            | Self::CatalogChanged { session_id }
            | Self::SessionRemoved { session_id }
            | Self::ResyncRequired { session_id, .. } => session_id,
        }
    }

    /// 该事件的投递分级（**帧属性由事件语义决定**，调用方不得自行挑选）。
    ///
    /// 只有三类高频增量是 `Replaceable`；其余一律 `Reliable`——包括 `DriverChanged`
    /// （驱动权丢了就是两台设备同时发指令，绝不允许被合帧丢掉）。
    pub fn delivery(&self) -> DeliveryClass {
        match self {
            Self::AnswerDelta { .. } | Self::ReasoningDelta { .. } | Self::ExecOutputDelta { .. } => {
                DeliveryClass::Replaceable
            }
            _ => DeliveryClass::Reliable,
        }
    }
}
