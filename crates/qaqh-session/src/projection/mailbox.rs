//! Rebuildable mailbox projection over canonical inter-agent communications.

use serde::{Deserialize, Serialize};

use crate::session_fact_v2::{
    AgentPath, FactPayload, InterAgentCommunication, MailboxDelta, MailboxMessage,
    MailboxMessageState, MessageId, SessionFact, SessionId,
};

use super::Projection;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxSnapshot {
    pub session_id: Option<SessionId>,
    pub messages: Vec<MailboxMessage>,
    pub revision: u64,
    pub last_fact_seq: u64,
    /// Last fact that changed mailbox activity, for `wait_agent`-style waits.
    pub last_activity_fact_seq: u64,
}

#[derive(Debug, Default)]
pub struct MailboxProjection {
    snapshot: MailboxSnapshot,
}

impl MailboxProjection {
    pub fn pending(&self) -> impl Iterator<Item = &MailboxMessage> {
        self.snapshot
            .messages
            .iter()
            .filter(|message| message.state == MailboxMessageState::Queued)
    }

    pub fn pending_for<'a>(
        &'a self,
        recipient: &'a AgentPath,
    ) -> impl Iterator<Item = &'a MailboxMessage> {
        self.pending()
            .filter(move |message| message.communication.is_for(recipient))
    }

    pub fn pending_count(&self) -> usize {
        self.pending().count()
    }

    pub fn last_activity_fact_seq(&self) -> u64 {
        self.snapshot.last_activity_fact_seq
    }

    fn next_revision(&mut self) -> u64 {
        self.snapshot.revision = self.snapshot.revision.saturating_add(1);
        self.snapshot.revision
    }

    fn apply_communication(
        &mut self,
        fact: &SessionFact,
        payload: &InterAgentCommunication,
    ) -> Option<MailboxDelta> {
        if self
            .snapshot
            .messages
            .iter()
            .any(|message| message.communication.message_id == payload.message_id)
        {
            return None;
        }
        let message = MailboxMessage {
            communication: payload.clone(),
            fact_seq: fact.fact_seq,
            state: MailboxMessageState::Queued,
            delivered_fact_seq: None,
        };
        self.snapshot.messages.push(message.clone());
        self.snapshot.last_activity_fact_seq = fact.fact_seq;
        Some(MailboxDelta::Queued {
            revision: self.next_revision(),
            message: Box::new(message),
        })
    }

    fn apply_input_accepted(
        &mut self,
        fact: &SessionFact,
        client_request_id: Option<&str>,
    ) -> Option<MailboxDelta> {
        let message_id = MessageId::new(client_request_id?);
        let message = self
            .snapshot
            .messages
            .iter_mut()
            .find(|message| message.communication.message_id == message_id)?;
        if message.state == MailboxMessageState::Delivered {
            return None;
        }
        message.state = MailboxMessageState::Delivered;
        message.delivered_fact_seq = Some(fact.fact_seq);
        self.snapshot.last_activity_fact_seq = fact.fact_seq;
        Some(MailboxDelta::Delivered {
            revision: self.next_revision(),
            message_id,
            delivered_fact_seq: fact.fact_seq,
        })
    }
}

impl Projection for MailboxProjection {
    type Snapshot = MailboxSnapshot;
    type Delta = MailboxDelta;

    fn apply(&mut self, fact: &SessionFact) -> Option<Self::Delta> {
        self.snapshot.last_fact_seq = fact.fact_seq;
        if self.snapshot.session_id.is_none() {
            self.snapshot.session_id = Some(fact.session_id.clone());
        }

        match &fact.payload {
            FactPayload::InterAgentCommunication(payload) => {
                self.apply_communication(fact, payload)
            }
            FactPayload::InputAccepted(payload) => {
                self.apply_input_accepted(fact, payload.client_request_id.as_deref())
            }
            _ => None,
        }
    }

    fn snapshot(&self) -> Self::Snapshot {
        self.snapshot.clone()
    }

    fn last_fact_seq(&self) -> u64 {
        self.snapshot.last_fact_seq
    }
}
