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
                                (snapshot, journal)
                            })
                        }) else {
                            continue;
                        };
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
                Some(store) => {
                    match store.list_seeds() {
                        Ok(cache_seeds) => seeds.extend(cache_seeds),
                        Err(error) => log::warn!("[timeline] cache index failed: {error}"),
                    }
                }
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
                self.rebuild_timeline_from_messages(seed);
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

    /// BUG-006：timeline 目录缺失/记录损坏时，它必须能从 messages.jsonl /
    /// compact-context 重建，否则 timeline 就不是"可重建投影"，而会变成第二份
    /// 事实源。重建结果与 conversation snapshot 同一基线（compact 优先），
    /// 并同步写回 timeline 缓存 + timeline journal（保证下次也 journal 权威）。
    pub(super) fn rebuild_timeline_from_messages(&self, seed: &str) {
        if let Some((snapshot, journal)) =
            super::timeline_rebuild::rebuild_timeline_snapshot(self.sessions.as_deref(), seed)
        {
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
            log::info!(
                "[ringing] rebuilt timeline {seed} from persisted messages (BUG-006 fallback)"
            );
        }
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
        // Terminal intents (block/round/turn sealed) are the recovery boundary
        // for a restarting client: persisting them synchronously shrinks the
        // window in which a crash can lose the transcript tail from "the whole
        // turn" to "the current open blocks". Everything else keeps the
        // coalesced async checkpoint to stay off the streaming hot path.
        let terminal = Self::timeline_intent_is_terminal(&intent);
        let entry = {
            let mut timeline = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
            timeline.apply_intent(seed, intent)?
        };
        if terminal {
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
        if let Err(error) = store.persist(seed, &snapshot, journal) {
            log::warn!("[timeline] sync persist failed for {seed}: {error}");
        }
    }

    /// 同步落盘所有待写 seed（daemon 优雅关闭收尾；Drop 只 join 异步线程，
    /// 而 Arc 引用可能仍在 tokio task 中存活，必须显式 flush）。
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

    /// Terminal intents seal a block/round/turn — the client's recovery
    /// boundary. They are persisted synchronously so a crash between the seal
    /// and the next async checkpoint cannot drop a completed unit of work.
    pub(super) fn timeline_intent_is_terminal(intent: &TimelineIntent) -> bool {
        matches!(
            intent,
            TimelineIntent::BlockSealed { .. }
                | TimelineIntent::RoundSealed { .. }
                | TimelineIntent::TurnSealed { .. }
        )
    }
}
