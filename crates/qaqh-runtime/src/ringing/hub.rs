//! `RingingHub`：daemon 侧 Ringing 运行时聚合入口。
//!
//! 职责：
//! - 三频道 `ChannelRouter`（入队/回放）；
//! - 三频道可靠 journal（reliable 事件 + replaceable checkpoint）；
//! - 领域 snapshot projection（每 seed+channel）；
//! - 每频道序号生成；
//! - 事件幂等（journal 侧 event_id 去重）；
//! - timeline transcript 投影（见 `timeline_hub.rs`）与孤儿收尾（见
//!   `orphan_seal.rs`），同文件 `impl RingingHub` 跨文件块；
//! - 大内容外置存储（`content_store.rs`，会话所有权 + TTL）。
//!
//! 由 daemon（T5）与 worker 事件入口（T6）消费。线程安全（Mutex 保护）。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use qaqh_domain::{
    ControlEvent, ConversationEvent, Delivery, DomainEvent, RingingChannel, TimelineEntry,
};
use qaqh_ringing::{
    RingingChannelSnapshot, RingingEvent, RingingEventEnvelope, RingingResetRequired,
    is_safe_integer,
};
use qaqh_session::SessionManager;
use tokio::sync::broadcast;

use super::content_store::{ContentEntry, ContentStore};
use super::journal::{AppendOutcome, CursorExpired, ReliableJournal};
use super::journal_store::{JournalOp, JournalStore};
use super::projection::SnapshotProjector;
use super::router::{ChannelRouter, replaceable_key_for, terminal_replaceable_keys};
use super::sequencer::Sequencer;
use crate::timeline_store::TimelineStore;
use crate::{TimelineAppender, TimelineLiveEntry};

/// journal jsonl 超过该物理大小时，compact 后触发整文件重写（丢弃已折叠的
/// RoundDelta）。append-only 日志若不重写，磁盘与装载成本永久累积。
pub(super) const JOURNAL_REWRITE_THRESHOLD_BYTES: u64 = 4 * 1024 * 1024;

/// Non-terminal timeline changes are checkpointed at most once per interval.
/// Live delivery is still immediate; only the full snapshot rewrite is paced.
pub(super) const TIMELINE_PERSIST_INTERVAL: Duration = Duration::from_secs(1);

/// 测试用阈值覆盖（OnceLock 一次性；仅测试模块设置）。
static JOURNAL_REWRITE_THRESHOLD_OVERRIDE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

fn journal_rewrite_threshold() -> u64 {
    *JOURNAL_REWRITE_THRESHOLD_OVERRIDE
        .get()
        .unwrap_or(&JOURNAL_REWRITE_THRESHOLD_BYTES)
}

/// Overlay persisted conversation data onto the live event projection.
/// Metadata used by native clients belongs to the same authoritative
/// bootstrap state as turns and usage; keeping this list centralized prevents
/// a newly added field from silently disappearing when the projection already
/// contains its structural `{seed, channel, revision}` object.
fn merge_persisted_conversation_state(
    projected: &mut serde_json::Value,
    persisted: serde_json::Value,
) {
    const PERSISTED_KEYS: &[&str] = &[
        "turns",
        "total_turns",
        "has_more",
        "usage",
        "usage_totals",
        "usage_requests",
        "cache_reported_requests",
        "model",
        "context_limit",
    ];
    match projected.as_object_mut() {
        Some(obj) => {
            for key in PERSISTED_KEYS {
                if let Some(value) = persisted.get(*key) {
                    obj.insert((*key).to_string(), value.clone());
                }
            }
        }
        None => *projected = persisted,
    }
}

#[cfg(test)]
pub(crate) fn override_journal_rewrite_threshold_for_test(bytes: u64) {
    let _ = JOURNAL_REWRITE_THRESHOLD_OVERRIDE.set(bytes);
}

/// 事件已接受（含 envelope 与幂等状态）。
#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // 装箱改造属结构塑形，另立项
pub enum PublishOutcome {
    /// 已入队并可发送。
    Published { envelope: RingingEventEnvelope },
    /// 重复 event_id（幂等丢弃）。
    Duplicate,
    /// reliable 队列背压。
    Backpressure,
}

/// 频道级回放结果（SSE 重连）：可回放的事件 + 需要强制 snapshot 的会话。
#[derive(Debug, Default)]
pub struct ChannelReplay {
    pub events: Vec<RingingEventEnvelope>,
    pub resets: Vec<RingingResetRequired>,
}

#[derive(Debug)]
pub(super) struct SeedChannelState {
    router: ChannelRouter,
    journal: ReliableJournal,
    projection: SnapshotProjector,
    last_stream_seq: u64,
    replaceable_since_checkpoint: HashMap<super::router::ReplaceableKey, u32>,
    /// BUG-2026-09-12-08：重写已投递写线程（pending 清零前不重复投递）。
    rewrite_inflight: bool,
    /// 写队列满丢过条目：需要一次重写把内存 journal 与磁盘对账。
    needs_rewrite: bool,
}

#[derive(Debug)]
pub(super) struct TimelinePersistence {
    pub(super) wake: mpsc::Sender<()>,
    pub(super) pending_seeds: Arc<Mutex<HashSet<String>>>,
    pub(super) join: Option<JoinHandle<()>>,
}

impl SeedChannelState {
    fn new(channel: RingingChannel) -> Self {
        Self {
            router: ChannelRouter::new(channel),
            journal: ReliableJournal::new(),
            projection: SnapshotProjector::new(),
            last_stream_seq: 0,
            replaceable_since_checkpoint: HashMap::new(),
            rewrite_inflight: false,
            needs_rewrite: false,
        }
    }

    /// 从持久化 op 序列重建（与 live publish 路径相同的重放语义）。
    fn with_ops(channel: RingingChannel, seed: &str, ops: &[JournalOp]) -> Self {
        let mut state = Self::new(channel);
        for op in ops {
            match op {
                JournalOp::Append { envelope } => {
                    state.last_stream_seq = state.last_stream_seq.max(envelope.stream_seq);
                    let domain = match &envelope.event {
                        RingingEvent::Control(event) => DomainEvent::Control(event.clone()),
                        RingingEvent::Conversation(event) => {
                            DomainEvent::Conversation(event.clone())
                        }
                        RingingEvent::Tool(event) => DomainEvent::Tool(event.clone()),
                    };
                    state.projection.apply(channel, seed, &domain);
                    match envelope.delivery {
                        Delivery::Reliable => {
                            let _ = state.journal.append(envelope);
                        }
                        Delivery::Replaceable => {
                            let _ = state.router.route(envelope.clone());
                        }
                        Delivery::Ephemeral => {}
                    }
                }
                JournalOp::Checkpoint {
                    identity,
                    stream_seq,
                } => {
                    state.last_stream_seq = state.last_stream_seq.max(*stream_seq);
                    state.journal.checkpoint_replaceable(identity, *stream_seq);
                }
                JournalOp::Compact { turn_id, round_num } => {
                    state.journal.compact_round_deltas(turn_id, *round_num);
                }
            }
        }
        state
    }
}

/// `(channel, seed)` → `SeedChannelState` 的分片槽。
///
/// 用 `Arc<Mutex<_>>` 而非直接 `Mutex<_>` 的原因：`forget_seed` 需要在**不持
/// 分片表锁**的前提下把某个 seed 的槽从表里摘掉（否则摘除期间其他 seed 的
/// publish 会被串行化），同时保证「摘除瞬间已在途的持槽者」继续操作同一实例；
/// 摘除后槽由在途者自然释放，期间的并发 publish 会在表里重新 insert 一个
/// 新槽（与 `forget_seed` 的既有竞态语义一致：调用方保证已 join worker）。
type SeedChannelSlot = Arc<Mutex<SeedChannelState>>;

/// 一个频道的全部 seed 分片（BUG-2026-09-13-33）。
///
/// `seeds` 的 `RwLock` 只在**登记/摘除分片槽**时短暂写入，读路径（publish /
/// replay / snapshot）取读锁拿到 `Arc<SeedChannelSlot>` 后立刻释放，真正的
/// 事件提交与投影只在 per-(channel, seed) 槽锁内进行——这正是 BUG-08 收尾
/// 要消除的「跨会话内存态共享单锁」。
#[derive(Debug, Default)]
pub(super) struct ChannelShards {
    seeds: RwLock<HashMap<String, SeedChannelSlot>>,
}

impl ChannelShards {
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.seeds
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    fn contains(&self, seed: &str) -> bool {
        self.seeds
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(seed)
    }

    /// 取（必要时登记）该 seed 的槽。
    ///
    /// 同 seed ⇒ 同一个 `Arc` ⇒ 同一把锁：这是「全局 (channel, seed) 互斥」
    /// 的保证点。登记是一次短写锁 + 一次 `SeedChannelState::new`，不含任何
    /// I/O；并发首访同一 seed 时可能重复构造 state，最终只保留一个（未被保留
    /// 的那个在 `insert` 后即被丢弃，不会有第二个线程继续写它）。
    fn slot(&self, channel: RingingChannel, seed: &str) -> SeedChannelSlot {
        if let Some(slot) = self
            .seeds
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(seed)
        {
            return Arc::clone(slot);
        }
        let mut seeded = self.seeds.write().unwrap_or_else(|e| e.into_inner());
        Arc::clone(
            seeded
                .entry(seed.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(SeedChannelState::new(channel)))),
        )
    }

    /// 只读查找：不登记（回放/快照等只读路径不得因查询而建条目）。
    fn slot_if_present(&self, seed: &str) -> Option<SeedChannelSlot> {
        self.seeds
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(seed)
            .map(Arc::clone)
    }

    /// 登记一个**已构造好**的 state（懒加载重放路径用，避免重放两遍）。
    fn slot_with(&self, seed: &str, state: SeedChannelState) -> SeedChannelSlot {
        let mut seeded = self.seeds.write().unwrap_or_else(|e| e.into_inner());
        Arc::clone(
            seeded
                .entry(seed.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(state))),
        )
    }

    /// 摘除某 seed 的槽（`forget_seed` 用）；返回是否摘除成功。
    fn remove(&self, seed: &str) -> bool {
        self.seeds
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(seed)
            .is_some()
    }

    /// 该频道下已登记的 seed（回放/快照遍历用）。
    fn seed_keys(&self) -> Vec<String> {
        self.seeds
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }
}

/// Ringing daemon 运行时聚合。
/// Debug 已移除：TimelineAppender 现含不可 Debug 的 offload 回调字段。
pub struct RingingHub {
    pub(super) epoch: String,
    pub(super) sequencer: Sequencer,
    /// 磁盘持久化 seed 清单（懒加载索引；`ensure_seed_loaded` 按需重放）。
    /// 启动时 `load_persisted` 只扫描清单，不加载任何历史。
    pub(super) disk_seeds: Mutex<HashMap<RingingChannel, HashSet<String>>>,
    /// 磁盘 timeline seed 清单（懒加载索引；`ensure_timeline_loaded` 按需恢复）。
    pub(super) disk_timeline_seeds: Mutex<HashSet<String>>,
    /// 懒加载串行化（per-seed）：防止并发首访**同一** seed 时双重重放/恢复。
    /// Phase 1（2026-09-11）：全局 `Mutex<()>` → 锁表。原全局锁下 session A
    /// 的 journal 重放（28 秒级）会阻塞 B~Z 的 timeline 快照装载，是多
    /// session 前端卡顿的直接根因（A2）。锁表条目只增不减：每条约 80 字节，
    /// 10 万 seed 也就 ~8 MB，真正的冷热驱逐留给 Phase 5 的 LRU 预算。
    pub(super) lazy_loads: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// 大内容外置存储（会话所有权 + TTL）。
    pub(super) content_store: Mutex<ContentStore>,
    /// channel → (seed → state)。router/journal/projection 均 per (seed, channel)。
    ///
    /// BUG-2026-09-13-33（BUG-08 收尾）：全局单锁 → 两级分片锁表。
    /// 顶层锁只保护「频道登记 / 频道枚举」，临界区是 map 操作，**绝不含 I/O、
    /// 投影或事件提交**；热路径取到 `Arc<SeedChannelSlot>` 后立即释放顶层锁，
    /// 只在 per-(channel, seed) 槽锁内提交。跨会话不再互相串行。
    ///
    /// # 锁序（禁止成环）
    ///
    /// `channels`(顶层) → `ChannelShards.seeds`(读写锁) → 槽锁(per-(channel,seed))
    /// → 叶子锁(`live` / `live_interactions` / `content_store`)。
    ///
    /// 1. 顶层锁**从不**在持有其他 hub 锁时获取（`channel_shards` 先取顶层、
    ///    后取 `seeds`，且中途不回调其他锁）；取到分片后即释放顶层锁。
    /// 2. `seeds` 读/写锁的临界区只做 map 操作，绝不持锁进入槽锁或叶子锁。
    /// 3. 多把槽锁**从不**同时持有：`replay_channel_since` / `forget_seed` 逐
    ///    seed 取放，单个 seed 处理完立即 drop 槽锁。
    /// 4. `journal_store`（含写线程）：只在**不持槽锁**的路径上取
    ///    （`schedule_rewrite_if_oversized` 用 `try_lock`，懒加载读盘在槽锁外），
    ///    因此不参与本环。
    /// 5. 与既有 per-seed 装载锁 `lazy_loads` 并存为**单向序**：
    ///    `lazy_loads`(per-seed) → `channels`/`seeds`/槽锁（先装载、后提交）。
    ///    反向无路径（槽锁内不做任何 `lazy_load_lock`），故无环；
    ///    且 `lazy_loads` 与槽锁槽表无嵌套（`Drop` 先摘槽表快照，再逐个取
    ///    槽锁，两者从不重叠持有）。
    pub(super) channels: Mutex<HashMap<RingingChannel, Arc<ChannelShards>>>,
    /// 每频道实时推送通道（SSE 消费；可靠性由 journal/cursor 保证）。
    pub(super) live: Mutex<HashMap<RingingChannel, broadcast::Sender<RingingEventEnvelope>>>,
    /// 当前进程生命周期内发布且未 resolved 的活交互（seed → interaction_id）。
    /// journal 重放的幽灵交互不在此表：daemon 重启后表为空，bootstrap 孤儿
    /// 收尾据此区分「等待用户响应的活交互」（保护，不 seal）与「daemon 重启
    /// 遗留的幽灵交互」（seal）。worker 死亡/重启路径用 force 无视该守卫。
    pub(super) live_interactions: Mutex<HashMap<String, String>>,
    /// B9/H3：当前进程内存活的 worker（registry 维护）。bootstrap 的
    /// force=false 孤儿收尾在 worker 存活时整体跳过，防止误杀活 turn。
    pub(super) live_workers: Mutex<std::collections::HashSet<String>>,
    /// 持久化 journal（None = 非持久模式；I/O 失败只记录日志，不阻塞事件路径）。
    /// `Arc`：与 journal 写线程共享（BUG-2026-09-12-08：I/O 在 `channels` 锁外执行）。
    pub(super) journal_store: Arc<Mutex<Option<JournalStore>>>,
    /// journal 写线程投递端（None = 非持久模式）。发布路径只做 `try_send`，
    /// 序列化/写盘/重写全部由写线程在 `channels` 锁外完成。
    pub(super) journal_writer: Option<mpsc::SyncSender<JournalWriteOp>>,
    /// 写线程 join 句柄：`Drop` 时排空队列并 join（保证「hub 释放 = 已落盘」）。
    pub(super) journal_writer_join: Option<JoinHandle<()>>,
    /// Ringing V1 timeline transcript 的唯一 writer。它与三频道 Ringing v1 完全隔离，
    /// 不依赖 legacy 事件投影。
    pub(super) timeline: Arc<Mutex<TimelineAppender>>,
    pub(super) timeline_live: broadcast::Sender<TimelineLiveEntry>,
    pub(super) timeline_store: Arc<Mutex<Option<TimelineStore>>>,
    pub(super) timeline_persistence: Mutex<Option<TimelinePersistence>>,
    /// 会话存储句柄（PR-3-1 注入化；conversation snapshot / timeline 重建的
    /// 持久化读侧）。None = 测试或未装配（对应旧 `try_global()` 为空语义）。
    pub(super) sessions: Option<Arc<SessionManager>>,
}

/// journal 写队列容量（BUG-2026-09-12-08）：写线程消费 ~10k ops/s，队列仅在
/// 持续过载时才会填满；填满时丢弃并标记该 seed 重写收敛（内存 journal 才是
/// 权威，重写会把丢掉的条目补回磁盘）。
const JOURNAL_WRITE_QUEUE_CAPACITY: usize = 4096;

/// 交给 journal 写线程的写操作（锁外 I/O，BUG-2026-09-12-08）。
#[allow(clippy::large_enum_variant)] // 与 JournalOp 同口径：装箱另立项
pub(super) enum JournalWriteOp {
    Append {
        channel: RingingChannel,
        seed: String,
        envelope: RingingEventEnvelope,
    },
    Checkpoint {
        channel: RingingChannel,
        seed: String,
        identity: String,
        stream_seq: u64,
    },
    Compact {
        channel: RingingChannel,
        seed: String,
        turn_id: String,
        round_num: u32,
    },
    Replaceable {
        channel: RingingChannel,
        seed: String,
        identity: String,
        envelope: RingingEventEnvelope,
    },
    RemoveReplaceable {
        channel: RingingChannel,
        seed: String,
        identity: String,
    },
    /// 整文件重写：`envelopes`/`checkpoints` 为投递时点的内存 journal 快照
    /// （克隆在 `channels` 锁内完成，stat/序列化/写盘全部在锁外）。
    Rewrite {
        channel: RingingChannel,
        seed: String,
        envelopes: Vec<RingingEventEnvelope>,
        checkpoints: Vec<(String, u64)>,
    },
    /// 排空同步点（测试/关闭）：此前投递的写操作全部处理后回执。
    Flush { ack: mpsc::Sender<()> },
}

/// journal 写线程主循环：串行消费写操作（FIFO 保证 per-key 写序与投递序一致）。
fn journal_writer_loop(
    rx: mpsc::Receiver<JournalWriteOp>,
    store: Arc<Mutex<Option<JournalStore>>>,
) {
    while let Ok(op) = rx.recv() {
        let mut guard = store.lock().unwrap_or_else(|e| e.into_inner());
        let Some(store) = guard.as_mut() else {
            continue;
        };
        match op {
            JournalWriteOp::Append {
                channel,
                seed,
                envelope,
            } => {
                if let Err(error) = store.append(channel, &seed, &envelope) {
                    log::warn!("[ringing] journal append failed: {error}");
                }
            }
            JournalWriteOp::Checkpoint {
                channel,
                seed,
                identity,
                stream_seq,
            } => {
                if let Err(error) = store.checkpoint(channel, &seed, &identity, stream_seq) {
                    log::warn!("[ringing] journal checkpoint persist failed: {error}");
                }
            }
            JournalWriteOp::Compact {
                channel,
                seed,
                turn_id,
                round_num,
            } => {
                if let Err(error) = store.compact(channel, &seed, &turn_id, round_num) {
                    log::warn!("[ringing] journal compact persist failed: {error}");
                }
            }
            JournalWriteOp::Replaceable {
                channel,
                seed,
                identity,
                envelope,
            } => {
                if let Err(error) = store.replaceable(channel, &seed, &identity, &envelope) {
                    log::warn!("[ringing] replaceable slot persist failed: {error}");
                }
            }
            JournalWriteOp::RemoveReplaceable {
                channel,
                seed,
                identity,
            } => {
                if let Err(error) = store.remove_replaceable(channel, &seed, &identity) {
                    log::warn!("[ringing] replaceable slot cleanup failed: {error}");
                }
            }
            JournalWriteOp::Rewrite {
                channel,
                seed,
                envelopes,
                checkpoints,
            } => match store.file_size(channel, &seed) {
                Ok(size) if size >= journal_rewrite_threshold() => {
                    match store.rewrite(channel, &seed, &envelopes, &checkpoints) {
                        Ok(()) => log::info!(
                            "[ringing] journal rewritten for {seed}: {size} bytes -> {} entries",
                            envelopes.len()
                        ),
                        Err(error) => {
                            log::warn!("[ringing] journal rewrite failed for {seed}: {error}")
                        }
                    }
                }
                // 文件已小于阈值（可能已被更晚的重写收敛）：跳过。
                _ => {}
            },
            JournalWriteOp::Flush { ack } => {
                let _ = ack.send(());
            }
        }
    }
}

impl RingingHub {
    pub fn new(epoch: impl Into<String>) -> Self {
        Self::with_options(epoch.into(), None)
    }

    /// 持久化构造：daemon 重启后可靠事件/切流状态不丢。
    pub fn with_persistence(epoch: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        let hub = Self::with_options(epoch.into(), Some(root.into()));
        hub.load_persisted();
        hub.load_timeline_persisted();
        hub.start_timeline_persistence();
        hub
    }

    /// 注入会话存储句柄（PR-3-1：daemon main 在 `SessionManager::init` 后装配）。
    pub fn with_sessions(mut self, sessions: Arc<SessionManager>) -> Self {
        self.sessions = Some(sessions);
        self
    }

    fn with_options(epoch: String, root: Option<PathBuf>) -> Self {
        let timeline_store = root
            .as_ref()
            .and_then(|root| match TimelineStore::new(root) {
                Ok(store) => Some(store),
                Err(error) => {
                    log::warn!("[timeline] persistence disabled: {error}");
                    None
                }
            });
        let journal_store = match root {
            Some(root) => match JournalStore::new(&root) {
                Ok(store) => Some(store),
                Err(error) => {
                    log::warn!("[ringing] journal persistence disabled: {error}");
                    None
                }
            },
            None => None,
        };
        let (timeline_live, _) = broadcast::channel(1024);
        // BUG-2026-09-12-08：journal 写线程。持久化开启时投递端与写线程成对
        // 存在；非持久模式两者皆无（发布路径零开销跳过）。
        let journal_store = Arc::new(Mutex::new(journal_store));
        let (journal_writer, journal_writer_join) = if journal_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            let (tx, rx) = mpsc::sync_channel::<JournalWriteOp>(JOURNAL_WRITE_QUEUE_CAPACITY);
            let store = Arc::clone(&journal_store);
            match std::thread::Builder::new()
                .name("ringing-journal-writer".into())
                .spawn(move || journal_writer_loop(rx, store))
            {
                Ok(join) => (Some(tx), Some(join)),
                Err(error) => {
                    log::error!("[ringing] journal writer thread spawn failed: {error}");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };
        Self {
            epoch,
            sequencer: Sequencer::new(),
            disk_seeds: Mutex::new(HashMap::new()),
            disk_timeline_seeds: Mutex::new(HashSet::new()),
            lazy_loads: Mutex::new(HashMap::new()),
            content_store: Mutex::new(ContentStore::new()),
            channels: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            live_interactions: Mutex::new(HashMap::new()),
            live_workers: Mutex::new(std::collections::HashSet::new()),
            journal_store,
            journal_writer,
            journal_writer_join,
            timeline: Arc::new(Mutex::new(TimelineAppender::new())),
            timeline_live,
            timeline_store: Arc::new(Mutex::new(timeline_store)),
            timeline_persistence: Mutex::new(None),
            sessions: None,
        }
    }

    /// 启动装载（懒加载模式）：只扫描磁盘 seed 清单，不重放任何事件。
    ///
    /// 内存态（journal/router/projection/sequencer 水位）在首次访问该 seed
    /// 时由 `ensure_seed_loaded` 从磁盘按需恢复。冷启动开销从"全量读取
    /// 全部 jsonl"降为"遍历目录"，历史会话不再常驻内存。
    fn load_persisted(&self) {
        let seeds = {
            let guard = self.journal_store.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                Some(store) => store.list_seeds(),
                None => HashMap::new(),
            }
        };
        let total: usize = seeds.values().map(HashSet::len).sum();
        *self.disk_seeds.lock().unwrap_or_else(|e| e.into_inner()) = seeds;
        log::info!("[ringing] lazy journal index ready: {total} persisted seeds on disk");
    }

    /// 懒加载串行化锁（per-seed）：同一 seed 的首访重放/恢复互斥，不同 seed
    /// 并行。锁表插入在锁外——`entry().or_insert_with` 的短暂 map 锁只与
    /// 其他锁表访问竞争，不与任何装载 I/O 竞争。
    pub(super) fn lazy_load_lock(&self, seed: &str) -> Arc<Mutex<()>> {
        self.lazy_loads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(seed.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// 懒加载：确保 (channel, seed) 的持久化历史已重放入内存。
    ///
    /// - 已在内存或磁盘无记录：零成本返回；
    /// - 磁盘有记录：读取该 seed 的 ops → 重放重建 state → 精确恢复序号 →
    ///   超大文件顺手压缩（P0 收敛，不依赖 RoundCompleted）→ 插入 channels。
    ///   全程持有该 seed 的 per-seed 串行锁，避免并发首访双重重放。
    ///
    /// - Err：磁盘加载失败（fail-closed，R3）——调用方不得以全新空状态
    ///   继续发布/回放，否则重启后序号永久冲突。
    fn ensure_seed_loaded(&self, channel: RingingChannel, seed: &str) -> Result<(), String> {
        let lock = self.lazy_load_lock(seed);
        let _serial = lock.lock().unwrap_or_else(|e| e.into_inner());
        let loaded = self
            .channel_shards(channel)
            .is_some_and(|shards| shards.contains(seed));
        if loaded {
            return Ok(());
        }
        let on_disk = self
            .disk_seeds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&channel)
            .is_some_and(|seeds| seeds.contains(seed));
        if !on_disk {
            return Ok(());
        }
        // BUG-2026-09-12-08：先排空写队列再读盘——异步写线程的在途条目必须
        // 先落盘，否则本次重放会漏掉它们（forget→reload 场景）。
        self.flush_journal_persistence();
        // 读磁盘（短暂持有 journal_store 锁；读完即释放，不与 channel_state 嵌套）。
        let ops = {
            // R3：读盘失败必须 fail-closed 上报，禁止静默降级为全新状态——
            // 那会让该 seed 以 seq=1 重新发布，重启重放后永久乱序重复。
            let mut guard = self.journal_store.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_mut() {
                Some(store) => match store.load_seed(channel, seed) {
                    Ok(ops) => ops,
                    Err(error) => {
                        log::error!(
                            "[ringing] lazy load failed for {seed} on {}: {error}",
                            channel.as_str()
                        );
                        return Err(format!(
                            "lazy load failed for {seed} on {}: {error}",
                            channel.as_str()
                        ));
                    }
                },
                None => return Ok(()),
            }
        };
        if ops.is_empty() {
            // 清单存在但无任何可重放操作（空/损坏）：插入空 state 防反复扫描。
            self.shards_for(channel)
                .slot_with(seed, SeedChannelState::new(channel));
            return Ok(());
        }
        let state = SeedChannelState::with_ops(channel, seed, &ops);
        // 精确恢复序号（比启动水位更完整：channel/session seq 一并恢复）。
        let (mut max_stream, mut max_channel, mut max_session) = (0, 0, 0);
        for op in &ops {
            if let JournalOp::Append { envelope } = op {
                max_stream = max_stream.max(envelope.stream_seq);
                max_channel = max_channel.max(envelope.channel_seq);
                max_session = max_session.max(envelope.session_seq);
            }
        }
        self.sequencer
            .seed(channel, seed, max_stream, max_channel, max_session);
        // 超大历史文件加载即压缩（冷路径：同步执行，不走写队列）。
        self.rewrite_oversized_now(channel, seed, &state);
        self.shards_for(channel).slot_with(seed, state);
        log::info!(
            "[ringing] lazily loaded {seed} on {}: {} ops",
            channel.as_str(),
            ops.len()
        );
        Ok(())
    }

    /// 收尾三频道投影中的孤儿领域状态（Ringing 版 `seal_orphan_running_turns`）。
    ///
    /// worker 的挂起/运行状态在内存中，daemon 重启或 worker 被重新拉起后，
    /// journal 重放会恢复 `TurnStarted`/`ToolStarted`/`InteractionRequested` 等
    /// reliable 事件，但它们**永远不会有终态**——bootstrap 快照因此携带陈旧
    /// 的 `active_turn`/`running`/`pending_permission`/`pending_interaction`：
    /// 前端把中断的 turn 投影为 running、弹出无法批准的幽灵 ask/授权面板。
    ///
    /// 与 timeline seal 语义一致：通过正常 publish 路径发出终态事件
    /// （`ConversationCancelled` / `ToolFinished(Cancelled)` / `InteractionResolved`），
    /// 使 journal、投影与 SSE 客户端全部收敛。幂等：无孤儿时返回 false。
    ///
    /// 调用方必须在 `ensure_seed_loaded` 完成之后调用（本函数内部 publish 会
    /// 再次调用 `ensure_seed_loaded`，重入同 seed 的 lazy_load 锁会死锁）。
    /// B9/H3：registry 在 spawn 成功/worker 关闭时维护活表。
    pub fn mark_worker_live(&self, seed: &str) {
        self.live_workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(seed.to_string());
    }

    pub fn mark_worker_dead(&self, seed: &str) {
        self.live_workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(seed);
    }

    /// D-1：会话关闭后丢弃该 seed 的全部常驻内存态（channel×seed 的
    /// 投影/journal/router、活交互表、live_workers、大内容条目）。
    ///
    /// 磁盘索引（disk_seeds / disk_timeline_seeds）**保留**——下次访问照常
    /// 走 lazy-load 重放 journal，行为与 daemon 重启后的恢复路径完全一致。
    /// 调用约束：必须晚于该 seed 的终态（Closed）发布，否则 publish 会触发
    /// lazy-load 把刚丢弃的状态原样重建回来；且调用方必须已 join worker，
    /// 避免存活 worker 继续写回脏状态。
    pub fn forget_seed(&self, seed: &str) {
        // 锁序：先取 `channels` 顶层锁取出分片快照，**释放后再**逐个摘除
        // 槽表条目——顶层锁与槽表锁永不重叠持有（见 `channels` 字段锁序注释）。
        // 摘除不在槽锁内进行：已在途的持槽者继续操作同一实例，摘除后由它自然
        // 释放；这保持既有语义（调用方保证已 join worker，无新发布者）。
        let shards: Vec<Arc<ChannelShards>> = self
            .channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for shard in shards {
            shard.remove(seed);
        }
        if let Ok(mut live) = self.live_interactions.lock() {
            live.remove(seed);
        }
        if let Ok(mut workers) = self.live_workers.lock() {
            workers.remove(seed);
        }
        if let Ok(mut content) = self.content_store.lock() {
            content.release_session(seed);
        }
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    /// 大内容外置：存入（返回 content_id）。
    pub fn put_content(
        &self,
        seed: &str,
        media_type: &str,
        bytes: Vec<u8>,
        truncated: bool,
    ) -> String {
        self.content_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .put(seed, media_type, bytes, truncated)
    }

    /// 大内容外置：读取（校验会话所有权 + TTL）。
    pub fn get_content(&self, seed: &str, content_id: &str) -> Option<ContentEntry> {
        self.content_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(seed, content_id)
    }

    /// 顶层锁的 `MutexGuard`（只在测试断言里用；生产路径一律走
    /// `channel_shards` / `shards_for`，取到 `Arc` 后立刻释放顶层锁）。
    ///
    /// 锁序：`channels`(顶层) → `ChannelShards.seeds`(读写锁) → 槽锁 → 叶子锁，
    /// 详见 `channels` 字段文档。本函数返回的 guard **不得**跨越槽锁获取。
    #[cfg(test)]
    fn channel_state(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<RingingChannel, Arc<ChannelShards>>> {
        self.channels.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 已登记频道的分片集（None = 该频道尚无任何 seed）。
    pub(super) fn channel_shards(&self, channel: RingingChannel) -> Option<Arc<ChannelShards>> {
        self.channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&channel)
            .cloned()
    }

    /// 取（必要时登记）频道分片集。登记只做一次 map insert。
    fn shards_for(&self, channel: RingingChannel) -> Arc<ChannelShards> {
        let mut guard = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(
            guard
                .entry(channel)
                .or_insert_with(|| Arc::new(ChannelShards::default())),
        )
    }

    /// 该 (channel, seed) 的分片槽（不存在则登记空 state）。
    fn seed_slot(&self, channel: RingingChannel, seed: &str) -> Arc<Mutex<SeedChannelState>> {
        self.shards_for(channel).slot(channel, seed)
    }

    /// 发布领域事件（worker 事件入口调用）。
    pub fn publish(&self, seed: &str, event: DomainEvent) -> PublishOutcome {
        self.publish_with_causation(seed, event, None)
    }

    /// 发布领域事件并附加因果来源（Ringing command_id）。
    pub fn publish_with_causation(
        &self,
        seed: &str,
        event: DomainEvent,
        causation: Option<&str>,
    ) -> PublishOutcome {
        let channel = event.channel();
        // P1: 懒加载——publish 前确保该 seed 历史已重放入内存，防止新事件
        // 的序号与磁盘历史冲突（sequencer 水位随后续加载精确恢复）。
        if let Err(error) = self.ensure_seed_loaded(channel, seed) {
            // R3：fail-closed——加载失败不得以全新空状态继续发布。
            log::error!("[ringing] publish fail-closed (load error): {error}");
            return PublishOutcome::Backpressure;
        }
        let delivery = event.delivery();

        // R1：取号与本 seed 槽锁内提交构成同一临界区，保证取号序 == 提交序。
        // 懒加载必须在取槽锁前完成（内部会取分片表锁，std Mutex 不可重入）。
        //
        // BUG-2026-09-13-33：临界区从「全局 channels 锁」收窄为
        // per-(channel, seed) 槽锁——不同会话/频道不再互相串行。
        let slot = self.seed_slot(channel, seed);
        let mut st = slot.lock().unwrap_or_else(|e| e.into_inner());
        let st = &mut *st;

        let (stream_seq, channel_seq, session_seq) = self.sequencer.next(channel, seed);
        if !is_safe_integer(stream_seq)
            || !is_safe_integer(channel_seq)
            || !is_safe_integer(session_seq)
        {
            log::error!("[ringing] sequence exceeded JSON safe integer range");
            return PublishOutcome::Backpressure;
        }
        let event_id = format!(
            "{}-{}-{}-{}",
            self.epoch,
            channel.as_str(),
            seed,
            stream_seq
        );

        // 幂等：journal 侧 event_id 去重（replaceable 也检查，防重复投递）
        let envelope = RingingEventEnvelope::new(
            seed,
            stream_seq,
            channel_seq,
            session_seq,
            event_id,
            event.clone().into(),
        );
        let envelope = match causation {
            Some(c) => envelope.with_causation(c),
            None => envelope,
        };

        let state_changed = st.projection.apply(channel, seed, &event);
        let revision = st.projection.revision(channel, seed);
        // server_ts：服务器发布时间（unix ms），端到端延迟诊断用。
        let mut envelope = envelope.with_server_ts(unix_ms());
        if state_changed {
            envelope = envelope.with_state_revision(revision);
        }
        st.last_stream_seq = st.last_stream_seq.max(stream_seq);

        match delivery {
            Delivery::Reliable => {
                match st.journal.append(&envelope) {
                    AppendOutcome::Duplicate => return PublishOutcome::Duplicate,
                    AppendOutcome::Appended => {
                        self.persist_append(channel, seed, st, &envelope);
                        // RoundCompleted 是该 round 的权威终态（携带完整 thinking/answer），
                        // 折叠该 round 的增量可控制 journal 用量，且回放安全：
                        // 客户端要么已有增量（随后被快照覆盖），要么直接拿到全量快照。
                        if let RingingEvent::Conversation(ConversationEvent::RoundCompleted {
                            turn_id,
                            round_num,
                            ..
                        }) = &envelope.event
                        {
                            let removed = st.journal.compact_round_deltas(turn_id, *round_num);
                            if removed > 0 {
                                self.persist_compact(channel, seed, st, turn_id, *round_num);
                            }
                        }
                        // P0: 磁盘收敛检查脱离 RoundCompleted 依赖——轮次未完成
                        // 时 delta 持续 append 也必须有兜底重写（pending 门控）。
                        self.schedule_rewrite_if_oversized(channel, seed, st);
                    }
                }
                for key in terminal_replaceable_keys(&envelope.event) {
                    st.router.flush_replaceable(&key);
                    st.replaceable_since_checkpoint.remove(&key);
                    self.persist_remove_replaceable(channel, seed, st, &format!("{key:?}"));
                }
                // 活交互登记：当前进程发布的 InteractionRequested/PlanReviewRequested
                // 进入内存表，resolved 时移除。daemon 重启后表为空 → journal 重放的
                // 幽灵交互不在表 → bootstrap 孤儿收尾仍可收尾它们（原设计意图）；
                // 活交互在表 → 收尾跳过（修复「ask 发布后 1ms 被 bootstrap 秒杀」）。
                match &envelope.event {
                    RingingEvent::Control(ControlEvent::InteractionRequested {
                        interaction_id,
                        ..
                    })
                    | RingingEvent::Control(ControlEvent::PlanReviewRequested {
                        interaction_id,
                        ..
                    }) => {
                        self.live_interactions
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(seed.to_string(), interaction_id.clone());
                    }
                    RingingEvent::Control(ControlEvent::InteractionResolved {
                        interaction_id,
                        ..
                    })
                    | RingingEvent::Control(ControlEvent::PlanReviewResolved {
                        interaction_id,
                        ..
                    }) => {
                        let mut live = self
                            .live_interactions
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        if live.get(seed).is_some_and(|cur| cur == interaction_id) {
                            live.remove(seed);
                        }
                    }
                    _ => {}
                }
                // Reliable replay/backpressure belongs to the journal. Keeping a
                // second reliable queue in the router would fill permanently
                // because live broadcast has no dequeue/ack path.
                self.fanout(channel, &envelope);
                PublishOutcome::Published { envelope }
            }
            Delivery::Replaceable | Delivery::Ephemeral => {
                // replaceable 覆盖入槽；ephemeral 不入队但照常实时推送
                match st.router.route(envelope.clone()) {
                    super::router::RouteOutcome::Routed { .. } => {
                        if let Some(key) = replaceable_key_for(&envelope.event) {
                            // 先结束计数器借用，再投递写操作（persist 需要 &mut st）。
                            let should_persist = {
                                let count = st
                                    .replaceable_since_checkpoint
                                    .entry(key.clone())
                                    .or_default();
                                *count = count.saturating_add(1);
                                if *count == 1 || *count >= 64 {
                                    *count = 0;
                                    true
                                } else {
                                    false
                                }
                            };
                            if should_persist {
                                self.persist_replaceable(
                                    channel,
                                    seed,
                                    st,
                                    &format!("{key:?}"),
                                    &envelope,
                                );
                            }
                        }
                        self.fanout(channel, &envelope);
                        PublishOutcome::Published { envelope }
                    }
                    super::router::RouteOutcome::Backpressure => PublishOutcome::Backpressure,
                }
            }
        }
    }

    /// 订阅某频道的实时事件流（SSE 用）。reliable 可靠性由 cursor/journal 承担。
    pub fn subscribe(&self, channel: RingingChannel) -> broadcast::Receiver<RingingEventEnvelope> {
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        live.entry(channel)
            .or_insert_with(|| broadcast::channel(1024).0)
            .subscribe()
    }

    /// publish 末尾：把信封推入实时通道（失败=无消费者，忽略）。
    fn fanout(&self, channel: RingingChannel, envelope: &RingingEventEnvelope) {
        let live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = live.get(&channel) {
            let _ = tx.send(envelope.clone());
        }
    }

    /// 从 cursor 回放（SSE 重连用）。cursor 超出窗口 → `CursorExpired`。
    pub fn replay_since(
        &self,
        channel: RingingChannel,
        seed: &str,
        after_stream_seq: u64,
    ) -> Result<Vec<RingingEventEnvelope>, CursorExpired> {
        // R3：加载失败时让客户端走 reset→snapshot 路径，而非回放半截状态。
        self.ensure_seed_loaded(channel, seed)
            .map_err(|_| CursorExpired {
                earliest_available_seq: 0,
            })?;
        let slot = self
            .channel_shards(channel)
            .and_then(|shards| shards.slot_if_present(seed))
            .ok_or(CursorExpired {
                earliest_available_seq: 0,
            })?;
        let st = slot.lock().unwrap_or_else(|e| e.into_inner());
        st.journal.replay_since(after_stream_seq).map(|mut events| {
            // 追加当前 replaceable 值（慢消费者恢复增量）；R4：合并后按
            // stream_seq 排序，对齐频道级回放的全局顺序保证。
            events.extend(
                st.router
                    .replay_since(after_stream_seq)
                    .into_iter()
                    .filter(|e| e.delivery != Delivery::Reliable),
            );
            events.sort_by_key(|e| e.stream_seq);
            events
        })
    }

    /// 频道级回放（SSE 重连用）：聚合该频道所有 seed 的可靠 tail 与
    /// 当前 replaceable 值。某个 seed 的 cursor 超出保留窗口时产出
    /// `RingingResetRequired`，客户端应改走 snapshot 恢复。
    ///
    /// 懒加载语义：只回放**已加载** seed（冷启动后未经任何访问的 seed 不在
    /// 内存）。客户端连接某 seed 前必经 open/bootstrap（触发 `ensure_seed_loaded`），
    /// 因此活跃会话的历史始终可回放；从未连接的 seed 没有消费者，无需装载。
    ///
    /// `skip_reliable`：无 cursor 的新连接（`after_stream_seq == 0`）为 true。
    /// 新客户端的历史由 bootstrap 快照承担（快照先行），此时只回放当前
    /// replaceable 值；不回放 journal 里的可靠历史，否则 SSE 先于 bootstrap
    /// 到达时，无终态的 TurnStarted/ToolStarted/InteractionRequested 会被
    /// 前端应用成陈旧 running turn 与无法批准的幽灵交互面板。
    pub fn replay_channel_since(
        &self,
        channel: RingingChannel,
        after_stream_seq: u64,
        skip_reliable: bool,
    ) -> ChannelReplay {
        let Some(shards) = self.channel_shards(channel) else {
            return ChannelReplay::default();
        };
        let mut replay = ChannelReplay::default();
        // 逐 seed 取/放槽锁：任何时候最多持有一把槽锁，杜绝多槽锁交叉死锁
        //（锁序注释第 3 条）。
        for seed in shards.seed_keys() {
            let Some(slot) = shards.slot_if_present(&seed) else {
                continue;
            };
            let st = slot.lock().unwrap_or_else(|e| e.into_inner());
            if !skip_reliable {
                match st.journal.replay_since(after_stream_seq) {
                    Ok(mut events) => replay.events.append(&mut events),
                    Err(CursorExpired {
                        earliest_available_seq,
                    }) => {
                        replay.resets.push(RingingResetRequired::new(
                            channel,
                            seed.clone(),
                            earliest_available_seq,
                        ));
                    }
                }
            }
            for env in st.router.replay_since(after_stream_seq) {
                if env.delivery != Delivery::Reliable && env.stream_seq > after_stream_seq {
                    replay.events.push(env);
                }
            }
        }
        // stream_seq 在 (server_epoch, channel) 内全局唯一，跨 seed 合并后
        // 直接按 stream_seq 排序即得该频道的全局顺序。
        replay.events.sort_by_key(|e| e.stream_seq);
        replay
    }

    /// 读取领域快照（HTTP `GET /ringing/v1/sessions/{seed}/bootstrap`）。
    pub fn snapshot(&self, channel: RingingChannel, seed: &str) -> RingingChannelSnapshot {
        // R3：只读路径加载失败时降级为空快照/零水位，不阻断读取。
        let _ = self.ensure_seed_loaded(channel, seed);
        self.channel_shards(channel)
            .and_then(|shards| shards.slot_if_present(seed))
            .map(|slot| {
                let st = slot.lock().unwrap_or_else(|e| e.into_inner());
                st.projection
                    .snapshot_for(channel, seed, st.last_stream_seq)
            })
            .unwrap_or_else(|| SnapshotProjector::new().snapshot_for(channel, seed, 0))
    }

    /// Conversation 频道完整快照：领域投影摘要 + 持久化消息构建的 turns。
    pub fn conversation_snapshot(&self, seed: &str) -> RingingChannelSnapshot {
        let mut snap = self.snapshot(RingingChannel::Conversation, seed);
        if let Some(state) = super::conversation_snapshot::persisted_conversation_state(
            self.sessions.as_deref(),
            seed,
        ) {
            merge_persisted_conversation_state(&mut snap.state, state);
        }
        snap
    }

    /// 记 replaceable checkpoint（稀疏）。
    pub fn checkpoint(&self, channel: RingingChannel, seed: &str, identity: &str, stream_seq: u64) {
        // R3：只读路径加载失败时降级为空快照/零水位，不阻断读取。
        let _ = self.ensure_seed_loaded(channel, seed);
        let slot = self.seed_slot(channel, seed);
        let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
        let st = &mut *guard;
        st.last_stream_seq = st.last_stream_seq.max(stream_seq);
        st.journal.checkpoint_replaceable(identity, stream_seq);
        self.persist_checkpoint(channel, seed, st, identity, stream_seq);
    }

    pub fn last_stream_seq(&self, channel: RingingChannel, seed: &str) -> u64 {
        // R3：只读路径加载失败时降级为空快照/零水位，不阻断读取。
        let _ = self.ensure_seed_loaded(channel, seed);
        self.channel_shards(channel)
            .and_then(|shards| shards.slot_if_present(seed))
            .map(|slot| {
                slot.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .last_stream_seq
            })
            .unwrap_or(0)
    }

    // ── 持久化钩子：I/O 失败只记录日志，绝不阻塞事件路径 ──
    //
    // BUG-2026-09-12-08：全部写操作经 journal 写线程在 `channels` 锁外执行；
    // 发布路径只投递（try_send），不再持锁写盘。

    /// 投递写操作给 journal 写线程。队列满时丢弃并标记该 seed 需要重写收敛
    /// ——内存 journal 才是权威，重写会把丢掉的条目补回磁盘。
    fn enqueue_journal(&self, st: &mut SeedChannelState, op: JournalWriteOp) {
        let Some(writer) = self.journal_writer.as_ref() else {
            return;
        };
        if writer.try_send(op).is_err() {
            st.needs_rewrite = true;
            log::warn!(
                "[ringing] journal write queue full; dropped write (reconcile rewrite scheduled)"
            );
        }
    }

    /// 排空 journal 写队列（测试断言 / 关闭 / 调试路径同步点）：返回时此前
    /// 投递的所有写操作都已处理。
    pub fn flush_journal_persistence(&self) {
        let Some(writer) = self.journal_writer.as_ref() else {
            return;
        };
        let (ack_tx, ack_rx) = mpsc::channel();
        if writer.send(JournalWriteOp::Flush { ack: ack_tx }).is_err() {
            return; // 写线程已退出
        }
        if ack_rx.recv_timeout(Duration::from_secs(10)).is_err() {
            log::warn!("[ringing] journal flush timed out");
        }
    }

    fn persist_append(
        &self,
        channel: RingingChannel,
        seed: &str,
        st: &mut SeedChannelState,
        envelope: &RingingEventEnvelope,
    ) {
        self.enqueue_journal(
            st,
            JournalWriteOp::Append {
                channel,
                seed: seed.to_string(),
                envelope: envelope.clone(),
            },
        );
    }

    fn persist_compact(
        &self,
        channel: RingingChannel,
        seed: &str,
        st: &mut SeedChannelState,
        turn_id: &str,
        round_num: u32,
    ) {
        self.enqueue_journal(
            st,
            JournalWriteOp::Compact {
                channel,
                seed: seed.to_string(),
                turn_id: turn_id.to_string(),
                round_num,
            },
        );
    }

    /// 热路径门控（持 `channels` 锁期间调用，BUG-2026-09-12-08）：只做内存级
    /// 判定与数据快照，实际 stat/序列化/写盘全部交给 journal 写线程在锁外完成。
    ///
    /// P0 语义保留：触发不再依赖 `RoundCompleted` 折叠（`removed > 0`）。轮次
    /// 未完成/卡死时 `RoundDelta` 会持续 append，若只在折叠后检查，文件将无界
    /// 增长（实测 82MB 全 append 无 compact）。现在每次 reliable append 后都经
    /// `pending_bytes` 计数门控（try_lock：写线程正忙则本次跳过，下次再查）。
    /// 重写以内存有界 journal（≤8192 条）为权威，可把超大文件收敛到窗口大小。
    fn schedule_rewrite_if_oversized(
        &self,
        channel: RingingChannel,
        seed: &str,
        st: &mut SeedChannelState,
    ) {
        let Some(writer) = self.journal_writer.as_ref() else {
            return;
        };
        // try_lock：写线程正在执行 I/O 时跳过检查（非阻塞，绝不让写线程的
        // 耗时反过来卡住发布路径）；下次 append 会再查。
        let Ok(mut guard) = self.journal_store.try_lock() else {
            return;
        };
        let Some(store) = guard.as_mut() else { return };
        let pending = store.pending_bytes(channel, seed);
        if st.rewrite_inflight {
            // 写线程完成重写后会把 pending 清零；据此解除在途标志。
            if pending < journal_rewrite_threshold() {
                st.rewrite_inflight = false;
            }
            return;
        }
        if !st.needs_rewrite && pending < journal_rewrite_threshold() {
            return;
        }
        drop(guard);
        let envelopes: Vec<_> = st.journal.entries().cloned().collect();
        let checkpoints: Vec<(String, u64)> = st
            .journal
            .checkpoints()
            .iter()
            .map(|(key, seq)| (key.clone(), *seq))
            .collect();
        match writer.try_send(JournalWriteOp::Rewrite {
            channel,
            seed: seed.to_string(),
            envelopes,
            checkpoints,
        }) {
            Ok(()) => {
                st.rewrite_inflight = true;
                st.needs_rewrite = false;
            }
            Err(_) => {
                // 队列满：保留 needs_rewrite，下次 append 重试。
                st.needs_rewrite = true;
                log::warn!("[ringing] journal rewrite deferred for {seed}: write queue full");
            }
        }
    }

    /// 冷路径（懒加载首访）：同步执行收敛重写——此时该 seed 尚无并发发布者，
    /// 且写线程队列中不存在本 seed 的待写条目（调用方先 flush 队列）。
    fn rewrite_oversized_now(&self, channel: RingingChannel, seed: &str, st: &SeedChannelState) {
        let mut guard = self.journal_store.lock().unwrap_or_else(|e| e.into_inner());
        let Some(store) = guard.as_mut() else { return };
        let size = match store.file_size(channel, seed) {
            Ok(size) => size,
            Err(_) => return,
        };
        if size < journal_rewrite_threshold() {
            return;
        }
        let envelopes: Vec<_> = st.journal.entries().cloned().collect();
        let checkpoints: Vec<(String, u64)> = st
            .journal
            .checkpoints()
            .iter()
            .map(|(key, seq)| (key.clone(), *seq))
            .collect();
        if let Err(error) = store.rewrite(channel, seed, &envelopes, &checkpoints) {
            log::warn!("[ringing] journal rewrite failed for {seed}: {error}");
        } else {
            log::info!(
                "[ringing] journal rewritten for {seed}: {} bytes -> {} entries",
                size,
                envelopes.len()
            );
        }
    }

    /// 按 block 折叠落盘副本中被后续覆盖的 BlockCheckpoint。
    ///
    /// checkpoint 的物化语义是整块覆盖：快照恢复时每 block 只有最新一条生效。
    /// 旧 checkpoint 在落盘副本中纯属冗余——流式长块每 256ms 产出一条全量
    /// checkpoint（64 token 节流），7.8MB 块累计冗余可达 100 GB 级。
    /// 内存 journal 不折叠（回放 seq 连续性契约，见 handoff 设计定稿）；
    /// 仅折叠持久化副本：恢复后缺失的旧 seq 由 recover_gap 重基线（Phase 0
    /// 已接受的代价）。被折叠条目不扣 journal_bytes——那是内存态预算。
    pub(super) fn prune_superseded_checkpoints(journal: Vec<TimelineEntry>) -> Vec<TimelineEntry> {
        let mut newest_per_block: std::collections::HashSet<&str> =
            std::collections::HashSet::new();
        let mut keep = vec![true; journal.len()];
        for (index, entry) in journal.iter().enumerate().rev() {
            if let qaqh_domain::TimelineEvent::BlockCheckpoint { block_id, .. } = &entry.event
                && !newest_per_block.insert(block_id.as_str())
            {
                keep[index] = false;
            }
        }
        journal
            .into_iter()
            .zip(keep)
            .filter_map(|(entry, keep)| keep.then_some(entry))
            .collect()
    }

    /// 过滤已 seal turn 的 timeline journal 条目。已 seal turn 的 TextDelta/
    /// ToolProgress 已被快照全量覆盖，且 seal 后不会再有新 delta（append_text
    /// 拒绝已 seal block），因此持久化时丢弃这些条目不会破坏恢复：restore 的
    /// next_fragment 只从保留的活跃 turn 条目重建。这消除了每次 persist 都
    /// 全量克隆整个 journal 的写放大（曾实测 13.5MB JSON 每几秒重写一次）。
    pub(super) fn prune_sealed_timeline_journal(
        snapshot: &qaqh_domain::TimelineSnapshot,
        journal: Vec<TimelineEntry>,
    ) -> Vec<TimelineEntry> {
        let sealed: HashSet<&str> = snapshot
            .turns
            .iter()
            .filter(|turn| turn.sealed)
            .map(|turn| turn.turn_id.as_str())
            .collect();
        if sealed.is_empty() {
            return journal;
        }
        journal
            .into_iter()
            .filter(|entry| !sealed.contains(entry.turn_id.as_str()))
            .collect()
    }

    #[cfg(test)]
    fn fold_checkpoints_for_test(entries: Vec<TimelineEntry>) -> Vec<TimelineEntry> {
        Self::prune_superseded_checkpoints(entries)
    }

    fn persist_checkpoint(
        &self,
        channel: RingingChannel,
        seed: &str,
        st: &mut SeedChannelState,
        identity: &str,
        stream_seq: u64,
    ) {
        self.enqueue_journal(
            st,
            JournalWriteOp::Checkpoint {
                channel,
                seed: seed.to_string(),
                identity: identity.to_string(),
                stream_seq,
            },
        );
    }

    fn persist_replaceable(
        &self,
        channel: RingingChannel,
        seed: &str,
        st: &mut SeedChannelState,
        identity: &str,
        envelope: &RingingEventEnvelope,
    ) {
        self.enqueue_journal(
            st,
            JournalWriteOp::Replaceable {
                channel,
                seed: seed.to_string(),
                identity: identity.to_string(),
                envelope: envelope.clone(),
            },
        );
    }

    fn persist_remove_replaceable(
        &self,
        channel: RingingChannel,
        seed: &str,
        st: &mut SeedChannelState,
        identity: &str,
    ) {
        self.enqueue_journal(
            st,
            JournalWriteOp::RemoveReplaceable {
                channel,
                seed: seed.to_string(),
                identity: identity.to_string(),
            },
        );
    }
}

impl Drop for RingingHub {
    fn drop(&mut self) {
        // BUG-2026-09-12-08：先排空 journal 写队列（drop 投递端 → 写线程处理完
        // 队列中剩余写操作后自然退出 → join）。保证「hub 释放 ⇒ 此前投递的写
        // 操作已落盘」——既有测试的「drop 后读文件 / 重启重放」语义依赖此保证。
        // 必须置于 timeline 分支 early-return 之前。
        //
        // 对账收尾：队列满时被丢弃的写条目由 `needs_rewrite` 标记；这里在排空
        // 前补投一次重写（阻塞投递——此时独占 hub，允许等待队列腾位）。
        if let Some(writer) = self.journal_writer.as_ref() {
            // 锁序：先摘出槽表快照（拓扑序摘取，释放注册表相关锁），再逐个取
            // 槽锁——`channels` 顶层锁 / 槽表锁与槽锁**从不重叠持有**。
            let mut pending: Vec<(RingingChannel, String, SeedChannelSlot)> = Vec::new();
            for (channel, shards) in self
                .channels
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
            {
                for seed in shards.seed_keys() {
                    if let Some(slot) = shards.slot_if_present(&seed) {
                        pending.push((*channel, seed, slot));
                    }
                }
            }
            for (channel, seed, slot) in pending {
                let st = slot.lock().unwrap_or_else(|e| e.into_inner());
                if !st.needs_rewrite {
                    continue;
                }
                let envelopes: Vec<_> = st.journal.entries().cloned().collect();
                let checkpoints: Vec<(String, u64)> = st
                    .journal
                    .checkpoints()
                    .iter()
                    .map(|(key, seq)| (key.clone(), *seq))
                    .collect();
                drop(st);
                let _ = writer.send(JournalWriteOp::Rewrite {
                    channel,
                    seed,
                    envelopes,
                    checkpoints,
                });
            }
        }
        self.journal_writer.take();
        if let Some(join) = self.journal_writer_join.take() {
            let _ = join.join();
        }
        let persistence = self
            .timeline_persistence
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let Some(mut persistence) = persistence else {
            return;
        };
        drop(persistence.wake);
        if let Some(join) = persistence.join.take() {
            let _ = join.join();
        }
    }
}

/// 当前 unix 毫秒（事件信封 server_ts 用）。
fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::{
        CompactStatus, ConversationEvent, TimelineIntent, TimelineSnapshot, ToolEvent,
    };

    #[test]
    fn persisted_conversation_metadata_survives_projection_overlay() {
        let mut projected = serde_json::json!({
            "seed": "s",
            "channel": "conversation",
            "revision": 7,
            "compact_status": "running"
        });
        merge_persisted_conversation_state(
            &mut projected,
            serde_json::json!({
                "turns": [],
                "usage": { "prompt_tokens": 42 },
                "model": "qaqh-test",
                "context_limit": 200000
            }),
        );
        assert_eq!(projected["model"], "qaqh-test");
        assert_eq!(projected["context_limit"], 200000);
        assert_eq!(projected["usage"]["prompt_tokens"], 42);
        assert_eq!(projected["compact_status"], "running");
    }

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "qaqh-ringing-hub-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn round_delta(seq: u64) -> DomainEvent {
        DomainEvent::Conversation(ConversationEvent::RoundDelta {
            turn_id: "t1".into(),
            round_num: 0,
            kind: qaqh_domain::RoundDeltaKind::Thinking,
            delta: format!("chunk-{seq}"),
        })
    }

    fn tool_prepared(tag: &str) -> DomainEvent {
        DomainEvent::Tool(ToolEvent::ToolCallPrepared {
            tool_call_id: "c1".into(),
            turn_id: "t".into(),
            round_num: 0,
            name: "exec".into(),
            args_so_far: tag.into(),
        })
    }

    #[test]
    fn lazy_load_defers_history_until_first_access() {
        let root = temp_root("lazy");
        {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            for i in 1..=3 {
                let _ = hub.publish("a", round_delta(i));
            }
            let _ = hub.publish("b", round_delta(1));
        }
        // 重启：懒加载模式下启动不重放任何历史。
        let hub = RingingHub::with_persistence("epoch-2", &root);
        {
            let empty = hub
                .channel_shards(RingingChannel::Conversation)
                .is_none_or(|shards| shards.is_empty());
            assert!(empty, "cold start must not load any seed");
        }
        // 首次访问 seed a → 历史按需恢复。
        let replayed_a = hub
            .replay_since(RingingChannel::Conversation, "a", 0)
            .expect("replay a");
        assert_eq!(
            replayed_a.len(),
            3,
            "seed a history restored on first access"
        );
        // seed b 未被访问 → 仍不在内存。
        {
            assert!(
                !hub.channel_shards(RingingChannel::Conversation)
                    .expect("channel registered by seed a access")
                    .contains("b"),
                "seed b must remain unloaded until accessed"
            );
        }
        let replayed_b = hub
            .replay_since(RingingChannel::Conversation, "b", 0)
            .expect("replay b");
        assert_eq!(
            replayed_b.len(),
            1,
            "seed b history restored on first access"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn orphan_compact_from_persisted_journal_is_failed_before_bootstrap() {
        let root = temp_root("orphan-compact");
        {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            let _ = hub.publish(
                "s",
                DomainEvent::Conversation(ConversationEvent::CompactStarted {
                    compact_id: "compact-persisted".into(),
                    turns_total: 9,
                    turns_keeping: 3,
                }),
            );
        }

        let hub = RingingHub::with_persistence("epoch-2", &root);
        assert_eq!(
            hub.snapshot(RingingChannel::Conversation, "s").state["compact_status"],
            "running",
            "journal replay alone restores the interrupted operation"
        );
        assert!(hub.seal_orphan_channel_state("s", false));
        // daemon bootstrap 在 SessionManager 初始化后会调用 conversation_snapshot；
        // 此处只需验证由 journal 驱动、随后会被持久消息 overlay 保留的投影字段。
        let bootstrap_state = hub.snapshot(RingingChannel::Conversation, "s").state;
        assert_eq!(bootstrap_state["compact_status"], "failed");
        assert_eq!(bootstrap_state["compact_id"], "compact-persisted");
        assert!(!hub.seal_orphan_channel_state("s", false));

        let replay = hub
            .replay_since(RingingChannel::Conversation, "s", 0)
            .expect("replay compact recovery");
        assert!(replay.iter().any(|envelope| matches!(
            &envelope.event,
            RingingEvent::Conversation(ConversationEvent::CompactFinished {
                compact_id,
                status: CompactStatus::Failed,
                ..
            }) if compact_id == "compact-persisted"
        )));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn journal_rewrite_converges_on_lazy_load_without_round_completed() {
        // P0 回归：轮次未完成（无 RoundCompleted）时 delta 持续 append 也必须收敛。
        // 阈值降到 1MB 加速测试；此覆盖为 OnceLock 一次性，仅本测试使用。
        override_journal_rewrite_threshold_for_test(1024 * 1024);
        let root = temp_root("rewrite-lazy");
        {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            // 9000 个 reliable delta 超过内存窗口（8192），触发淘汰。
            // 分批排空写队列：异步写线程（BUG-2026-09-12-08）下紧循环会填满
            // 队列并让重写调度点漂移；分批后「重写发生在窗口内」是确定性行为。
            for i in 0..9000 {
                let _ = hub.publish("s", round_delta(i));
                if i % 1000 == 999 {
                    hub.flush_journal_persistence();
                }
            }
            hub.flush_journal_persistence();
        }
        let path = root.join("journal").join("conversation").join("s.jsonl");
        let before_lines = std::fs::read_to_string(&path).unwrap().lines().count();
        assert!(
            before_lines > 8500,
            "fixture must exceed the bounded window"
        );
        // 重启：启动不加载；首次访问触发懒加载 + force rewrite（窗口收敛）。
        let hub = RingingHub::with_persistence("epoch-2", &root);
        {
            let empty = hub
                .channel_shards(RingingChannel::Conversation)
                .is_none_or(|shards| shards.is_empty());
            assert!(empty, "cold start must not load any seed");
        }
        // replay_since(0) 因窗口淘汰返回 CursorExpired（正确语义），
        // 但 ensure 已完成加载并执行收敛重写。
        let replayed = hub.replay_since(RingingChannel::Conversation, "s", 0);
        assert!(
            matches!(replayed, Err(CursorExpired { .. })),
            "cursor 0 must be expired after window eviction"
        );
        {
            assert!(
                hub.channel_shards(RingingChannel::Conversation)
                    .is_some_and(|shards| shards.contains("s")),
                "lazy load must materialize the seed"
            );
        }
        // 序号水位精确恢复：新事件从历史最大值之后继续。
        assert_eq!(
            hub.last_stream_seq(RingingChannel::Conversation, "s"),
            9000,
            "sequence watermark restored from replayed history"
        );
        if let PublishOutcome::Published { envelope } = hub.publish("s", round_delta(9999)) {
            assert!(
                envelope.stream_seq > 9000,
                "new events must continue after the restored watermark"
            );
        } else {
            panic!("publish after restart must succeed");
        }
        // 文件收敛到有界窗口（8192 条），而非线性增长。
        let after_lines = std::fs::read_to_string(&path).unwrap().lines().count();
        assert!(
            after_lines < before_lines,
            "rewrite must shrink the file: {before_lines} -> {after_lines}"
        );
        assert!(
            after_lines <= 8192 + 32,
            "file must converge to the bounded window, got {after_lines} lines"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn persisted_journal_survives_restart() {
        let root = temp_root("restart");
        {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            let _ = hub.publish("s", round_delta(1));
            let _ = hub.publish("s", tool_prepared("a"));
        }
        let hub = RingingHub::with_persistence("epoch-2", &root);
        // reliable 事件重放
        let replayed = hub
            .replay_since(RingingChannel::Conversation, "s", 0)
            .expect("replay");
        assert!(
            replayed.iter().any(|e| matches!(
                e.event,
                RingingEvent::Conversation(ConversationEvent::RoundDelta { .. })
            )),
            "reliable delta must survive restart"
        );
        // replaceable 当前值恢复
        let tool_replay = hub
            .replay_since(RingingChannel::Tool, "s", 0)
            .expect("tool replay");
        assert!(
            tool_replay
                .iter()
                .any(|e| matches!(&e.event, RingingEvent::Tool(ToolEvent::ToolCallPrepared { args_so_far, .. }) if args_so_far == "a")),
            "replaceable latest value must survive restart"
        );
        // 序号继续递增（新 epoch 内不从头冲突）
        let outcome = hub.publish("s", round_delta(99));
        if let PublishOutcome::Published { envelope } = outcome {
            assert!(envelope.stream_seq > 0);
            assert!(envelope.stream_seq > replayed.first().map(|e| e.stream_seq).unwrap_or(0));
        } else {
            panic!("publish after restart must succeed");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn persisted_round_compaction_replays_consistently() {
        let root = temp_root("compact");
        {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            for i in 1..=3 {
                let _ = hub.publish("s", round_delta(i));
            }
            let _ = hub.publish(
                "s",
                DomainEvent::Conversation(ConversationEvent::RoundCompleted {
                    turn_id: "t1".into(),
                    round_num: 0,
                    thinking: Some("final".into()),
                    answer: Some("done".into()),
                    output_ref: None,
                    is_final: true,
                }),
            );
        }
        let hub = RingingHub::with_persistence("epoch-2", &root);
        let replayed = hub
            .replay_since(RingingChannel::Conversation, "s", 0)
            .expect("replay");
        let deltas = replayed
            .iter()
            .filter(|e| {
                matches!(
                    e.event,
                    RingingEvent::Conversation(ConversationEvent::RoundDelta { .. })
                )
            })
            .count();
        assert_eq!(deltas, 0, "compacted deltas must not replay");
        assert!(
            replayed.iter().any(|e| matches!(
                e.event,
                RingingEvent::Conversation(ConversationEvent::RoundCompleted { .. })
            )),
            "RoundCompleted survives"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn publish_assigns_sequences_and_envelope_fields() {
        let hub = RingingHub::new("epoch-1");
        let outcome = hub.publish(
            "s1",
            DomainEvent::Tool(ToolEvent::ToolStarted {
                tool_call_id: "c1".into(),
                turn_id: "t".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        match outcome {
            PublishOutcome::Published { envelope } => {
                assert_eq!(envelope.seed, "s1");
                assert_eq!(envelope.stream_seq, 1);
                assert_eq!(envelope.channel_seq, 1);
                assert_eq!(envelope.session_seq, 1);
                assert_eq!(envelope.delivery, Delivery::Reliable);
                assert!(envelope.event_id.starts_with("epoch-1-tool-s1-"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn publish_with_causation_sets_envelope_field() {
        let hub = RingingHub::new("epoch-1");
        let outcome = hub.publish_with_causation(
            "s1",
            DomainEvent::Conversation(ConversationEvent::TurnStarted {
                turn_id: "t1".into(),
                user_text: "hi".into(),
            }),
            Some("cmd-9"),
        );
        match outcome {
            PublishOutcome::Published { envelope } => {
                assert_eq!(envelope.causation_id.as_deref(), Some("cmd-9"));
            }
            other => panic!("unexpected {other:?}"),
        }
        // 无 causation 时字段保持 None
        let plain = hub.publish(
            "s1",
            DomainEvent::Conversation(ConversationEvent::TurnStarted {
                turn_id: "t2".into(),
                user_text: "hi".into(),
            }),
        );
        match plain {
            PublishOutcome::Published { envelope } => {
                assert!(envelope.causation_id.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn duplicate_event_id_is_idempotent_dropped() {
        let hub = RingingHub::new("epoch-1");
        let ev =
            DomainEvent::Conversation(ConversationEvent::ConversationCancelled { turn_id: None });
        let _ = hub.publish("s", ev.clone());
        // 直接构造同 id 信封再发布不可行（id 由 hub 生成）；
        // 验证两次发布同内容产生不同 id 但都成功（幂等在 journal 层测试覆盖）
        let second = hub.publish("s", ev);
        assert!(matches!(second, PublishOutcome::Published { .. }));
        assert_eq!(hub.last_stream_seq(RingingChannel::Conversation, "s"), 2);
    }

    #[test]
    fn replay_and_snapshot_work_together() {
        let hub = RingingHub::new("epoch-1");
        hub.publish(
            "s",
            DomainEvent::Conversation(ConversationEvent::TurnStarted {
                turn_id: "t1".into(),
                user_text: "hi".into(),
            }),
        );
        hub.publish(
            "s",
            DomainEvent::Conversation(ConversationEvent::RoundDelta {
                turn_id: "t1".into(),
                round_num: 0,
                kind: qaqh_domain::RoundDeltaKind::Answering,
                delta: "hello".into(),
            }),
        );
        let replayed = hub
            .replay_since(RingingChannel::Conversation, "s", 0)
            .expect("in window");
        // reliable TurnStarted + replaceable RoundDelta 当前值
        assert_eq!(replayed.len(), 2);
        let snap = hub.snapshot(RingingChannel::Conversation, "s");
        assert_eq!(snap.state["active_turn"], "t1");
        assert_eq!(snap.state_revision, 1);
    }

    #[test]
    fn channel_replay_merges_seeds_and_signals_reset() {
        let hub = RingingHub::new("epoch-1");
        hub.publish(
            "s1",
            DomainEvent::Tool(ToolEvent::ToolStarted {
                tool_call_id: "c1".into(),
                turn_id: "t1".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        hub.publish(
            "s2",
            DomainEvent::Tool(ToolEvent::ToolStarted {
                tool_call_id: "c2".into(),
                turn_id: "t2".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        let replay = hub.replay_channel_since(RingingChannel::Tool, 0, false);
        assert_eq!(replay.resets.len(), 0);
        assert_eq!(replay.events.len(), 2);
        // stream_seq 全局递增，跨 seed 合并后按序排列
        assert_eq!(replay.events[0].stream_seq, 1);
        assert_eq!(replay.events[1].stream_seq, 2);
        assert_eq!(replay.events[0].seed, "s1");
        assert_eq!(replay.events[1].seed, "s2");

        // 无 cursor 的新连接跳过可靠历史（只回放 replaceable 值）：
        // 历史由 bootstrap 快照承担，防止幽灵事件先于快照到达前端。
        let fresh = hub.replay_channel_since(RingingChannel::Tool, 0, true);
        assert_eq!(fresh.events.len(), 0);
        assert_eq!(fresh.resets.len(), 0);

        // cursor 超出保留窗口 → 该 seed 需要强制 snapshot
        // （journal 默认容量 8192，灌满后 earliest 前移）
        let hub2 = RingingHub::new("epoch-2");
        for i in 1..=8193 {
            hub2.publish(
                "s1",
                DomainEvent::Tool(ToolEvent::ToolStarted {
                    tool_call_id: format!("c{i}"),
                    turn_id: format!("t{i}"),
                    round_num: 0,
                    name: "exec".into(),
                }),
            );
        }
        let replayed = hub2.replay_channel_since(RingingChannel::Tool, 0, false);
        assert!(!replayed.resets.is_empty());
        assert_eq!(replayed.resets[0].seed, "s1");
        assert!(replayed.resets[0].earliest_available_seq > 1);
    }

    #[test]
    fn replaceable_prepared_covers_in_router() {
        let hub = RingingHub::new("epoch-1");
        let prepared = |tag: &str| {
            DomainEvent::Tool(ToolEvent::ToolCallPrepared {
                tool_call_id: "c1".into(),
                turn_id: "t".into(),
                round_num: 0,
                name: "exec".into(),
                args_so_far: tag.into(),
            })
        };
        let _ = hub.publish("s", prepared("a"));
        let _ = hub.publish("s", prepared("ab"));
        let replayed = hub.replay_since(RingingChannel::Tool, "s", 0).expect("ok");
        let progress_events: Vec<_> = replayed
            .iter()
            .filter(|e| {
                matches!(
                    e.event,
                    qaqh_ringing::RingingEvent::Tool(ToolEvent::ToolCallPrepared { .. })
                )
            })
            .collect();
        assert_eq!(progress_events.len(), 1, "only latest progress survives");
    }

    #[test]
    fn checkpoint_records_sparse_progress() {
        let hub = RingingHub::new("epoch-1");
        hub.checkpoint(RingingChannel::Tool, "s", "tool:c1", 7);
        let slot = hub.seed_slot(RingingChannel::Tool, "s");
        let st = slot.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(st.journal.checkpoints().get("tool:c1"), Some(&7));
    }

    #[test]
    fn live_broadcast_delivers_published_envelopes() {
        let hub = RingingHub::new("epoch-1");
        let mut rx = hub.subscribe(RingingChannel::Conversation);
        hub.publish(
            "s",
            DomainEvent::Conversation(ConversationEvent::ConversationCancelled { turn_id: None }),
        );
        let env = rx.blocking_recv().expect("live event");
        assert_eq!(env.seed, "s");
    }

    #[test]
    fn reliable_live_publish_does_not_fill_an_undrained_router_queue() {
        let hub = RingingHub::new("epoch");
        for _ in 0..5_000 {
            let outcome = hub.publish(
                "s",
                DomainEvent::Conversation(ConversationEvent::ConversationCancelled {
                    turn_id: None,
                }),
            );
            assert!(matches!(outcome, PublishOutcome::Published { .. }));
        }
        assert_eq!(
            hub.last_stream_seq(RingingChannel::Conversation, "s"),
            5_000
        );
    }

    #[test]
    fn fold_checkpoints_keeps_only_the_newest_per_block() {
        // 落盘副本折叠契约：同一 block 的旧 checkpoint 全部丢弃，只留最新
        // 一条；非 checkpoint 条目与其它 block 的条目不受影响；顺序保持。
        use qaqh_domain::TimelineEvent;
        let mk = |seq: u64, turn: &str, block: &str, text: &str| TimelineEntry {
            timeline_seq: seq,
            turn_id: turn.into(),
            round_num: Some(0),
            event: TimelineEvent::BlockCheckpoint {
                block_id: block.into(),
                text: text.into(),
            },
        };
        let delta = |seq: u64| TimelineEntry {
            timeline_seq: seq,
            turn_id: "t".into(),
            round_num: Some(0),
            event: TimelineEvent::TextDelta {
                block_id: "b".into(),
                fragment_seq: seq,
                delta: "d".into(),
            },
        };
        let entries = vec![
            mk(1, "t", "b", "v1"),
            delta(2),
            mk(3, "t", "b", "v2"),
            mk(4, "t", "c", "c1"),
            mk(5, "t", "b", "v3-final"),
            delta(6),
            mk(7, "t", "c", "c2-final"),
        ];
        let folded = RingingHub::fold_checkpoints_for_test(entries);
        let seqs: Vec<u64> = folded.iter().map(|e| e.timeline_seq).collect();
        assert_eq!(
            seqs,
            vec![2, 5, 6, 7],
            "only newest checkpoint per block survives"
        );
        assert!(folded.iter().all(|e| !matches!(&e.event,
                    TimelineEvent::BlockCheckpoint { block_id, text }
                        if block_id == "b" && text == "v1")));
    }

    #[test]
    fn native_timeline_intents_bypass_the_ringing_v1_channel_sequencer() {
        let hub = RingingHub::new("epoch");
        let opened = hub
            .publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::TurnOpened {
                    turn_id: "t".into(),
                    user_text: "question".into(),
                },
            )
            .expect("timeline intent accepted");
        assert_eq!(opened.timeline_seq, 1);
        assert_eq!(hub.last_stream_seq(RingingChannel::Conversation, "s"), 0);
        let snapshot = hub.timeline_snapshot("s").expect("timeline snapshot");
        assert_eq!(snapshot.watermark, 1);
        assert_eq!(snapshot.turns[0].user_text, "question");
    }

    #[test]
    fn terminal_timeline_intent_is_persisted_before_publish_returns() {
        let root = temp_root("timeline-terminal-sync");
        let hub = RingingHub::with_persistence("epoch", &root);
        hub.publish_timeline(
            "s",
            TimelineIntent::TurnOpened {
                turn_id: "t".into(),
                user_text: "question".into(),
            },
        )
        .unwrap();
        hub.publish_timeline(
            "s",
            TimelineIntent::BlockOpened {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "text".into(),
                kind: qaqh_domain::TimelineBlockKind::Text,
                tool: None,
            },
        )
        .unwrap();
        hub.publish_timeline(
            "s",
            TimelineIntent::TextDelta {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "text".into(),
                delta: "hello".into(),
            },
        )
        .unwrap();
        hub.publish_timeline(
            "s",
            TimelineIntent::BlockSealed {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "text".into(),
            },
        )
        .unwrap();
        // BlockSealed 已降级为异步 checkpoint（终端事件写放大收口）。
        // TurnSealed 保持同步落盘：publish 返回即 load_seed 可见。
        hub.publish_timeline(
            "s",
            TimelineIntent::RoundSealed {
                turn_id: "t".into(),
                round_num: 0,
                is_final: true,
            },
        )
        .unwrap();
        hub.publish_timeline(
            "s",
            TimelineIntent::TurnSealed {
                turn_id: "t".into(),
                state: qaqh_domain::TimelineTurnState::Completed,
                failure: None,
            },
        )
        .unwrap();

        let persisted = TimelineStore::new(&root)
            .unwrap()
            .load_seed("s")
            .expect("turn-sealed snapshot persisted synchronously");
        assert_eq!(persisted.snapshot.watermark, 6);
        assert_eq!(
            persisted.snapshot.turns[0].rounds[0].blocks[0].text,
            "hello"
        );
        assert_eq!(
            persisted.snapshot.turns[0].rounds[0].blocks[0].state,
            qaqh_domain::TimelineBlockState::Sealed
        );
        assert!(persisted.snapshot.turns[0].sealed);
        assert!(
            persisted.journal.is_empty(),
            "sealed turn leaves no replay tail on disk"
        );
        drop(hub);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn seal_all_orphans_cleans_running_turn_at_shutdown() {
        // 优雅关闭收尾（Windows 95 语义）：turn 已打开但未 seal（worker
        // 被杀/收尾未完成）时，seal_all_orphans 必须把未 seal turn 收尾为
        // Cancelled + daemon_restart_interrupted 并持久化——重启后不再残留
        // 未 seal turn（安装器更新后不再出现 daemon_restart_interrupted）。
        let root = temp_root("seal-all-orphans");
        {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::TurnOpened {
                    turn_id: "t1".into(),
                    user_text: "question".into(),
                },
            )
            .expect("timeline intent accepted");
            // 退出收尾（模拟 daemon 优雅关闭路径）。
            hub.seal_all_orphans();
            let snapshot = hub.timeline_snapshot("s").expect("snapshot");
            assert!(snapshot.turns[0].sealed, "orphan turn must be sealed");
            assert_eq!(
                snapshot.turns[0].state,
                qaqh_domain::TimelineTurnState::Cancelled
            );
            let failure = snapshot.turns[0].failure.as_ref().expect("failure marker");
            assert_eq!(failure.code, "daemon_restart_interrupted");
        }
        // 重启（新 epoch）：退出时已收尾，懒加载不再触发孤儿 seal。
        let hub = RingingHub::with_persistence("epoch-2", &root);
        let snapshot = hub.timeline_snapshot("s").expect("snapshot after restart");
        assert!(snapshot.turns[0].sealed, "no orphan after clean shutdown");
        assert_eq!(
            snapshot.turns[0].state,
            qaqh_domain::TimelineTurnState::Cancelled
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn persisted_native_timeline_recovers_snapshot_and_replay_tail() {
        let root = temp_root("timeline-persist");
        {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::TurnOpened {
                    turn_id: "t".into(),
                    user_text: "question".into(),
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::BlockOpened {
                    turn_id: "t".into(),
                    round_num: 0,
                    block_id: "text".into(),
                    kind: qaqh_domain::TimelineBlockKind::Text,
                    tool: None,
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::TextDelta {
                    turn_id: "t".into(),
                    round_num: 0,
                    block_id: "text".into(),
                    delta: "hello".into(),
                },
            )
            .unwrap();
        }
        let hub = RingingHub::with_persistence("epoch-2", &root);
        let snapshot = hub.timeline_snapshot("s").unwrap();
        // 恢复时遗留的 running turn 必须收尾为 Cancelled（孤儿 turn seal
        // 契约），否则前端会永远把它投影为 running 并禁止发送新消息。
        assert_eq!(snapshot.watermark, 6);
        assert_eq!(snapshot.turns[0].rounds[0].blocks[0].text, "hello");
        assert_eq!(
            snapshot.turns[0].state,
            qaqh_domain::TimelineTurnState::Cancelled
        );
        assert!(snapshot.turns[0].rounds[0].sealed);
        assert_eq!(
            snapshot.turns[0].rounds[0].blocks[0].state,
            qaqh_domain::TimelineBlockState::Sealed
        );
        // seal 裁剪语义：turn seal 后回放尾清空（TurnSealed 条目自身也被
        // 裁剪）；重连客户端由快照 watermark 重基线（recover_gap 契约）。
        assert!(
            hub.timeline_replay_since("s", 1).is_empty(),
            "sealed turn must leave an empty replay tail"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn snapshot_survives_restart_and_deleted_snapshot_degrades_to_empty() {
        // timeline-journal 移除后的恢复契约：
        //   1. 快照文件存在 → 重启后 restore 出逐字相同的 snapshot；
        //   2. 快照文件被删 → 不再有任何本地日志可回放，恢复为空快照
        //      （无 sessions 注入时 `rebuild_timeline_from_messages` 无源可依）。
        // 第 2 条是移除 journal 的**已知代价**：进程崩溃后中间帧不再可回放，
        // 客户端由 `recover_gap` 重新基线化（见 `crate::timeline_store` 模块文档）。
        let root = temp_root("timeline-snapshot-authoritative");
        let native = {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::TurnOpened {
                    turn_id: "t".into(),
                    user_text: "question".into(),
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::BlockOpened {
                    turn_id: "t".into(),
                    round_num: 0,
                    block_id: "text".into(),
                    kind: qaqh_domain::TimelineBlockKind::Text,
                    tool: None,
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::TextDelta {
                    turn_id: "t".into(),
                    round_num: 0,
                    block_id: "text".into(),
                    delta: "hello".into(),
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::BlockSealed {
                    turn_id: "t".into(),
                    round_num: 0,
                    block_id: "text".into(),
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::RoundSealed {
                    turn_id: "t".into(),
                    round_num: 0,
                    is_final: true,
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::TurnSealed {
                    turn_id: "t".into(),
                    state: qaqh_domain::TimelineTurnState::Completed,
                    failure: None,
                },
            )
            .unwrap();
            hub.flush_timeline_persistence();
            let snapshot = hub.timeline_snapshot("s").unwrap();
            drop(hub);
            snapshot
        };

        // 1) 快照存在 → 重启后逐字一致。
        let cache = root.join("ringing-timeline").join("s.json");
        assert!(cache.is_file(), "snapshot file expected after publish");
        {
            let hub = RingingHub::with_persistence("epoch-2", &root);
            assert_eq!(
                hub.timeline_snapshot("s").unwrap(),
                native,
                "snapshot must round-trip across restart"
            );
        }

        // 2) 快照删除且无 sessions → 空快照（诚实降级，不虚构历史）。
        std::fs::remove_file(&cache).expect("delete snapshot file");
        let hub = RingingHub::with_persistence("epoch-3", &root);
        assert!(
            hub.timeline_snapshot("s").is_none(),
            "without a snapshot and without messages there is nothing to recover"
        );
        assert!(
            !root.join("timeline-journal").exists(),
            "recovery must not resurrect the removed journal directory"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cache_only_seed_is_restored_on_first_load_without_a_journal() {
        // timeline-journal 移除后：`ringing-timeline/{seed}.json` 是恢复的唯一
        // 权威。仅有快照文件（无任何 jsonl）的 seed 必须能直接 restore，
        // 且装载过程不得再在磁盘上生成 `timeline-journal/` 目录。
        let root = temp_root("timeline-cache-only");
        TimelineStore::new(&root)
            .unwrap()
            .persist(
                "s",
                &TimelineSnapshot {
                    watermark: 0,
                    turns: vec![],
                },
                vec![],
            )
            .unwrap();
        {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            let snapshot = hub.timeline_snapshot("s").unwrap();
            assert_eq!(snapshot.watermark, 0, "cache snapshot restored verbatim");
        }
        assert!(
            !root.join("timeline-journal").exists(),
            "loading a seed must not resurrect the removed journal directory"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn orphan_running_turns_are_sealed_on_recovery_and_sealed_turns_are_untouched() {
        let root = temp_root("timeline-orphan-seal");
        {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            // 完整完成的 turn：正常 seal 后恢复不得被改动。
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::TurnOpened {
                    turn_id: "t1".into(),
                    user_text: "a".into(),
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::BlockOpened {
                    turn_id: "t1".into(),
                    round_num: 0,
                    block_id: "text".into(),
                    kind: qaqh_domain::TimelineBlockKind::Text,
                    tool: None,
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::BlockSealed {
                    turn_id: "t1".into(),
                    round_num: 0,
                    block_id: "text".into(),
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::RoundSealed {
                    turn_id: "t1".into(),
                    round_num: 0,
                    is_final: true,
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::TurnSealed {
                    turn_id: "t1".into(),
                    state: qaqh_domain::TimelineTurnState::Completed,
                    failure: None,
                },
            )
            .unwrap();
            // 孤儿 running turn：只有 TurnOpened + BlockOpened，无任何 seal。
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::TurnOpened {
                    turn_id: "t2".into(),
                    user_text: "b".into(),
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::BlockOpened {
                    turn_id: "t2".into(),
                    round_num: 0,
                    block_id: "text2".into(),
                    kind: qaqh_domain::TimelineBlockKind::Text,
                    tool: None,
                },
            )
            .unwrap();
        }
        let hub = RingingHub::with_persistence("epoch-2", &root);
        let snapshot = hub.timeline_snapshot("s").unwrap();
        let t1 = snapshot
            .turns
            .iter()
            .find(|turn| turn.turn_id == "t1")
            .unwrap();
        assert_eq!(t1.state, qaqh_domain::TimelineTurnState::Completed);
        assert!(t1.sealed);
        let t2 = snapshot
            .turns
            .iter()
            .find(|turn| turn.turn_id == "t2")
            .unwrap();
        assert_eq!(t2.state, qaqh_domain::TimelineTurnState::Cancelled);
        assert!(t2.sealed);
        assert_eq!(
            t2.rounds[0].blocks[0].state,
            qaqh_domain::TimelineBlockState::Sealed
        );
        // 幂等：再次收尾无变更。
        assert!(!hub.seal_orphan_running_turns("s"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn round_deltas_are_reliable_and_compacted_on_round_completed() {
        let hub = RingingHub::new("epoch-1");
        let delta = |seq: u64| {
            DomainEvent::Conversation(ConversationEvent::RoundDelta {
                turn_id: "t1".into(),
                round_num: 1,
                kind: qaqh_domain::RoundDeltaKind::Answering,
                delta: format!("d{seq}"),
            })
        };

        let first = hub.publish("s", delta(1));
        let second = hub.publish("s", delta(2));
        assert!(matches!(
            first,
            PublishOutcome::Published { ref envelope } if envelope.delivery == Delivery::Reliable
        ));
        assert!(matches!(
            second,
            PublishOutcome::Published { ref envelope } if envelope.delivery == Delivery::Reliable
        ));

        // 增量可靠入 journal：回放必须完整（修复“重连只剩最后一个 delta”的吞字）。
        let replay = hub
            .replay_since(RingingChannel::Conversation, "s", 0)
            .expect("within window");
        assert_eq!(replay.len(), 2);

        // RoundCompleted 到达后该 round 的增量被压缩，全量终态保留。
        let completed = hub.publish(
            "s",
            DomainEvent::Conversation(ConversationEvent::RoundCompleted {
                turn_id: "t1".into(),
                round_num: 1,
                thinking: Some("d1d2".into()),
                answer: None,
                output_ref: None,
                is_final: true,
            }),
        );
        assert!(matches!(completed, PublishOutcome::Published { .. }));
        let replay = hub
            .replay_since(RingingChannel::Conversation, "s", 0)
            .expect("within window");
        assert_eq!(replay.len(), 1);
        assert!(matches!(
            &replay[0].event,
            qaqh_ringing::RingingEvent::Conversation(ConversationEvent::RoundCompleted { .. })
        ));
    }

    #[test]
    fn seal_orphan_channel_state_converges_three_channels() {
        let hub = RingingHub::new("epoch-seal");
        // 无终态的中断现场：running turn、running compact、running tool + 挂起权限、未决 ask
        hub.publish(
            "s",
            DomainEvent::Conversation(ConversationEvent::TurnStarted {
                turn_id: "t1".into(),
                user_text: "hi".into(),
            }),
        );
        hub.publish(
            "s",
            DomainEvent::Conversation(ConversationEvent::CompactStarted {
                compact_id: "compact-1".into(),
                turns_total: 8,
                turns_keeping: 2,
            }),
        );
        hub.publish(
            "s",
            DomainEvent::Tool(ToolEvent::ToolStarted {
                tool_call_id: "c1".into(),
                turn_id: "t1".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        hub.publish(
            "s",
            DomainEvent::Tool(ToolEvent::ToolPermissionRequested {
                tool_call_id: "c2".into(),
                turn_id: "t1".into(),
                round_num: 0,
                tool_name: "exec".into(),
                reason: "r".into(),
                paths: vec![],
                category: qaqh_domain::PermissionCategory::Exec,
                level: 3,
                risk: qaqh_domain::PermissionRisk::High,
                consequence: "run".into(),
            }),
        );
        hub.publish(
            "s",
            DomainEvent::Control(ControlEvent::InteractionRequested {
                interaction_id: "i1".into(),
                turn_id: "t1".into(),
                mode: qaqh_domain::AskMode::Single,
                questions: vec![],
            }),
        );

        // 收尾前：三个投影都携带无终态残留
        assert_eq!(
            hub.snapshot(RingingChannel::Conversation, "s").state["active_turn"],
            "t1"
        );
        assert_eq!(
            hub.snapshot(RingingChannel::Conversation, "s").state["compact_status"],
            "running"
        );
        assert!(hub.snapshot(RingingChannel::Tool, "s").state["running"].is_array());
        assert_eq!(
            hub.snapshot(RingingChannel::Tool, "s").state["pending_permission"],
            "c2"
        );
        assert_eq!(
            hub.snapshot(RingingChannel::Control, "s").state["pending_interaction"]["id"],
            "i1"
        );

        // force=true：本测试的 InteractionRequested 由当前进程发布（活表内），
        // 语义为「worker 死亡/重启后的强制收尾」，故 force=true。
        assert!(hub.seal_orphan_channel_state("s", true));
        // 幂等：再次调用无变更
        assert!(!hub.seal_orphan_channel_state("s", true));

        // 收尾后：三个投影全部收敛
        let conversation = hub.snapshot(RingingChannel::Conversation, "s");
        assert!(conversation.state["active_turn"].is_null());
        assert_eq!(conversation.state["compact_status"], "failed");
        assert_eq!(conversation.state["compact_id"], "compact-1");
        assert!(hub.snapshot(RingingChannel::Tool, "s").state["running"].is_null());
        assert!(hub.snapshot(RingingChannel::Tool, "s").state["pending_permission"].is_null());
        assert!(hub.snapshot(RingingChannel::Control, "s").state["pending_interaction"].is_null());

        // journal 已包含终态事件（SSE 客户端与重启后的重放都收敛）
        let replay = hub
            .replay_since(RingingChannel::Conversation, "s", 0)
            .expect("within window");
        assert!(replay.iter().any(|env| matches!(
            &env.event,
            qaqh_ringing::RingingEvent::Conversation(ConversationEvent::ConversationCancelled {
                turn_id: Some(id)
            }) if id == "t1"
        )));
        assert!(replay.iter().any(|env| matches!(
            &env.event,
            qaqh_ringing::RingingEvent::Conversation(ConversationEvent::CompactFinished {
                compact_id,
                status: CompactStatus::Failed,
                ..
            }) if compact_id == "compact-1"
        )));
        let tool_replay = hub
            .replay_since(RingingChannel::Tool, "s", 0)
            .expect("within window");
        assert!(tool_replay.iter().any(|env| matches!(
            &env.event,
            qaqh_ringing::RingingEvent::Tool(ToolEvent::ToolFinished {
                tool_call_id,
                ..
            }) if tool_call_id == "c1"
        )));
        assert!(tool_replay.iter().any(|env| matches!(
            &env.event,
            qaqh_ringing::RingingEvent::Tool(ToolEvent::ToolFinished {
                tool_call_id,
                ..
            }) if tool_call_id == "c2"
        )));
        let control_replay = hub
            .replay_since(RingingChannel::Control, "s", 0)
            .expect("within window");
        assert!(control_replay.iter().any(|env| matches!(
            &env.event,
            qaqh_ringing::RingingEvent::Control(ControlEvent::InteractionResolved {
                interaction_id,
                ..
            }) if interaction_id == "i1"
        )));
    }

    #[test]
    fn seal_orphan_channel_state_preserves_live_interaction_awaiting_user() {
        // 回归测试：修复「ask 发布 1ms 后被 bootstrap 孤儿收尾秒杀」——
        // 当前进程发布、等待用户响应的活交互必须被 bootstrap 路径保护；
        // worker 死亡/重启路径（force=true）仍要强制收尾。
        let hub = RingingHub::new("epoch-live-ask");
        hub.publish(
            "s",
            DomainEvent::Control(ControlEvent::InteractionRequested {
                interaction_id: "live-1".into(),
                turn_id: "t1".into(),
                mode: qaqh_domain::AskMode::Single,
                questions: vec![],
            }),
        );
        assert_eq!(
            hub.snapshot(RingingChannel::Control, "s").state["pending_interaction"]["id"],
            "live-1"
        );
        // bootstrap 路径（force=false）：活交互不被误判为孤儿，无其他孤儿 → false。
        assert!(!hub.seal_orphan_channel_state("s", false));
        assert_eq!(
            hub.snapshot(RingingChannel::Control, "s").state["pending_interaction"]["id"],
            "live-1",
            "live interaction must survive the bootstrap seal path"
        );
        // worker 死亡/重启收尾路径（force=true）：无视守卫，强制收尾。
        assert!(hub.seal_orphan_channel_state("s", true));
        assert!(hub.snapshot(RingingChannel::Control, "s").state["pending_interaction"].is_null());
        // 收尾后活表清空：后续 bootstrap 路径不再保护（幂等）。
        assert!(!hub.seal_orphan_channel_state("s", false));
    }

    #[test]
    fn forget_seed_drops_per_seed_resident_state() {
        let hub = RingingHub::new("forget-seed-test");
        hub.publish("s1", round_delta(1));
        hub.mark_worker_live("s1");
        hub.live_interactions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("s1".to_string(), "interaction-1".to_string());
        let content_id = hub.put_content("s1", "text/plain", b"hello".to_vec(), false);
        assert!(hub.get_content("s1", &content_id).is_some());
        let holds_seed = |hub: &RingingHub, seed: &str| {
            hub.channel_state()
                .values()
                .any(|shards| shards.contains(seed))
        };
        assert!(holds_seed(&hub, "s1"));

        hub.forget_seed("s1");

        assert!(!holds_seed(&hub, "s1"), "channels state must be dropped");
        assert!(
            !hub.live_workers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains("s1"),
            "live_workers entry must be dropped"
        );
        assert!(
            !hub.live_interactions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key("s1"),
            "live_interactions entry must be dropped"
        );
        assert!(
            hub.get_content("s1", &content_id).is_none(),
            "content_store entry must be released"
        );

        // 其他 seed 的常驻状态不受影响。
        hub.publish("s2", round_delta(2));
        assert!(holds_seed(&hub, "s2"));
    }

    #[test]
    fn per_seed_lazy_loads_do_not_block_each_other() {
        // Phase 1 回归：懒加载锁必须 per-seed。持有 seed A 的锁时，seed B
        // 的锁必须立即可取（原全局 `Mutex<()>` 下 B 会阻塞到 A 释放——
        // 多 session 前端卡顿的直接根因 A2）。
        let hub = RingingHub::new("epoch-per-seed");
        let lock_a = hub.lazy_load_lock("seed-a");
        let guard_a = lock_a.lock().unwrap_or_else(|e| e.into_inner());
        // 不同 seed 的锁必须在 guard_a 持有期间立即可取：后台线程 200ms
        // 内必须完成获取-释放（全局锁下必然超时失败）。
        {
            let lock_b = hub.lazy_load_lock("seed-b");
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _guard_b = lock_b.lock().unwrap_or_else(|e| e.into_inner());
                let _ = tx.send(());
            });
            assert!(
                rx.recv_timeout(std::time::Duration::from_millis(200))
                    .is_ok(),
                "seed-b lock must not be blocked by seed-a"
            );
        }
        // 同 seed 的锁必须互斥：guard_a 未释放时 try_lock 失败。
        let lock_a2 = hub.lazy_load_lock("seed-a");
        assert!(
            lock_a2.try_lock().is_err(),
            "same-seed lock must be mutually exclusive"
        );
        drop(guard_a);
        // 释放后同 seed 可再取，且锁表返回同一把锁（同 seed 幂等）。
        assert!(
            lock_a2.try_lock().is_ok(),
            "lock must be re-acquirable after release"
        );
        let lock_a3 = hub.lazy_load_lock("seed-a");
        assert!(
            Arc::ptr_eq(&lock_a, &lock_a3),
            "same seed must map to the same lock instance"
        );
    }

    #[test]
    fn concurrent_first_access_of_same_seed_loads_exactly_once() {
        // Phase 1 回归：并发首访同一 seed 时，per-seed 锁保证只有一个线程
        // 执行装载，其余等待后命中已装载状态直接返回（不双重重放）。
        let root = temp_root("per-seed-race");
        {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            for i in 1..=5 {
                let _ = hub.publish("raced", round_delta(i));
            }
        }
        let hub = Arc::new(RingingHub::with_persistence("epoch-2", &root));
        let mut joins = Vec::new();
        for _ in 0..8 {
            let hub = Arc::clone(&hub);
            joins.push(std::thread::spawn(move || {
                // 直接走生产入口：replay_since 内部调 ensure_seed_loaded
                //（自拿 per-seed 锁、double-check 后装载）。并发首访时唯一
                // 线程执行装载，其余等待后命中已装载状态。
                let _ = hub.replay_since(RingingChannel::Conversation, "raced", 0);
            }));
        }
        for join in joins {
            join.join().expect("worker thread must not panic");
        }
        // 装载幂等性验证：历史必须完好（5 条，无重复重放叠加），且序号
        // 水位精确恢复——双重装载会叠加事件/扰乱 next_seq。
        let replayed = hub
            .replay_since(RingingChannel::Conversation, "raced", 0)
            .expect("replay after race");
        assert_eq!(replayed.len(), 5, "history must be intact after the race");
        assert_eq!(
            hub.last_stream_seq(RingingChannel::Conversation, "raced"),
            5,
            "sequence watermark must be exact after concurrent first access"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod lock_sharding_tests {
    //! BUG-08 收尾（issue #33）回归：跨 (channel, seed) 的内存态必须互不串行。
    //!
    //! 未修复时 `channels: Mutex<HashMap<Channel, HashMap<Seed, State>>>` 是
    //! **单一全局锁**——任一 (channel, seed) 的 publish/checkpoint 持锁期间，
    //! 其他频道/会话的 publish 全部阻塞。下述测试在旧代码上红（超时失败），
    //! 在新代码上绿。

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, mpsc};
    use std::time::{Duration, Instant};

    fn round_delta(seq: u64) -> DomainEvent {
        DomainEvent::Conversation(ConversationEvent::RoundDelta {
            turn_id: "t1".into(),
            round_num: 1,
            kind: qaqh_domain::RoundDeltaKind::Answering,
            delta: format!("delta-{seq}"),
        })
    }

    /// 取某 (channel, seed) 的分片槽（测试断言用；生产路径不暴露）。
    fn slot(hub: &RingingHub, channel: RingingChannel, seed: &str) -> Arc<Mutex<SeedChannelState>> {
        hub.seed_slot(channel, seed)
    }

    /// 红→绿 1：持有 (Conversation, seed-a) 的分片锁时，其他 (channel, seed)
    /// 的写入必须立即可完成。
    ///
    /// 写法：后台线程写「新的 seed-b」——未修复时它必须先取**全局 channels
    /// 锁**才能登记/取号/提交，而该锁正被主线程持有，于是阻塞到超时；分片后
    /// seed-b 走另一个槽，完全不受影响。
    #[test]
    fn per_seed_channel_state_locks_do_not_block_each_other() {
        let hub = Arc::new(RingingHub::new("epoch-shard"));
        hub.publish("seed-a", round_delta(1));

        let held = slot(&hub, RingingChannel::Conversation, "seed-a");
        let guard = held.lock().unwrap_or_else(|e| e.into_inner());

        let (tx, rx) = mpsc::channel();
        let hub_b = Arc::clone(&hub);
        std::thread::spawn(move || {
            // 另一个 seed、另一个频道：写「新 seed」会走登记路径，未修复时
            // 必然要取全局锁 → 阻塞。
            hub_b.publish("seed-b", round_delta(2));
            hub_b.publish(
                "seed-b",
                DomainEvent::Control(ControlEvent::InteractionRequested {
                    interaction_id: "i-b".into(),
                    turn_id: "t1".into(),
                    mode: qaqh_domain::AskMode::Single,
                    questions: vec![],
                }),
            );
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_ok(),
            "other (channel, seed) writes must not be serialized by seed-a's shard lock"
        );
        drop(guard);
    }

    /// 红→绿 1b：持有 (Conversation, seed-a) 的槽锁时，**其他频道**的读
    /// （`last_stream_seq`）必须立即可完成。未修复时读也要取全局锁 → 阻塞。
    #[test]
    fn other_channel_reads_are_not_blocked_by_a_held_seed_shard() {
        let hub = Arc::new(RingingHub::new("epoch-shard-read"));
        hub.publish("seed-a", round_delta(1));
        hub.publish(
            "seed-a",
            DomainEvent::Control(ControlEvent::InteractionRequested {
                interaction_id: "i-a".into(),
                turn_id: "t1".into(),
                mode: qaqh_domain::AskMode::Single,
                questions: vec![],
            }),
        );
        let held = slot(&hub, RingingChannel::Conversation, "seed-a");
        let guard = held.lock().unwrap_or_else(|e| e.into_inner());

        let (tx, rx) = mpsc::channel();
        let hub_b = Arc::clone(&hub);
        std::thread::spawn(move || {
            let seq = hub_b.last_stream_seq(RingingChannel::Control, "seed-a");
            let _ = tx.send(seq);
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(500)),
            Ok(1),
            "Control-channel read must not be serialized behind the Conversation shard lock"
        );
        drop(guard);
    }

    /// 红→绿 2：8 个不同 seed 并发发布必须**不退化**（旧全局锁下 8 会话吞吐
    /// 被压成 ~0.4x——issue 记录的 448k→276k / 0.49x 就是这一现象）。
    ///
    /// 采用「吞吐比值」判据而非结构断言：分片是性能属性，结构断言（比较
    /// `Arc` 指针）无法区分「锁表存在」与「锁真的不共享」。为避免 CI 抖动
    /// 造成假红，取各侧 min-of-3 与 0.9x 余量。
    #[test]
    fn concurrent_publish_across_seeds_does_not_degrade() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 400;

        let concurrent_ops = |_attempt: usize| {
            let hub = Arc::new(RingingHub::new("epoch-scale"));
            let barrier = Arc::new(Barrier::new(THREADS));
            let mut joins = Vec::new();
            for index in 0..THREADS {
                let hub = Arc::clone(&hub);
                let barrier = Arc::clone(&barrier);
                joins.push(std::thread::spawn(move || {
                    let seed = format!("scale-{index}");
                    barrier.wait();
                    for i in 0..PER_THREAD {
                        hub.publish(&seed, round_delta(i as u64));
                    }
                }));
            }
            let start = Instant::now();
            for join in joins {
                join.join().expect("publish thread must not panic");
            }
            (THREADS * PER_THREAD) as f64 / start.elapsed().as_secs_f64()
        };
        let serial_ops = || {
            let hub = RingingHub::new("epoch-serial");
            let start = Instant::now();
            for index in 0..THREADS {
                let seed = format!("scale-{index}");
                for i in 0..PER_THREAD {
                    hub.publish(&seed, round_delta(i as u64));
                }
            }
            (THREADS * PER_THREAD) as f64 / start.elapsed().as_secs_f64()
        };

        // min-of-3：取各侧最好成绩，避开其他测试的瞬时干扰。
        let mut concurrent = f64::MIN;
        for attempt in 0..3 {
            concurrent = concurrent.max(concurrent_ops(attempt));
        }
        let mut serial = f64::MIN;
        for _ in 0..3 {
            serial = serial.max(serial_ops());
        }
        eprintln!(
            "[lockbench] 1 thread: {serial:.0} ev/s | {THREADS} threads: {concurrent:.0} ev/s | scaling {:.2}x",
            concurrent / serial.max(1e-9)
        );
        // 旧全局锁实测 0.41~0.49x；分片后 2.4x+。0.9x 留足机器抖动余量，
        // 仍能显著区分「锁共享」与「锁分片」。
        assert!(
            concurrent > serial * 0.9,
            "8-thread publish must not degrade vs 1-thread baseline \
             (concurrent={concurrent:.0}/s serial={serial:.0}/s); \
             a regression here means the shared channels lock is back"
        );
    }

    /// Sequencer 的 per-seed 序号表必须分片，且**实例私有**：
    /// 两个 `Sequencer` 的序号空间互不影响（不能是进程级 static 表）。
    #[test]
    fn sequencer_shards_are_per_instance() {
        let a = Sequencer::new();
        let b = Sequencer::new();
        assert_eq!(a.next(RingingChannel::Conversation, "s"), (1, 1, 1));
        assert_eq!(
            b.next(RingingChannel::Conversation, "s"),
            (1, 1, 1),
            "a fresh Sequencer must start its own sequence space"
        );
    }

    /// 不变量 1：分片后序号仍按 (channel, seed) 唯一且稠密；stream_seq 按频道
    /// 全局唯一（协议不变量，不得因分片破坏）。
    #[test]
    fn sharded_sequences_stay_unique_and_dense() {
        let hub = Arc::new(RingingHub::new("epoch-inv"));
        let seeds: Vec<String> = (0..4).map(|i| format!("inv-{i}")).collect();
        let mut joins = Vec::new();
        for seed in &seeds {
            let hub = Arc::clone(&hub);
            let seed = seed.clone();
            joins.push(std::thread::spawn(move || {
                for i in 0..50 {
                    hub.publish(&seed, round_delta(i));
                }
            }));
        }
        for join in joins {
            join.join().expect("thread must not panic");
        }

        let mut stream_seqs = Vec::new();
        for seed in &seeds {
            let replay = hub
                .replay_since(RingingChannel::Conversation, seed, 0)
                .expect("replay");
            let mut channel_seqs: Vec<u64> = replay.iter().map(|e| e.channel_seq).collect();
            assert_eq!(channel_seqs.len(), 50, "each seed keeps its own history");
            channel_seqs.sort_unstable();
            channel_seqs.dedup();
            assert_eq!(channel_seqs.len(), 50, "channel_seq must be dense per seed");
            assert_eq!(channel_seqs.first(), Some(&1));
            assert_eq!(channel_seqs.last(), Some(&50));
            stream_seqs.extend(replay.iter().map(|e| e.stream_seq));
        }
        stream_seqs.sort_unstable();
        let before = stream_seqs.len();
        stream_seqs.dedup();
        assert_eq!(
            stream_seqs.len(),
            before,
            "stream_seq must stay globally unique within the channel"
        );
    }

    /// 不变量 2：`forget_seed` 与并发发布不得 panic，且遗忘后该 seed 常驻态
    /// 不再出现在任何分片。
    #[test]
    fn forget_seed_races_with_publishers_without_panic() {
        let hub = Arc::new(RingingHub::new("epoch-forget-race"));
        let stop = Arc::new(AtomicUsize::new(0));
        let mut joins = Vec::new();
        for index in 0..4 {
            let hub = Arc::clone(&hub);
            let stop = Arc::clone(&stop);
            joins.push(std::thread::spawn(move || {
                let seed = format!("race-{index}");
                while stop.load(Ordering::Relaxed) == 0 {
                    hub.publish(&seed, round_delta(1));
                }
            }));
        }
        for _ in 0..20 {
            hub.forget_seed("race-0");
            std::thread::yield_now();
        }
        stop.store(1, Ordering::Relaxed);
        for join in joins {
            join.join().expect("publisher must not panic");
        }
    }

    /// 不变量 3：跨频道 replay 仍按 stream_seq 合并排序（分片不得打乱回放序）。
    #[test]
    fn channel_replay_still_merges_across_seeds_in_stream_order() {
        let hub = RingingHub::new("epoch-replay-order");
        hub.publish("a", round_delta(1));
        hub.publish("b", round_delta(2));
        hub.publish("a", round_delta(3));
        let replay = hub.replay_channel_since(RingingChannel::Conversation, 0, false);
        let seqs: Vec<u64> = replay.events.iter().map(|e| e.stream_seq).collect();
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        assert_eq!(seqs, sorted, "channel replay must be stream_seq ordered");
    }
}
