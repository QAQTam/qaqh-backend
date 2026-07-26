//! Bounded batching for high-frequency streaming deltas.
//!
//! Provider SSE chunks are coalesced for one short display frame before they
//! enter the worker stdout pipe. Critical/non-stream events still pass through
//! immediately, while terminal events wait for pending text so ordering stays
//! deterministic.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use deepx_proto::{Agent2Ui, RoundDeltaKind};

use super::types::Emitter;

pub const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_millis(16);
const MAX_BATCH_CHARS: usize = 512;

struct PendingDelta {
    turn_id: String,
    round_num: u32,
    kind: RoundDeltaKind,
    text: String,
    chars: usize,
}

impl PendingDelta {
    fn matches(&self, turn_id: &str, round_num: u32, kind: RoundDeltaKind) -> bool {
        self.turn_id == turn_id && self.round_num == round_num && self.kind == kind
    }

    fn into_event(self) -> Agent2Ui {
        Agent2Ui::RoundDelta {
            turn_id: self.turn_id,
            round_num: self.round_num,
            kind: self.kind,
            delta: self.text,
        }
    }
}

enum PendingEvent {
    Delta(PendingDelta),
    Latest { key: String, event: Agent2Ui },
}

impl PendingEvent {
    fn into_event(self) -> Agent2Ui {
        match self {
            Self::Delta(delta) => delta.into_event(),
            Self::Latest { event, .. } => event,
        }
    }
}

#[derive(Default)]
struct BatchState {
    pending: VecDeque<PendingEvent>,
    in_flight: bool,
}

pub struct PacedEmitter {
    tx: mpsc::SyncSender<Agent2Ui>,
    writer_dead: Arc<AtomicBool>,
    state: Arc<Mutex<BatchState>>,
    shutdown: Arc<AtomicBool>,
    _drainer: JoinHandle<()>,
}

impl PacedEmitter {
    pub fn new(
        tx: mpsc::SyncSender<Agent2Ui>,
        writer_dead: Arc<AtomicBool>,
        flush_interval: Duration,
    ) -> Self {
        assert!(!flush_interval.is_zero(), "flush interval must be positive");
        let state = Arc::new(Mutex::new(BatchState::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let drainer_state = state.clone();
        let drainer_shutdown = shutdown.clone();
        let drainer_tx = tx.clone();
        let drainer_writer_dead = writer_dead.clone();

        let drainer = thread::Builder::new()
            .name("stream-batcher".into())
            .spawn(move || {
                loop {
                    thread::sleep(flush_interval);
                    let pending = {
                        let mut state = drainer_state.lock().unwrap_or_else(|e| e.into_inner());
                        let pending = state.pending.pop_front();
                        state.in_flight = pending.is_some();
                        pending
                    };
                    if let Some(pending) = pending {
                        if !drainer_writer_dead.load(Ordering::SeqCst) {
                            let _ = drainer_tx.send(pending.into_event());
                        }
                    }
                    drainer_state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .in_flight = false;
                    if drainer_shutdown.load(Ordering::SeqCst)
                        && drainer_state
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .pending
                            .is_empty()
                    {
                        break;
                    }
                }
            })
            .expect("failed to spawn stream batcher");

        Self {
            tx,
            writer_dead,
            state,
            shutdown,
            _drainer: drainer,
        }
    }

    fn is_terminal(event: &Agent2Ui) -> bool {
        matches!(
            event,
            Agent2Ui::RoundComplete { .. } | Agent2Ui::TurnEnd { .. } | Agent2Ui::Done
        )
    }

    fn wait_for_pending(&self) {
        loop {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.pending.is_empty() && !state.in_flight {
                return;
            }
            drop(state);
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn replace_latest(&self, key: String, event: Agent2Ui) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(PendingEvent::Latest {
            key: previous_key,
            event: previous_event,
        }) = state.pending.back_mut()
            && *previous_key == key
        {
            *previous_event = event;
        } else {
            state.pending.push_back(PendingEvent::Latest { key, event });
        }
    }

    fn enqueue_delta(&self, turn_id: &str, round_num: u32, kind: RoundDeltaKind, delta: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for ch in delta.chars() {
            let append = state.pending.back_mut().and_then(|pending| match pending {
                PendingEvent::Delta(delta)
                    if delta.matches(turn_id, round_num, kind) && delta.chars < MAX_BATCH_CHARS =>
                {
                    Some(delta)
                }
                _ => None,
            });
            if let Some(pending) = append {
                pending.text.push(ch);
                pending.chars += 1;
            } else {
                state.pending.push_back(PendingEvent::Delta(PendingDelta {
                    turn_id: turn_id.to_string(),
                    round_num,
                    kind,
                    text: ch.to_string(),
                    chars: 1,
                }));
            }
        }
    }
}

impl Emitter for PacedEmitter {
    fn emit(&self, event: Agent2Ui) {
        if Self::is_terminal(&event) {
            self.wait_for_pending();
        }
        if !self.writer_dead.load(Ordering::SeqCst) {
            let _ = self.tx.send(event);
        }
    }

    fn emit_delta(&self, event: Agent2Ui) {
        match &event {
            Agent2Ui::RoundDelta {
                turn_id,
                round_num,
                kind,
                delta,
            } => self.enqueue_delta(turn_id, *round_num, *kind, delta),
            Agent2Ui::ToolCallPreview { id, .. } => {
                self.replace_latest(format!("tool-preview:{id}"), event)
            }
            _ if !self.writer_dead.load(Ordering::SeqCst) => {
                let _ = self.tx.send(event);
            }
            _ => {}
        }
    }
}

impl Drop for PacedEmitter {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        _tx: mpsc::SyncSender<Agent2Ui>,
    }

    impl TestHarness {
        fn new(interval: Duration) -> Self {
            let (tx, rx) = mpsc::sync_channel::<Agent2Ui>(128);
            let events = Arc::new(Mutex::new(Vec::new()));
            let collected = events.clone();
            thread::spawn(move || {
                while let Ok(event) = rx.recv() {
                    collected
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(event);
                }
            });
            Self {
                events,
                pacer: PacedEmitter::new(tx.clone(), Arc::new(AtomicBool::new(false)), interval),
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
    fn adjacent_text_is_coalesced_into_one_batch() {
        let h = TestHarness::new(Duration::from_millis(20));
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
        assert_eq!(deltas, ["abc你好"]);
    }

    #[test]
    fn metadata_boundaries_are_not_merged() {
        let h = TestHarness::new(Duration::from_millis(10));
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
        let h = TestHarness::new(Duration::from_millis(100));
        h.pacer.emit_delta(Agent2Ui::Ready);
        assert!(matches!(h.take_events()[0], Agent2Ui::Ready));
    }

    #[test]
    fn tool_preview_keeps_only_latest_revision_per_call() {
        let h = TestHarness::new(Duration::from_millis(20));
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
        assert_eq!(previews, ["{\"path\":\"x\"}"]);
    }

    #[test]
    fn interleaved_event_kinds_keep_input_order() {
        let h = TestHarness::new(Duration::from_millis(5));
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
        let h = TestHarness::new(Duration::from_millis(10));
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
}
