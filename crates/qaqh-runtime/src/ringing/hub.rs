//! `RingingHub`：daemon 侧 Ringing 运行时聚合入口。
//!
//! 职责（阶段 3d 后）：
//! - 领域 snapshot projection（每 seed+channel；orphan_seal 收敛与测试读入口）；
//! - 每频道序号生成；
//! - timeline transcript 投影（见 `timeline_hub.rs`）与孤儿收尾（见
//!   `orphan_seal.rs`），同文件 `impl RingingHub` 跨文件块；
//! - 大内容外置存储（`content_store.rs`，会话所有权 + TTL）。
//!
//! 由 daemon 与孤儿收尾路径消费。线程安全（Mutex 保护）。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use qaqh_domain::{RingingChannel, TimelineEntry};
use qaqh_session::SessionManager;
use tokio::sync::broadcast;

use super::content_store::{ContentEntry, ContentQuotaExceeded, ContentStore};
use crate::timeline_store::TimelineStore;
use crate::{TimelineAppender, TimelineLiveEntry};

/// timeline live broadcast 的环形缓冲容量。
/// 溢出即 `Lagged`——由 daemon SSE 侧发终止帧让客户端重连重定基。
pub(super) const LIVE_BROADCAST_CAPACITY: usize = 1024;

/// Non-terminal timeline changes are checkpointed at most once per interval.
/// Live delivery is still immediate; only the full snapshot rewrite is paced.
pub(super) const TIMELINE_PERSIST_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub(super) struct TimelinePersistence {
    pub(super) wake: mpsc::Sender<()>,
    /// 结构事件（BlockSealed/RoundSealed/BlockCheckpoint…）→ 1s 合并窗口。
    pub(super) pending_sessions: Arc<Mutex<HashSet<String>>>,
    /// 回合边界（TurnSealed）→ 优先排空，且立即唤醒 worker。
    ///
    /// 与 `pending_seeds` 由**同一个 worker 线程**处理、terminal 每轮先 drain，
    /// 所以"更晚的写永远更新"：worker 每次落盘都取当前内存态，软窗口不可能
    /// 让旧快照盖过 terminal 边界。崩溃窗口从 0 变为毫秒级（与既有
    /// BlockSealed/RoundSealed 异步窗口同量级），显式同步边界
    /// （`flush_timeline_persistence` / `Drop` / 优雅关闭）仍是 fail-closed。
    pub(super) terminal_sessions: Arc<Mutex<HashSet<String>>>,
    pub(super) join: Option<JoinHandle<()>>,
}
/// Ringing daemon 运行时聚合。
pub struct RingingHub {
    pub(super) epoch: String,
    /// 磁盘 timeline seed 清单（懒加载索引；`ensure_timeline_loaded` 按需恢复）。
    pub(super) disk_timeline_sessions: Mutex<HashSet<String>>,
    /// 懒加载串行化（per-seed）：timeline 恢复与 seal 提交的首访互斥点。
    pub(super) lazy_loads: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// 大内容外置存储（会话所有权 + TTL）。
    pub(super) content_store: Mutex<ContentStore>,
    /// #345：活交互正文在 content store 里的归属（seed → (interaction_id, content_id)）。
    ///
    /// 交互正文是**展示面旁路**（canonical fact 只存 ref）：正文在
    /// [`RingingHub::put_interaction_content`] 入 store 并 pin，交互
    /// resolved / expired 时按此表 unpin。表里没有 = 没有正文（permission，
    /// 或写入时超配额）。
    pub(super) live_interaction_content: Mutex<HashMap<String, (String, String)>>,
    /// B9/H3：当前进程内存活的 worker（registry 维护）。bootstrap 的
    /// force=false 孤儿收尾在 worker 存活时整体跳过，防止误杀活 turn。
    pub(super) live_workers: Mutex<std::collections::HashSet<String>>,
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

impl RingingHub {
    pub fn new(epoch: impl Into<String>) -> Self {
        Self::with_options(epoch.into(), None)
    }

    /// 持久化构造：timeline transcript 与大内容外置落盘（v1 事件持久化已删除）。
    pub fn with_persistence(epoch: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        let hub = Self::with_options(epoch.into(), Some(root.into()));
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
        let content_store = match root.as_ref() {
            Some(root) => ContentStore::with_root(root.join("content")),
            None => ContentStore::new(),
        };
        let timeline_store = root
            .as_ref()
            .and_then(|root| match TimelineStore::new(root) {
                Ok(store) => Some(store),
                Err(error) => {
                    log::warn!("[timeline] persistence disabled: {error}");
                    None
                }
            });
        let (timeline_live, _) = broadcast::channel(LIVE_BROADCAST_CAPACITY);
        Self {
            epoch,
            disk_timeline_sessions: Mutex::new(HashSet::new()),
            lazy_loads: Mutex::new(HashMap::new()),
            content_store: Mutex::new(content_store),
            live_interaction_content: Mutex::new(HashMap::new()),
            live_workers: Mutex::new(std::collections::HashSet::new()),
            timeline: Arc::new(Mutex::new(TimelineAppender::new())),
            timeline_live,
            timeline_store: Arc::new(Mutex::new(timeline_store)),
            timeline_persistence: Mutex::new(None),
            sessions: None,
        }
    }

    /// 登记 worker 存活（B9/H3）：registry 在 spawn 成功时调用，worker 关闭时
    /// 由 [`RingingHub::mark_worker_dead`] 摘除。
    pub fn mark_worker_live(&self, session_id: &str) {
        self.live_workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_string());
    }

    pub fn mark_worker_dead(&self, session_id: &str) {
        self.live_workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }

    pub fn memory_components(&self) -> Vec<qaqh_memwatch::ComponentMemory> {
        let mut components = Vec::new();
        if let Ok(timeline) = self.timeline.lock() {
            components.extend(timeline.memory_components());
        }
        if let Ok(content) = self.content_store.lock() {
            components.extend(content.memory_components());
        }
        let live_interactions = self
            .live_interaction_content
            .lock()
            .map(|live| live.len() as u64)
            .unwrap_or_default();
        let live_workers = self
            .live_workers
            .lock()
            .map(|workers| workers.len() as u64)
            .unwrap_or_default();
        let lazy_loads = self
            .lazy_loads
            .lock()
            .map(|loads| loads.len() as u64)
            .unwrap_or_default();
        components.extend([
            qaqh_memwatch::ComponentMemory {
                name: "ringing.live_interactions".into(),
                item_count: live_interactions,
                payload_bytes: None,
                heap_estimate_bytes: None,
                ..Default::default()
            },
            qaqh_memwatch::ComponentMemory {
                name: "ringing.live_workers".into(),
                item_count: live_workers,
                payload_bytes: None,
                heap_estimate_bytes: None,
                ..Default::default()
            },
            qaqh_memwatch::ComponentMemory {
                name: "ringing.lazy_load_locks".into(),
                item_count: lazy_loads,
                payload_bytes: None,
                heap_estimate_bytes: None,
                ..Default::default()
            },
        ]);
        components
    }

    /// D-1：会话关闭后丢弃该 seed 的全部常驻内存态（活交互正文表、
    /// live_workers、大内容条目）。
    ///
    /// 调用约束：调用方必须已 join worker，避免存活 worker 继续写回脏状态。
    pub fn forget_session(&self, session_id: &str) {
        if let Ok(mut live) = self.live_interaction_content.lock() {
            live.remove(session_id);
        }
        if let Ok(mut workers) = self.live_workers.lock() {
            workers.remove(session_id);
        }
        if let Ok(mut content) = self.content_store.lock() {
            content.release_session(session_id);
        }
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    /// 大内容外置：存入（返回 content_id）。
    pub fn put_content(
        &self,
        session_id: &str,
        media_type: &str,
        bytes: Vec<u8>,
        truncated: bool,
    ) -> String {
        self.content_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .put(session_id, media_type, bytes, truncated)
    }

    /// 大内容外置：读取（校验会话所有权 + TTL）。
    pub fn get_content(&self, session_id: &str, content_id: &str) -> Option<ContentEntry> {
        self.content_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id, content_id)
    }

    /// #345：交互正文入 store 并 pin（v2 content 端点按 id 取，不带 seed）。
    ///
    /// 超配额返回 [`ContentQuotaExceeded`]（fail-closed，见 content_store 文档）；
    /// 调用方不得在写入失败时假装交互正文可用。
    pub fn put_interaction_content(
        &self,
        session_id: &str,
        interaction_id: &str,
        media_type: &str,
        bytes: Vec<u8>,
    ) -> Result<String, ContentQuotaExceeded> {
        let content_id = self
            .content_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .put_pinned_for(session_id, media_type, bytes, Some(interaction_id))?;
        self.live_interaction_content
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                session_id.to_string(),
                (interaction_id.to_string(), content_id.clone()),
            );
        Ok(content_id)
    }

    /// #345：按 content_id 读取（**不校验 seed**）。调用方（daemon）拿条目的
    /// `seed` 再校验请求方归属——v2 content 端点的 wire 形态不带 seed 参数。
    pub fn get_content_any(&self, content_id: &str) -> Option<ContentEntry> {
        self.content_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_any(content_id)
    }

    /// 解除某交互正文的 pin（交互 resolved / expired / permission tool finished）。
    ///
    /// 先走内存活表（当前进程路径），再按持久化的 pin_key 兜底——daemon 重启后
    /// 活表为空，但 metadata 里仍记录着 interaction_id，孤儿收尾时也能释放 pin。
    ///
    /// §4.0.5：副作用由事件产生侧调用（actor 桥 / orphan_seal 补终态），
    /// 不再由 publish 承载。
    pub fn release_interaction_content(&self, session_id: &str, interaction_id: &str) {
        let content_id = {
            let mut live = self
                .live_interaction_content
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match live.get(session_id) {
                Some((current, _)) if current == interaction_id => {
                    live.remove(session_id).map(|(_, content_id)| content_id)
                }
                _ => None,
            }
        };
        if let Some(content_id) = content_id {
            self.content_store
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .unpin(&content_id);
        }
        // 重启后 live 表为空，按 pin_key 兜底解除。幂等：已 unpin 时返回 0。
        self.content_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unpin_key(session_id, interaction_id);
    }
    /// 懒加载串行化锁（per-seed）：同一 seed 的首访互斥，不同 seed 并行。
    pub(super) fn lazy_load_lock(&self, session_id: &str) -> Arc<Mutex<()>> {
        self.lazy_loads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
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
}

impl Drop for RingingHub {
    fn drop(&mut self) {
        // v1 journal 写线程已随广播面删除；只排空 timeline 持久化。
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

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::{TimelineIntent, TimelineSnapshot};

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

    fn publish_open_tool_turn(hub: &RingingHub, session_id: &str, turn_id: &str, progress: &str) {
        hub.publish_timeline(
            session_id,
            TimelineIntent::TurnOpened {
                turn_id: turn_id.into(),
                user_text: format!("question-{turn_id}"),
            },
        )
        .unwrap();
        hub.publish_timeline(
            session_id,
            TimelineIntent::BlockOpened {
                turn_id: turn_id.into(),
                round_num: 0,
                block_id: "tool".into(),
                kind: qaqh_domain::TimelineBlockKind::Tool,
                tool: Some(qaqh_domain::TimelineTool {
                    exit_code: None,
                    completed_at_ms: None,
                    tool_call_id: format!("call-{turn_id}"),
                    name: "exec".into(),
                    state: qaqh_domain::TimelineToolState::Running,
                    summary: None,
                    args_json: None,
                    output: None,
                    diff: None,
                    progress: String::new(),
                    progress_truncated: false,
                    progress_stream: None,
                    progress_bytes_total: 0,
                    display: None,
                    failure: None,
                    permission: None,
                }),
            },
        )
        .unwrap();
        hub.publish_timeline(
            session_id,
            TimelineIntent::ToolProgress {
                turn_id: turn_id.into(),
                round_num: 0,
                block_id: "tool".into(),
                chunk: progress.into(),
                stream: None,
                bytes_total: 0,
            },
        )
        .unwrap();
        hub.publish_timeline(
            session_id,
            TimelineIntent::ToolUpdated {
                turn_id: turn_id.into(),
                round_num: 0,
                block_id: "tool".into(),
                tool: qaqh_domain::TimelineTool {
                    exit_code: None,
                    completed_at_ms: None,
                    tool_call_id: format!("call-{turn_id}"),
                    name: "exec".into(),
                    state: qaqh_domain::TimelineToolState::Succeeded,
                    summary: Some(format!("summary-{turn_id}")),
                    args_json: None,
                    output: Some(format!("full-output-{turn_id}")),
                    diff: Some(format!("diff-{turn_id}")),
                    progress: progress.into(),
                    progress_truncated: false,
                    progress_stream: None,
                    progress_bytes_total: 0,
                    display: None,
                    failure: None,
                    permission: None,
                },
            },
        )
        .unwrap();
    }

    fn publish_tool_turn(hub: &RingingHub, session_id: &str, turn_id: &str, progress: &str) {
        publish_open_tool_turn(hub, session_id, turn_id, progress);
        hub.publish_timeline(
            session_id,
            TimelineIntent::BlockSealed {
                turn_id: turn_id.into(),
                round_num: 0,
                block_id: "tool".into(),
            },
        )
        .unwrap();
        hub.publish_timeline(
            session_id,
            TimelineIntent::RoundSealed {
                turn_id: turn_id.into(),
                round_num: 0,
                is_final: true,
            },
        )
        .unwrap();
        hub.publish_timeline(
            session_id,
            TimelineIntent::TurnSealed {
                turn_id: turn_id.into(),
                state: qaqh_domain::TimelineTurnState::Completed,
                failure: None,
            },
        )
        .unwrap();
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
                arg: None,
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
                    TimelineEvent::BlockCheckpoint { block_id, text, .. }
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
        let snapshot = hub.timeline_snapshot("s").expect("timeline snapshot");
        assert_eq!(snapshot.watermark, 1);
        assert_eq!(snapshot.turns[0].user_text, "question");
    }

    #[test]
    fn terminal_timeline_intent_is_persisted_at_the_async_boundary() {
        let root = temp_root("timeline-terminal-async");
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
        // BlockSealed / TurnSealed 都走异步持久化（issue #28：发布会话线程不再
        // 同步全量重写快照）。TurnSealed 进 terminal 优先队列，立即唤醒 worker。
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

        // fail-closed：显式同步边界返回即落盘（崩溃一致性由它兜底）。
        hub.flush_timeline_persistence();
        let persisted = TimelineStore::new(&root)
            .unwrap()
            .load_session("s")
            .expect("turn-sealed snapshot persisted at the explicit sync boundary");
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
    fn sealed_tool_turn_is_offloaded_but_paginated_read_restores_full_text() {
        let root = temp_root("timeline-offload-page");
        let hub = RingingHub::with_persistence("epoch", &root);
        // Keep this below the reducer cap so the assertion verifies the full
        // sidecar round-trip rather than the intentionally lossy progress tail.
        let progress = "p".repeat(8 * 1024);
        publish_tool_turn(&hub, "s", "t1", &progress);

        // The resident snapshot is bounded: output/diff are gone and progress
        // is only the preview shell.
        let resident = hub.timeline_snapshot("s").expect("resident snapshot");
        let shell = resident.turns[0].rounds[0].blocks[0]
            .tool
            .as_ref()
            .expect("tool");
        assert!(resident.turns[0].offloaded);
        assert!(shell.output.is_none());
        assert!(shell.diff.is_none());
        assert!(shell.progress.len() <= 512);
        assert!(shell.progress_truncated);

        // Reading one page rehydrates only the returned value. It must not
        // mutate the resident shell.
        let page = hub.rehydrate_timeline_page("s", resident.turns.clone());
        let full = page[0].rounds[0].blocks[0].tool.as_ref().unwrap();
        assert!(!page[0].offloaded);
        assert_eq!(full.output.as_deref(), Some("full-output-t1"));
        assert_eq!(full.diff.as_deref(), Some("diff-t1"));
        assert_eq!(full.progress, progress);
        assert!(
            hub.timeline_snapshot("s").unwrap().turns[0].offloaded,
            "rehydration must remain response-local"
        );

        // The on-disk snapshot is independently complete even while memory is
        // shelled.
        hub.flush_timeline_persistence();
        let persisted = TimelineStore::new(&root)
            .unwrap()
            .load_session("s")
            .expect("persisted snapshot");
        let persisted_tool = persisted.snapshot.turns[0].rounds[0].blocks[0]
            .tool
            .as_ref()
            .unwrap();
        assert_eq!(persisted_tool.output.as_deref(), Some("full-output-t1"));
        assert_eq!(persisted_tool.progress, progress);

        drop(hub);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn offload_page_keeps_shell_when_sidecar_is_missing_or_stale() {
        let root = temp_root("timeline-offload-degrade");
        let hub = RingingHub::with_persistence("epoch", &root);
        publish_tool_turn(&hub, "missing", "t1", "complete-progress");
        let missing = hub.timeline_snapshot("missing").unwrap();
        assert!(missing.turns[0].offloaded);

        // Missing sidecar row: bounded shell is preferable to inventing data.
        std::fs::remove_file(root.join("ringing-offload").join("missing.jsonl")).unwrap();
        let page = hub.rehydrate_timeline_page("missing", missing.turns.clone());
        assert!(page[0].offloaded);
        assert!(
            page[0].rounds[0].blocks[0]
                .tool
                .as_ref()
                .unwrap()
                .output
                .is_none()
        );

        // Reopen the same turn id, then append an older generation as the
        // latest sidecar row. Neither page reads nor full persistence may let
        // that row replace the current generation.
        publish_tool_turn(&hub, "stale", "t1", "first-generation");
        let first = hub.timeline_snapshot("stale").unwrap();
        assert!(first.turns[0].offloaded);
        let first_created_seq = first.turns[0].created_seq;
        hub.publish_timeline(
            "stale",
            TimelineIntent::TurnOpened {
                turn_id: "t1".into(),
                user_text: "second".into(),
            },
        )
        .unwrap();
        hub.publish_timeline(
            "stale",
            TimelineIntent::BlockOpened {
                turn_id: "t1".into(),
                round_num: 0,
                block_id: "tool".into(),
                kind: qaqh_domain::TimelineBlockKind::Tool,
                tool: Some(qaqh_domain::TimelineTool {
                    exit_code: None,
                    completed_at_ms: None,
                    tool_call_id: "call-t1".into(),
                    name: "exec".into(),
                    state: qaqh_domain::TimelineToolState::Running,
                    summary: None,
                    args_json: None,
                    output: None,
                    diff: None,
                    progress: String::new(),
                    progress_truncated: false,
                    progress_stream: None,
                    progress_bytes_total: 0,
                    display: None,
                    failure: None,
                    permission: None,
                }),
            },
        )
        .unwrap();
        hub.publish_timeline(
            "stale",
            TimelineIntent::BlockSealed {
                turn_id: "t1".into(),
                round_num: 0,
                block_id: "tool".into(),
            },
        )
        .unwrap();
        hub.publish_timeline(
            "stale",
            TimelineIntent::RoundSealed {
                turn_id: "t1".into(),
                round_num: 0,
                is_final: true,
            },
        )
        .unwrap();
        hub.publish_timeline(
            "stale",
            TimelineIntent::TurnSealed {
                turn_id: "t1".into(),
                state: qaqh_domain::TimelineTurnState::Completed,
                failure: None,
            },
        )
        .unwrap();
        // Establish the second generation as the latest sidecar row before
        // appending the stale row used by this test.
        hub.flush_timeline_persistence();

        let mut stale_full = first.turns[0].clone();
        stale_full.created_seq = first_created_seq;
        stale_full.offloaded = false;
        let stale_tool = stale_full.rounds[0].blocks[0].tool.as_mut().unwrap();
        stale_tool.output = Some("stale-output".into());
        stale_tool.diff = Some("stale-diff".into());
        stale_tool.progress = "stale-progress".into();
        hub.timeline_store
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .append_offloaded_turn("stale", &stale_full)
            .unwrap();

        let current = hub.timeline_snapshot("stale").unwrap();
        assert!(current.turns[0].offloaded);
        assert_ne!(current.turns[0].created_seq, first_created_seq);
        let page = hub.rehydrate_timeline_page("stale", current.turns.clone());
        assert!(page[0].offloaded);
        assert_eq!(page[0].user_text, "second");
        assert!(
            page[0].rounds[0].blocks[0]
                .tool
                .as_ref()
                .unwrap()
                .output
                .is_none()
        );
        hub.flush_timeline_persistence();
        let persisted = TimelineStore::new(&root)
            .unwrap()
            .load_session("stale")
            .unwrap()
            .snapshot;
        assert_eq!(persisted.turns[0].user_text, "second");
        assert!(
            persisted.turns[0].rounds[0].blocks[0]
                .tool
                .as_ref()
                .unwrap()
                .output
                .is_none()
        );

        drop(hub);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn offload_write_failure_keeps_full_turn_resident() {
        let root = temp_root("timeline-offload-write-failure");
        let offload_dir = root.join("ringing-offload");
        std::fs::create_dir_all(&offload_dir).unwrap();
        std::fs::create_dir(offload_dir.join("s.jsonl")).unwrap();

        let hub = RingingHub::with_persistence("epoch", &root);
        publish_tool_turn(&hub, "s", "t1", "must-stay-resident");
        let snapshot = hub.timeline_snapshot("s").unwrap();
        assert!(!snapshot.turns[0].offloaded);
        let tool = snapshot.turns[0].rounds[0].blocks[0].tool.as_ref().unwrap();
        assert_eq!(tool.output.as_deref(), Some("full-output-t1"));
        assert_eq!(tool.progress, "must-stay-resident");

        drop(hub);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn offloaded_turn_survives_restart_and_restores_on_page_read() {
        let root = temp_root("timeline-offload-restart");
        {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            publish_tool_turn(&hub, "s", "t1", "restart-progress");
            hub.flush_timeline_persistence();
        }

        let hub = RingingHub::with_persistence("epoch-2", &root);
        let resident = hub.timeline_snapshot("s").unwrap();
        assert!(resident.turns[0].offloaded);
        let page = hub.rehydrate_timeline_page("s", resident.turns.clone());
        let tool = page[0].rounds[0].blocks[0].tool.as_ref().unwrap();
        assert_eq!(tool.progress, "restart-progress");
        assert_eq!(tool.output.as_deref(), Some("full-output-t1"));

        drop(hub);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn orphan_seal_offloads_tool_text_and_page_read_restores_it() {
        let root = temp_root("timeline-orphan-offload");
        let hub = RingingHub::with_persistence("epoch", &root);
        let progress = "orphan-progress".repeat(64);
        publish_open_tool_turn(&hub, "s", "t1", &progress);

        assert!(hub.seal_orphan_running_turns("s"));
        let resident = hub.timeline_snapshot("s").unwrap();
        let turn = &resident.turns[0];
        assert!(turn.sealed);
        assert_eq!(turn.state, qaqh_domain::TimelineTurnState::Cancelled);
        assert!(turn.offloaded);
        let tool = turn.rounds[0].blocks[0].tool.as_ref().unwrap();
        assert!(tool.output.is_none());
        assert!(tool.diff.is_none());
        assert!(tool.progress.len() <= 512);
        assert!(tool.progress_truncated);

        let page = hub.rehydrate_timeline_page("s", resident.turns.clone());
        let restored = page[0].rounds[0].blocks[0].tool.as_ref().unwrap();
        assert_eq!(restored.output.as_deref(), Some("full-output-t1"));
        assert_eq!(restored.diff.as_deref(), Some("diff-t1"));
        assert_eq!(restored.progress, progress);

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
        // #314（2026-09-23 契约变更）：seal **不再**裁剪回放尾。sealed turn 的条目
        // 必须留到双限驱逐为止，否则「回合中途重基线」的客户端拿不到补齐所需的
        // `TurnSealed`。此处孤儿 turn 收尾后，从 seq 1 起仍应能回放到 TurnSealed。
        let tail = hub.timeline_replay_since("s", 1);
        assert!(
            !tail.is_empty(),
            "sealed turn 的条目必须留在回放尾里（#314）"
        );
        assert!(
            matches!(
                tail.last().map(|entry| &entry.event),
                Some(qaqh_domain::TimelineEvent::TurnSealed { .. })
            ),
            "回放尾必须收在 TurnSealed 上"
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
    fn cache_only_session_is_restored_on_first_load_without_a_journal() {
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
    fn forget_session_drops_per_session_resident_state() {
        let hub = RingingHub::new("forget-seed-test");
        hub.mark_worker_live("s1");
        let content_id = hub.put_content("s1", "text/plain", b"hello".to_vec(), false);
        assert!(hub.get_content("s1", &content_id).is_some());

        hub.forget_session("s1");

        assert!(
            !hub.live_workers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains("s1"),
            "live_workers entry must be dropped"
        );
        assert!(
            hub.get_content("s1", &content_id).is_none(),
            "content_store entry must be released"
        );
    }

    #[test]
    fn persisted_content_survives_hub_restart_and_unpins_by_key() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (plain, pinned) = {
            let hub = RingingHub::with_persistence("epoch-content-1", tmp.path());
            let plain = hub.put_content("s1", "text/plain", b"persisted".to_vec(), false);
            let pinned = hub
                .put_interaction_content(
                    "s1",
                    "int_ask_restart",
                    "application/json",
                    b"{\"kind\":\"ask\"}".to_vec(),
                )
                .expect("pinned content admitted");
            (plain, pinned)
        };

        let hub = RingingHub::with_persistence("epoch-content-2", tmp.path());
        let entry = hub
            .get_content("s1", &plain)
            .expect("plain content survives hub restart");
        assert_eq!(entry.bytes, b"persisted");
        let entry = hub
            .get_content_any(&pinned)
            .expect("pinned content survives hub restart");
        assert!(entry.pinned);
        assert_eq!(entry.owners, vec!["s1".to_string()]);

        // 重启后 live_interaction_content 为空，release 仍按持久化的 pin_key
        // 解除 pin（否则 pending interaction 的正文会永久占额度）。
        hub.release_interaction_content("s1", "int_ask_restart");
        assert!(
            !hub.get_content_any(&pinned)
                .expect("unpinned content remains readable")
                .pinned
        );
    }

    #[test]
    fn per_session_lazy_loads_do_not_block_each_other() {
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
}

impl RingingHub {
    /// timeline live 广播容量（qaqh-daemon SSE 测试装置用；与
    /// subscribe_timeline 同源。v1 三频道广播删除后不再有其他读者）。
    pub fn live_capacity(_channel: RingingChannel) -> usize {
        LIVE_BROADCAST_CAPACITY
    }
}
