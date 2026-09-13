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
use crate::{TimelineError, TimelineLiveEntry};

/// 落后判定的尾部读取窗口（条数）。与 `timeline_rebuild` 的投影窗口同量级：
/// 只用于「快照最后一回合是否仍是归档最后一回合」的同一性确认，不物化历史。
const RECONCILE_TAIL_MESSAGES: usize = 200;

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
        let pending_for_worker = Arc::clone(&pending_seeds);
        let timeline = Arc::clone(&self.timeline);
        let timeline_store = Arc::clone(&self.timeline_store);
        let join = match std::thread::Builder::new()
            .name("qaqh-timeline-persist".into())
            .spawn(move || {
                let persist_pending = || {
                    let seeds: Vec<String> = {
                        let mut pending =
                            pending_for_worker.lock().unwrap_or_else(|e| e.into_inner());
                        pending.drain().collect()
                    };
                    for seed in seeds {
                        // Serialize snapshot selection and file replacement with
                        // terminal persistence. Taking the store lock first
                        // prevents an older async snapshot from overwriting a
                        // newer terminal checkpoint.
                        let mut store = timeline_store.lock().unwrap_or_else(|e| e.into_inner());
                        let Some(store) = store.as_mut() else {
                            continue;
                        };
                        // 快照是唯一持久化产物（journal 已移除）：直接选取当前
                        // 内存态并原子替换写盘。已 seal turn 的条目不再进
                        // replay tail（见 `prune_sealed_timeline_journal`）。
                        //
                        // 顺带在本 checkpoint 窗口追加轻量审计行（seq/ts/type）：
                        // 借助已有的 1s 合并窗口，不落到流式热路径上。
                        let audit_entries = {
                            let timeline = timeline.lock().unwrap_or_else(|e| e.into_inner());
                            timeline.replay_since(&seed, 0)
                        };
                        store.append_audit(&seed, &audit_entries);
                        let Some((snapshot, journal)) = ({
                            let timeline = timeline.lock().unwrap_or_else(|e| e.into_inner());
                            timeline.snapshot(&seed).map(|snapshot| {
                                let journal = timeline.replay_since(&seed, 0);
                                let journal =
                                    Self::prune_sealed_timeline_journal(&snapshot, journal);
                                let journal = Self::prune_superseded_checkpoints(journal);
                                (snapshot, journal)
                            })
                        }) else {
                            continue;
                        };
                        // offload 壳补齐（异步窗口；磁盘文件始终完整）。
                        // `store` 已在本轮持锁（:59）：此处只能借用，绝不能再取锁；
                        // std::sync::Mutex 同线程重入 = 永久死锁（2026-09-12 冻结事故）。
                        let snapshot = rehydrate_offloaded_turns(store, &seed, snapshot);
                        if let Err(error) = store.persist(&seed, &snapshot, journal) {
                            log::warn!("[timeline] persist failed for {seed}: {error}");
                        }
                    }
                };

                while rx.recv().is_ok() {
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
            return;
        }
        if !self
            .disk_timeline_seeds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(seed)
        {
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
            }
            None => {
                // 无快照 → BUG-006：从 messages.jsonl / compact-context 重建投影。
                let _ = self.rebuild_timeline_from_messages(seed);
                return;
            }
        }

        // 上次运行遗留的孤儿 running turn 在此收尾（见 seal_orphan_running_turns）。
        // 有变更时同步落盘快照，使下次启动可直接 restore。
        if self.seal_orphan_running_turns(seed) {
            self.persist_timeline_sync(seed);
        }
        log::info!("[ringing] lazily loaded timeline {seed}");
    }

    /// 持久化快照是否落后于写侧事实（`messages.jsonl`）。
    ///
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
        let Some(messages) = sessions.load_recent_for_projection(seed, RECONCILE_TAIL_MESSAGES)
        else {
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
        }
        let mut store = self
            .timeline_store
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(store) = store.as_mut()
            && let Err(error) = store.persist(seed, &snapshot, journal)
        {
            log::warn!("[timeline] rebuild persist failed for {seed}: {error}");
        }
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
        // Terminal intents are the recovery boundary for a restarting client.
        // TurnSealed 保持同步落盘（turn 级边界，一个 turn 一次，成本可控）；
        // BlockSealed/RoundSealed 降级为 1s 合并窗口异步 checkpoint：长上下文
        // 会话每 turn 可产生几十个 round（实测 79 个），每次同步全量重写快照
        // 使终端事件成为主要写放大源，而 crash 窗口差异只有毫秒级（合并窗口
        // 本身就是周期性 crash checkpoint）。
        let is_turn_sealed = matches!(intent, TimelineIntent::TurnSealed { .. });
        let entry = {
            let mut timeline = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
            timeline.apply_intent(seed, intent)?
        };
        if is_turn_sealed {
            self.persist_timeline_sync(seed);
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
        }
        let mut store_guard = self
            .timeline_store
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(store) = store_guard.as_mut() else {
            return;
        };
        // IIFE：条件块中部复用 `?` 提前返回（clippy redundant_closure_call 豁免）
        #[allow(clippy::redundant_closure_call)]
        let Some((snapshot, journal)) = (|| {
            let timeline = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
            timeline.snapshot(seed).map(|snapshot| {
                let journal = timeline.replay_since(seed, 0);
                let journal = Self::prune_sealed_timeline_journal(&snapshot, journal);
                let journal = Self::prune_superseded_checkpoints(journal);
                (snapshot, journal)
            })
        })() else {
            return;
        };
        // 轻量审计（seq/ts/type）：与 terminal 幂等，水位去重后只追加新条目。
        {
            let timeline = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
            let audit_entries = timeline.replay_since(seed, 0);
            drop(timeline);
            store.append_audit(seed, &audit_entries);
        }
        // offload 壳补齐：内存中已卸载的 sealed turn 在落盘前恢复全文，
        // 保证快照文件始终是「无侧车也能独立恢复」的完整权威。
        // store_guard 已持锁：只能借用，不得再次 lock（同线程重入 = 死锁）。
        let snapshot = rehydrate_offloaded_turns(store, seed, snapshot);
        if let Err(error) = store.persist(seed, &snapshot, journal) {
            log::warn!("[timeline] sync persist failed for {seed}: {error}");
        }
    }

    /// 启用 turn-seal 卸载：seal 后该 turn 的 reasoning/text 全文移出内存，
    /// 经 offload 侧车（`ringing-offload/{seed}.jsonl`）持久化；落盘快照时
    /// 由 rehydrate 补齐。见 `timeline_store::append_offloaded_turn`。
    pub fn enable_turn_offload(&self, seed: &str) {
        let store = self
            .timeline_store
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(store) = store.as_ref() else {
            return;
        };
        let store_seed = seed.to_string();
        let append_store = std::sync::Arc::new(());
        let _ = append_store;
        // 捕获 store 的方法引用：store 在 Arc<Mutex<Option<TimelineStore>>> 里，
        // 回调里再锁。为避免回调内死锁（persist_timeline_sync 持 store 锁时
        // seal 路径不会再触发），offload 回调只做 append（append-only 文件，
        // 不需要 store 可变状态），因此回调内短暂拿锁即可。
        let timeline = self.timeline.clone();
        let timeline_store = self.timeline_store.clone();
        let offload: crate::timeline::OffloadFn =
            std::sync::Arc::new(move |seed: &str, turn: &qaqh_domain::TimelineTurn| {
                let store_guard = timeline_store.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(store) = store_guard.as_ref() {
                    store.append_offloaded_turn(seed, turn);
                }
                drop(store_guard);
                let _ = &timeline;
                let _ = &store_seed;
            });
        drop(store);
        self.timeline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_offload(seed, Some(offload));
    }
}

/// 用 offload 侧车补齐快照中已卸载（壳化）的 turn 文本。
/// 侧车缺失/损坏时保留壳（快照仍可恢复，全文降级为预览）。
///
/// 入参是**已持锁**的 store 引用：本函数内禁止任何 `lock()`。同步路径
/// （`persist_timeline_sync`）与异步 worker 均在持有 `timeline_store` 守卫的
/// 临界区内调用它；一旦在此再次取锁，同一线程立即死锁（2026-09-12 冻结事故根因）。
fn rehydrate_offloaded_turns(
    store: &crate::timeline_store::TimelineStore,
    seed: &str,
    mut snapshot: TimelineSnapshot,
) -> TimelineSnapshot {
    for turn in &mut snapshot.turns {
        // 壳判定（启发式）：sealed turn 的任一 block 文本/进度 ≤ 512 字符
        // 预览上限且侧车有更新版本，则补齐（侧车没有时保持现状，无害）。
        if turn.sealed
            && turn.rounds.iter().any(|round| {
                round.blocks.iter().any(|block| {
                    block.text.chars().count() <= 512
                        || block
                            .tool
                            .as_ref()
                            .is_some_and(|tool| tool.progress.chars().count() <= 512)
                })
            })
        {
            if let Some(full) = store.load_offloaded_turn(seed, &turn.turn_id) {
                *turn = full;
            }
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
                persistence
                    .pending_seeds
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .drain()
                    .collect()
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

    /// TurnSealed 是 turn 级恢复边界，同步落盘（见 publish_timeline 注释）。
    /// BlockSealed/RoundSealed 已降级为异步 checkpoint，不再视作同步终端。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn timeline_intent_is_terminal(intent: &TimelineIntent) -> bool {
        matches!(intent, TimelineIntent::TurnSealed { .. })
    }
}
