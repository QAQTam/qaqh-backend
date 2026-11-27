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
    ActivityState, ContentHash, ContentRef, ExecSessionId, InputId, InteractionId,
    PermissionGrantScope, SessionId, ToolCallId, TurnId, TurnPhase, TurnTerminal, UsageInfo,
};
use serde::{Deserialize, Serialize};

use crate::session::BlockView;
use crate::team::{GoalView, SubagentSummary};

/// Blob 流方向角色（同时支撑服务端主动推、客户端按哈希拉与客户端上传附件）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlobTransferRole {
    /// 服务端主动推送（大输出外置）。
    ServerPush,
    /// 客户端按内容哈希拉取。
    ClientFetch {
        /// 目标内容哈希。
        hash: ContentHash,
    },
    /// 客户端上传附件（服务端校验权限与 `sha256(bytes) == hash`，并归属目标会话）。
    ClientUpload {
        /// 所属会话 id（与后端 `(session_id, content_id)` 复合主键对齐）。
        session_id: SessionId,
        /// 内容哈希。
        hash: ContentHash,
        /// 预期总字节数。
        len: u64,
    },
}

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
    /// Blob 传输：大输出/图片/附件按窗口分块，天然可背压。
    BlobTransfer {
        /// 传输角色与方向。
        role: BlobTransferRole,
    },
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
    /// 等待用户确认或回答（需结合 `interaction_id` 关联交互卡片）。
    AwaitingInteraction,
    /// 执行中。
    Running,
    /// 正在取消。
    Cancelling,
    /// 成功。
    Succeeded,
    /// 失败。
    Failed,
    /// 取消。
    Cancelled,
    /// 结果不确定（已产生副作用，结论未知）。
    Indeterminate,
}

/// 工具对代码的净变更统计（前端 code_panel 的 +/- 徽标）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeDeltaView {
    /// 新增行数。
    pub lines_added: u32,
    /// 删除行数。
    pub lines_removed: u32,
    /// 新建文件数。
    pub files_created: u32,
    /// 删除文件数。
    pub files_deleted: u32,
    /// 涉及的单一目标文件路径（如适用）。
    pub file: Option<String>,
}

/// 工具执行度量（耗时 / 字节数 / 重试次数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolMetricsView {
    /// 开始时刻（毫秒）。
    pub started_at_ms: u64,
    /// 结束时刻（毫秒）。
    pub finished_at_ms: u64,
    /// 重试次数。
    pub retry_count: u32,
    /// 权威输出总字节数。
    pub output_bytes: u64,
    /// 累计进度字节数。
    pub progress_bytes_total: u64,
}

/// 工具产生的结构化效果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolEffectView {
    /// 派生了子代理会话。
    SubagentSpawned {
        /// 子会话 id。
        child_session_id: SessionId,
        /// 子代理名称。
        name: String,
        /// 任务文本。
        task_text: String,
    },
}

/// 内容寻址图片引用（原始字节经 `BlobTransfer` 传输，禁止 base64 内联）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRefView {
    /// 原始字节的 SHA-256 哈希。
    pub hash: ContentHash,
    /// 原始字节长度。
    pub len: u64,
    /// MIME 类型（如 `"image/png"`）。
    pub mime_type: String,
}

/// 工具调用视图（跨页工具归属的唯一键是 `tool_call_id`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallView {
    /// 调用 id。
    pub tool_call_id: ToolCallId,
    /// 所属回合 id（截断/交接投影用）。
    pub turn_id: Option<TurnId>,
    /// 工具名。
    pub tool_name: String,
    /// 状态。
    pub status: ToolCallStatus,
    /// 开始时刻（毫秒）。
    pub started_at_ms: u64,
    /// 结束时刻（毫秒；未结束为 `None`）。
    pub finished_at_ms: Option<u64>,
    /// 参数 JSON 全文（大参数走 blob）。
    pub args: Option<ContentRef>,
    /// 权威输出全文（小内容内联，大内容走 blob）。
    pub output: Option<ContentRef>,
    /// 输出是否因截断策略被截断。
    pub output_truncated: bool,
    /// 写工具的 unified diff（大 diff 走 blob）。
    pub diff: Option<ContentRef>,
    /// 净代码变更统计。
    pub code_delta: Option<CodeDeltaView>,
    /// 执行度量。
    pub metrics: Option<ToolMetricsView>,
    /// 关联的执行会话 id（yielded 调用携带）。
    pub exec_session_id: Option<ExecSessionId>,
    /// 当前等待中的交互 id（`status == AwaitingInteraction` 时存在）。
    pub interaction_id: Option<InteractionId>,
    /// 工具产生的结构化效果列表。
    pub effects: Vec<ToolEffectView>,
}

/// Ask 单个问题项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskQuestionView {
    /// 问题 id。
    pub id: String,
    /// 题干提示。
    pub prompt: String,
    /// 可选项标签。
    pub options: Vec<String>,
    /// 是否允许自由文本输入。
    pub allow_free_text: bool,
}

/// Ask 单个回答。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskAnswer {
    /// 对应问题 id。
    pub question_id: String,
    /// 用户选择的标签或输入的文本。
    pub answer: String,
}

/// 交互按钮视觉权重。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChoiceTone {
    /// 主按钮。
    Primary,
    /// 次按钮。
    Secondary,
    /// 破坏性/拒绝按钮。
    Destructive,
}

/// 交互可选项的处置动作词表。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InteractionDecisionKind {
    /// 单次批准。
    Approved,
    /// 本会话批准。
    ApprovedForSession,
    /// 拒绝。
    Rejected,
    /// 已回答 Ask。
    Answered,
    /// 忽略/关闭 Ask。
    Dismissed,
}

/// 交互卡片操作按钮视图（由后端直供，前端不猜按钮组）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionChoiceView {
    /// 点击后提交的处置动作。
    pub decision: InteractionDecisionKind,
    /// 按钮展示文案。
    pub label: String,
    /// 按钮补充说明。
    pub description: Option<String>,
    /// 视觉权重。
    pub tone: ChoiceTone,
}

/// 结构化权限审批请求详情（严禁退化为 JSON 字符串）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequestView {
    /// 请求权限的工具名。
    pub tool_name: String,
    /// 动作摘要。
    pub action_summary: Option<String>,
    /// 模型主动请求提权时附带的提权理由。
    pub justification: Option<String>,
    /// 触发审批的原因。
    pub reason: String,
    /// 涉及路径列表。
    pub paths: Vec<String>,
    /// 权限类别（`read` / `write` / `exec` / `net`）。
    pub category: String,
    /// 风险等级（`1..=4`）。
    pub level: u8,
    /// 风险评估（`low` / `medium` / `high`）。
    pub risk: String,
    /// 放行后果说明。
    pub consequence: String,
}

/// 交互请求视图（审批 / Ask）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionView {
    /// 交互 id。
    pub interaction_id: InteractionId,
    /// 所属回合 id。
    pub turn_id: Option<TurnId>,
    /// 交互种类。
    pub kind: InteractionKind,
    /// 若为工具权限审批，关联的工具调用 id。
    pub tool_call_id: Option<ToolCallId>,
    /// 标题。
    pub title: String,
    /// 人类可读引导正文（严禁塞入原始 JSON 字符串）。
    pub body: String,
    /// 扁平选项（快捷审批/单选展示）。
    pub options: Vec<String>,
    /// 结构化操作按钮列表。
    pub choices: Vec<InteractionChoiceView>,
    /// 多题 Ask 结构化问题列表（权限审批为空）。
    pub questions: Vec<AskQuestionView>,
    /// 结构化权限审批详情（`kind == Permission` 时提供）。
    pub permission: Option<PermissionRequestView>,
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
    /// 批准（含授权作用域）。
    Approved {
        /// 批准作用范围。
        scope: PermissionGrantScope,
    },
    /// 拒绝（带理由）。
    Denied {
        /// 理由。
        reason: String,
    },
    /// 已回答（结构化多题答案）。
    Answered {
        /// 各题回答列表。
        answers: Vec<AskAnswer>,
    },
    /// 用户主动关闭/忽略 Ask。
    Dismissed,
    /// 超时/系统取消。
    Abandoned,
}

/// 执行会话停止发起原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecStopReasonView {
    /// 用户/控制面中断。
    Interrupt,
    /// 强行终止。
    Terminate,
    /// 回合取消连带停止。
    TurnCancel,
    /// 超时。
    Timeout,
    /// 守护进程关闭。
    Shutdown,
}

/// 执行会话取消原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecCancelReasonView {
    /// 中断。
    Interrupt,
    /// 终止。
    Terminate,
    /// 回合取消。
    TurnCancel,
    /// 停机。
    Shutdown,
}

/// 执行会话异常中断原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecInterruptReasonView {
    /// 守护进程重启。
    DaemonRestart,
    /// 失去执行进程句柄。
    LostExecution,
    /// 升级状态未知。
    UpgradeStateUnknown,
}

/// 执行会话终态结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecOutcomeView {
    /// 进程正常/非零退出。
    Exited {
        /// 退出码。
        exit_code: i32,
    },
    /// 被取消。
    Cancelled {
        /// 取消原因。
        reason: ExecCancelReasonView,
        /// 可选 OS 退出码。
        exit_code: Option<i32>,
    },
    /// 超时。
    TimedOut {
        /// 可选 OS 退出码。
        exit_code: Option<i32>,
    },
    /// 被中断。
    Interrupted {
        /// 中断原因。
        reason: ExecInterruptReasonView,
    },
}

/// 执行会话状态机。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecSessionStateView {
    /// 运行中。
    Running,
    /// 正在停止。
    Stopping {
        /// 停止原因。
        reason: ExecStopReasonView,
    },
    /// 已结束。
    Finished {
        /// 终态结果。
        outcome: ExecOutcomeView,
    },
}

/// 执行会话视图（V3 形状：状态机 + 尾窗预览 + 水位 + 全文引用）。
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
    /// 权威状态机。
    pub state: ExecSessionStateView,
    /// 宿主会话 id。
    pub owner_session_id: SessionId,
    /// 发起回合 id（历史缺省或未关联时为 `None`）。
    pub origin_turn_id: Option<TurnId>,
    /// 发起工具调用 id（历史缺省或未关联时为 `None`）。
    pub origin_call_id: Option<ToolCallId>,
    /// 执行工作目录。
    pub cwd: Option<String>,
    /// 开始时刻（毫秒）。
    pub started_at_ms: u64,
    /// 结束时刻（毫秒）。
    pub finished_at_ms: Option<u64>,
    /// UTF-8 边界对齐的连续尾窗预览（≤ 16 KiB）。
    pub output_preview: String,
    /// 尾窗起始字节偏移。
    pub preview_start_offset: u64,
    /// 捕获是否完整。
    pub output_complete: bool,
    /// 因捕获上限省略的字节数。
    pub capture_omitted_bytes: u64,
    /// 流背压丢弃的字节数。
    pub stream_dropped_bytes: u64,
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
        /// 触发该回合的用户输入 id（如有）。
        input_id: Option<InputId>,
        /// 开始时刻（毫秒）。
        started_at_ms: u64,
    },
    /// 回合执行子阶段变更。
    TurnPhaseChanged {
        /// 会话 id。
        session_id: SessionId,
        /// 回合 id。
        turn_id: TurnId,
        /// 新子阶段。
        phase: TurnPhase,
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
    /// 上游 Provider 重试通知（不推进序列阻塞主状态，仅透传重试提示）。
    ProviderRetrying {
        /// 会话 id。
        session_id: SessionId,
        /// 当前尝试次数。
        attempt: u32,
        /// 最大尝试次数。
        max_attempts: u32,
        /// 等待退避毫秒。
        delay_ms: u64,
        /// 原因摘要。
        reason: String,
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
    /// 时间线新块落库通知（含权威 `ord`）。
    TimelineAppended {
        /// 会话 id。
        session_id: SessionId,
        /// 新落库的块视图。
        block: BlockView,
    },
    /// 时间线按回合回滚/截断（触发 `timeline_epoch` 递增）。
    TimelineTruncated {
        /// 会话 id。
        session_id: SessionId,
        /// 截断起点回合。
        from_turn_id: TurnId,
        /// 被移除回合的权威集合（用于整枝移除前端回合/工具/交互）。
        removed_turn_ids: Vec<TurnId>,
        /// 截断后新代际。
        timeline_epoch: u64,
    },
    /// 工具调用状态变更。
    ToolCallUpdated {
        /// 会话 id。
        session_id: SessionId,
        /// 调用视图。
        call: ToolCallView,
    },
    /// 工具参数流式增量（`Replaceable`；`delta` 为增量，`lines_*`/`target_files`/`args_complete` 为最新估算快照）。
    ToolArgumentsDelta {
        /// 会话 id。
        session_id: SessionId,
        /// 调用 id。
        tool_call_id: ToolCallId,
        /// 工具名。
        tool_name: String,
        /// JSON 片段增量。
        delta: String,
        /// 估算新增行数（仅写工具）。
        lines_added: Option<u32>,
        /// 估算删除行数（仅写工具）。
        lines_removed: Option<u32>,
        /// 目标文件路径（如 `apply_patch`）。
        target_files: Vec<String>,
        /// 参数文本是否已闭合完成。
        args_complete: bool,
    },
    /// 工具执行流式输出增量（`Replaceable`）。
    ToolOutputDelta {
        /// 会话 id。
        session_id: SessionId,
        /// 调用 id。
        tool_call_id: ToolCallId,
        /// 输出增量。
        text: String,
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
    /// 会话元信息变更（标题/工作区/归档态等）。
    SessionMetaChanged {
        /// 会话 id。
        session_id: SessionId,
        /// 最新标题。
        title: Option<String>,
    },
    /// 子代理/同事状态变更。
    SubagentUpdated {
        /// 宿主会话 id。
        session_id: SessionId,
        /// 子代理摘要。
        subagent: SubagentSummary,
    },
    /// 会话持久目标变更（清空时为 `None`）。
    GoalUpdated {
        /// 会话 id。
        session_id: SessionId,
        /// 当前目标视图。
        goal: Option<GoalView>,
    },
    /// 交接封箱完成（指明封箱会话与后继子会话；在两条线上双发）。
    HandoffSealed {
        /// 当前流所属会话 id。
        session_id: SessionId,
        /// 被封箱的源会话 id。
        sealed_session_id: SessionId,
        /// 后继子会话 id。
        successor_session_id: SessionId,
        /// 产生交接的回合 id。
        turn_id: TurnId,
        /// 交接摘要引用（投影保证 `Inline`）。
        summary: ContentRef,
        /// 封箱完成时刻（毫秒）。
        at_ms: u64,
    },
    /// 用户输入被策略或队列阻塞。
    PromptBlocked {
        /// 会话 id。
        session_id: SessionId,
        /// 阻塞原因文案。
        reason: String,
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
            | Self::TurnPhaseChanged { session_id, .. }
            | Self::TurnFinished { session_id, .. }
            | Self::ProviderRetrying { session_id, .. }
            | Self::AnswerDelta { session_id, .. }
            | Self::ReasoningDelta { session_id, .. }
            | Self::BlockFinal { session_id, .. }
            | Self::TimelineAppended { session_id, .. }
            | Self::TimelineTruncated { session_id, .. }
            | Self::ToolCallUpdated { session_id, .. }
            | Self::ToolArgumentsDelta { session_id, .. }
            | Self::ToolOutputDelta { session_id, .. }
            | Self::InteractionRequested { session_id, .. }
            | Self::InteractionResolved { session_id, .. }
            | Self::ExecSessionUpdated { session_id, .. }
            | Self::ExecOutputDelta { session_id, .. }
            | Self::DriverChanged { session_id, .. }
            | Self::PinnedCompactionChanged { session_id, .. }
            | Self::CatalogChanged { session_id }
            | Self::SessionMetaChanged { session_id, .. }
            | Self::SubagentUpdated { session_id, .. }
            | Self::GoalUpdated { session_id, .. }
            | Self::HandoffSealed { session_id, .. }
            | Self::PromptBlocked { session_id, .. }
            | Self::SessionRemoved { session_id }
            | Self::ResyncRequired { session_id, .. } => session_id,
        }
    }

    /// 该事件的投递分级（**帧属性由事件语义决定**，调用方不得自行挑选）。
    ///
    /// 只有五类高频流式增量是 `Replaceable`；其余一律 `Reliable`——包括
    /// `DriverChanged`（驱动权丢了就是两台设备同时发指令，绝不允许被合帧丢掉）。
    pub fn delivery(&self) -> DeliveryClass {
        match self {
            Self::AnswerDelta { .. }
            | Self::ReasoningDelta { .. }
            | Self::ExecOutputDelta { .. }
            | Self::ToolArgumentsDelta { .. }
            | Self::ToolOutputDelta { .. } => DeliveryClass::Replaceable,
            _ => DeliveryClass::Reliable,
        }
    }
}
