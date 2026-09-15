//! ringing::timeline_hub — timeline 投影子系统（持久化/懒加载/发布/快照）。
//!
//! 由 `hub.rs` 拆分（Phase 2-6）：`impl RingingHub` 跨文件块。对外 API 不变；
//! `Drop`/锁语义保留在 `hub.rs`。
//!
//! 2026-09-10：timeline-journal（append-only jsonl 权威日志）已移除。恢复语义现为
//! 「内存投影 + 原子替换快照 + messages 重建」三层，详见 `crate::timeline_store`。

use std::collections::HashSet;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

use qaqh_domain::{TimelineEntry, TimelineIntent, TimelineSnapshot};
use tokio::sync::broadcast;

use super::hub::RingingHub;
use super::hub::{TIMELINE_PERSIST_INTERVAL, TimelinePersistence};
use crate::{TimelineAppender, TimelineError, TimelineLiveEntry};

/// 落后判定的尾部读取窗口（条数）。与 `timeline_rebuild` 的投影窗口同量级：
/// 只用于「快照最后一回合是否仍是归档最后一回合」的同一性确认，不物化历史。
const RECONCILE_TAIL_MESSAGES: usize = 200;

/// 深翻页的归档首读条数（不足则加倍重试）。
const DEEP_PAGE_INITIAL_MESSAGES: usize = 200;
/// 深翻页的硬上限：再往前的历史不再读，如实报 `truncated_before`。
/// 4000 条消息 ≈ 数百轮会话，覆盖现实长会话；GB 级归档也不会被一次请求拖垮。
const DEEP_PAGE_MAX_MESSAGES: usize = 4000;

impl RingingHub {
    /// Move timeline checkpoint I/O off the producer/writer hot path.
    ///
    /// The live TimelineAppender remains the sole source of sequence allocation and
    /// broadcast ordering. Persistence is a best-effort, single-writer checkpoint
    /// queue: notifications are coalesced per seed for a fixed checkpoint window,
    /// and the worker snapshots the latest in-memory state only when the window
    /// expires. The on-disk record shape is unchanged, so bootstrap/replay
    /// compatibility is preserved. Terminal intents still use synchronous
    /// persistence as the recovery boundary.
    pub(super) fn start_timeline_persistence(&self) {
        let enabled = self
            .timeline_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        if !enabled {
            return;
        }

        let (wake, rx) = mpsc::channel::<()>();
        let pending_seeds = Arc::new(Mutex::new(HashSet::<String>::new()));
        let terminal_seeds = Arc::new(Mutex::new(HashSet::<String>::new()));
        let pending_for_worker = Arc::clone(&pending_seeds);
        let terminal_for_worker = Arc::clone(&terminal_seeds);
        let timeline = Arc::clone(&self.timeline);
        let timeline_store = Arc::clone(&self.timeline_store);
        let join = match std::thread::Builder::new()
            .name("qaqh-timeline-persist".into())
            .spawn(move || {
                let persist_pending = || {
                    // 回合边界（TurnSealed）优先：每轮先把 terminal 集合排空，
                    // 再处理 1s 窗口内的结构事件。两者共用本线程 → 串行、FIFO，
                    // 且每次落盘都取"当前"内存态，后写永远更新。
                    let mut seeds: Vec<String> = {
                        let mut terminal = terminal_for_worker
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        terminal.drain().collect()
                    };
                    {
                        let mut pending =
                            pending_for_worker.lock().unwrap_or_else(|e| e.into_inner());
                        for seed in pending.drain() {
                            if !seeds.contains(&seed) {
                                seeds.push(seed);
                            }
                        }
                    }
                    for seed in seeds {
                        // Seal 卸载必须先于快照：先把完整 turn 写入 sidecar，
                        // 再让内存进入壳模式，最后由 rehydrate 生成完整落盘快照。
                        // 整个流程运行在持久化 worker 上，不阻塞发布线程。
                        offload_all_sealed_turns_from(&timeline, &timeline_store, &seed);
                        // 锁序：先在 timeline 锁内取一份自洽快照，释放后再取
                        // store 锁。两条持久化路径都不允许嵌套持有两把锁。
                        let Some((snapshot, journal, audit_entries)) = ({
                            let timeline = timeline.lock().unwrap_or_else(|e| e.into_inner());
                            timeline.snapshot(&seed).map(|snapshot| {
                                let journal = timeline.replay_since(&seed, 0);
                                let audit_entries = journal.clone();
                                let journal =
                                    Self::prune_sealed_timeline_journal(&snapshot, journal);
                                let journal = Self::prune_superseded_checkpoints(journal);
                                (snapshot, journal, audit_entries)
                            })
                        }) else {
                            continue;
                        };
                        // 快照是唯一持久化产物（journal 已移除）：直接选取当前
                        // 内存态并原子替换写盘。已 seal turn 的条目不再进
                        // replay tail（见 `prune_sealed_timeline_journal`）。
                        let mut store = timeline_store.lock().unwrap_or_else(|e| e.into_inner());
                        let Some(store) = store.as_mut() else {
                            continue;
                        };
                        // 顺带在本 checkpoint 窗口追加轻量审计行（seq/ts/type）：
                        // 借助已有的 1s 合并窗口，不落到流式热路径上。
                        store.append_audit(&seed, &audit_entries);
                        // offload 壳补齐（异步窗口；磁盘文件始终完整）。
                        let snapshot = rehydrate_offloaded_turns(store, &seed, snapshot);
                        if let Err(error) = store.persist(&seed, &snapshot, journal) {
                            log::warn!("[timeline] persist failed for {seed}: {error}");
                        }
                    }
                };

                while rx.recv().is_ok() {
                    // 立即处理一轮：terminal（TurnSealed）的唤醒必须马上落盘，
                    // 不能等 1s 窗口——那是回合恢复边界。
                    persist_pending();
                    // Fixed window rather than a quiet-period debounce: a long,
                    // uninterrupted model stream still receives periodic crash
                    // checkpoints without rewriting at disk speed.
                    let deadline = Instant::now() + TIMELINE_PERSIST_INTERVAL;
                    let mut disconnected = false;
                    loop {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            break;
                        }
                        match rx.recv_timeout(remaining) {
                            Ok(()) => {}
                            Err(mpsc::RecvTimeoutError::Timeout) => break,
                            Err(mpsc::RecvTimeoutError::Disconnected) => {
                                disconnected = true;
                                break;
                            }
                        }
                    }
                    persist_pending();
                    if disconnected {
                        return;
                    }
                }
                // Drain the final coalesced notifications before the worker exits.
                persist_pending();
            }) {
            Ok(join) => join,
            Err(error) => {
                log::warn!("[timeline] persistence worker unavailable: {error}");
                return;
            }
        };

        *self
            .timeline_persistence
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(TimelinePersistence {
            wake,
            pending_seeds,
            terminal_seeds,
            join: Some(join),
        });
    }

    pub(super) fn request_timeline_persistence(&self, seed: &str) {
        let persistence = self
            .timeline_persistence
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(persistence) = persistence.as_ref() else {
            return;
        };
        let should_wake = persistence
            .pending_seeds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(seed.to_string());
        if should_wake {
            let _ = persistence.wake.send(());
        }
    }

    /// 回合边界（TurnSealed）入队：写进 terminal 集合并**立即唤醒** worker。
    ///
    /// 语义从"发布会话线程同步全量重写"变为"优先于 1s 软窗口的毫秒级异步
    /// 落盘"；顺序由单一 worker 线程 + 每次取当前内存态保证——terminal 之后
    /// 到达的写不会丢失，软窗口也不会让旧快照盖过它。
    pub(super) fn request_timeline_terminal_persistence(&self, seed: &str) {
        let persistence = self
            .timeline_persistence
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(persistence) = persistence.as_ref() else {
            return;
        };
        persistence
            .terminal_seeds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(seed.to_string());
        let _ = persistence.wake.send(());
    }

    /// 启动装载（懒加载模式）：只扫描磁盘 timeline seed 清单，不 restore 任何
    /// 快照。内存态（TimelineAppender）在首次访问该 seed 时由
    /// `ensure_timeline_loaded` 从磁盘按需恢复。
    pub(super) fn load_timeline_persisted(&self) {
        let mut seeds = HashSet::new();
        {
            let guard = self
                .timeline_store
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                Some(store) => match store.list_seeds() {
                    Ok(cache_seeds) => seeds.extend(cache_seeds),
                    Err(error) => log::warn!("[timeline] cache index failed: {error}"),
                },
                None => return,
            }
        }
        let total = seeds.len();
        *self
            .disk_timeline_seeds
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = seeds;
        log::info!("[ringing] lazy timeline index ready: {total} persisted timelines on disk");
    }

    /// 懒加载：确保 seed 的 timeline 快照 + replay tail 已 restore 入内存。
    ///
    /// - 已在内存或磁盘无记录：零成本返回；
    /// - 磁盘有记录：读取该 seed 的持久化快照 → restore → 收尾孤儿 running
    ///   turn（原 `load_timeline_persisted` 语义，有变更则同步写回）。
    pub(super) fn ensure_timeline_loaded(&self, seed: &str) {
        // 登记（幂等）：退出时 seal_all_orphans 需要覆盖全部已知 seed——
        // 包括本次运行新建、尚未异步落盘的 seed（异步 checkpoint 落盘前
        // 磁盘清单还没有它，但内存里已有未 seal turn）。
        self.disk_timeline_seeds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(seed.to_string());
        let lock = self.lazy_load_lock(seed);
        let _serial = lock.lock().unwrap_or_else(|e| e.into_inner());
        if self
            .timeline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(seed)
        {
            self.offload_all_sealed_turns(seed);
            return;
        }
        if !self
            .disk_timeline_seeds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(seed)
        {
            self.enable_turn_offload(seed);
            return;
        }
        // 快照即权威（2026-09-10 起）：不再有 append-only 日志需要重放，
        // 恢复路径只需装载 `ringing-timeline/{seed}.json`。
        let persisted_cache = {
            let mut store = self
                .timeline_store
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match store.as_mut() {
                Some(store) => store.load_seed(seed),
                None => return,
            }
        };

        match persisted_cache {
            Some(persisted) => {
                // 快照可能落后于写侧事实：timeline 是**可重建投影**，
                // `messages.jsonl` 才是历史真源（BUG-006 语义）。持久化中断
                // （崩溃/冻结窗口）会把快照冻结在某一刻，此后每次启动都会把
                // 它当作权威装载，前端于是只看到那一刻之前的回合（实测：重启
                // 后只剩第一条 user 消息，其余回合永久不可见）。因此落后判定
                // 必须在装载**之前**做——先装载再重建会被 `contains` 幂等门
                // 挡掉，出现"内存旧、磁盘新"的分叉。
                if self.persisted_timeline_is_behind(seed, &persisted.snapshot) {
                    log::warn!(
                        "[ringing] timeline {seed} is behind persisted messages ({} turns in \
                         snapshot) — rebuilding projection before restore",
                        persisted.snapshot.turns.len()
                    );
                    if self.rebuild_timeline_from_messages(seed) {
                        self.enable_turn_offload(seed);
                        if self.seal_orphan_running_turns(seed) {
                            self.persist_timeline_sync(seed);
                        }
                        log::info!("[ringing] lazily loaded timeline {seed} (rebuilt)");
                        return;
                    }
                    log::warn!(
                        "[ringing] timeline rebuild unavailable for {seed}; keeping persisted \
                         snapshot"
                    );
                }
                let mut appender = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
                if !appender.contains(seed) {
                    appender.restore(
                        persisted.seed.clone(),
                        persisted.snapshot.clone(),
                        persisted.journal.clone(),
                    );
                }
                appender.enable_offload(seed);
                drop(appender);
            }
            None => {
                // 无快照 → BUG-006：从 messages.jsonl / compact-context 重建投影。
                if self.rebuild_timeline_from_messages(seed) {
                    self.enable_turn_offload(seed);
                }
                return;
            }
        }

        self.offload_all_sealed_turns(seed);
        // 上次运行遗留的孤儿 running turn 在此收尾（见 seal_orphan_running_turns）。
        // 有变更时同步落盘快照，使下次启动可直接 restore。
        if self.seal_orphan_running_turns(seed) {
            self.persist_timeline_sync(seed);
        }
        log::info!("[ringing] lazily loaded timeline {seed}");
    }

    /// 持久化快照是否落后于写侧事实（`messages.jsonl`）。
    ///
    /// 该 seed 会话持久化的**真实回合总数**，与 timeline 物化窗口无关。
    ///
    /// T-08：重建路径（[`super::timeline_rebuild::rebuild_timeline_snapshot`]）只
    /// 物化最近 [`super::timeline_rebuild::REBUILD_RECENT_TURNS`] 轮，若分页响应
    /// 把「物化窗口大小」当总数上报，客户端会以为历史就这么多，于是既不提示
    /// 被裁剪、也无从知道更早的回合存在。
    ///
    /// 取不到（无 sessions / 无 meta）返回 `None`，调用方按**不可判定**保守处理
    /// ——宁可报 `false`，也不要谎报「还有更多」。
    pub fn persisted_turn_count(&self, seed: &str) -> Option<usize> {
        Some(self.sessions.as_deref()?.load_meta(seed)?.turn_count)
    }

    /// 两级判定，廉价在前：
    /// 1. **数量门**：快照回合数不得少于会话已完成回合数（`meta.turn_count`）
    ///    减一——运行中回合可能已开而 meta 尚未更新。不触发即直接判新鲜，
    ///    不产生任何额外 I/O（只读一个小 meta 文件）。
    /// 2. **同一性确认**：仅当数量门判定落后时才读归档尾部，比较"归档最后一回合
    ///    = 快照最后一回合"。重建窗口（`timeline_rebuild::REBUILD_RECENT_TURNS`）
    ///    本身会让长会话的快照天然少于 `turn_count`，用"尾部一致"排除这种正常态，
    ///    否则每次装载都会重建 → 自激写盘。
    ///
    /// 只比尾部 → 幂等：重建后的快照尾部必与归档一致，不会反复重建。
    /// 无法判定（无 sessions、无 meta、归档投影不出回合）一律返回 false，
    /// 保持"宁可少动"的保守姿态。
    fn persisted_timeline_is_behind(&self, seed: &str, snapshot: &TimelineSnapshot) -> bool {
        let Some(sessions) = self.sessions.as_deref() else {
            return false;
        };
        let Some(meta) = sessions.load_meta(seed) else {
            return false;
        };
        if snapshot.turns.len() + 1 >= meta.turn_count {
            return false;
        }
        // 读归档而非 compact 视图：快照本身就由归档重建（见
        // `timeline_rebuild::rebuild_timeline_snapshot`），两边同源才谈得上「尾部一致」。
        // 最末回合在两个视图里本是同一个，故这条改动不改变既有判定结果。
        let Some(messages) = sessions.load_archive_tail(seed, RECONCILE_TAIL_MESSAGES) else {
            return false;
        };
        let (_, archive_turns) =
            super::projection::project_recent_turns_from_messages(seed, &messages, 1);
        let Some(last_archive) = archive_turns.last() else {
            return false;
        };
        match snapshot.turns.last() {
            // 尾回合不同一：快照停在了更早的输入上（冻结窗口 / 回合丢失）。
            Some(last_snapshot) => last_snapshot.user_text != last_archive.user_text,
            // 归档有回合而快照没有任何回合：时序上必然落后。
            None => true,
        }
    }

    /// BUG-006：timeline 目录缺失/记录损坏/记录落后时，它必须能从 messages.jsonl /
    /// compact-context 重建，否则 timeline 就不是"可重建投影"，而会变成第二份
    /// 事实源。重建结果与 conversation snapshot 同一基线（compact 优先），
    /// 并同步写回 timeline 缓存 + timeline journal（保证下次也 journal 权威）。
    ///
    /// 返回 `true` = 已用重建结果接管该 seed 的 timeline（调用方无需再装载旧快照）。
    pub(super) fn rebuild_timeline_from_messages(&self, seed: &str) -> bool {
        let Some((snapshot, journal)) =
            super::timeline_rebuild::rebuild_timeline_snapshot(self.sessions.as_deref(), seed)
        else {
            return false;
        };
        {
            let mut appender = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
            if !appender.contains(seed) {
                appender.restore(seed.to_string(), snapshot.clone(), journal.clone());
            }
            appender.enable_offload(seed);
        }
        {
            let mut store = self
                .timeline_store
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(store) = store.as_mut()
                && let Err(error) = store.persist(seed, &snapshot, journal)
            {
                log::warn!("[timeline] rebuild persist failed for {seed}: {error}");
            }
        }
        self.offload_all_sealed_turns(seed);
        log::info!("[ringing] rebuilt timeline {seed} from persisted messages (BUG-006 fallback)");
        true
    }

    /// 接收原生 Ringing V1 timeline producer intent。此路径不接受 Agent2Ui 或 RingingEvent，
    /// 因而不会形成旧协议包装链。
    pub fn publish_timeline(
        &self,
        seed: &str,
        intent: TimelineIntent,
    ) -> Result<TimelineEntry, TimelineError> {
        // P1: 懒加载——publish 前确保该 seed 历史 timeline 已 restore，
        // 否则新条目会与磁盘快照断链（replay tail 丢失历史）。
        self.ensure_timeline_loaded(seed);
        // TurnSealed 是回合恢复边界，但**不再在发布会话线程上同步全量重写
        // 快照**（生产 17 MiB 快照实测 60 ms+，直接冻结发布方）。它改为进
        // terminal 优先队列：同一持久化 worker 立即排空，落盘仍取当前内存态。
        // 显式同步边界（flush_timeline_persistence / Drop / 优雅关闭）保持
        // fail-closed；崩溃窗口 ≈ 毫秒级，与既有异步 checkpoint 同量级。
        let is_turn_sealed = matches!(intent, TimelineIntent::TurnSealed { .. });
        let entry = {
            let mut timeline = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
            timeline.apply_intent(seed, intent)?
        };
        if is_turn_sealed {
            self.request_timeline_terminal_persistence(seed);
        } else {
            self.request_timeline_persistence(seed);
        }
        let _ = self.timeline_live.send(TimelineLiveEntry {
            seed: seed.to_string(),
            entry: entry.clone(),
        });
        Ok(entry)
    }

    /// 同步写入一个 seed 的 timeline 快照 + replay tail（daemon 优雅关闭或
    /// terminal intent 时调用）。从 pending 集合移除，避免异步线程重复写。
    pub(super) fn persist_timeline_sync(&self, seed: &str) {
        // Drop the pending flag so the async worker does not rewrite the same
        // seed again; the synchronous write below is strictly newer.
        if let Some(persistence) = self
            .timeline_persistence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            persistence
                .pending_seeds
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(seed);
            persistence
                .terminal_seeds
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(seed);
        }
        // 显式同步边界仍是 fail-closed：先把完整 turn 写进 sidecar，再生成
        // 完整快照。这里是关闭/恢复路径，不在发布热路径上。
        self.offload_all_sealed_turns(seed);
        // 先在 timeline 锁内克隆自洽状态，释放后再取 store 锁。这样 seal
        // 路径的 sidecar I/O、持久化 worker 与同步写盘都不会嵌套两把锁。
        let Some((snapshot, journal, audit_entries)) = ({
            let timeline = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
            timeline.snapshot(seed).map(|snapshot| {
                let journal = timeline.replay_since(seed, 0);
                let audit_entries = journal.clone();
                let journal = Self::prune_sealed_timeline_journal(&snapshot, journal);
                let journal = Self::prune_superseded_checkpoints(journal);
                (snapshot, journal, audit_entries)
            })
        }) else {
            return;
        };
        let mut store_guard = self
            .timeline_store
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(store) = store_guard.as_mut() else {
            return;
        };
        // 轻量审计（seq/ts/type）：与 terminal 幂等，水位去重后只追加新条目。
        store.append_audit(seed, &audit_entries);
        // offload 壳补齐：内存中已卸载的 sealed turn 在落盘前恢复全文，
        // 保证快照文件始终是「无侧车也能独立恢复」的完整权威。
        let snapshot = rehydrate_offloaded_turns(store, seed, snapshot);
        if let Err(error) = store.persist(seed, &snapshot, journal) {
            log::warn!("[timeline] sync persist failed for {seed}: {error}");
        }
    }

    /// 启用 turn-seal 卸载。幂等；只打开开关，实际 sidecar I/O 在 seal
    /// 调用返回后由 `offload_sealed_turns` 在两把锁之外执行。
    pub fn enable_turn_offload(&self, seed: &str) {
        if self
            .timeline_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
        {
            return;
        }
        self.timeline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .enable_offload(seed);
    }

    /// 把该 seed 当前所有已 seal、尚未卸载的 turn 写入 sidecar 并壳化。
    /// 枚举与写盘分阶段取锁，确保 timeline/store 两把锁永不嵌套。
    pub(super) fn offload_all_sealed_turns(&self, seed: &str) {
        self.enable_turn_offload(seed);
        offload_all_sealed_turns_from(&self.timeline, &self.timeline_store, seed);
    }
}

/// Persist sealed turns and then replace their resident bodies with shells.
///
/// The helper is shared by the persistence worker and explicit synchronous
/// flush paths. It never holds the timeline and store locks at the same time,
/// and the generation check prevents a delayed write from shelling a reopened
/// turn.
fn offload_all_sealed_turns_from(
    timeline: &Mutex<TimelineAppender>,
    timeline_store: &Mutex<Option<crate::timeline_store::TimelineStore>>,
    seed: &str,
) {
    let turn_ids = timeline
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .sealed_turn_ids(seed);
    offload_sealed_turns_from(timeline, timeline_store, seed, &turn_ids);
}

fn offload_sealed_turns_from(
    timeline: &Mutex<TimelineAppender>,
    timeline_store: &Mutex<Option<crate::timeline_store::TimelineStore>>,
    seed: &str,
    turn_ids: &[String],
) {
    for turn_id in turn_ids {
        let Some(candidate) = timeline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .offload_candidate(seed, turn_id)
        else {
            continue;
        };
        let write_result = {
            let mut store_guard = timeline_store.lock().unwrap_or_else(|e| e.into_inner());
            match store_guard.as_mut() {
                Some(store) => store.append_offloaded_turn(seed, &candidate),
                None => return,
            }
        };
        if let Err(error) = write_result {
            log::warn!(
                "[timeline] offload append failed for {seed}/{}: {error}",
                candidate.turn_id
            );
            continue;
        }
        timeline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mark_offloaded_if_current(seed, turn_id, candidate.created_seq);
    }
}

/// 用 offload 侧车补齐快照中已卸载（壳化）的 turn 文本。
/// 侧车缺失/损坏时保留壳（快照仍可恢复，全文降级为预览）。
fn rehydrate_offloaded_turns(
    store: &mut crate::timeline_store::TimelineStore,
    seed: &str,
    mut snapshot: TimelineSnapshot,
) -> TimelineSnapshot {
    for turn in &mut snapshot.turns {
        // 显式标记判定；侧车缺失/损坏时保留壳（全文降级为预览）。
        if !turn.offloaded {
            continue;
        }
        if let Some(full) = store.load_offloaded_turn(seed, &turn.turn_id) {
            if full.created_seq != turn.created_seq {
                log::warn!(
                    "[timeline] ignoring stale offload row while persisting {seed}/{}: \
                     expected generation {}, got {}",
                    turn.turn_id,
                    turn.created_seq,
                    full.created_seq
                );
                continue;
            }
            *turn = full;
            turn.offloaded = false;
        }
    }
    snapshot
}

impl RingingHub {
    /// 同步落盘所有待写 seed + 排空 journal 写队列（daemon 优雅关闭收尾；
    /// Drop 只 join 异步线程，而 Arc 引用可能仍在 tokio task 中存活，必须
    /// 显式 flush）。
    pub fn flush_timeline_persistence(&self) {
        let seeds: Vec<String> = self
            .timeline_persistence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|persistence| {
                let mut seeds: Vec<String> = persistence
                    .terminal_seeds
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .drain()
                    .collect();
                for seed in persistence
                    .pending_seeds
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .drain()
                {
                    if !seeds.contains(&seed) {
                        seeds.push(seed);
                    }
                }
                seeds
            })
            .unwrap_or_default();
        for seed in seeds {
            self.persist_timeline_sync(&seed);
        }
        // BUG-2026-09-12-08：journal 写队列一并排空（关闭/调试同步点共用）。
        self.flush_journal_persistence();
    }

    /// Ringing V1 bootstrap 的权威 transcript 快照。
    pub fn timeline_snapshot(&self, seed: &str) -> Option<TimelineSnapshot> {
        self.ensure_timeline_loaded(seed);
        self.timeline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot(seed)
    }

    /// 从**归档**投影一页更早的回合（BUG-2026-09-15-05 深翻页）。
    ///
    /// 常驻 snapshot 是**有界窗口**（重建后只有最近 `REBUILD_RECENT_TURNS` 轮），
    /// 窗口之外的回合只存在于 append-only 的 `messages.jsonl` 里。本方法按需读归档
    /// 尾部——`need = total - start` 可精确算出，故先按估算读，不够就加倍重试，
    /// 直到覆盖请求或触顶（`DEEP_PAGE_MAX_MESSAGES`）。**不**新建索引、**不**改文件
    /// 格式：与 `bounded_read` 模块文档拒绝持久化偏移索引的既有决策一致。
    ///
    /// 返回 `(页, 本页最旧回合的全局序号, 是否触顶)`。触顶表示更旧的回合取不到，
    /// 调用方据此把 `has_more` 置假、`truncated_before` 置真——「翻不动」必须如实说，
    /// 否则客户端会永远请求同一个空页（BUG-2026-09-13-18 那一族）。
    pub fn archive_turn_page(
        &self,
        seed: &str,
        before_index: usize,
        limit: usize,
    ) -> Option<(Vec<qaqh_domain::TimelineTurn>, usize, bool)> {
        let sessions = self.sessions.as_deref()?;
        let meta_total = sessions.load_meta(seed)?.turn_count;
        let limit = limit.max(1);
        let mut read = DEEP_PAGE_INITIAL_MESSAGES;
        loop {
            let messages = sessions.load_archive_tail(seed, read)?;
            let (pool, turns) =
                super::projection::project_turns_from_messages(seed, &messages, None, None);
            if pool == 0 {
                return None;
            }
            // meta 可能落后于归档（运行中回合已落盘而 meta 未更新）。取两者较大值，
            // 与 `window_metadata` 的 `total_never_below_materialized` 同一条保守原则。
            let total = meta_total.max(pool);
            let base = total - pool; // 池内第 0 个回合的全局序号
            let end = before_index.min(total);
            if end == 0 {
                return None; // 没有更旧的回合
            }
            let want_start = end.saturating_sub(limit);
            let capped = base > want_start && read >= DEEP_PAGE_MAX_MESSAGES;
            if base <= want_start || capped {
                let start = want_start.max(base);
                let hi = end.min(start.saturating_add(limit));
                let lo = start - base;
                let hi = hi.saturating_sub(base).min(turns.len());
                if lo >= hi {
                    return Some((Vec::new(), start, capped));
                }
                // id 由全局序号派生（见 `timeline_snapshot_from_turns` 的说明）：
                // 池内下标会让相邻两页的边界对不上。
                let snapshot = super::timeline_rebuild::timeline_snapshot_from_turns(
                    seed,
                    &turns[lo..hi],
                    start,
                )
                .map(|(snapshot, _)| snapshot)?;
                return Some((snapshot.turns, start, capped));
            }
            read = read.saturating_mul(2);
        }
    }

    /// Restore full turn bodies for one already-paginated timeline page.
    ///
    /// `timeline_snapshot` intentionally returns the bounded in-memory shell:
    /// restoring every offloaded turn there would defeat the memory bound.
    /// The HTTP read path pages first, then calls this method for at most one
    /// page. The restored turns are returned by value and never written back
    /// into the resident timeline.
    pub fn rehydrate_timeline_page(
        &self,
        seed: &str,
        mut turns: Vec<qaqh_domain::TimelineTurn>,
    ) -> Vec<qaqh_domain::TimelineTurn> {
        if !turns.iter().any(|turn| turn.offloaded) {
            return turns;
        }
        let mut store = self
            .timeline_store
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(store) = store.as_mut() else {
            return turns;
        };
        for turn in &mut turns {
            if !turn.offloaded {
                continue;
            }
            let Some(full) = store.load_offloaded_turn(seed, &turn.turn_id) else {
                // Keep the bounded shell when the sidecar is missing/corrupt.
                continue;
            };
            // A reopen can reuse a turn id. Only the current generation may
            // replace the shell; a stale sidecar row must not leak old text.
            if full.created_seq != turn.created_seq {
                log::warn!(
                    "[timeline] ignoring stale offload row for {seed}/{}: expected generation {}, got {}",
                    turn.turn_id,
                    turn.created_seq,
                    full.created_seq
                );
                continue;
            }
            *turn = full;
            turn.offloaded = false;
        }
        turns
    }

    /// Ringing V1 reconnect tail。调用方用 snapshot watermark 作为 after 参数。
    pub fn timeline_replay_since(&self, seed: &str, watermark: u64) -> Vec<TimelineEntry> {
        self.ensure_timeline_loaded(seed);
        self.timeline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replay_since(seed, watermark)
    }

    /// Live Ringing V1 timeline transcript feed. Reliability comes from `timeline_replay_since`
    /// and snapshot watermark; a lagged receiver must reconnect and replay.
    pub fn subscribe_timeline(&self) -> broadcast::Receiver<TimelineLiveEntry> {
        self.timeline_live.subscribe()
    }
}
