//! `RingingHub`：daemon 侧 Ringing 运行时聚合入口。
//!
//! 职责：
//! - 三频道 `ChannelRouter`（入队/回放）；
//! - 三频道可靠 journal（reliable 事件 + replaceable checkpoint）；
//! - 领域 snapshot projection（每 seed+channel）；
//! - 每频道序号生成；
//! - 事件幂等（journal 侧 event_id 去重）。
//!
//! 由 daemon（T5）与 worker 事件入口（T6）消费。线程安全（Mutex 保护），
//! 与 legacy `EventBus` 并行存在，互不嵌套。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use deepx_domain::{ConversationEvent, Delivery, DomainEvent, RingingChannel};
use deepx_ringing::{
    RingingChannelSnapshot, RingingEvent, RingingEventEnvelope, RingingResetRequired,
    is_safe_integer,
};
use tokio::sync::broadcast;

use super::content_store::{ContentEntry, ContentStore};
use super::journal::{AppendOutcome, CursorExpired, ReliableJournal};
use super::journal_store::{JournalOp, JournalStore};
use super::projection::SnapshotProjector;
use super::router::{ChannelRouter, replaceable_key_for, terminal_replaceable_keys};
use super::sequencer::Sequencer;

/// 事件已接受（含 envelope 与幂等状态）。
#[derive(Debug)]
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
struct SeedChannelState {
    router: ChannelRouter,
    journal: ReliableJournal,
    projection: SnapshotProjector,
    last_stream_seq: u64,
    replaceable_since_checkpoint: HashMap<super::router::ReplaceableKey, u32>,
}

impl SeedChannelState {
    fn new(channel: RingingChannel) -> Self {
        Self {
            router: ChannelRouter::new(channel),
            journal: ReliableJournal::new(),
            projection: SnapshotProjector::new(),
            last_stream_seq: 0,
            replaceable_since_checkpoint: HashMap::new(),
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

/// Ringing daemon 运行时聚合。
#[derive(Debug)]
pub struct RingingHub {
    epoch: String,
    sequencer: Sequencer,
    /// 大内容外置存储（会话所有权 + TTL）。
    content_store: Mutex<ContentStore>,
    /// channel → (seed → state)。router/journal/projection 均 per (seed, channel)。
    channels: Mutex<HashMap<RingingChannel, HashMap<String, SeedChannelState>>>,
    /// 每频道实时推送通道（SSE 消费；可靠性由 journal/cursor 保证）。
    live: Mutex<HashMap<RingingChannel, broadcast::Sender<RingingEventEnvelope>>>,
    /// 持久化 journal（None = 非持久模式；I/O 失败只记录日志，不阻塞事件路径）。
    journal_store: Mutex<Option<JournalStore>>,
}

impl RingingHub {
    pub fn new(epoch: impl Into<String>) -> Self {
        Self::with_options(epoch.into(), None)
    }

    /// 持久化构造：daemon 重启后可靠事件/切流状态不丢。
    pub fn with_persistence(epoch: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        let hub = Self::with_options(epoch.into(), Some(root.into()));
        hub.load_persisted();
        hub
    }

    fn with_options(epoch: String, root: Option<PathBuf>) -> Self {
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
        Self {
            epoch,
            sequencer: Sequencer::new(),
            content_store: Mutex::new(ContentStore::new()),
            channels: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            journal_store: Mutex::new(journal_store),
        }
    }

    /// 启动装载：重建 journal/router/projection，并恢复序号。
    fn load_persisted(&self) {
        let root = {
            let guard = self.journal_store.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                Some(store) => store.root().to_path_buf(),
                None => return,
            }
        };
        let loaded = match JournalStore::load(&root) {
            Ok(loaded) => loaded,
            Err(error) => {
                log::warn!("[ringing] load persisted journal failed: {error}");
                return;
            }
        };
        let mut guard = self.channel_state(RingingChannel::Control);
        for (channel, seed, ops) in loaded.per_seed {
            let (mut max_stream, mut max_channel, mut max_session) = (0, 0, 0);
            for op in &ops {
                if let JournalOp::Append { envelope } = op {
                    max_stream = max_stream.max(envelope.stream_seq);
                    max_channel = max_channel.max(envelope.channel_seq);
                    max_session = max_session.max(envelope.session_seq);
                }
            }
            guard
                .entry(channel)
                .or_default()
                .entry(seed.clone())
                .or_insert_with(|| SeedChannelState::with_ops(channel, &seed, &ops));
            self.sequencer
                .seed(channel, &seed, max_stream, max_channel, max_session);
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

    fn channel_state(
        &self,
        _channel: RingingChannel,
    ) -> std::sync::MutexGuard<'_, HashMap<RingingChannel, HashMap<String, SeedChannelState>>> {
        self.channels.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn seed_state<'a>(
        &self,
        guard: &'a mut HashMap<RingingChannel, HashMap<String, SeedChannelState>>,
        channel: RingingChannel,
        seed: &str,
    ) -> &'a mut SeedChannelState {
        guard
            .entry(channel)
            .or_insert_with(HashMap::new)
            .entry(seed.to_string())
            .or_insert_with(|| SeedChannelState::new(channel))
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
        let delivery = event.delivery();
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

        let mut guard = self.channel_state(channel);
        let st = self.seed_state(&mut guard, channel, seed);

        // 幂等：journal 侧 event_id 去重（replaceable 也检查，防重复投递）
        let envelope = RingingEventEnvelope::new(
            &self.epoch,
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
        let mut envelope = envelope;
        if state_changed {
            envelope = envelope.with_state_revision(revision);
        }
        st.last_stream_seq = st.last_stream_seq.max(stream_seq);

        match delivery {
            Delivery::Reliable => {
                match st.journal.append(&envelope) {
                    AppendOutcome::Duplicate => return PublishOutcome::Duplicate,
                    AppendOutcome::Appended => self.persist_append(channel, seed, &envelope),
                }
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
                        self.persist_compact(channel, seed, turn_id, *round_num);
                    }
                }
                for key in terminal_replaceable_keys(&envelope.event) {
                    st.router.flush_replaceable(&key);
                    st.replaceable_since_checkpoint.remove(&key);
                    self.persist_remove_replaceable(channel, seed, &format!("{key:?}"));
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
                            let count = st
                                .replaceable_since_checkpoint
                                .entry(key.clone())
                                .or_default();
                            *count = count.saturating_add(1);
                            let first_progress = matches!(
                                &envelope.event,
                                RingingEvent::Tool(deepx_domain::ToolEvent::ToolProgress {
                                    seq_start: 0,
                                    ..
                                })
                            );
                            if *count == 1 || *count >= 64 || first_progress {
                                self.persist_replaceable(
                                    channel,
                                    seed,
                                    &format!("{key:?}"),
                                    &envelope,
                                );
                                *count = 0;
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
        let guard = self.channel_state(channel);
        let st = guard
            .get(&channel)
            .and_then(|seeds| seeds.get(seed))
            .ok_or(CursorExpired {
                earliest_available_seq: 0,
            })?;
        st.journal.replay_since(after_stream_seq).map(|mut events| {
            // 追加当前 replaceable 值（慢消费者恢复增量）
            events.extend(
                st.router
                    .replay_since(after_stream_seq)
                    .into_iter()
                    .filter(|e| e.delivery != Delivery::Reliable),
            );
            events
        })
    }

    /// 频道级回放（SSE 重连用）：聚合该频道所有 seed 的可靠 tail 与
    /// 当前 replaceable 值。某个 seed 的 cursor 超出保留窗口时产出
    /// `RingingResetRequired`，客户端应改走 snapshot 恢复。
    pub fn replay_channel_since(
        &self,
        channel: RingingChannel,
        after_stream_seq: u64,
    ) -> ChannelReplay {
        let guard = self.channel_state(channel);
        let mut replay = ChannelReplay::default();
        let Some(seeds) = guard.get(&channel) else {
            return replay;
        };
        for (seed, st) in seeds {
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

    /// 读取领域快照（HTTP `GET /ringing/v2/sessions/{seed}/bootstrap`）。
    pub fn snapshot(&self, channel: RingingChannel, seed: &str) -> RingingChannelSnapshot {
        let guard = self.channel_state(channel);
        guard
            .get(&channel)
            .and_then(|seeds| seeds.get(seed))
            .map(|st| {
                st.projection
                    .snapshot_for(channel, seed, st.last_stream_seq)
            })
            .unwrap_or_else(|| SnapshotProjector::new().snapshot_for(channel, seed, 0))
    }

    /// Conversation 频道完整快照：领域投影摘要 + 持久化消息构建的 turns。
    pub fn conversation_snapshot(&self, seed: &str) -> RingingChannelSnapshot {
        let mut snap = self.snapshot(RingingChannel::Conversation, seed);
        if let Some(state) = super::conversation_snapshot::persisted_conversation_state(seed) {
            match snap.state.as_object_mut() {
                Some(obj) => {
                    for key in ["turns", "total_turns", "has_more", "usage", "usage_totals"] {
                        if let Some(value) = state.get(key) {
                            obj.insert(key.to_string(), value.clone());
                        }
                    }
                }
                None => snap.state = state,
            }
        }
        snap
    }

    /// 记 replaceable checkpoint（稀疏）。
    pub fn checkpoint(&self, channel: RingingChannel, seed: &str, identity: &str, stream_seq: u64) {
        let mut guard = self.channel_state(channel);
        let st = self.seed_state(&mut guard, channel, seed);
        st.last_stream_seq = st.last_stream_seq.max(stream_seq);
        st.journal.checkpoint_replaceable(identity, stream_seq);
        self.persist_checkpoint(channel, seed, identity, stream_seq);
    }

    pub fn last_stream_seq(&self, channel: RingingChannel, seed: &str) -> u64 {
        let guard = self.channel_state(channel);
        guard
            .get(&channel)
            .and_then(|seeds| seeds.get(seed))
            .map(|s| s.last_stream_seq)
            .unwrap_or(0)
    }

    // ── 持久化钩子：I/O 失败只记录日志，绝不阻塞事件路径 ──

    fn persist_append(&self, channel: RingingChannel, seed: &str, envelope: &RingingEventEnvelope) {
        let mut guard = self.journal_store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(store) = guard.as_mut()
            && let Err(error) = store.append(channel, seed, envelope)
        {
            log::warn!("[ringing] journal append failed: {error}");
        }
    }

    fn persist_compact(&self, channel: RingingChannel, seed: &str, turn_id: &str, round_num: u32) {
        let mut guard = self.journal_store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(store) = guard.as_mut()
            && let Err(error) = store.compact(channel, seed, turn_id, round_num)
        {
            log::warn!("[ringing] journal compact persist failed: {error}");
        }
    }

    fn persist_checkpoint(
        &self,
        channel: RingingChannel,
        seed: &str,
        identity: &str,
        stream_seq: u64,
    ) {
        let mut guard = self.journal_store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(store) = guard.as_mut()
            && let Err(error) = store.checkpoint(channel, seed, identity, stream_seq)
        {
            log::warn!("[ringing] journal checkpoint persist failed: {error}");
        }
    }

    fn persist_replaceable(
        &self,
        channel: RingingChannel,
        seed: &str,
        identity: &str,
        envelope: &RingingEventEnvelope,
    ) {
        let mut guard = self.journal_store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(store) = guard.as_mut()
            && let Err(error) = store.replaceable(channel, seed, identity, envelope)
        {
            log::warn!("[ringing] replaceable slot persist failed: {error}");
        }
    }

    fn persist_remove_replaceable(&self, channel: RingingChannel, seed: &str, identity: &str) {
        let mut guard = self.journal_store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(store) = guard.as_mut()
            && let Err(error) = store.remove_replaceable(channel, seed, identity)
        {
            log::warn!("[ringing] replaceable slot cleanup failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepx_domain::{ConversationEvent, ToolEvent};

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "deepx-ringing-hub-{label}-{}-{}",
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
            kind: deepx_domain::RoundDeltaKind::Thinking,
            delta: format!("chunk-{seq}"),
        })
    }

    fn tool_progress(chunk: &str) -> DomainEvent {
        DomainEvent::Tool(ToolEvent::ToolProgress {
            tool_call_id: "c1".into(),
            turn_id: "t".into(),
            round_num: 0,
            stream: "stdout".into(),
            seq_start: 0,
            seq_end: 1,
            chunk: chunk.into(),
            dropped_bytes: 0,
            truncated: false,
        })
    }

    #[test]
    fn persisted_journal_survives_restart() {
        let root = temp_root("restart");
        {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            let _ = hub.publish("s", round_delta(1));
            let _ = hub.publish("s", tool_progress("a"));
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
                .any(|e| matches!(&e.event, RingingEvent::Tool(ToolEvent::ToolProgress { chunk, .. }) if chunk == "a")),
            "replaceable latest value must survive restart"
        );
        // 序号继续递增（新 epoch 内不从头冲突）
        let outcome = hub.publish("s", round_delta(99));
        if let PublishOutcome::Published { envelope } = outcome {
            assert!(envelope.stream_seq > 0);
            assert_eq!(envelope.server_epoch, "epoch-2");
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
                assert_eq!(envelope.server_epoch, "epoch-1");
                assert_eq!(envelope.seed, "s1");
                assert_eq!(envelope.stream_seq, 1);
                assert_eq!(envelope.channel_seq, 1);
                assert_eq!(envelope.session_seq, 1);
                assert_eq!(envelope.channel, RingingChannel::Tool);
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
                kind: deepx_domain::RoundDeltaKind::Answering,
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
        let replay = hub.replay_channel_since(RingingChannel::Tool, 0);
        assert_eq!(replay.resets.len(), 0);
        assert_eq!(replay.events.len(), 2);
        // stream_seq 全局递增，跨 seed 合并后按序排列
        assert_eq!(replay.events[0].stream_seq, 1);
        assert_eq!(replay.events[1].stream_seq, 2);
        assert_eq!(replay.events[0].seed, "s1");
        assert_eq!(replay.events[1].seed, "s2");

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
        let replayed = hub2.replay_channel_since(RingingChannel::Tool, 0);
        assert!(!replayed.resets.is_empty());
        assert_eq!(replayed.resets[0].seed, "s1");
        assert!(replayed.resets[0].earliest_available_seq > 1);
    }

    #[test]
    fn replaceable_progress_covers_in_router() {
        let hub = RingingHub::new("epoch-1");
        let progress = |chunk: &str| {
            DomainEvent::Tool(ToolEvent::ToolProgress {
                tool_call_id: "c1".into(),
                turn_id: "t".into(),
                round_num: 0,
                stream: "stdout".into(),
                seq_start: 0,
                seq_end: 1,
                chunk: chunk.into(),
                dropped_bytes: 0,
                truncated: false,
            })
        };
        let _ = hub.publish("s", progress("a"));
        let _ = hub.publish("s", progress("ab"));
        let replayed = hub.replay_since(RingingChannel::Tool, "s", 0).expect("ok");
        let progress_events: Vec<_> = replayed
            .iter()
            .filter(|e| {
                matches!(
                    e.event,
                    deepx_ringing::RingingEvent::Tool(ToolEvent::ToolProgress { .. })
                )
            })
            .collect();
        assert_eq!(progress_events.len(), 1, "only latest progress survives");
    }

    #[test]
    fn checkpoint_records_sparse_progress() {
        let hub = RingingHub::new("epoch-1");
        hub.checkpoint(RingingChannel::Tool, "s", "tool:c1", 7);
        let guard = hub.channels.lock().unwrap_or_else(|e| e.into_inner());
        let st = guard
            .get(&RingingChannel::Tool)
            .and_then(|seeds| seeds.get("s"))
            .expect("channel+seed exists");
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
        assert_eq!(env.channel, RingingChannel::Conversation);
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
    fn round_deltas_are_reliable_and_compacted_on_round_completed() {
        let hub = RingingHub::new("epoch-1");
        let delta = |seq: u64| {
            DomainEvent::Conversation(ConversationEvent::RoundDelta {
                turn_id: "t1".into(),
                round_num: 1,
                kind: deepx_domain::RoundDeltaKind::Answering,
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
            deepx_ringing::RingingEvent::Conversation(ConversationEvent::RoundCompleted { .. })
        ));
    }
}
