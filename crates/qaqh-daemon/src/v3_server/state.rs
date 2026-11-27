//! 服务端全局状态与配置（模块入口：各子表已按锁边界拆出）。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use qaqh_core_vocab::SessionId;
use tokio::sync::watch;

use super::blob_store::BlobStore;
use super::bus::{DEFAULT_RING_CAPACITY, DEFAULT_SUBSCRIBER_QUEUE};
use super::identity::{DeviceKey, IdentityTable};
use super::lock;
use super::receipts::ReceiptTable;
use super::session_runtime::SessionRuntime;
use super::shell_table::ShellTable;

/// 服务端配置（骨架期的全部可调项）。
#[derive(Debug, Clone)]
pub(crate) struct ServerConfig {
    /// 构建 id（与 `daemon.endpoint.build_id` 同源）。
    pub(crate) build_id: String,
    /// 服务端实例 id（客户端据此确认"我说的是哪个 daemon"）。
    pub(crate) server_instance_id: String,
    /// 发布渠道。
    pub(crate) channel: String,
    /// 回放环容量（帧）。
    pub(crate) ring_capacity: usize,
    /// 订阅队列容量（帧）。
    pub(crate) subscriber_queue: usize,
    /// 内联内容预算（字节）：超过即外置为 blob。
    pub(crate) inline_budget_bytes: usize,
    /// blob 分片大小（字节）。
    pub(crate) blob_chunk_bytes: usize,
    /// exec 输出增量分片大小（字节）。
    pub(crate) exec_delta_chunk_bytes: usize,
    /// 时间线块上限（骨架期内存护栏）。
    pub(crate) max_timeline_blocks: usize,
    /// shell 回放历史上限（字节）。
    pub(crate) shell_history_bytes: usize,
    /// shell PTY 流队列容量（块）。
    pub(crate) shell_queue_chunks: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            build_id: "v3-skeleton".to_string(),
            server_instance_id: "v3-skeleton-instance".to_string(),
            channel: "dev".to_string(),
            ring_capacity: DEFAULT_RING_CAPACITY,
            subscriber_queue: DEFAULT_SUBSCRIBER_QUEUE,
            // 64 KiB：一屏终端/一次工具输出摘要远小于它，模型面泄洪闸是 256 KiB，
            // 这里更小是故意的——V3 的"内联"只服务首屏，其余一律走引用。
            inline_budget_bytes: 64 * 1024,
            blob_chunk_bytes: 64 * 1024,
            exec_delta_chunk_bytes: 4 * 1024,
            max_timeline_blocks: 50_000,
            shell_history_bytes: 64 * 1024,
            shell_queue_chunks: 256,
        }
    }
}

/// 服务端全局状态（仅在建/查会话时短暂持锁，热路径不跨 await）。
pub(crate) struct ServerState {
    pub(crate) config: ServerConfig,
    pub(crate) identity: IdentityTable,
    pub(crate) shells: ShellTable,
    pub(crate) receipts: ReceiptTable,
    pub(crate) blobs: BlobStore,
    pub(crate) started_at: Instant,
    pub(crate) peers: AtomicU64,
    pub(crate) shutdown: watch::Sender<bool>,
    /// 进程内大输出的推送广播（AgentLoop 的位置）。
    pub(crate) push_bus: tokio::sync::broadcast::Sender<crate::v3_server::facade::BlobPush>,
    sessions: Mutex<BTreeMap<SessionId, Arc<SessionRuntime>>>,
    next_session: AtomicU64,
}

impl ServerState {
    /// 建状态。
    pub(crate) fn new(config: ServerConfig, os_user: impl Into<String>) -> Arc<Self> {
        let (shutdown, _) = watch::channel(false);
        let (push_bus, _) = tokio::sync::broadcast::channel(64);
        Arc::new(Self {
            config,
            identity: IdentityTable::new(os_user),
            shells: ShellTable::new(),
            receipts: ReceiptTable::new(),
            blobs: BlobStore::new(),
            started_at: Instant::now(),
            peers: AtomicU64::new(0),
            shutdown,
            push_bus,
            sessions: Mutex::new(BTreeMap::new()),
            next_session: AtomicU64::new(0),
        })
    }

    /// 建一个会话（创建者设备获得访问权）。
    pub(crate) fn create_session(&self, owner: &DeviceKey) -> Arc<SessionRuntime> {
        let ordinal = self.next_session.fetch_add(1, Ordering::SeqCst) + 1;
        let id = SessionId::new(format!("session-{ordinal:04}")).expect("session id 形状固定");
        let runtime = Arc::new(SessionRuntime::new(id.clone(), owner, &self.config));
        lock(&self.sessions).insert(id, Arc::clone(&runtime));
        runtime
    }

    /// 注册既有业务会话（创建者设备获得访问权）。
    pub(crate) fn register_session(&self, id: SessionId, owner: &DeviceKey) -> Arc<SessionRuntime> {
        let runtime = Arc::new(SessionRuntime::new(id.clone(), owner, &self.config));
        lock(&self.sessions).insert(id, Arc::clone(&runtime));
        runtime
    }

    /// 查会话。
    pub(crate) fn session(&self, id: &SessionId) -> Option<Arc<SessionRuntime>> {
        lock(&self.sessions).get(id).cloned()
    }

    /// 全部会话（按 id 升序）。
    pub(crate) fn sessions(&self) -> Vec<Arc<SessionRuntime>> {
        lock(&self.sessions).values().cloned().collect()
    }

    /// 删除会话。
    pub(crate) fn remove_session(&self, id: &SessionId) -> bool {
        lock(&self.sessions).remove(id).is_some()
    }

    /// 已运行毫秒。
    pub(crate) fn uptime_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    /// 请求停机（`graceful` 只影响上层收尾策略，骨架期用同一信号）。
    pub(crate) fn request_shutdown(&self, graceful: bool) -> bool {
        self.shutdown.send_replace(true);
        log::info!("v3_server: 停机请求已受理（graceful={graceful}）");
        true
    }

    /// 停机信号是否已置位。
    pub(crate) fn shutdown_requested(&self) -> bool {
        *self.shutdown.borrow()
    }
}
