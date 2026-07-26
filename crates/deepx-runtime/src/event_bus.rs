use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use deepx_proto::{Agent2Ui, ControlServerMessage};
use tokio::sync::broadcast;

const DEFAULT_EVENT_CAPACITY: usize = 4096;

#[derive(Debug, Clone)]
pub struct PublishedAgentEvent {
    pub seq: u64,
    pub seed: String,
    pub session_seq: u64,
    pub event: Agent2Ui,
}

struct EventState {
    next_seq: u64,
    session_seq: HashMap<String, u64>,
    journal: VecDeque<PublishedAgentEvent>,
    capacity: usize,
    projections: HashMap<String, Vec<Agent2Ui>>,
}

#[derive(Clone)]
pub struct EventBus {
    epoch: Arc<str>,
    state: Arc<Mutex<EventState>>,
    sender: broadcast::Sender<ControlServerMessage>,
}

impl EventBus {
    pub fn new(epoch: impl Into<String>) -> Self {
        Self::with_capacity(epoch, DEFAULT_EVENT_CAPACITY)
    }

    pub fn with_capacity(epoch: impl Into<String>, capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(16));
        Self {
            epoch: Arc::from(epoch.into()),
            state: Arc::new(Mutex::new(EventState {
                next_seq: 0,
                session_seq: HashMap::new(),
                journal: VecDeque::new(),
                capacity: capacity.max(1),
                projections: HashMap::new(),
            })),
            sender,
        }
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    pub fn current_seq(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .next_seq
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ControlServerMessage> {
        self.sender.subscribe()
    }

    pub fn publish(&self, seed: &str, event: Agent2Ui) -> PublishedAgentEvent {
        let published = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.next_seq = state.next_seq.saturating_add(1);
            let seq = state.next_seq;
            let session_seq = {
                let value = state.session_seq.entry(seed.to_string()).or_default();
                *value = value.saturating_add(1);
                *value
            };
            let published = PublishedAgentEvent {
                seq,
                seed: seed.to_string(),
                session_seq,
                event: event.clone(),
            };
            let projection = state.projections.entry(seed.to_string()).or_default();
            if matches!(
                event,
                Agent2Ui::SessionCreated { .. } | Agent2Ui::SessionRestored { .. }
            ) {
                projection.clear();
            }
            update_projection(projection, &event);
            while state.journal.len() >= state.capacity {
                state.journal.pop_front();
            }
            state.journal.push_back(published.clone());
            published
        };
        let _ = self.sender.send(ControlServerMessage::Event {
            server_epoch: self.epoch.to_string(),
            seq: published.seq,
            seed: published.seed.clone(),
            session_seq: published.session_seq,
            event: published.event.clone(),
        });
        published
    }

    pub fn projections_for(&self, seeds: &[String]) -> HashMap<String, Vec<Agent2Ui>> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        seeds
            .iter()
            .filter_map(|seed| {
                state
                    .projections
                    .get(seed)
                    .cloned()
                    .map(|events| (seed.clone(), events))
            })
            .collect()
    }

    pub fn publish_activity(&self, activity: deepx_proto::SessionActivity) {
        let _ = self
            .sender
            .send(ControlServerMessage::SessionActivity { activity });
    }

    pub fn replay_after(&self, epoch: &str, after_seq: u64) -> Option<Vec<ControlServerMessage>> {
        if epoch != self.epoch.as_ref() {
            return None;
        }
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if after_seq > state.next_seq {
            return None;
        }
        if let Some(first) = state.journal.front()
            && after_seq.saturating_add(1) < first.seq
        {
            return None;
        }
        Some(
            state
                .journal
                .iter()
                .filter(|item| item.seq > after_seq)
                .map(|item| ControlServerMessage::Event {
                    server_epoch: self.epoch.to_string(),
                    seq: item.seq,
                    seed: item.seed.clone(),
                    session_seq: item.session_seq,
                    event: item.event.clone(),
                })
                .collect(),
        )
    }
}

/// Keep Snapshot projections canonical instead of retaining every transient
/// streaming revision. Live subscribers still receive every published event;
/// this only compacts the state used for reconnect/recovery.
fn update_projection(projection: &mut Vec<Agent2Ui>, event: &Agent2Ui) {
    match event {
        Agent2Ui::RoundDelta {
            turn_id,
            round_num,
            kind,
            delta,
        } => {
            if let Some(Agent2Ui::RoundDelta {
                turn_id: previous_turn,
                round_num: previous_round,
                kind: previous_kind,
                delta: previous_delta,
            }) = projection.last_mut()
                && previous_turn == turn_id
                && previous_round == round_num
                && std::mem::discriminant(previous_kind) == std::mem::discriminant(kind)
            {
                previous_delta.push_str(delta);
                return;
            }
        }
        Agent2Ui::ToolCallPreview {
            turn_id,
            round_num,
            index,
            ..
        } => {
            if let Some(previous) = projection.iter_mut().rev().find(|candidate| {
                matches!(candidate, Agent2Ui::ToolCallPreview {
                    turn_id: previous_turn,
                    round_num: previous_round,
                    index: previous_index,
                    ..
                } if previous_turn == turn_id && previous_round == round_num && previous_index == index)
            }) {
                *previous = event.clone();
                return;
            }
        }
        Agent2Ui::RoundComplete {
            turn_id,
            round_num,
            thinking,
            answer,
            tool_calls,
            ..
        } => {
            projection.retain(|candidate| {
                let same_round = match candidate {
                    Agent2Ui::RoundDelta {
                        turn_id: candidate_turn,
                        round_num: candidate_round,
                        ..
                    }
                    | Agent2Ui::ToolCallPreview {
                        turn_id: candidate_turn,
                        round_num: candidate_round,
                        ..
                    } => candidate_turn == turn_id && candidate_round == round_num,
                    _ => false,
                };
                if !same_round {
                    return true;
                }
                match candidate {
                    Agent2Ui::RoundDelta { kind, .. } => match kind {
                        deepx_proto::RoundDeltaKind::Thinking => thinking.is_none(),
                        deepx_proto::RoundDeltaKind::Answering => answer.is_none(),
                        deepx_proto::RoundDeltaKind::ToolCalling => tool_calls.is_empty(),
                    },
                    Agent2Ui::ToolCallPreview { .. } => tool_calls.is_empty(),
                    _ => true,
                }
            });
        }
        Agent2Ui::ToolExecDelta {
            tool_call_id,
            delta,
        } => {
            if let Some(Agent2Ui::ToolExecDelta {
                tool_call_id: previous_id,
                delta: previous_delta,
            }) = projection.last_mut()
                && previous_id == tool_call_id
            {
                previous_delta.push_str(delta);
                return;
            }
        }
        Agent2Ui::ToolResults { results, .. } => {
            projection.retain(|candidate| {
                !matches!(candidate,
                    Agent2Ui::ToolExecDelta { tool_call_id, .. }
                    | Agent2Ui::ExecProgress { tool_call_id, .. }
                    if results.iter().any(|result| result.tool_call_id == *tool_call_id))
            });
        }
        _ => {}
    }
    projection.push(event.clone());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assigns_global_and_per_session_sequences() {
        let bus = EventBus::with_capacity("epoch", 2);
        let a = bus.publish("a", Agent2Ui::Ready);
        let b = bus.publish("b", Agent2Ui::Ready);
        let a2 = bus.publish("a", Agent2Ui::Done);
        assert_eq!((a.seq, b.seq, a2.seq), (1, 2, 3));
        assert_eq!((a.session_seq, b.session_seq, a2.session_seq), (1, 1, 2));
        assert!(bus.replay_after("epoch", 0).is_none());
        assert_eq!(bus.replay_after("epoch", 1).unwrap().len(), 2);
        assert!(bus.replay_after("other", 3).is_none());
    }

    #[test]
    fn session_restore_replaces_the_projection_baseline() {
        let bus = EventBus::new("epoch");
        bus.publish("s", Agent2Ui::Ready);
        bus.publish(
            "s",
            Agent2Ui::SessionRestored {
                seed: "s".into(),
                turns: vec![],
                tokens_used: 0,
                cache_hit_pct: 0.0,
                total_turns: 0,
                has_more: false,
            },
        );
        bus.publish("s", Agent2Ui::Ready);
        let projection = bus.projections_for(&["s".into()]);
        assert_eq!(projection["s"].len(), 2);
        assert!(matches!(
            projection["s"][0],
            Agent2Ui::SessionRestored { .. }
        ));
    }

    #[test]
    fn snapshot_projection_compacts_transient_stream_events() {
        let bus = EventBus::new("epoch");
        bus.publish(
            "s",
            Agent2Ui::RoundDelta {
                turn_id: "t".into(),
                round_num: 1,
                kind: deepx_proto::RoundDeltaKind::Answering,
                delta: "a".into(),
            },
        );
        bus.publish(
            "s",
            Agent2Ui::RoundDelta {
                turn_id: "t".into(),
                round_num: 1,
                kind: deepx_proto::RoundDeltaKind::Answering,
                delta: "b".into(),
            },
        );
        for args in ["{", "{\"path\"", "{\"path\":\"x\"}"] {
            bus.publish(
                "s",
                Agent2Ui::ToolCallPreview {
                    turn_id: "t".into(),
                    round_num: 1,
                    index: 0,
                    id: "call".into(),
                    name: "read".into(),
                    args_so_far: args.into(),
                },
            );
        }
        let before = bus.projections_for(&["s".into()]);
        assert_eq!(before["s"].len(), 2);
        assert!(matches!(&before["s"][0], Agent2Ui::RoundDelta { delta, .. } if delta == "ab"));
        assert!(
            matches!(&before["s"][1], Agent2Ui::ToolCallPreview { args_so_far, .. } if args_so_far == "{\"path\":\"x\"}")
        );

        bus.publish(
            "s",
            Agent2Ui::RoundComplete {
                turn_id: "t".into(),
                round_num: 1,
                thinking: None,
                answer: Some("ab".into()),
                tool_calls: vec![deepx_proto::ToolCallDef {
                    id: "call".into(),
                    name: "read".into(),
                    args_display: "x".into(),
                    args_json: "{\"path\":\"x\"}".into(),
                }],
                blocks: vec![],
                is_final: true,
            },
        );
        let after = bus.projections_for(&["s".into()]);
        assert_eq!(after["s"].len(), 1);
        assert!(matches!(after["s"][0], Agent2Ui::RoundComplete { .. }));
    }

    #[test]
    fn snapshot_projection_keeps_preview_when_completion_omits_it() {
        let bus = EventBus::new("epoch");
        bus.publish(
            "s",
            Agent2Ui::RoundDelta {
                turn_id: "t".into(),
                round_num: 1,
                kind: deepx_proto::RoundDeltaKind::Thinking,
                delta: "draft".into(),
            },
        );
        bus.publish(
            "s",
            Agent2Ui::RoundComplete {
                turn_id: "t".into(),
                round_num: 1,
                thinking: None,
                answer: Some("answer".into()),
                tool_calls: vec![],
                blocks: vec![],
                is_final: true,
            },
        );
        let projection = bus.projections_for(&["s".into()]);
        assert_eq!(projection["s"].len(), 2);
        assert!(
            matches!(&projection["s"][0], Agent2Ui::RoundDelta { delta, .. } if delta == "draft")
        );
    }
}
