//! Rebuildable control projection for status, tools and interactions.

use serde::{Deserialize, Serialize};

use crate::session_fact_v2::{
    ActivityState, ActorRef, ContentHash, ContentRef, ContentValue, ControlDelta, ExecutionId,
    FactPayload, InteractionDecision, InteractionExpired, InteractionExpiryReason, InteractionId,
    InteractionKind, InteractionRequested, InteractionResolved, RecoveryOutcome, RecoveryRef,
    SessionFact, SessionId, SessionRecovered, SubagentFinished, SubagentSpawned,
    SubagentTerminalStatus, ToolCallId, ToolError, ToolFinished, ToolIntent, ToolMetrics,
    ToolTerminalStatus, TurnId,
};

use super::Projection;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlRoundState {
    pub turn_id: TurnId,
    pub round: u32,
    pub request_hash: ContentHash,
    pub context_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlToolState {
    pub call_id: ToolCallId,
    pub execution_id: Option<ExecutionId>,
    pub terminal_status: Option<ToolTerminalStatus>,
    pub output: Option<ContentValue>,
    pub error: Option<ToolError>,
    pub metrics: Option<ToolMetrics>,
    pub reconciled: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlInteractionResolution {
    pub decision: ContentValue,
    /// Structured verdict. `None` for facts written before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<InteractionDecision>,
    pub resolved_by: ActorRef,
    pub resolution_seq: u64,
}

/// Canonical driver seat state (see `session_fact_v2::DriverChanged`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlDriverState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    pub driver_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlInteractionState {
    pub interaction_id: InteractionId,
    pub call_id: Option<ToolCallId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    pub kind: InteractionKind,
    pub request: ContentValue,
    pub expires_at_ms: Option<i64>,
    pub resolution: Option<ControlInteractionResolution>,
    pub expired_reason: Option<InteractionExpiryReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlSubagentState {
    pub child_session_id: SessionId,
    pub parent_call_id: ToolCallId,
    pub role: Option<String>,
    pub status: Option<SubagentTerminalStatus>,
    pub result: Option<ContentValue>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlSnapshot {
    pub session_id: Option<SessionId>,
    pub activity: ActivityState,
    pub current_turn_id: Option<TurnId>,
    pub current_call_id: Option<ToolCallId>,
    pub round: Option<ControlRoundState>,
    pub tools: Vec<ControlToolState>,
    pub interactions: Vec<ControlInteractionState>,
    pub subagents: Vec<ControlSubagentState>,
    pub last_recovery: Option<RecoveryOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<ControlDriverState>,
    pub revision: u64,
    pub last_fact_seq: u64,
}

#[derive(Debug, Default)]
pub struct ControlProjection {
    snapshot: ControlSnapshot,
}

impl Projection for ControlProjection {
    type Snapshot = ControlSnapshot;
    type Delta = ControlDelta;

    fn apply(&mut self, fact: &SessionFact) -> Option<Self::Delta> {
        self.snapshot.last_fact_seq = fact.fact_seq;
        match &fact.payload {
            FactPayload::SessionCreated(payload) => {
                self.snapshot.session_id = Some(fact.session_id.clone());
                self.snapshot.activity = ActivityState::Idle;
                self.snapshot.current_turn_id = None;
                self.snapshot.current_call_id = None;
                self.snapshot.last_recovery = None;
                Some(ControlDelta::SessionCreated {
                    revision: self.next_revision(),
                    session_id: fact.session_id.clone(),
                    cwd: payload.cwd.clone(),
                    model: payload.model.clone(),
                    schema_caps: payload.schema_caps.clone(),
                })
            }
            FactPayload::TurnStarted(payload) => {
                self.snapshot.activity = ActivityState::Running;
                self.snapshot.current_turn_id = Some(payload.turn_id.clone());
                self.snapshot.current_call_id = None;
                Some(ControlDelta::Activity {
                    revision: self.next_revision(),
                    turn_id: Some(payload.turn_id.clone()),
                    call_id: None,
                    state: ActivityState::Running,
                })
            }
            FactPayload::ModelRoundStarted(payload) => {
                self.snapshot.round = Some(ControlRoundState {
                    turn_id: payload.turn_id.clone(),
                    round: payload.round,
                    request_hash: payload.request_hash.clone(),
                    context_revision: payload.context_revision,
                });
                Some(ControlDelta::Round {
                    revision: self.next_revision(),
                    turn_id: payload.turn_id.clone(),
                    round: payload.round,
                    request_hash: payload.request_hash.clone(),
                    context_revision: payload.context_revision,
                })
            }
            FactPayload::ToolIntent(payload) => {
                self.apply_tool_intent(payload);
                Some(ControlDelta::ToolIntent {
                    revision: self.next_revision(),
                    call_id: payload.call_id.clone(),
                    execution_id: payload.execution_id.clone(),
                    policy_decision: payload.policy_decision.clone(),
                    replay_capability: payload.replay_capability.clone(),
                    side_effect_class: payload.side_effect_class,
                    intent_at_ms: payload.intent_at_ms,
                })
            }
            FactPayload::ToolFinished(payload) => {
                self.apply_tool_finished(payload);
                Some(ControlDelta::ToolFinished {
                    revision: self.next_revision(),
                    call_id: payload.call_id.clone(),
                    execution_id: payload.execution_id.clone(),
                    terminal_status: payload.terminal_status,
                    output: payload.output_ref.clone().map(content_ref_value),
                    error: payload.error.clone(),
                    metrics: payload.metrics.clone(),
                    reconciled: payload.reconciled,
                })
            }
            FactPayload::InteractionRequested(payload) => {
                self.apply_interaction_requested(payload, fact.turn_id.as_ref());
                Some(ControlDelta::InteractionRequested {
                    revision: self.next_revision(),
                    interaction_id: payload.interaction_id.clone(),
                    call_id: payload.call_id.clone(),
                    kind: payload.kind,
                    request: content_ref_value(payload.request_ref.clone()),
                    expires_at_ms: payload.expires_at_ms,
                })
            }
            FactPayload::InteractionResolved(payload) => {
                self.apply_interaction_resolved(payload);
                Some(ControlDelta::InteractionResolved {
                    revision: self.next_revision(),
                    interaction_id: payload.interaction_id.clone(),
                    decision: content_ref_value(payload.decision_ref.clone()),
                    verdict: payload.decision,
                    resolved_by: payload.resolved_by.clone(),
                    resolution_seq: payload.resolution_seq,
                })
            }
            FactPayload::DriverChanged(payload) => {
                self.snapshot.driver = Some(ControlDriverState {
                    holder: payload.holder.clone(),
                    driver_epoch: payload.driver_epoch,
                });
                Some(ControlDelta::DriverChanged {
                    revision: self.next_revision(),
                    holder: payload.holder.clone(),
                    driver_epoch: payload.driver_epoch,
                })
            }
            FactPayload::InteractionExpired(payload) => {
                self.apply_interaction_expired(payload);
                Some(ControlDelta::InteractionExpired {
                    revision: self.next_revision(),
                    interaction_id: payload.interaction_id.clone(),
                    reason: payload.reason,
                })
            }
            FactPayload::TurnFinished(payload) => {
                self.snapshot.activity = ActivityState::Idle;
                self.snapshot.current_turn_id = Some(payload.turn_id.clone());
                self.snapshot.current_call_id = None;
                Some(ControlDelta::Activity {
                    revision: self.next_revision(),
                    turn_id: Some(payload.turn_id.clone()),
                    call_id: None,
                    state: ActivityState::Idle,
                })
            }
            FactPayload::TurnInterrupted(payload) => {
                self.snapshot.activity = ActivityState::Interrupted;
                self.snapshot.current_turn_id = Some(payload.turn_id.clone());
                self.snapshot.current_call_id = None;
                Some(ControlDelta::Activity {
                    revision: self.next_revision(),
                    turn_id: Some(payload.turn_id.clone()),
                    call_id: None,
                    state: ActivityState::Interrupted,
                })
            }
            FactPayload::SessionRecovered(payload) => {
                self.snapshot.last_recovery = Some(payload.outcome);
                Some(ControlDelta::SessionRecovered {
                    revision: self.next_revision(),
                    outcome: payload.outcome,
                    recovery_ref: recovery_ref(payload),
                    actions: payload.actions.clone(),
                })
            }
            FactPayload::SubagentSpawned(payload) => {
                self.apply_subagent_spawned(payload);
                Some(ControlDelta::SubagentSpawned {
                    revision: self.next_revision(),
                    child_session_id: payload.child_session_id.clone(),
                    parent_call_id: payload.parent_call_id.clone(),
                    role: payload.role.clone(),
                })
            }
            FactPayload::SubagentFinished(payload) => {
                self.apply_subagent_finished(payload);
                Some(ControlDelta::SubagentFinished {
                    revision: self.next_revision(),
                    child_session_id: payload.child_session_id.clone(),
                    parent_call_id: payload.parent_call_id.clone(),
                    status: payload.status,
                    result: payload.result_ref.clone().map(content_ref_value),
                })
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

impl ControlProjection {
    fn apply_tool_intent(&mut self, payload: &ToolIntent) {
        self.snapshot.current_call_id = Some(payload.call_id.clone());
        let state = self.tool_state_mut(&payload.call_id);
        state.execution_id = Some(payload.execution_id.clone());
        state.terminal_status = None;
        state.output = None;
        state.error = None;
        state.metrics = None;
        state.reconciled = None;
    }

    fn apply_tool_finished(&mut self, payload: &ToolFinished) {
        if self.snapshot.current_call_id.as_ref() == Some(&payload.call_id) {
            self.snapshot.current_call_id = None;
        }
        let state = self.tool_state_mut(&payload.call_id);
        if payload.execution_id.is_some() {
            state.execution_id = payload.execution_id.clone();
        }
        state.terminal_status = Some(payload.terminal_status);
        state.output = payload.output_ref.clone().map(content_ref_value);
        state.error = payload.error.clone();
        state.metrics = Some(payload.metrics.clone());
        state.reconciled = Some(payload.reconciled);
    }

    fn apply_interaction_requested(
        &mut self,
        payload: &InteractionRequested,
        turn_id: Option<&TurnId>,
    ) {
        let state = self.interaction_state_mut(&payload.interaction_id);
        state.call_id = payload.call_id.clone();
        state.turn_id = turn_id.cloned();
        state.kind = payload.kind;
        state.request = content_ref_value(payload.request_ref.clone());
        state.expires_at_ms = payload.expires_at_ms;
        state.resolution = None;
        state.expired_reason = None;
    }

    fn apply_interaction_resolved(&mut self, payload: &InteractionResolved) {
        if let Some(state) = self
            .snapshot
            .interactions
            .iter_mut()
            .find(|state| state.interaction_id == payload.interaction_id)
        {
            state.resolution = Some(ControlInteractionResolution {
                decision: content_ref_value(payload.decision_ref.clone()),
                verdict: payload.decision,
                resolved_by: payload.resolved_by.clone(),
                resolution_seq: payload.resolution_seq,
            });
            state.expired_reason = None;
        }
    }

    fn apply_interaction_expired(&mut self, payload: &InteractionExpired) {
        if let Some(state) = self
            .snapshot
            .interactions
            .iter_mut()
            .find(|state| state.interaction_id == payload.interaction_id)
        {
            state.expired_reason = Some(payload.reason);
            state.resolution = None;
        }
    }

    fn apply_subagent_spawned(&mut self, payload: &SubagentSpawned) {
        if let Some(state) = self
            .snapshot
            .subagents
            .iter_mut()
            .find(|state| state.child_session_id == payload.child_session_id)
        {
            state.parent_call_id = payload.parent_call_id.clone();
            state.role = payload.role.clone();
            state.status = None;
            state.result = None;
        } else {
            self.snapshot.subagents.push(ControlSubagentState {
                child_session_id: payload.child_session_id.clone(),
                parent_call_id: payload.parent_call_id.clone(),
                role: payload.role.clone(),
                status: None,
                result: None,
            });
        }
    }

    fn apply_subagent_finished(&mut self, payload: &SubagentFinished) {
        if let Some(state) = self
            .snapshot
            .subagents
            .iter_mut()
            .find(|state| state.child_session_id == payload.child_session_id)
        {
            state.parent_call_id = payload.parent_call_id.clone();
            state.status = Some(payload.status);
            state.result = payload.result_ref.clone().map(content_ref_value);
        } else {
            self.snapshot.subagents.push(ControlSubagentState {
                child_session_id: payload.child_session_id.clone(),
                parent_call_id: payload.parent_call_id.clone(),
                role: None,
                status: Some(payload.status),
                result: payload.result_ref.clone().map(content_ref_value),
            });
        }
    }

    fn tool_state_mut(&mut self, call_id: &ToolCallId) -> &mut ControlToolState {
        if let Some(index) = self
            .snapshot
            .tools
            .iter()
            .position(|state| &state.call_id == call_id)
        {
            return &mut self.snapshot.tools[index];
        }
        self.snapshot.tools.push(ControlToolState {
            call_id: call_id.clone(),
            execution_id: None,
            terminal_status: None,
            output: None,
            error: None,
            metrics: None,
            reconciled: None,
        });
        self.snapshot
            .tools
            .last_mut()
            .expect("just inserted tool state")
    }

    fn interaction_state_mut(
        &mut self,
        interaction_id: &InteractionId,
    ) -> &mut ControlInteractionState {
        if let Some(index) = self
            .snapshot
            .interactions
            .iter()
            .position(|state| &state.interaction_id == interaction_id)
        {
            return &mut self.snapshot.interactions[index];
        }
        self.snapshot.interactions.push(ControlInteractionState {
            interaction_id: interaction_id.clone(),
            call_id: None,
            turn_id: None,
            kind: InteractionKind::Ask,
            request: ContentValue::Inline {
                text: String::new(),
            },
            expires_at_ms: None,
            resolution: None,
            expired_reason: None,
        });
        self.snapshot
            .interactions
            .last_mut()
            .expect("just inserted interaction state")
    }

    fn next_revision(&mut self) -> u64 {
        self.snapshot.revision = self.snapshot.revision.saturating_add(1);
        self.snapshot.revision
    }
}

fn content_ref_value(content_ref: ContentRef) -> ContentValue {
    ContentValue::Ref { content_ref }
}

fn recovery_ref(payload: &SessionRecovered) -> RecoveryRef {
    RecoveryRef {
        recovery_id: payload.recovery_id.clone(),
        recovery_event_id: payload.recovery_event_id.clone(),
        recovery_input_fingerprint: payload.recovery_input_fingerprint.clone(),
    }
}
