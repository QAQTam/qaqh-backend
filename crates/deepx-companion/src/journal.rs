use std::collections::HashMap;

use deepx_proto::{
    CompanionEvent, CompanionInteraction, CompanionInteractionKey, CompanionSession,
    CompanionSnapshot,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedEvent {
    pub server_epoch: String,
    pub seq: u64,
    pub event: CompanionEvent,
}

#[derive(Debug)]
pub struct CompanionState {
    server_epoch: String,
    seq: u64,
    sessions: HashMap<String, CompanionSession>,
    pending_interactions: HashMap<CompanionInteractionKey, CompanionInteraction>,
}

impl CompanionState {
    pub fn new(server_epoch: impl Into<String>) -> Self {
        Self {
            server_epoch: server_epoch.into(),
            seq: 0,
            sessions: HashMap::new(),
            pending_interactions: HashMap::new(),
        }
    }

    pub fn publish(&mut self, event: CompanionEvent) -> PublishedEvent {
        self.seq = self.seq.saturating_add(1);
        match &event {
            CompanionEvent::SessionActivity { session } => {
                let should_replace = self
                    .sessions
                    .get(&session.seed)
                    .is_none_or(|current| session.session_seq >= current.session_seq);
                if should_replace {
                    self.sessions.insert(session.seed.clone(), session.clone());
                }
            }
            CompanionEvent::InteractionRequested { interaction } => {
                self.pending_interactions
                    .insert(interaction.key.clone(), interaction.clone());
            }
            CompanionEvent::InteractionResolved { key, .. } => {
                self.pending_interactions.remove(key);
            }
            CompanionEvent::Notification { .. } => {}
        }
        PublishedEvent {
            server_epoch: self.server_epoch.clone(),
            seq: self.seq,
            event,
        }
    }

    pub fn snapshot(&self) -> CompanionSnapshot {
        let mut sessions: Vec<_> = self.sessions.values().cloned().collect();
        sessions.sort_by(|left, right| left.seed.cmp(&right.seed));
        let mut pending_interactions: Vec<_> =
            self.pending_interactions.values().cloned().collect();
        pending_interactions.sort_by(|left, right| {
            left.key
                .seed
                .cmp(&right.key.seed)
                .then_with(|| left.key.generation.cmp(&right.key.generation))
                .then_with(|| left.key.request_id.cmp(&right.key.request_id))
        });
        CompanionSnapshot {
            snapshot_seq: self.seq,
            sessions,
            pending_interactions,
        }
    }

    pub(crate) fn next_sequence(&mut self) -> u64 {
        self.seq = self.seq.saturating_add(1);
        self.seq
    }
}
