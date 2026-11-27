//! 全量强类型 RPC（`ADR-0042` 铁律 2："协议一网打尽，零协议外孤岛"）。
//!
//! 十一个域，一个都不许留在协议外：`System` / `Session` / `Command` / `Shell` /
//! `Marketplace` / `Config` / `Account` / `Pairing` / `Gitx` / `Fs` / `ToolCatalog`。
//!
//! ## 三条显式排除（铁律 7）
//!
//! - **不纳入** `InspectModelContext`（跨进程直读模型上下文推迟到 `v5.0` 之后）；
//! - **不加入** `local_readonly_path: Option<PathBuf>` 这类"读穿工作区"的后门字段——
//!   `Fs` 域的范围由会话授权决定，不由客户端指定；
//! - **不带上**后端内部 `opaque` 锚点（客户端拿不到、也不该猜的句柄）。
//!
//! ## 动态参数只有两条出路
//!
//! [`JsonText`]（原始文本，不解释）与 [`bytes::Bytes`]（裸字节）。
//! 没有第三种选择，也就没有"协议里塞裸 `Value`"这条路。
//!
//! ## 幂等
//!
//! 每条请求都带 `command_id`：断线重放**必须**返回原回执（`ADR-0042` 铁律 4
//! "断联不杀进程与幂等回执"）。校验与去重是服务端的责任，契约只负责**要求**
//! 它存在且不可省略。

use bytes::Bytes;
use qaqh_core_vocab::{
    ContentRef, ExecSessionId, InputId, PermissionGrantScope, SessionId, ToolCatalog,
};
use serde::{Deserialize, Serialize};

use crate::account::{AccountRequest, AccountResponse};
use crate::config::{ConfigRequest, ConfigResponse};
use crate::session::{SessionRequest, SessionResponse};
use crate::stream::{
    AskAnswer, ExecSessionSnapshot, ImageRefView, SessionEvent, ToolCallStatus, ToolCallView,
};
use crate::workspace::{WorkspaceRequest, WorkspaceResponse};

/// 控制面请求信封（`command_id` 是幂等键，**不可省略**）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlCall {
    /// 幂等键：同 `command_id` 重放必须返回原回执。
    pub command_id: String,
    /// 请求体。
    pub request: ControlRequest,
}

/// 控制面回执信封。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlReply {
    /// 与请求相同的幂等键。
    pub command_id: String,
    /// 回执体（失败也走这里，用 [`ControlResponse::Error`]）。
    pub response: ControlResponse,
}

/// RPC 错误码（机器可读，前端据此决定重试/提示/回滚）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ErrorCode {
    /// 参数不合法。
    BadRequest,
    /// 未认证。
    Unauthenticated,
    /// 无权限（含跨会话越权）。
    PermissionDenied,
    /// 目标不存在。
    NotFound,
    /// 游标/代际已失效（翻页必须重取快照）。
    StaleCursor,
    /// 状态冲突（并发写、重复定义）。
    Conflict,
    /// 限流。
    RateLimited,
    /// 暂时不可用（可重试）。
    Unavailable,
    /// 内部错误（不可重试）。
    Internal,
}

/// RPC 错误体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    /// 错误码。
    pub code: ErrorCode,
    /// 人类可读说明（不得包含可用于枚举的敏感细节）。
    pub message: String,
    /// 是否建议重试。
    pub retryable: bool,
}

/// 控制面请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlRequest {
    /// 系统域。
    System(SystemRequest),
    /// 会话域。
    Session(SessionRequest),
    /// 命令域（用户输入 → 会话）。
    Command(CommandRequest),
    /// Shell 域。
    Shell(ShellRequest),
    /// Marketplace 域。
    Marketplace(MarketplaceRequest),
    /// 配置域。
    Config(ConfigRequest),
    /// 账号域。
    Account(AccountRequest),
    /// 配对域。
    Pairing(PairingRequest),
    /// Gitx 域。
    Gitx(GitxRequest),
    /// 文件系统域（范围由会话授权决定）。
    Fs(FsRequest),
    /// 工具目录域。
    ToolCatalog(ToolCatalogRequest),
    /// 工作区分组域。
    Workspace(WorkspaceRequest),
}

/// 控制面回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlResponse {
    /// 系统域。
    System(SystemResponse),
    /// 会话域。
    Session(SessionResponse),
    /// 命令域。
    Command(CommandResponse),
    /// Shell 域。
    Shell(ShellResponse),
    /// Marketplace 域。
    Marketplace(MarketplaceResponse),
    /// 配置域。
    Config(ConfigResponse),
    /// 账号域。
    Account(AccountResponse),
    /// 配对域。
    Pairing(PairingResponse),
    /// Gitx 域。
    Gitx(GitxResponse),
    /// 文件系统域。
    Fs(FsResponse),
    /// 工具目录域。
    ToolCatalog(ToolCatalogResponse),
    /// 工作区分组域。
    Workspace(WorkspaceResponse),
    /// 失败（任何域共用一个错误通道，前端不需要逐域写错误处理）。
    Error(RpcError),
}

// ── System ──

/// 系统域请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SystemRequest {
    /// 存活探针。
    Health,
    /// 活动面板（在线客户端、活跃会话数）。
    Activity,
    /// 版本与协议指纹。
    Version,
    /// 停止守护进程。
    Stop {
        /// `true` = 先等活跃回合收口；`false` = 立即。
        graceful: bool,
    },
    /// 空闲才停（不打断任何活跃回合）。
    StopIfIdle,
}

/// 系统域回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SystemResponse {
    /// 探针成功。
    Health {
        /// 已运行毫秒。
        uptime_ms: u64,
        /// 在线客户端数。
        peers: u32,
    },
    /// 活动快照。
    Activity {
        /// 活跃会话数。
        active_sessions: u32,
        /// 正在执行的工具调用数。
        running_tool_calls: u32,
    },
    /// 版本信息。
    Version {
        /// 构建 id。
        build_id: String,
        /// 协议修订号。
        protocol_rev: u32,
    },
    /// 停机受理（若被拒，`reason` 说明谁在挡）。
    StopAck {
        /// 是否受理。
        accepted: bool,
        /// 拒绝原因。
        reason: Option<String>,
    },
}

// Session 域已拆分到独立模块 `session`，在根 crate 统一重导出。

// ── Command ──

/// 命令域请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandRequest {
    /// 投递用户输入（驱动权之外发 → `PermissionDenied`）。
    SendPrompt {
        /// 会话 id。
        session_id: SessionId,
        /// 输入 id（客户端生成，用于本地回声去重）。
        input_id: InputId,
        /// 文本。
        text: String,
        /// 文本附件引用。
        attachments: Vec<ContentRef>,
        /// 多模态图片引用（必须先经 `BlobTransfer::ClientUpload` 上传）。
        images: Vec<ImageRefView>,
    },
    /// 取消当前回合。
    CancelTurn {
        /// 会话 id。
        session_id: SessionId,
    },
    /// 响应交互（审批 / Ask）。
    RespondInteraction {
        /// 会话 id。
        session_id: SessionId,
        /// 交互 id。
        interaction_id: qaqh_core_vocab::InteractionId,
        /// 处置结果。
        resolution: CommandInteractionResolution,
    },
    /// 中断正在运行的 `exec` 执行会话（发送 SIGINT/Ctrl-C）。
    ExecInterrupt {
        /// 执行会话 id。
        exec_session_id: ExecSessionId,
    },
    /// 强行终止正在运行的 `exec` 执行会话。
    ExecTerminate {
        /// 执行会话 id。
        exec_session_id: ExecSessionId,
    },
}

/// 交互处置（覆盖审批作用域批准/拒绝、多题 Ask 回答与忽略关闭）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandInteractionResolution {
    /// 批准（携带授予作用域：`Once` / `Session` / `Persistent`）。
    Approve {
        /// 批准作用域。
        scope: PermissionGrantScope,
    },
    /// 拒绝（带理由）。
    Reject {
        /// 拒绝理由。
        text: String,
    },
    /// Ask 多题回答。
    Answer {
        /// 各题回答列表。
        answers: Vec<AskAnswer>,
    },
    /// 忽略/关闭 Ask 交互。
    Dismiss,
}

/// 命令域回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandResponse {
    /// 已入队（回合真正开始由 `TurnStarted` 事件宣告）。
    Accepted {
        /// 回显的输入 id。
        input_id: InputId,
    },
    /// 取消或 exec 控制指令已受理。
    CancelAccepted,
    /// 交互已处置。
    InteractionAccepted,
}

// ── Shell ──

/// Shell 域请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShellRequest {
    /// 打开一个交互 PTY（**独立并发句柄表**，不碰中央会话锁）。
    ///
    /// 既可显式绑定某会话（`Some(session_id)`），也可由桌面终端坞全局打开（`None`）。
    Open {
        /// 可选关联会话 id（`None` = 全局终端坞）。
        session_id: Option<SessionId>,
        /// 列数。
        cols: u16,
        /// 行数。
        rows: u16,
        /// 工作目录（`None` = 关联会话工作区或默认目录）。
        cwd: Option<String>,
        /// 可选指定 shell 程序路径/档案（`None` = 宿主默认）。
        shell: Option<String>,
    },
    /// 写入字节（原始 PTY 字节，不是 UTF-8 文本）。
    Write {
        /// shell id。
        shell_id: String,
        /// 字节。
        bytes: Bytes,
    },
    /// 调整窗口。
    Resize {
        /// shell id。
        shell_id: String,
        /// 列数。
        cols: u16,
        /// 行数。
        rows: u16,
    },
    /// 关闭。
    Close {
        /// shell id。
        shell_id: String,
    },
    /// 列出 PTY 句柄（`Some(session_id)` 按会话过滤；`None` 列当前设备全部句柄）。
    List {
        /// 可选会话过滤键。
        session_id: Option<SessionId>,
    },
}

/// Shell 视图（对齐前端终端坞标签与状态栏信息）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellView {
    /// shell id。
    pub shell_id: String,
    /// 所属子进程 PID。
    pub pid: u32,
    /// Shell 可执行程序名称/路径。
    pub shell: String,
    /// 初始工作目录。
    pub cwd: String,
    /// 当前列数。
    pub cols: u16,
    /// 当前行数。
    pub rows: u16,
    /// 进程已产生的总字节数（客户端续读偏移的权威水位）。
    pub output_offset: u64,
    /// 是否已结束。
    pub exited: bool,
}

/// Shell 域回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShellResponse {
    /// 已打开。
    Opened(ShellView),
    /// 写入已接受（回显走 `ShellPty` 流）。
    Written {
        /// 实际写入字节数。
        bytes_written: u64,
    },
    /// 已调整。
    Resized,
    /// 已关闭。
    Closed,
    /// 列表。
    Shells(Vec<ShellView>),
}

// ── Marketplace ──

/// Marketplace 域请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarketplaceRequest {
    /// 检索。
    Search {
        /// 关键词。
        query: String,
        /// 结果上限。
        limit: u32,
    },
    /// 已安装清单。
    Installed,
    /// 安装。
    Install {
        /// 条目 id。
        id: String,
        /// 版本（`None` = 最新）。
        version: Option<String>,
    },
    /// 卸载。
    Uninstall {
        /// 条目 id。
        id: String,
    },
}

/// Marketplace 条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketplaceItem {
    /// 条目 id。
    pub id: String,
    /// 名称。
    pub name: String,
    /// 版本。
    pub version: String,
    /// 简述。
    pub description: String,
    /// 是否已安装。
    pub installed: bool,
}

/// Marketplace 域回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarketplaceResponse {
    /// 检索/已安装结果。
    Items(Vec<MarketplaceItem>),
    /// 安装/卸载受理。
    Ack,
}

// Config 与 Account 域已拆分到独立模块 `config` 与 `account`，在根 crate 统一重导出。

// ── Pairing ──

/// 配对域请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairingRequest {
    /// 生成邀请码。
    CreateInvite {
        /// 有效期（毫秒）。
        ttl_ms: u64,
    },
    /// 兑换邀请码。
    Redeem {
        /// 邀请码。
        code: String,
        /// 客户端种类。
        client_kind: crate::handshake::ClientKind,
    },
    /// 列出已配对设备。
    ListDevices,
    /// 吊销设备（**即踢流**：`ADR-0042` 铁律 5）。
    RevokeDevice {
        /// 设备 id。
        device_id: String,
    },
}

/// 已配对设备视图。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceView {
    /// 设备 id。
    pub device_id: String,
    /// 展示名。
    pub label: String,
    /// 配对时刻（毫秒）。
    pub paired_at_ms: u64,
    /// 是否在线。
    pub online: bool,
}

/// 配对域回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairingResponse {
    /// 邀请码。
    Invite {
        /// 邀请码。
        code: String,
        /// 到期时刻（毫秒）。
        expires_at_ms: u64,
    },
    /// 兑换成功（令牌只在这一次下发，服务端不重复展示）。
    Redeemed {
        /// 设备 id。
        device_id: String,
        /// 设备令牌。
        token: String,
    },
    /// 设备列表。
    Devices(Vec<DeviceView>),
    /// 已吊销。
    Revoked,
}

// ── Gitx ──

/// Gitx 域请求（`ADR-0041` worktree 面）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitxRequest {
    /// 工作树状态。
    Status {
        /// 会话 id（worktree 与会话一一绑定）。
        session_id: SessionId,
    },
    /// 取差异。
    Diff {
        /// 会话 id。
        session_id: SessionId,
        /// 相对路径（`None` = 全树）。
        path: Option<String>,
    },
    /// 提交。
    Commit {
        /// 会话 id。
        session_id: SessionId,
        /// 提交信息。
        message: String,
    },
    /// 列出工作树。
    Worktrees,
}

/// 工作树状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitStatus {
    /// 分支名。
    pub branch: String,
    /// 是否有未提交改动。
    pub dirty: bool,
    /// 领先上游提交数。
    pub ahead: u32,
    /// 落后上游提交数。
    pub behind: u32,
}

/// 工作树视图。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeView {
    /// 路径。
    pub path: String,
    /// 分支。
    pub branch: String,
    /// 绑定的会话（无绑定为 `None`）。
    pub session_id: Option<SessionId>,
}

/// Gitx 域回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitxResponse {
    /// 状态。
    Status(GitStatus),
    /// 差异（大差异走引用）。
    Diff(ContentRef),
    /// 提交完成（新提交哈希 hex）。
    Committed {
        /// 提交哈希（hex，人类面）。
        commit: String,
    },
    /// 工作树列表。
    Worktrees(Vec<WorktreeView>),
}

// ── Fs ──

/// 文件系统域请求（**范围由所属会话授权决定，客户端无权指定越界路径**）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FsRequest {
    /// 元信息。
    Stat {
        /// 会话 id。
        session_id: SessionId,
        /// 路径（会话授权根内的相对路径）。
        path: String,
    },
    /// 列目录。
    List {
        /// 会话 id。
        session_id: SessionId,
        /// 目录路径。
        path: String,
    },
    /// 区间读（大文件分段，**不整读**）。
    ReadRange {
        /// 会话 id。
        session_id: SessionId,
        /// 路径。
        path: String,
        /// 起始偏移。
        offset: u64,
        /// 读取长度上限。
        len: u32,
    },
    /// 整写（原子替换）。
    Write {
        /// 会话 id。
        session_id: SessionId,
        /// 路径。
        path: String,
        /// 字节。
        bytes: Bytes,
    },
}

/// 文件种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FsKind {
    /// 普通文件。
    File,
    /// 目录。
    Directory,
    /// 符号链接。
    Symlink,
}

/// 文件元信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsEntry {
    /// 路径。
    pub path: String,
    /// 种类。
    pub kind: FsKind,
    /// 字节长度（目录为 `0`）。
    pub len: u64,
    /// 修改时刻（毫秒）。
    pub modified_at_ms: u64,
}

/// Fs 域回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FsResponse {
    /// 元信息。
    Stat(FsEntry),
    /// 目录项。
    Entries(Vec<FsEntry>),
    /// 区间读结果。
    Bytes {
        /// 起始偏移。
        offset: u64,
        /// 字节（可能短于请求长度＝到文件尾）。
        bytes: Bytes,
    },
    /// 写入完成。
    Written {
        /// 写入字节数。
        bytes_written: u64,
    },
}

// ── ToolCatalog ──

/// 工具目录域请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolCatalogRequest {
    /// 取某会话的工具目录快照（前端工具面板唯一真相源）。
    Get {
        /// 会话 id。
        session_id: SessionId,
    },
}

/// 工具目录域回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolCatalogResponse {
    /// 目录快照（幂等整体替换）。
    Catalog(Box<ToolCatalog>),
}

/// 会话事件的单流序号与代际校验（`SessionEvents` 流的第一帧必带）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamAttach {
    /// 流身份。
    pub stream: crate::stream::StreamKind,
    /// 服务端当前事件序号。
    pub seq: u64,
    /// 服务端当前时间线代际。
    pub timeline_epoch: u64,
}

/// 把工具调用状态与执行会话视图的公共字段收在一处，避免各处重复定义。
impl ToolCallView {
    /// 是否终态（终态不可回退）。
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            ToolCallStatus::Succeeded
                | ToolCallStatus::Failed
                | ToolCallStatus::Cancelled
                | ToolCallStatus::Indeterminate
        )
    }
}

/// 执行会话视图与事件里的同名字段必须同源（此函数是唯一投影入口）。
impl ExecSessionSnapshot {
    /// 从既有事件里取出执行会话视图（避免逐字段复制出错）。
    pub fn of(event: &SessionEvent) -> Option<&Self> {
        match event {
            SessionEvent::ExecSessionUpdated { exec, .. } => Some(exec),
            _ => None,
        }
    }

    /// 执行会话 id。
    pub fn id(&self) -> &ExecSessionId {
        &self.exec_session_id
    }
}
