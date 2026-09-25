//! Rebuildable team roster and inbox projection.

use serde::{Deserialize, Serialize};

use crate::session_fact_v2::{
    AgentPath, FactPayload, SessionFact, SessionId, SubagentTerminalStatus, TeamAgentResidency,
    TeamAgentSnapshot, TeamAgentStatus, TeamDelta, TeamInboxSummary,
};

use super::Projection;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamSnapshot {
    pub root_session_id: Option<SessionId>,
    pub agents: Vec<TeamAgentSnapshot>,
    pub unread_messages: Vec<TeamInboxSummary>,
    pub revision: u64,
    pub last_fact_seq: u64,
}

#[derive(Debug, Default)]
pub struct TeamProjection {
    snapshot: TeamSnapshot,
}

impl TeamProjection {
    fn next_revision(&mut self) -> u64 {
        self.snapshot.revision = self.snapshot.revision.saturating_add(1);
        self.snapshot.revision
    }

    fn agent_mut(&mut self, agent_id: &SessionId) -> Option<&mut TeamAgentSnapshot> {
        self.snapshot
            .agents
            .iter_mut()
            .find(|agent| &agent.agent_id == agent_id)
    }

    fn upsert_agent(&mut self, agent: TeamAgentSnapshot) -> Option<TeamDelta> {
        if let Some(existing) = self.agent_mut(&agent.agent_id) {
            *existing = agent;
            return None;
        }
        self.snapshot.agents.push(agent.clone());
        Some(TeamDelta::AgentJoined {
            revision: self.next_revision(),
            agent: Box::new(agent),
        })
    }

    fn apply_subagent_finished(
        &mut self,
        payload: &crate::session_fact_v2::SubagentFinished,
    ) -> Option<TeamDelta> {
        let status = match payload.status {
            SubagentTerminalStatus::Completed => TeamAgentStatus::Completed,
            SubagentTerminalStatus::Failed => TeamAgentStatus::Errored,
            SubagentTerminalStatus::Cancelled => TeamAgentStatus::Interrupted,
            SubagentTerminalStatus::TimedOut => TeamAgentStatus::Errored,
        };
        let agent = self.agent_mut(&payload.child_session_id)?;
        agent.status = status;
        agent.residency = TeamAgentResidency::Unloaded;
        Some(TeamDelta::AgentCompleted {
            revision: self.next_revision(),
            agent_id: payload.child_session_id.clone(),
            status,
        })
    }

    fn apply_input_accepted(
        &mut self,
        payload: &crate::session_fact_v2::InputAccepted,
    ) -> Option<TeamDelta> {
        let message_id = payload.client_request_id.as_ref()?;
        let index = self
            .snapshot
            .unread_messages
            .iter()
            .position(|message| message.message_id.as_str() == message_id)?;
        self.snapshot.unread_messages.remove(index);
        Some(TeamDelta::AgentMessageDelivered {
            revision: self.next_revision(),
            message_id: crate::session_fact_v2::MessageId::new(message_id.clone()),
        })
    }
}

impl Projection for TeamProjection {
    type Snapshot = TeamSnapshot;
    type Delta = TeamDelta;

    fn apply(&mut self, fact: &SessionFact) -> Option<Self::Delta> {
        self.snapshot.last_fact_seq = fact.fact_seq;
        match &fact.payload {
            FactPayload::SessionCreated(_) => {
                if self.snapshot.root_session_id.is_none() {
                    self.snapshot.root_session_id = Some(fact.session_id.clone());
                }
                self.upsert_agent(TeamAgentSnapshot {
                    agent_id: fact.session_id.clone(),
                    agent_path: AgentPath::root(),
                    nickname: None,
                    role: Some("root".to_string()),
                    status: TeamAgentStatus::PendingInit,
                    residency: TeamAgentResidency::Loaded,
                    parent_agent_path: None,
                    current_task_id: None,
                })
            }
            FactPayload::SubagentSpawned(payload) => {
                let (Some(parent_agent_path), Some(child_agent_path)) =
                    (&payload.parent_agent_path, &payload.child_agent_path)
                else {
                    return None;
                };
                self.upsert_agent(TeamAgentSnapshot {
                    agent_id: payload.child_session_id.clone(),
                    agent_path: child_agent_path.clone(),
                    nickname: None,
                    role: payload.role.clone(),
                    status: TeamAgentStatus::PendingInit,
                    residency: TeamAgentResidency::Loaded,
                    parent_agent_path: Some(parent_agent_path.clone()),
                    current_task_id: None,
                })
            }
            FactPayload::SubagentFinished(payload) => self.apply_subagent_finished(payload),
            FactPayload::TurnStarted(payload) => {
                let agent = self.agent_mut(&fact.session_id)?;
                agent.status = TeamAgentStatus::Running;
                agent.current_task_id = Some(payload.turn_id.as_str().to_string());
                Some(TeamDelta::AgentStatusChanged {
                    revision: self.next_revision(),
                    agent_id: fact.session_id.clone(),
                    status: TeamAgentStatus::Running,
                })
            }
            FactPayload::TurnInterrupted(_) => {
                let agent = self.agent_mut(&fact.session_id)?;
                agent.status = TeamAgentStatus::Interrupted;
                Some(TeamDelta::AgentInterrupted {
                    revision: self.next_revision(),
                    agent_id: fact.session_id.clone(),
                })
            }
            FactPayload::InterAgentCommunication(payload) => {
                let summary = TeamInboxSummary {
                    message_id: payload.message_id.clone(),
                    author: payload.author.clone(),
                    recipient: payload.recipient.clone(),
                    task_id: payload.task_id.clone(),
                    delivery: payload.delivery,
                    created_at_ms: payload.created_at_ms,
                };
                self.snapshot.unread_messages.push(summary.clone());
                Some(TeamDelta::AgentMessageQueued {
                    revision: self.next_revision(),
                    message: Box::new(summary),
                })
            }
            FactPayload::InputAccepted(payload) => self.apply_input_accepted(payload),
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
