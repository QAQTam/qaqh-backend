//! Direct streaming output for high-frequency model deltas.
//!
//! The renderer owns frame-level coalescing. Keeping the worker transport
//! immediate prevents hidden server-side latency at high token rates.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use deepx_proto::Agent2Ui;

use super::types::Emitter;
use super::types::WriterEvent;

pub struct PacedEmitter {
    tx: mpsc::SyncSender<WriterEvent>,
    writer_dead: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

impl PacedEmitter {
    pub fn new(
        tx: mpsc::SyncSender<WriterEvent>,
        writer_dead: Arc<AtomicBool>,
        cancelled: Arc<AtomicBool>,
    ) -> Self {
        Self { tx, writer_dead, cancelled }
    }
}

impl Emitter for PacedEmitter {
    fn emit(&self, event: Agent2Ui) {
        if !self.writer_dead.load(Ordering::SeqCst) {
            let _ = self.tx.send(WriterEvent::Legacy(event));
        }
    }

    fn emit_delta(&self, event: Agent2Ui) {
        // Do not pace or discard model text. When the downstream pipe is
        // temporarily saturated, retry until it drains. Cancellation remains
        // responsive because the reader thread sets `cancelled` directly.
        let mut pending = event;
        loop {
            if self.writer_dead.load(Ordering::SeqCst) || self.cancelled.load(Ordering::SeqCst) {
                return;
            }
            match self.tx.try_send(WriterEvent::Legacy(pending)) {
                Ok(()) | Err(mpsc::TrySendError::Disconnected(_)) => return,
                Err(mpsc::TrySendError::Full(event)) => {
                    pending = match event {
                        WriterEvent::Legacy(agent_event) => agent_event,
                        WriterEvent::Ringing(_) => unreachable!("emit_delta only sends legacy"),
                    };
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }

    fn emit_domain(&self, event: deepx_domain::DomainEvent) {
        if self.writer_dead.load(Ordering::SeqCst) {
            return;
        }
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let env = deepx_ringing::RingingWorkerEventEnvelope::new(
            "worker",
            format!("w-{seq}"),
            event.into(),
        );
        let _ = self.tx.send(WriterEvent::Ringing(env));
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::thread;

    use deepx_proto::RoundDeltaKind;

    fn round_delta(delta: &str) -> Agent2Ui {
        Agent2Ui::RoundDelta {
            turn_id: "t1".into(),
            round_num: 1,
            kind: RoundDeltaKind::Answering,
            delta: delta.into(),
        }
    }

    struct TestHarness {
        events: Arc<Mutex<Vec<Agent2Ui>>>,
        pacer: PacedEmitter,
        _tx: mpsc::SyncSender<WriterEvent>,
    }

    impl TestHarness {
        fn new() -> Self {
            let (tx, rx) = mpsc::sync_channel::<WriterEvent>(128);
            let events = Arc::new(Mutex::new(Vec::new()));
            let collected = events.clone();
            thread::spawn(move || {
                while let Ok(event) = rx.recv() {
                    if let WriterEvent::Legacy(agent_event) = event {
                        collected
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(agent_event);
                    }
                }
            });
            Self {
                events,
            pacer: PacedEmitter::new(
                tx.clone(),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
            ),
                _tx: tx,
            }
        }

        fn take_events(&self) -> Vec<Agent2Ui> {
            thread::sleep(Duration::from_millis(5));
            self.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain(..)
                .collect()
        }
    }

    #[test]
    fn text_deltas_pass_through_without_a_timer_batch() {
        let h = TestHarness::new();
        h.pacer.emit_delta(round_delta("abc"));
        h.pacer.emit_delta(round_delta("你好"));
        thread::sleep(Duration::from_millis(30));
        let deltas: Vec<_> = h
            .take_events()
            .into_iter()
            .filter_map(|event| match event {
                Agent2Ui::RoundDelta { delta, .. } => Some(delta),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, ["abc", "你好"]);
    }

    #[test]
    fn metadata_boundaries_are_not_merged() {
        let h = TestHarness::new();
        h.pacer.emit_delta(round_delta("answer"));
        h.pacer.emit_delta(Agent2Ui::RoundDelta {
            turn_id: "t1".into(),
            round_num: 1,
            kind: RoundDeltaKind::Thinking,
            delta: "thought".into(),
        });
        h.pacer.emit(Agent2Ui::Done);
        let deltas: Vec<_> = h
            .take_events()
            .into_iter()
            .filter_map(|event| match event {
                Agent2Ui::RoundDelta { kind, delta, .. } => Some((kind, delta)),
                _ => None,
            })
            .collect();
        assert_eq!(deltas.len(), 2);
        assert_eq!(deltas[0], (RoundDeltaKind::Answering, "answer".into()));
        assert_eq!(deltas[1], (RoundDeltaKind::Thinking, "thought".into()));
    }

    #[test]
    fn non_delta_events_pass_through_without_waiting_for_tick() {
        let h = TestHarness::new();
        h.pacer.emit_delta(Agent2Ui::Ready);
        assert!(matches!(h.take_events()[0], Agent2Ui::Ready));
    }

    #[test]
    fn tool_preview_revisions_pass_through_without_a_timer_batch() {
        let h = TestHarness::new();
        for args in ["{", "{\"path\"", "{\"path\":\"x\"}"] {
            h.pacer.emit_delta(Agent2Ui::ToolCallPreview {
                turn_id: "t1".into(),
                round_num: 1,
                index: 0,
                id: "call".into(),
                name: "read".into(),
                args_so_far: args.into(),
            });
        }
        thread::sleep(Duration::from_millis(30));
        let previews: Vec<_> = h
            .take_events()
            .into_iter()
            .filter_map(|event| match event {
                Agent2Ui::ToolCallPreview { args_so_far, .. } => Some(args_so_far),
                _ => None,
            })
            .collect();
        assert_eq!(previews, ["{", "{\"path\"", "{\"path\":\"x\"}"]);
    }

    #[test]
    fn usage_updates_pass_through_without_a_timer_batch() {
        let h = TestHarness::new();
        for total_tokens in [10, 20, 30] {
            h.pacer.emit_delta(Agent2Ui::UsageUpdated {
                turn_id: "t1".into(),
                round_num: 2,
                usage: deepx_types::UsageInfo {
                    prompt_tokens: 10,
                    completion_tokens: total_tokens - 10,
                    total_tokens,
                    prompt_cache_hit_tokens: 8,
                    prompt_cache_miss_tokens: 2,
                    reasoning_tokens: 0,
                    cache_usage_reported: Some(true),
                },
                context_limit: 1000,
                model: "test".into(),
            });
        }
        thread::sleep(Duration::from_millis(30));
        let totals: Vec<_> = h
            .take_events()
            .into_iter()
            .filter_map(|event| match event {
                Agent2Ui::UsageUpdated { usage, .. } => Some(usage.total_tokens),
                _ => None,
            })
            .collect();
        assert_eq!(totals, [10, 20, 30]);
    }

    #[test]
    fn interleaved_event_kinds_keep_input_order() {
        let h = TestHarness::new();
        h.pacer.emit_delta(round_delta("before"));
        h.pacer.emit_delta(Agent2Ui::ToolCallPreview {
            turn_id: "t1".into(),
            round_num: 1,
            index: 0,
            id: "call".into(),
            name: "read".into(),
            args_so_far: "{}".into(),
        });
        h.pacer.emit_delta(round_delta("after"));
        h.pacer.emit(Agent2Ui::Done);
        let kinds: Vec<_> = h
            .take_events()
            .into_iter()
            .filter_map(|event| match event {
                Agent2Ui::RoundDelta { delta, .. } => Some(delta),
                Agent2Ui::ToolCallPreview { .. } => Some("preview".into()),
                Agent2Ui::Done => None,
                _ => None,
            })
            .collect();
        assert_eq!(kinds, ["before", "preview", "after"]);
    }

    #[test]
    fn terminal_event_never_overtakes_pending_text() {
        let h = TestHarness::new();
        h.pacer.emit_delta(round_delta("complete"));
        h.pacer.emit(Agent2Ui::Done);
        let events = h.take_events();
        let delta = events
            .iter()
            .position(|event| matches!(event, Agent2Ui::RoundDelta { .. }))
            .unwrap();
        let done = events
            .iter()
            .position(|event| matches!(event, Agent2Ui::Done))
            .unwrap();
        assert!(delta < done);
    }

    #[test]
    fn domain_events_flow_through_ringing_envelope_channel() {
        let (tx, rx) = mpsc::sync_channel::<WriterEvent>(16);
        let emitter = PacedEmitter::new(
            tx,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        emitter.emit_domain(deepx_domain::DomainEvent::Tool(
            deepx_domain::ToolEvent::ToolStarted {
                tool_call_id: "c1".into(),
                turn_id: "t1".into(),
                round_num: 0,
                name: "exec".into(),
            },
        ));
        match rx.recv().expect("envelope") {
            WriterEvent::Ringing(env) => {
                assert_eq!(env.wire, deepx_ringing::worker::WIRE_RINGING_DOMAIN_V1);
                assert_eq!(env.seed, "worker");
                assert!(matches!(
                    env.event,
                    deepx_ringing::RingingEvent::Tool(deepx_domain::ToolEvent::ToolStarted { .. })
                ));
            }
            other => panic!("expected Ringing envelope, got {other:?}"),
        }
    }

    #[test]
    fn saturated_channel_waits_for_capacity_without_losing_a_delta() {
        let (tx, rx) = mpsc::sync_channel::<WriterEvent>(1);
        tx.send(WriterEvent::Legacy(round_delta("first"))).unwrap();
        let emitter = PacedEmitter::new(
            tx,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let worker = thread::spawn(move || emitter.emit_delta(round_delta("second")));

        thread::sleep(Duration::from_millis(5));
        assert!(matches!(rx.recv().unwrap(), WriterEvent::Legacy(Agent2Ui::RoundDelta { ref delta, .. }) if delta == "first"));
        worker.join().unwrap();
        assert!(matches!(rx.recv().unwrap(), WriterEvent::Legacy(Agent2Ui::RoundDelta { ref delta, .. }) if delta == "second"));
    }

    #[test]
    fn cancellation_releases_a_delta_waiting_for_capacity() {
        let (tx, _rx) = mpsc::sync_channel::<WriterEvent>(1);
        tx.send(WriterEvent::Legacy(round_delta("first"))).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let emitter = PacedEmitter::new(tx, Arc::new(AtomicBool::new(false)), cancelled.clone());
        let worker = thread::spawn(move || emitter.emit_delta(round_delta("discarded")));

        thread::sleep(Duration::from_millis(5));
        cancelled.store(true, Ordering::SeqCst);
        worker.join().unwrap();
    }
}
