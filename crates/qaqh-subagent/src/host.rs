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
