//! 子代理宿主直连接口（Knife-1 step-2 收尾）。
//!
//! 主 session 与子代理 loop 均已 in-process（PR #17/#19/#21），工具 handler
//! 就在 daemon 进程内执行，因此 `spawn_subagent` 无需再经 daemon HTTP/SSE
//! 回连自己——直接调用宿主（daemon 进程内 AgentRegistry + RingingHub +
//! SessionManager）提供的 actor 句柄即可。
//!
//! 本 trait 只依赖 domain/ringing 规范 wire 类型（`RingingCommand`、
//! `ContentRef`、`EventBatch` 等，PR-4-2 起直接取自 qaqh-domain /
//! qaqh-ringing），不引用 qaqh-runtime 任何类型，保证依赖方向
//! `runtime → subagent` 不回环。
//!
//! 安装：daemon 装配（`QaqhService::init`）时调用 [`install_host`]；工具
//! handler 通过 [`host`] 探测。宿主不可用（非 daemon 进程 / 单元测试）即
//! spawn 失败——legacy HTTP/SSE 回连降级路径已随 PR-4-2 删除。

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use qaqh_ringing::RingingCommand;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 大内容引用与事件批次（宿主实现 `download_content` / 事件流需要；与 trait
/// 签名同类型），re-export 供 qaqh-runtime 消费。
pub use qaqh_domain::ContentRef;
// PR-4-2：`EventBatch` 即 ringing 规范类型（此前经 qaqh-client 转手）。
pub use qaqh_ringing::RingingEventBatch as EventBatch;

/// Result of creating an in-process subagent actor.
///
/// The actor is not yet running its task. The caller must durably record the
/// `SubagentSpawned` edge before invoking [`SubagentHost::start_subagent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnedSubagent {
    pub seed: String,
    pub child_session_id: String,
    pub parent_agent_path: String,
    pub child_agent_path: String,
}

/// Explicit logical status exposed by `list_agents`.
///
/// `Completed` describes the last terminal lifecycle fact, while
/// [`ListedAgentResidency::Unloaded`] only means that no worker is resident.
/// The two values must not be collapsed into a single "closed" state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ListedAgentStatus {
    #[default]
    PendingInit,
    Running,
    WaitingUser,
    Interrupted,
    Completed,
    Errored,
    Shutdown,
    NotFound,
}

/// Whether an agent worker is resident in the daemon.
///
/// This is an explicit registry state, not a `SessionManager` or process-table
/// guess. Unloaded agents remain listable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ListedAgentResidency {
    Loaded,
    #[default]
    Unloaded,
}

/// Logical agent metadata returned by `list_agents`.
///
/// This is intentionally independent of runtime handles and session storage:
/// unloaded agents remain listable as long as their canonical graph metadata
/// exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ListedAgent {
    pub root_session_id: String,
    pub agent_id: String,
    pub agent_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub status: ListedAgentStatus,
    pub residency: ListedAgentResidency,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct SpawnSubagentRequest<'a> {
    pub parent_session_id: &'a str,
    pub requested_name: &'a str,
    pub tools: &'a [String],
    pub model: Option<&'a str>,
    pub base_url: Option<&'a str>,
    pub max_tokens: Option<u32>,
    pub workspace: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct StartSubagentRequest<'a> {
    pub seed: &'a str,
    pub child_session_id: &'a str,
    pub name: &'a str,
    pub task_text: &'a str,
    pub timeout_secs: u64,
    pub parent_session_id: &'a str,
    pub parent_call_id: &'a str,
    pub process_id: u32,
    pub inter_agent: Option<qaqh_domain::InterAgentEnvelope>,
}

#[derive(Debug, Clone)]
pub struct SendAgentMessageRequest<'a> {
    pub caller_session_id: &'a str,
    pub target: &'a str,
    pub text: &'a str,
    pub delivery: qaqh_domain::InterAgentDelivery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentAgentMessage {
    pub message_id: String,
    pub recipient: String,
    pub delivery: qaqh_domain::InterAgentDelivery,
}

#[derive(Clone)]
pub struct WaitAgentRequest<'a> {
    pub caller_session_id: &'a str,
    pub timeout: Duration,
    /// Cooperative cancellation probe. The host polls this between reads so a
    /// cancelled turn does not remain blocked until the wait deadline.
    pub should_cancel: &'a dyn Fn() -> bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitAgentOutcome {
    Activity { activity_fact_seq: u64 },
    TimedOut { activity_fact_seq: u64 },
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct InterruptAgentRequest<'a> {
    pub caller_session_id: &'a str,
    pub target: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterruptedAgent {
    pub recipient: String,
    pub previous_status: String,
}

#[derive(Debug, Clone)]
pub struct ArmSubagentCollectorRequest<'a> {
    pub seed: &'a str,
    pub child_session_id: &'a str,
    pub name: &'a str,
    pub parent_session_id: &'a str,
    pub parent_call_id: &'a str,
    pub timeout_secs: u64,
    pub root_session_id: &'a str,
    pub parent_agent_path: &'a str,
    pub child_agent_path: &'a str,
}

/// Task board task projection exposed to tools and the daemon wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskBoardTask {
    pub task_id: String,
    pub title: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub claim_epoch: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<TaskBoardArtifact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_ref: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskBoardArtifact {
    pub content_id: String,
    pub media_type: String,
    pub added_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BoardChannel {
    pub channel_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub created_by: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BoardThread {
    pub thread_id: String,
    pub channel_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub created_by: String,
    pub created_at_ms: i64,
    pub post_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BoardPost {
    pub post_id: String,
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub author: String,
    pub body: String,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BoardSubscriptionTarget {
    Channel { channel_id: String },
    Thread { thread_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BoardSubscription {
    pub target: BoardSubscriptionTarget,
    pub subscriber: String,
    pub subscribed: bool,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BoardSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board_id: Option<String>,
    pub revision: u64,
    pub last_fact_seq: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<BoardChannel>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub threads: Vec<BoardThread>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub posts: Vec<BoardPost>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscriptions: Vec<BoardSubscription>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BoardNotificationSkip {
    pub agent: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BoardPostOutcome {
    pub post: BoardPost,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notified: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<BoardNotificationSkip>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskClaimAction {
    Claim,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskUpdateAction {
    AddDependency,
    AttachArtifact,
    SetAcceptance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskCloseAction {
    Complete,
    Close,
    Cancel,
}

#[derive(Debug, Clone)]
pub struct TaskCreateRequest<'a> {
    pub caller_session_id: &'a str,
    pub title: &'a str,
    pub description_ref: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct TaskClaimRequest<'a> {
    pub caller_session_id: &'a str,
    pub task_id: &'a str,
    pub action: TaskClaimAction,
    pub reason: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct TaskUpdateRequest<'a> {
    pub caller_session_id: &'a str,
    pub task_id: &'a str,
    pub action: TaskUpdateAction,
    pub depends_on: Option<&'a str>,
    pub artifact_ref: Option<&'a str>,
    pub media_type: Option<&'a str>,
    pub acceptance: Option<&'a [String]>,
}

#[derive(Debug, Clone)]
pub struct TaskCloseRequest<'a> {
    pub caller_session_id: &'a str,
    pub task_id: &'a str,
    pub action: TaskCloseAction,
    pub result_ref: Option<&'a str>,
    pub reason: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct TaskListRequest<'a> {
    pub caller_session_id: &'a str,
    pub state: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BoardSubscriptionAction {
    Subscribe,
    Unsubscribe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BoardSubscriptionTargetKind {
    Channel,
    Thread,
}

#[derive(Debug, Clone)]
pub struct BoardChannelCreateRequest<'a> {
    pub caller_session_id: &'a str,
    pub name: &'a str,
    pub topic: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct BoardThreadCreateRequest<'a> {
    pub caller_session_id: &'a str,
    pub channel_id: &'a str,
    pub title: &'a str,
    pub task_id: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct BoardPostRequest<'a> {
    pub caller_session_id: &'a str,
    pub thread_id: &'a str,
    pub body: &'a str,
    pub task_id: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct BoardSubscriptionRequest<'a> {
    pub caller_session_id: &'a str,
    pub target_kind: BoardSubscriptionTargetKind,
    pub target_id: &'a str,
    pub action: BoardSubscriptionAction,
}

#[derive(Debug, Clone)]
pub struct BoardListRequest<'a> {
    pub caller_session_id: &'a str,
    pub channel_id: Option<&'a str>,
    pub thread_id: Option<&'a str>,
    pub include_posts: bool,
    pub post_limit: Option<usize>,
}

/// In-process task board host. The team aggregate is keyed by the caller's
/// root session id; implementations must reject cross-tree access.
pub trait TaskBoardHost: Send + Sync {
    fn task_create(&self, request: TaskCreateRequest<'_>) -> Result<TaskBoardTask, String>;
    fn task_claim(&self, request: TaskClaimRequest<'_>) -> Result<TaskBoardTask, String>;
    fn task_update(&self, request: TaskUpdateRequest<'_>) -> Result<TaskBoardTask, String>;
    fn task_close(&self, request: TaskCloseRequest<'_>) -> Result<TaskBoardTask, String>;
    fn task_list(&self, request: TaskListRequest<'_>) -> Result<Vec<TaskBoardTask>, String>;
}

static TASK_HOST: OnceLock<Arc<dyn TaskBoardHost>> = OnceLock::new();

/// Install the process-wide task board host. Idempotent: the first host wins.
pub fn install_task_host(host: Arc<dyn TaskBoardHost>) {
    let _ = TASK_HOST.set(host);
}

/// Return the installed task board host, if any.
pub fn task_host() -> Option<Arc<dyn TaskBoardHost>> {
    TASK_HOST.get().cloned()
}

/// In-process message board host. The board aggregate is keyed by the caller's
/// root session id; implementations must reject cross-tree access.
pub trait BoardHost: Send + Sync {
    fn board_channel_create(
        &self,
        request: BoardChannelCreateRequest<'_>,
    ) -> Result<BoardChannel, String>;
    fn board_thread_create(
        &self,
        request: BoardThreadCreateRequest<'_>,
    ) -> Result<BoardThread, String>;
    fn board_post(&self, request: BoardPostRequest<'_>) -> Result<BoardPostOutcome, String>;
    fn board_subscribe(
        &self,
        request: BoardSubscriptionRequest<'_>,
    ) -> Result<BoardSubscription, String>;
    fn board_list(&self, request: BoardListRequest<'_>) -> Result<BoardSnapshot, String>;
}

static BOARD_HOST: OnceLock<Arc<dyn BoardHost>> = OnceLock::new();

/// Install the process-wide message board host. Idempotent: the first host wins.
pub fn install_board_host(host: Arc<dyn BoardHost>) {
    let _ = BOARD_HOST.set(host);
}

/// Return the installed message board host, if any.
pub fn board_host() -> Option<Arc<dyn BoardHost>> {
    BOARD_HOST.get().cloned()
}

/// 进程内子代理宿主演进接口。所有方法都是同步阻塞语义（与工具 worker
/// 线程的 std 线程模型匹配），由 qaqh-runtime 的 `QaqhService` 提供实现。
pub trait SubagentHost: Send + Sync {
    /// 生成新 seed 并在宿主进程内注册一个 in-process 子代理 actor。
    ///
    /// 与 daemon `subagent.spawn` action 等价：继承 workspace（写入
    /// SessionMeta.cwd）、应用 subagent 工具白名单。返回生成的 seed 与
    /// canonical path；本方法不发送任务。
    fn spawn_subagent(&self, request: SpawnSubagentRequest<'_>) -> Result<SpawnedSubagent, String>;

    /// List logical agents at or below `path_prefix` in the caller's root tree.
    ///
    /// Relative prefixes resolve below the caller's `AgentPath`; absolute
    /// prefixes may select any path in the same root tree. Listing never
    /// starts or reloads an agent.
    fn list_agents(
        &self,
        caller_session_id: &str,
        path_prefix: &str,
    ) -> Result<Vec<ListedAgent>, String>;

    /// Deliver the initial task after the caller durably recorded the spawn
    /// edge. Implementations own process registration and result collection.
    fn start_subagent(&self, request: StartSubagentRequest<'_>) -> Result<(), String>;

    /// Deliver a canonical inter-agent message to a loaded target agent.
    fn send_agent_message(
        &self,
        request: SendAgentMessageRequest<'_>,
    ) -> Result<SentAgentMessage, String>;

    /// Wait for the caller's mailbox watermark to advance.
    ///
    /// This deliberately returns no message body. The runtime merges accepted
    /// queue-only communications at the next turn/lap boundary.
    fn wait_agent(&self, request: WaitAgentRequest<'_>) -> Result<WaitAgentOutcome, String>;

    /// Interrupt the target agent's current turn without unloading or deleting
    /// its logical identity. Root and self interrupts are rejected.
    fn interrupt_agent(
        &self,
        request: InterruptAgentRequest<'_>,
    ) -> Result<InterruptedAgent, String>;

    /// Roll back a child whose canonical spawn edge could not be committed.
    ///
    /// Unlike [`Self::abort_subagent`], this also removes the logical catalog
    /// registration because the edge never became authoritative.
    fn rollback_subagent(&self, seed: &str, child_session_id: &str, process_id: u32);

    /// Close a child after its canonical spawn edge was committed.
    fn abort_subagent(&self, seed: &str, process_id: u32);

    /// 进程内直接向指定 seed 的 actor 命令队列发送一条 Ringing 命令
    /// （等价 HTTP attach + send_command，但进程内无 lease/owns 语义）。
    fn send_ringing(&self, seed: &str, command: RingingCommand) -> Result<(), String>;

    /// 订阅某 seed 的实时事件批次流（等价 SSE 单条连接；宿主内部按 seed
    /// 过滤后以 `EventBatch` 聚合）。返回 std mpsc receiver，供 std 线程消费。
    fn subscribe(&self, seed: &str) -> std::sync::mpsc::Receiver<EventBatch>;

    /// 进程内读取外置大内容（等价 HTTP `download_content`）。
    fn download_content(&self, seed: &str, reference: &ContentRef) -> Result<Vec<u8>, String>;

    /// 进程内关闭子代理 worker（等价 HTTP `SessionClose`）。
    fn close(&self, seed: &str) -> Result<(), String>;
}

/// 进程级宿主安装位。多个 actor 并发调用 `host()` 读取；daemon 只安装一次。
static HOST: OnceLock<Mutex<Option<Arc<dyn SubagentHost>>>> = OnceLock::new();

/// 安装进程内宿主。重复安装被忽略（保留首次）。
pub fn install_host(host: Arc<dyn SubagentHost>) {
    let slot = HOST.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(host);
        log::info!("[SUBAGENT] in-process subagent host installed");
    }
}

/// 读取已安装的宿主；未安装返回 `None`（调用方回退 HTTP/SSE 路径）。
pub fn host() -> Option<Arc<dyn SubagentHost>> {
    let slot = HOST.get()?;
    let guard = slot.lock().ok()?;
    guard.clone()
}
