//! 服务端状态：会话运行时、Shell 独立句柄表、幂等回执表、内容寻址 blob 库。
//!
//! ## 锁的分层（不碰中央 registry 锁）
//!
//! V2 的痛点是所有会话串在一把中央锁上。V3 骨架从一开始就分层：
//!
//! - [`ServerState::sessions`]：**只**在创建/查找会话时短暂持锁，绝不跨 `await`；
//! - [`SessionRuntime`] 内部各自持锁（事件总线、时间线、驱动权各一把）；
//! - [`ShellTable`] 是**完全独立的句柄表**：Shell PTY 刷屏时的热路径一次中央锁都不碰；
//! - [`ReceiptTable`]/[`BlobStore`] 同理独立。
//!
//! ## 内存护栏
//!
//! 骨架期没有 SQLite 兜底，所以每条时间线都有块数上限
//! （[`ServerConfig::max_timeline_blocks`]）：超出即从最老处丢弃。这不是"临时妥协"，
//! 而是骨架期就明确写下来的上界——`v4.7` 接存储层后由存储层承载，不动这里的语义。

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytes::Bytes;
use qaqh_core_vocab::{
    ActivityState, BlockId, ContentHash, ContentRef, ExecSessionId, SessionId, ToolCallId, ToolMode,
    TurnId, TurnTerminal, UsageInfo,
};
use qaqh_ringing_v3::{
    BlockKind, BlockView, CompactionSummary, ControlReply, ErrorCode, RpcError, SessionEvent,
    SessionSnapshot, SessionSummary, ShellView, ToolCallView, TranscriptPage, TurnSummary,
};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};

use super::bus::{DEFAULT_RING_CAPACITY, DEFAULT_SUBSCRIBER_QUEUE, DeltaLedger, EventBus};
use super::error::{not_found, rpc};
use super::identity::{ConnectionIdentity, DeviceKey, DriverKey, IdentityTable, Scope};
use super::lock;
use super::now_ms;

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

/// 会话级计数（守卫断言用；不含业务语义）。
#[derive(Debug, Default)]
pub(crate) struct SessionMetrics {
    /// 已开始的回合数。
    pub(crate) turns_started: AtomicU64,
    /// 已收口的回合数。
    pub(crate) turns_finished: AtomicU64,
    /// 已产生的 exec 会话数。
    pub(crate) exec_sessions: AtomicU64,
    /// 驱动权切换次数。
    pub(crate) driver_switches: AtomicU64,
    /// 交互请求数。
    pub(crate) interactions_requested: AtomicU64,
}

struct Timeline {
    epoch: u64,
    next_ord: u64,
    blocks: Vec<BlockView>,
    turns: Vec<TurnSummary>,
    tool_calls: BTreeMap<String, ToolCallView>,
    pinned_compaction: Option<CompactionSummary>,
}

/// 一个会话的运行时（`SessionEvents` 的唯一真相源就在这里产生）。
pub(crate) struct SessionRuntime {
    pub(crate) id: SessionId,
    pub(crate) bus: EventBus,
    pub(crate) ledger: DeltaLedger,
    pub(crate) metrics: SessionMetrics,
    title: Mutex<Option<String>>,
    activity: Mutex<ActivityState>,
    driver: Mutex<Option<DriverKey>>,
    driver_ordinal: AtomicU64,
    /// 已附着的设备 → 实例集合（多端联动）。
    attached: Mutex<BTreeMap<DeviceKey, BTreeSet<String>>>,
    /// 允许访问本会话的设备集合。
    allowed: Mutex<BTreeSet<DeviceKey>>,
    timeline: Mutex<Timeline>,
    tool_mode: Mutex<ToolMode>,
    updated_at_ms: AtomicU64,
    tool_calls_running: std::sync::atomic::AtomicU32,
    /// 当前回合序号（空闲为 `None`）。
    current_turn: Mutex<Option<u64>>,
    max_blocks: usize,
}

impl SessionRuntime {
    fn new(id: SessionId, owner: &DeviceKey, config: &ServerConfig) -> Self {
        let mut allowed = BTreeSet::new();
        allowed.insert(owner.clone());
        Self {
            id,
            bus: EventBus::new(config.ring_capacity, config.subscriber_queue),
            ledger: DeltaLedger::default(),
            metrics: SessionMetrics::default(),
            title: Mutex::new(None),
            activity: Mutex::new(ActivityState::Idle),
            driver: Mutex::new(None),
            driver_ordinal: AtomicU64::new(0),
            attached: Mutex::new(BTreeMap::new()),
            allowed: Mutex::new(allowed),
            timeline: Mutex::new(Timeline {
                epoch: 1,
                next_ord: 0,
                blocks: Vec::new(),
                turns: Vec::new(),
                tool_calls: BTreeMap::new(),
                pinned_compaction: None,
            }),
            tool_mode: Mutex::new(ToolMode::Standard),
            updated_at_ms: AtomicU64::new(now_ms()),
            tool_calls_running: std::sync::atomic::AtomicU32::new(0),
            current_turn: Mutex::new(None),
            max_blocks: config.max_timeline_blocks,
        }
    }

    /// 发布一个事件（`seq` 由总线给，**无洞单调**）。
    pub(crate) fn emit(&self, event: SessionEvent) -> u64 {
        self.ledger.record(&event);
        self.updated_at_ms.store(now_ms(), Ordering::Relaxed);
        self.bus.publish(event)
    }

    /// 正在执行的工具调用数（活动面板）。
    pub(crate) fn running_tool_calls(&self) -> u32 {
        self.tool_calls_running.load(Ordering::Relaxed)
    }

    /// 工具调用开始。
    pub(crate) fn begin_tool_call(&self) {
        self.tool_calls_running.fetch_add(1, Ordering::Relaxed);
    }

    /// 工具调用收口。
    pub(crate) fn end_tool_call(&self) {
        self.tool_calls_running.fetch_sub(1, Ordering::Relaxed);
    }

    /// 当前时间线代际。
    pub(crate) fn epoch(&self) -> u64 {
        lock(&self.timeline).epoch
    }

    /// 当前事件序号。
    pub(crate) fn seq(&self) -> u64 {
        self.bus.last_seq()
    }

    /// 标题（`Rename` 后可见）。
    pub(crate) fn title(&self) -> Option<String> {
        lock(&self.title).clone()
    }

    /// 改名。
    pub(crate) fn rename(&self, title: impl Into<String>) {
        *lock(&self.title) = Some(title.into());
        self.updated_at_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// 活动状态。
    pub(crate) fn activity(&self) -> ActivityState {
        *lock(&self.activity)
    }

    /// 设置活动状态（调用方负责发事件）。
    pub(crate) fn set_activity(&self, activity: ActivityState) {
        *lock(&self.activity) = activity;
    }

    /// 工具模式。
    pub(crate) fn tool_mode(&self) -> ToolMode {
        *lock(&self.tool_mode)
    }

    /// 切换工具模式。
    pub(crate) fn set_tool_mode(&self, mode: ToolMode) {
        *lock(&self.tool_mode) = mode;
    }

    /// 允许某设备访问。
    pub(crate) fn allow_device(&self, device: &DeviceKey) {
        lock(&self.allowed).insert(device.clone());
    }

    /// 是否允许某设备访问。
    pub(crate) fn can_access(&self, device: &DeviceKey) -> bool {
        lock(&self.allowed).contains(device)
    }

    /// 记录附着（设备 + 实例）。
    pub(crate) fn attach(&self, identity: &ConnectionIdentity) {
        lock(&self.attached)
            .entry(identity.device.clone())
            .or_default()
            .insert(identity.client_instance_id.clone());
    }

    /// 撤销附着。
    pub(crate) fn detach(&self, device: &DeviceKey, instance: &str) {
        let mut attached = lock(&self.attached);
        if let Some(instances) = attached.get_mut(device) {
            instances.remove(instance);
            if instances.is_empty() {
                attached.remove(device);
            }
        }
    }

    /// 当前附着实例（设备 → 实例）。
    pub(crate) fn attached(&self) -> BTreeMap<DeviceKey, BTreeSet<String>> {
        lock(&self.attached).clone()
    }

    /// 当前驱动权身份。
    pub(crate) fn driver(&self) -> Option<DriverKey> {
        lock(&self.driver).clone()
    }

    /// 本连接是否持有驱动权。
    pub(crate) fn is_driver(&self, key: &DriverKey) -> bool {
        lock(&self.driver).as_ref() == Some(key)
    }

    /// 首个附着者取得驱动权（已有驱动权则不动），返回新序号。
    pub(crate) fn claim_driver_if_vacant(&self, key: DriverKey) -> Option<u64> {
        let mut driver = lock(&self.driver);
        if driver.is_some() {
            return None;
        }
        *driver = Some(key);
        let ordinal = self.driver_ordinal.fetch_add(1, Ordering::SeqCst) + 1;
        self.metrics.driver_switches.fetch_add(1, Ordering::Relaxed);
        Some(ordinal)
    }

    /// 切换驱动权（单点定序：序号只在这里递增）。
    pub(crate) fn switch_driver(
        &self,
        caller: &ConnectionIdentity,
        target: Option<&str>,
    ) -> Result<(DriverKey, u64), RpcError> {
        let attached = self.attached();
        let current = self.driver();
        let caller_key = caller.driver_key();
        if caller.scope != Scope::Owner && current.as_ref() != Some(&caller_key) {
            return Err(super::error::denied(
                "只有当前驱动端或主设备可以变更驱动权".to_string(),
            ));
        }
        let next = match target {
            Some(instance) => {
                let owner = attached
                    .iter()
                    .find(|(_, instances)| instances.contains(instance))
                    .map(|(device, _)| device.clone())
                    .ok_or_else(|| not_found(format!("没有附着中的客户端实例 `{instance}`")))?;
                if caller.scope != Scope::Owner && owner != caller.device {
                    return Err(super::error::denied(
                        "已配对设备只能把驱动权交给自己设备上的窗口".to_string(),
                    ));
                }
                DriverKey {
                    device: owner,
                    instance: instance.to_string(),
                }
            }
            None => {
                // 让出：挑下一个**别的**设备；没有别的设备就保持现状（驱动权永不悬空）。
                let next_device = attached
                    .iter()
                    .find(|(device, instances)| {
                        **device != caller.device && !instances.is_empty()
                    })
                    .and_then(|(device, instances)| {
                        instances.iter().next().map(|instance| DriverKey {
                            device: device.clone(),
                            instance: instance.clone(),
                        })
                    });
                match next_device {
                    Some(key) => key,
                    None => {
                        return Ok((
                            current.unwrap_or(caller_key),
                            self.driver_ordinal.load(Ordering::SeqCst),
                        ));
                    }
                }
            }
        };
        *lock(&self.driver) = Some(next.clone());
        let ordinal = self.driver_ordinal.fetch_add(1, Ordering::SeqCst) + 1;
        self.metrics.driver_switches.fetch_add(1, Ordering::Relaxed);
        Ok((next, ordinal))
    }

    /// 驱动端断开：若驱动权正落在它身上则**悬空**（等新实例 Attach 取得）。
    ///
    /// 为什么不在这里广播 `DriverChanged`：契约里驱动端是"某个实例 id"，
    /// 没有"无人驱动"的表示。悬空 + 让下一个 Attach 取得，比编一个空 id 更诚实。
    pub(crate) fn release_driver(&self, device: &DeviceKey, instance: &str) {
        let mut driver = lock(&self.driver);
        if driver
            .as_ref()
            .is_some_and(|key| &key.device == device && key.instance == instance)
        {
            *driver = None;
            self.driver_ordinal.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// 会话被吊销设备时的清理：把它从驱动权与附着表里摘掉。
    pub(crate) fn evict_device(&self, device: &DeviceKey) {
        lock(&self.attached).remove(device);
        let mut driver = lock(&self.driver);
        if driver.as_ref().is_some_and(|key| &key.device == device) {
            *driver = None;
            self.driver_ordinal.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// 追加一个时间线块。
    pub(crate) fn append_block(
        &self,
        kind: BlockKind,
        content: ContentRef,
        tool_call_id: Option<ToolCallId>,
    ) -> BlockView {
        let mut timeline = lock(&self.timeline);
        timeline.next_ord += 1;
        let ord = timeline.next_ord;
        let block = BlockView {
            ord,
            kind,
            content,
            tool_call_id,
            created_at_ms: now_ms(),
        };
        timeline.blocks.push(block.clone());
        while timeline.blocks.len() > self.max_blocks {
            // 骨架期的内存护栏：从最老处丢弃（v4.7 由存储层承载分页）。
            timeline.blocks.remove(0);
        }
        self.updated_at_ms.store(now_ms(), Ordering::Relaxed);
        block
    }

    /// 记一条工具调用视图（跨页归属靠它）。
    pub(crate) fn upsert_tool_call(&self, view: ToolCallView) {
        lock(&self.timeline)
            .tool_calls
            .insert(view.tool_call_id.as_str().to_string(), view);
    }

    /// 当前回合序号（空闲为 `None`）。
    pub(crate) fn current_turn(&self) -> Option<u64> {
        *lock(&self.current_turn)
    }

    /// 开始一个回合。
    pub(crate) fn start_turn(&self, terminal: Option<TurnTerminal>) -> TurnSummary {
        let mut timeline = lock(&self.timeline);
        let ordinal = timeline.turns.len() as u64 + 1;
        let summary = TurnSummary {
            ordinal,
            terminal,
            started_at_ms: now_ms(),
        };
        timeline.turns.push(summary.clone());
        self.metrics.turns_started.fetch_add(1, Ordering::Relaxed);
        *lock(&self.current_turn) = Some(ordinal);
        summary
    }

    /// 收口一个回合（终态一旦成立不可回退）。
    pub(crate) fn finish_turn(&self, ordinal: u64, terminal: TurnTerminal) {
        let mut current = lock(&self.current_turn);
        if *current == Some(ordinal) {
            *current = None;
        }
        drop(current);
        let mut timeline = lock(&self.timeline);
        if let Some(turn) = timeline
            .turns
            .iter_mut()
            .find(|turn| turn.ordinal == ordinal)
            && turn.terminal.is_none()
        {
            turn.terminal = Some(terminal);
            self.metrics.turns_finished.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// 固定压缩：**改写时间线并 bump 代际**（旧代际的翻页游标从此一律 `StaleCursor`）。
    pub(crate) fn compact(&self, summary: CompactionSummary) {
        let mut timeline = lock(&self.timeline);
        timeline.epoch += 1;
        timeline.pinned_compaction = Some(summary);
    }

    /// 首屏快照（`pinned_compaction` 独立于 `turns` 窗口）。
    pub(crate) fn snapshot(&self, tail_blocks: u32) -> SessionSnapshot {
        let timeline = lock(&self.timeline);
        let take = (tail_blocks as usize).min(timeline.blocks.len());
        let tail_blocks_view: Vec<BlockView> = timeline
            .blocks
            .iter()
            .skip(timeline.blocks.len() - take)
            .cloned()
            .collect();
        let mut tool_calls = Vec::new();
        for block in &tail_blocks_view {
            if let Some(id) = &block.tool_call_id
                && let Some(view) = timeline.tool_calls.get(id.as_str())
                && !tool_calls
                    .iter()
                    .any(|existing: &ToolCallView| &existing.tool_call_id == id)
            {
                tool_calls.push(view.clone());
            }
        }
        let turns: Vec<TurnSummary> = timeline
            .turns
            .iter()
            .rev()
            .take(32)
            .rev()
            .cloned()
            .collect();
        SessionSnapshot {
            session_id: self.id.clone(),
            title: lock(&self.title).clone(),
            activity: *lock(&self.activity),
            timeline_epoch: timeline.epoch,
            seq: self.bus.last_seq(),
            turns,
            tail_blocks: tail_blocks_view,
            tool_calls,
            pinned_compaction: timeline.pinned_compaction.clone(),
            tool_mode: *lock(&self.tool_mode),
        }
    }

    /// 向上翻页（代际失配一律 `StaleCursor`）。
    pub(crate) fn page(
        &self,
        before_ord: u64,
        limit: u32,
        timeline_epoch: u64,
    ) -> Result<TranscriptPage, RpcError> {
        let timeline = lock(&self.timeline);
        if timeline_epoch != timeline.epoch {
            return Err(rpc(
                ErrorCode::StaleCursor,
                format!(
                    "时间线代际已从 {timeline_epoch} 变为 {}，必须重取快照",
                    timeline.epoch
                ),
            ));
        }
        let limit = limit.clamp(1, 500) as usize;
        let end = if before_ord == 0 {
            timeline.blocks.len()
        } else {
            timeline.blocks.partition_point(|block| block.ord < before_ord)
        };
        let start = end.saturating_sub(limit);
        let blocks: Vec<BlockView> = timeline.blocks[start..end].to_vec();
        let mut tool_calls = Vec::new();
        for block in &blocks {
            if let Some(id) = &block.tool_call_id
                && let Some(view) = timeline.tool_calls.get(id.as_str())
                && !tool_calls
                    .iter()
                    .any(|existing: &ToolCallView| &existing.tool_call_id == id)
            {
                tool_calls.push(view.clone());
            }
        }
        Ok(TranscriptPage {
            blocks,
            tool_calls,
            next_before_ord: if start > 0 {
                Some(timeline.blocks[start].ord)
            } else {
                None
            },
            timeline_epoch: timeline.epoch,
        })
    }

    /// 摘要（会话列表用）。
    pub(crate) fn summary(&self) -> SessionSummary {
        SessionSummary {
            session_id: self.id.clone(),
            title: lock(&self.title).clone(),
            activity: *lock(&self.activity),
            updated_at_ms: self.updated_at_ms.load(Ordering::Relaxed),
        }
    }
}

/// 服务端全局状态。
pub(crate) struct ServerState {
    pub(crate) config: ServerConfig,
    pub(crate) identity: IdentityTable,
    pub(crate) shells: ShellTable,
    pub(crate) receipts: ReceiptTable,
    pub(crate) blobs: BlobStore,
    pub(crate) started_at: Instant,
    pub(crate) peers: AtomicU64,
    pub(crate) shutdown: watch::Sender<bool>,
    /// 进程内大输出的推送广播（AgentLoop 的位置：它知道有什么要推）。
    ///
    /// 骨架期广播给**全部连接**；`v4.7` 接真会话订阅后收敛到"只有附着该会话的
    /// 连接收到"。现在这么写是为了让 blob 推送有一条与"控制面 RPC"完全独立的
    /// 触发路径——守卫②要量的正是这条路径的隔离性。
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

    /// 活跃会话数。
    pub(crate) fn active_sessions(&self) -> u32 {
        u32::try_from(self.sessions().len()).unwrap_or(u32::MAX)
    }
}

// ── Shell（独立并发句柄表） ──

/// Shell PTY 的一个附着点（当前只允许一个写者：终端不是广播）。
pub(crate) struct ShellSink {
    tx: mpsc::Sender<(u64, Bytes)>,
    lag: watch::Sender<bool>,
    generation: u64,
}

/// 附着结果（先写完 `replay`，再接 `live`）。
pub(crate) struct ShellAttach {
    /// 回放块（偏移 + 字节，从请求偏移开始）。
    pub(crate) replay: Vec<(u64, Bytes)>,
    /// 实时输出（**每块自带起始偏移**：并入 sink 与读水位不可能同时原子，
    /// 所以让每个块自己说清楚它在整条输出里的位置，避免任何 off-by-one）。
    pub(crate) live: mpsc::Receiver<(u64, Bytes)>,
    /// 队列溢出哨兵。
    pub(crate) lagged: watch::Receiver<bool>,
    /// 服务端权威输出水位。
    pub(crate) attach_offset: u64,
    /// 请求偏移已跌出历史（必须从 `attach_offset` 全文重拉）。
    pub(crate) gap: bool,
    /// 本附着的代际（断开时只清自己这一代）。
    pub(crate) generation: u64,
}

struct ShellHistory {
    chunks: VecDeque<(u64, Bytes)>,
    bytes: usize,
    capacity: usize,
}

/// 一个 shell 句柄（**独立于连接**：断联不杀它）。
pub(crate) struct ShellHandle {
    pub(crate) id: String,
    pub(crate) session_id: SessionId,
    pub(crate) owner: DeviceKey,
    exited: std::sync::atomic::AtomicBool,
    output_offset: AtomicU64,
    size: Mutex<(u16, u16)>,
    history: Mutex<ShellHistory>,
    sink: Mutex<Option<ShellSink>>,
    generation: AtomicU64,
}

impl ShellHandle {
    /// 权威输出水位。
    pub(crate) fn output_offset(&self) -> u64 {
        self.output_offset.load(Ordering::SeqCst)
    }

    /// 视图。
    pub(crate) fn view(&self) -> ShellView {
        ShellView {
            shell_id: self.id.clone(),
            output_offset: self.output_offset(),
            exited: self.exited.load(Ordering::SeqCst),
        }
    }

    /// 调整窗口。
    pub(crate) fn resize(&self, cols: u16, rows: u16) {
        *lock(&self.size) = (cols, rows);
    }

    /// 标记结束。
    pub(crate) fn mark_exited(&self) {
        self.exited.store(true, Ordering::SeqCst);
        if let Some(sink) = lock(&self.sink).take() {
            sink.lag.send_replace(true);
        }
    }

    /// 是否已结束。
    pub(crate) fn exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    /// 产出一段 PTY 字节（真实后端是 PTY 读线程；骨架期由 echo 或测试注入）。
    ///
    /// 语义与真终端一致：**要么按序送达，要么告诉附着点"你落后了，重来"**。
    /// 绝不静默丢字节——终端少一段字节就是满屏花屏。
    pub(crate) fn push_output(&self, bytes: Bytes) {
        let len = bytes.len() as u64;
        let start = self.output_offset.fetch_add(len, Ordering::SeqCst);
        {
            let mut history = lock(&self.history);
            history.bytes += bytes.len();
            history.chunks.push_back((start, bytes.clone()));
            while history.bytes > history.capacity {
                if let Some((_, dropped)) = history.chunks.pop_front() {
                    history.bytes = history.bytes.saturating_sub(dropped.len());
                } else {
                    break;
                }
            }
        }
        let mut sink_guard = lock(&self.sink);
        if let Some(sink) = sink_guard.as_ref()
            && sink.tx.try_send((start, bytes)).is_err()
        {
            // 队列满：置哨兵并摘掉附着点。**不静默丢字节**——终端少一段就是花屏。
            sink.lag.send_replace(true);
            *sink_guard = None;
        }
    }

    /// 附着（替换已有附着点：终端同一时刻只有一个写者）。
    pub(crate) fn attach(&self, from_offset: u64, config: &ServerConfig) -> ShellAttach {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, live) = mpsc::channel(config.shell_queue_chunks.max(1));
        let (lag, lagged) = watch::channel(false);
        {
            let mut sink = lock(&self.sink);
            if let Some(previous) = sink.take() {
                previous.lag.send_replace(true);
            }
            *sink = Some(ShellSink {
                tx,
                lag,
                generation,
            });
        }
        let offset = self.output_offset();
        let mut replay = Vec::new();
        let mut gap = false;
        {
            let history = lock(&self.history);
            let oldest = history
                .chunks
                .front()
                .map(|(start, _)| *start)
                .unwrap_or(offset);
            if from_offset < oldest {
                gap = true;
            } else {
                for (start, chunk) in &history.chunks {
                    if start + chunk.len() as u64 > from_offset {
                        replay.push((*start, chunk.clone()));
                    }
                }
            }
        }
        ShellAttach {
            replay,
            live,
            lagged,
            attach_offset: offset,
            gap,
            generation,
        }
    }

    /// 断开附着（只清自己那一代，避免旧流的清理砸掉新流）。
    pub(crate) fn detach(&self, generation: u64) {
        let mut sink = lock(&self.sink);
        if sink.as_ref().is_some_and(|s| s.generation == generation) {
            *sink = None;
        }
    }
}

/// Shell 句柄表（**独立锁**：PTY 热路径不碰任何会话/中央锁）。
pub(crate) struct ShellTable {
    inner: Mutex<BTreeMap<String, Arc<ShellHandle>>>,
    next: AtomicU64,
}

impl ShellTable {
    /// 建表。
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(BTreeMap::new()),
            next: AtomicU64::new(0),
        }
    }

    /// 打开一个 shell。
    pub(crate) fn open(
        &self,
        session_id: &SessionId,
        owner: &DeviceKey,
        cols: u16,
        rows: u16,
        config: &ServerConfig,
    ) -> Arc<ShellHandle> {
        let ordinal = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        let id = format!("shell-{ordinal:04}");
        let handle = Arc::new(ShellHandle {
            id: id.clone(),
            session_id: session_id.clone(),
            owner: owner.clone(),
            exited: std::sync::atomic::AtomicBool::new(false),
            output_offset: AtomicU64::new(0),
            size: Mutex::new((cols, rows)),
            history: Mutex::new(ShellHistory {
                chunks: VecDeque::new(),
                bytes: 0,
                capacity: config.shell_history_bytes,
            }),
            sink: Mutex::new(None),
            generation: AtomicU64::new(0),
        });
        lock(&self.inner).insert(id, Arc::clone(&handle));
        handle
    }

    /// 查句柄。
    pub(crate) fn get(&self, id: &str) -> Option<Arc<ShellHandle>> {
        lock(&self.inner).get(id).cloned()
    }

    /// 本会话的 shell 列表（按 id 升序）。
    pub(crate) fn list(&self, session_id: &SessionId) -> Vec<ShellView> {
        lock(&self.inner)
            .values()
            .filter(|handle| &handle.session_id == session_id)
            .map(|handle| handle.view())
            .collect()
    }

    /// 关闭并移除。
    pub(crate) fn close(&self, id: &str) -> bool {
        let handle = lock(&self.inner).remove(id);
        match handle {
            Some(handle) => {
                handle.mark_exited();
                true
            }
            None => false,
        }
    }

    /// 句柄数（守卫断言"断联不杀进程"用）。
    pub(crate) fn len(&self) -> usize {
        lock(&self.inner).len()
    }
}

// ── 幂等回执 ──

enum ReceiptSlot {
    InFlight(watch::Receiver<Option<Arc<ControlReply>>>),
    Done(Arc<ControlReply>),
}

/// 幂等回执表的入口判定。
pub(crate) enum ReceiptStart {
    /// 本连接是第一个：执行完必须 [`ReceiptTable::finish`]。
    Fresh {
        /// 完成后用它唤醒所有等待者。
        publish: watch::Sender<Option<Arc<ControlReply>>>,
    },
    /// 已有原回执：原样返回，**绝不再执行一次**。
    Duplicate(Arc<ControlReply>),
    /// 另一个连接正在执行同一个 `command_id`：等它的结果。
    Wait(watch::Receiver<Option<Arc<ControlReply>>>),
}

/// 按 `command_id` 去重的回执表（`ADR-0042` 铁律 4）。
pub(crate) struct ReceiptTable {
    inner: Mutex<BTreeMap<String, ReceiptSlot>>,
}

impl ReceiptTable {
    /// 建表。
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(BTreeMap::new()),
        }
    }

    /// 开始一次调用。
    pub(crate) fn begin(&self, command_id: &str) -> ReceiptStart {
        let mut inner = lock(&self.inner);
        match inner.get(command_id) {
            Some(ReceiptSlot::Done(reply)) => ReceiptStart::Duplicate(Arc::clone(reply)),
            Some(ReceiptSlot::InFlight(receiver)) => ReceiptStart::Wait(receiver.clone()),
            None => {
                let (publish, receiver) = watch::channel(None);
                inner.insert(command_id.to_string(), ReceiptSlot::InFlight(receiver));
                ReceiptStart::Fresh { publish }
            }
        }
    }

    /// 落库并唤醒等待者。
    pub(crate) fn finish(
        &self,
        command_id: &str,
        reply: ControlReply,
        publish: &watch::Sender<Option<Arc<ControlReply>>>,
    ) -> Arc<ControlReply> {
        let reply = Arc::new(reply);
        lock(&self.inner).insert(
            command_id.to_string(),
            ReceiptSlot::Done(Arc::clone(&reply)),
        );
        publish.send_replace(Some(Arc::clone(&reply)));
        reply
    }

    /// 领导者异常退出：把在飞槽位摘掉，让后来者重新执行（不会留下永久黑洞）。
    pub(crate) fn abandon(&self, command_id: &str) {
        let mut inner = lock(&self.inner);
        if matches!(inner.get(command_id), Some(ReceiptSlot::InFlight(_))) {
            inner.remove(command_id);
        }
    }

    /// 已落库回执数。
    pub(crate) fn len(&self) -> usize {
        lock(&self.inner).len()
    }
}

// ── 内容寻址 blob ──

/// 内容寻址 blob 库（大输出/图片的外置端）。
pub(crate) struct BlobStore {
    inner: Mutex<BTreeMap<ContentHash, Arc<Vec<u8>>>>,
}

impl BlobStore {
    /// 建库。
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(BTreeMap::new()),
        }
    }

    /// 存入并返回内容哈希。
    pub(crate) fn put(&self, bytes: Vec<u8>) -> ContentHash {
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        let hash = ContentHash::from_bytes(digest);
        lock(&self.inner).insert(hash, Arc::new(bytes));
        hash
    }

    /// 取回。
    pub(crate) fn get(&self, hash: &ContentHash) -> Option<Arc<Vec<u8>>> {
        lock(&self.inner).get(hash).cloned()
    }
}

/// 造一个骨架期的回合 id。
pub(crate) fn skeleton_turn_id(ordinal: u64) -> TurnId {
    TurnId::new(format!("turn-{ordinal:04}")).expect("turn id 形状固定")
}

/// 造一个骨架期的块 id。
pub(crate) fn skeleton_block_id(ordinal: u64) -> BlockId {
    BlockId::new(format!("block-{ordinal:04}")).expect("block id 形状固定")
}

/// 造一个骨架期的 exec 会话 id。
pub(crate) fn skeleton_exec_id(ordinal: u64) -> ExecSessionId {
    ExecSessionId::new(format!("execs_{ordinal:04}")).expect("exec id 形状固定")
}

/// 空用量（骨架期不产 token 用量：`None` 才是"上游没给"，不是 0）。
pub(crate) fn empty_usage() -> UsageInfo {
    UsageInfo::default()
}

/// 驱动权变更事件的便捷构造（**只准从 [`SessionRuntime::switch_driver`] 的序号来**）。
pub(crate) fn driver_changed(
    session_id: &SessionId,
    driver: &DriverKey,
    ordinal: u64,
) -> SessionEvent {
    SessionEvent::DriverChanged {
        session_id: session_id.clone(),
        driver_client_instance_id: driver.instance.clone(),
        ordinal,
    }
}
