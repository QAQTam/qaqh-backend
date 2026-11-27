//! 单会话运行时：`SessionEvents` 真相源、时间线窗口与原子驱动权定序。

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use qaqh_core_vocab::{
    ActivityState, BlockId, ContentRef, ExecSessionId, InteractionId, SessionId, ToolCallId,
    ToolMode, TurnId, TurnTerminal, UsageInfo,
};
use qaqh_ringing_v3::{
    BlockKind, BlockView, CompactionSummary, ErrorCode, ExecSessionSnapshot, InteractionView,
    RpcError, SessionEvent, SessionSnapshot, SessionSummary, ToolCallView, TranscriptPage,
    TurnSummary,
};

use super::bus::{DeltaLedger, EventBus};
use super::error::{denied, not_found, rpc};
use super::identity::{ConnectionIdentity, DeviceKey, DriverKey, Scope};
use super::lock;
use super::now_ms;
use super::state::ServerConfig;

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
}

struct Timeline {
    epoch: u64,
    next_ord: u64,
    blocks: VecDeque<BlockView>,
    turns: Vec<TurnSummary>,
    tool_calls: BTreeMap<String, ToolCallView>,
    pending_interactions: BTreeMap<String, InteractionView>,
    exec_sessions: BTreeMap<String, ExecSessionSnapshot>,
    pinned_compaction: Option<CompactionSummary>,
}

/// 一个会话的运行时（`SessionEvents` 的唯一真相源）。
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
    tool_calls_running: AtomicU32,
    /// 当前回合序号（空闲为 `None`）。
    current_turn: Mutex<Option<u64>>,
    max_blocks: usize,
}

impl SessionRuntime {
    pub(crate) fn new(id: SessionId, owner: &DeviceKey, config: &ServerConfig) -> Self {
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
                blocks: VecDeque::new(),
                turns: Vec::new(),
                tool_calls: BTreeMap::new(),
                pending_interactions: BTreeMap::new(),
                exec_sessions: BTreeMap::new(),
                pinned_compaction: None,
            }),
            tool_mode: Mutex::new(ToolMode::Standard),
            updated_at_ms: AtomicU64::new(now_ms()),
            tool_calls_running: AtomicU32::new(0),
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

    /// 设置活动状态。
    pub(crate) fn set_activity(&self, activity: ActivityState) {
        *lock(&self.activity) = activity;
    }

    /// 原子检查空闲并切换为 `Running`（防止并发 `SendPrompt` TOCTOU 竞争）。
    pub(crate) fn try_begin_turn(&self) -> Result<(), RpcError> {
        let mut activity = lock(&self.activity);
        if *activity != ActivityState::Idle {
            return Err(rpc(ErrorCode::Conflict, "上一回合尚未收口，拒绝并发投递"));
        }
        *activity = ActivityState::Running;
        Ok(())
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

    pub(crate) fn driver_with_ordinal(&self) -> (Option<DriverKey>, u64) {
        let driver = lock(&self.driver);
        let ordinal = if driver.is_some() {
            self.driver_ordinal.load(Ordering::SeqCst)
        } else {
            0
        };
        (driver.clone(), ordinal)
    }

    /// 本连接是否持有驱动权。
    pub(crate) fn is_driver(&self, key: &DriverKey) -> bool {
        lock(&self.driver).as_ref() == Some(key)
    }

    /// 首个附着者原子取得驱动权并按同一把锁序发广播（防止序号与事件流乱序）。
    pub(crate) fn claim_driver_if_vacant_and_emit(&self, key: DriverKey) -> Option<u64> {
        let mut driver = lock(&self.driver);
        if driver.is_some() {
            return None;
        }
        *driver = Some(key.clone());
        let ordinal = self.driver_ordinal.fetch_add(1, Ordering::SeqCst) + 1;
        self.metrics.driver_switches.fetch_add(1, Ordering::Relaxed);
        self.emit(driver_changed(&self.id, &key, ordinal));
        Some(ordinal)
    }

    /// 原子切换驱动权并广播（持 `driver` 锁写总线：单点定序与单调 `seq` 同一序）。
    pub(crate) fn switch_driver_and_emit(
        &self,
        caller: &ConnectionIdentity,
        target: Option<&str>,
    ) -> Result<(DriverKey, u64), RpcError> {
        let attached = self.attached();
        let mut driver_guard = lock(&self.driver);
        let caller_key = caller.driver_key();
        if caller.scope != Scope::Owner && driver_guard.as_ref() != Some(&caller_key) {
            return Err(denied("只有当前驱动端或主设备可以变更驱动权"));
        }
        let next = match target {
            Some(instance) => {
                let owner = attached
                    .iter()
                    .find(|(_, instances)| instances.contains(instance))
                    .map(|(device, _)| device.clone())
                    .ok_or_else(|| not_found(format!("没有附着中的客户端实例 `{instance}`")))?;
                if caller.scope != Scope::Owner && owner != caller.device {
                    return Err(denied("已配对设备只能把驱动权交给自己设备上的窗口"));
                }
                DriverKey {
                    device: owner,
                    instance: instance.to_string(),
                }
            }
            None => {
                let next_device = attached
                    .iter()
                    .find(|(device, instances)| **device != caller.device && !instances.is_empty())
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
                            driver_guard.clone().unwrap_or(caller_key),
                            self.driver_ordinal.load(Ordering::SeqCst),
                        ));
                    }
                }
            }
        };
        *driver_guard = Some(next.clone());
        let ordinal = self.driver_ordinal.fetch_add(1, Ordering::SeqCst) + 1;
        self.metrics.driver_switches.fetch_add(1, Ordering::Relaxed);
        self.emit(driver_changed(&self.id, &next, ordinal));
        Ok((next, ordinal))
    }

    /// 驱动端断开：若驱动权正落在它身上则悬空（等新实例 Attach 取得）。
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

    /// 设备被吊销时的即时清理：撤权、摘附着、撤驱动权。
    pub(crate) fn evict_device(&self, device: &DeviceKey) {
        lock(&self.allowed).remove(device);
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
        timeline.blocks.push_back(block.clone());
        while timeline.blocks.len() > self.max_blocks {
            timeline.blocks.pop_front();
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

    /// 记一条执行会话快照（开箱即渲染快照带上）。
    pub(crate) fn upsert_exec_session(&self, exec: ExecSessionSnapshot) {
        lock(&self.timeline)
            .exec_sessions
            .insert(exec.exec_session_id.as_str().to_string(), exec);
    }

    /// 消除一条已处置交互。
    pub(crate) fn resolve_interaction(&self, interaction_id: &InteractionId) {
        lock(&self.timeline)
            .pending_interactions
            .remove(interaction_id.as_str());
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

    /// 首屏快照（`pinned_compaction` 独立于 `turns` 窗口；开箱即渲染）。
    pub(crate) fn snapshot(&self, tail_blocks: u32, tail_turns: u32) -> SessionSnapshot {
        let driver_snapshot = self.driver();
        let driver_ordinal = self.driver_ordinal.load(Ordering::SeqCst);
        let mut timeline = lock(&self.timeline);
        let slices = timeline.blocks.make_contiguous();
        let take = (tail_blocks as usize).min(slices.len());
        let tail_blocks_view: Vec<BlockView> = slices[slices.len() - take..].to_vec();
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
        let turn_limit = tail_turns.clamp(1, 256) as usize;
        let turns: Vec<TurnSummary> = timeline
            .turns
            .iter()
            .rev()
            .take(turn_limit)
            .rev()
            .cloned()
            .collect();
        let pending_interactions = timeline.pending_interactions.values().cloned().collect();
        let exec_sessions = timeline.exec_sessions.values().cloned().collect();
        SessionSnapshot {
            session_id: self.id.clone(),
            title: lock(&self.title).clone(),
            workspace_id: None,
            cwd: None,
            archived: false,
            settings_revision: 0,
            model_selection: None,
            activity: *lock(&self.activity),
            timeline_epoch: timeline.epoch,
            seq: self.bus.last_seq(),
            turns,
            tail_blocks: tail_blocks_view,
            tool_calls,
            pending_interactions,
            exec_sessions,
            subagents: Vec::new(),
            goal: None,
            driver_client_instance_id: driver_snapshot.map(|key| key.instance),
            driver_ordinal,
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
        let mut timeline = lock(&self.timeline);
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
        let slice = timeline.blocks.make_contiguous();
        let end = if before_ord == 0 {
            slice.len()
        } else {
            slice.partition_point(|block| block.ord < before_ord)
        };
        let start = end.saturating_sub(limit);
        let blocks: Vec<BlockView> = slice[start..end].to_vec();
        let next_before_ord = if start > 0 {
            Some(slice[start].ord)
        } else {
            None
        };
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
            next_before_ord,
            timeline_epoch: timeline.epoch,
        })
    }

    /// 摘要（会话列表用）。
    pub(crate) fn summary(&self) -> SessionSummary {
        SessionSummary {
            session_id: self.id.clone(),
            title: lock(&self.title).clone(),
            workspace_id: None,
            cwd: None,
            archived: false,
            model_selection: None,
            tool_mode: *lock(&self.tool_mode),
            settings_revision: 0,
            handoff_from: None,
            activity: *lock(&self.activity),
            updated_at_ms: self.updated_at_ms.load(Ordering::Relaxed),
        }
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

/// 驱动权变更事件的便捷构造（**只准从 [`SessionRuntime`] 的定序入口来**）。
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
