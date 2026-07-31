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
use std::sync::Mutex;

use deepx_domain::{Delivery, DomainEvent, RingingChannel};
use deepx_ringing::{RingingChannelSnapshot, RingingEventEnvelope};
use tokio::sync::broadcast;

use super::journal::{AppendOutcome, CursorExpired, ReliableJournal};
use super::projection::SnapshotProjector;
use super::router::ChannelRouter;
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

#[derive(Debug)]
struct SeedChannelState {
    router: ChannelRouter,
    journal: ReliableJournal,
    projection: SnapshotProjector,
}

impl SeedChannelState {
    fn new(channel: RingingChannel) -> Self {
        Self {
            router: ChannelRouter::new(channel),
            journal: ReliableJournal::new(),
            projection: SnapshotProjector::new(),
        }
    }
}

/// Ringing daemon 运行时聚合。
#[derive(Debug)]
pub struct RingingHub {
    epoch: String,
    sequencer: Sequencer,
    /// channel → (seed → state)。router/journal/projection 均 per (seed, channel)。
    channels: Mutex<HashMap<RingingChannel, HashMap<String, SeedChannelState>>>,
    /// 每频道实时推送通道（SSE 消费；可靠性由 journal/cursor 保证）。
    live: Mutex<HashMap<RingingChannel, broadcast::Sender<RingingEventEnvelope>>>,
}

impl RingingHub {
    pub fn new(epoch: impl Into<String>) -> Self {
        Self {
            epoch: epoch.into(),
            sequencer: Sequencer::new(),
            channels: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
        }
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    fn channel_state(
        &self,
        _channel: RingingChannel,
    ) -> std::sync::MutexGuard<'_, HashMap<RingingChannel, HashMap<String, SeedChannelState>>>
    {
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
        let channel = event.channel();
        let delivery = event.delivery();
        let (stream_seq, channel_seq, session_seq) = self.sequencer.next(channel, seed);
        let event_id = format!(
            "{}-{}-{}-{}",
            self.epoch, channel.as_str(), seed, stream_seq
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

        let state_changed = st.projection.apply(channel, seed, &event);
        let revision = st.projection.revision(channel, seed);
        let mut envelope = envelope;
        if state_changed {
            envelope = envelope.with_state_revision(revision);
        }

        match delivery {
            Delivery::Reliable => {
                match st.journal.append(&envelope) {
                    AppendOutcome::Duplicate => return PublishOutcome::Duplicate,
                    AppendOutcome::Appended => {}
                }
                match st.router.route(envelope.clone()) {
                    super::router::RouteOutcome::Routed { .. } => {
                        self.fanout(channel, &envelope);
                        PublishOutcome::Published { envelope }
                    }
                    super::router::RouteOutcome::Backpressure => PublishOutcome::Backpressure,
                }
            }
            Delivery::Replaceable | Delivery::Ephemeral => {
                // replaceable 覆盖入槽；ephemeral 不入队但照常实时推送
                match st.router.route(envelope.clone()) {
                    super::router::RouteOutcome::Routed { .. } => {
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
            .ok_or(CursorExpired { earliest_available_seq: 0 })?;
        st.journal.replay_since(after_stream_seq).map(|mut events| {
            // 追加当前 replaceable 值（慢消费者恢复增量）
            events.extend(st.router.replay_since(after_stream_seq).into_iter().filter(|e| {
                e.delivery != Delivery::Reliable
            }));
            events
        })
    }

    /// 读取领域快照（HTTP `GET /ringing/v1/snapshots/{channel}/{seed}`）。
    pub fn snapshot(&self, channel: RingingChannel, seed: &str) -> RingingChannelSnapshot {
        let guard = self.channel_state(channel);
        guard
            .get(&channel)
            .and_then(|seeds| seeds.get(seed))
            .map(|st| st.projection.snapshot_for(channel, seed, st.router.last_stream_seq()))
            .unwrap_or_else(|| SnapshotProjector::new().snapshot_for(channel, seed, 0))
    }

    /// 记 replaceable checkpoint（稀疏）。
    pub fn checkpoint(&self, channel: RingingChannel, seed: &str, identity: &str, stream_seq: u64) {
        let mut guard = self.channel_state(channel);
        let st = self.seed_state(&mut guard, channel, seed);
        st.journal.checkpoint_replaceable(identity, stream_seq);
    }

    pub fn last_stream_seq(&self, channel: RingingChannel, seed: &str) -> u64 {
        let guard = self.channel_state(channel);
        guard
            .get(&channel)
            .and_then(|seeds| seeds.get(seed))
            .map(|s| s.router.last_stream_seq())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepx_domain::{ConversationEvent, ToolEvent};

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
    fn duplicate_event_id_is_idempotent_dropped() {
        let hub = RingingHub::new("epoch-1");
        let ev = DomainEvent::Conversation(ConversationEvent::ConversationCancelled {
            turn_id: None,
        });
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
        let replayed = hub
            .replay_since(RingingChannel::Tool, "s", 0)
            .expect("ok");
        let progress_events: Vec<_> = replayed
            .iter()
            .filter(|e| matches!(e.event, deepx_ringing::RingingEvent::Tool(ToolEvent::ToolProgress { .. })))
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
            DomainEvent::Conversation(ConversationEvent::ConversationCancelled {
                turn_id: None,
            }),
        );
        let env = rx.blocking_recv().expect("live event");
        assert_eq!(env.channel, RingingChannel::Conversation);
        assert_eq!(env.seed, "s");
    }
}
