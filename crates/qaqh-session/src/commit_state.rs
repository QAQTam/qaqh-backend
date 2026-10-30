//! Minimal business invariants for isolated session commits.
//!
//! This is not a complete session reducer. Unsupported recovery and inter-agent
//! transitions must be added explicitly before the owner is production-ready.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::session_fact_v2::{
    ContextMessageOrigin, ExecutionId, FactPayload, InputId, InteractionId, ProviderFinish,
    RecoveryAction, SessionFact, ToolCallId, ToolIntentPolicyOutcome, ToolReplayCapability, TurnId,
};

const MAX_TRACKED: usize = 100_000;

#[derive(Debug, thiserror::Error)]
#[error("invalid committed business transition: {0}")]
pub struct CommitStateError(pub &'static str);

/// Unsettled tool declaration or intent observed when reopening a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsettledTool {
    pub call_id: ToolCallId,
    pub turn_id: TurnId,
    pub tool_name: String,
    pub execution_id: Option<ExecutionId>,
    pub replay_capability: Option<ToolReplayCapability>,
}

/// Unsettled user interaction observed when reopening a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsettledInteraction {
    pub interaction_id: InteractionId,
    pub turn_id: TurnId,
    pub call_id: Option<ToolCallId>,
}

/// Snapshot of open obligations folded from the committed prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredObligations {
    pub active_turn: Option<TurnId>,
    pub active_input: Option<InputId>,
    pub open_round: Option<u32>,
    pub last_response_finish: Option<ProviderFinish>,
    pub pending_inputs: Vec<InputId>,
    pub unsettled_tools: Vec<UnsettledTool>,
    pub unsettled_interactions: Vec<UnsettledInteraction>,
}

impl RecoveredObligations {
    pub fn has_open_work(&self) -> bool {
        self.active_turn.is_some()
            || !self.unsettled_tools.is_empty()
            || !self.unsettled_interactions.is_empty()
    }
}

#[derive(Clone)]
struct ToolState {
    turn: TurnId,
    tool_name: String,
    execution: Option<ExecutionId>,
    replay: Option<ToolReplayCapability>,
    terminal: bool,
}

#[derive(Clone)]
struct InteractionState {
    turn: TurnId,
    call: Option<ToolCallId>,
    resolved: bool,
}

#[derive(Clone, Default)]
pub(crate) struct CommitState {
    created: bool,
    deleted: bool,
    inputs: HashSet<InputId>,
    pending: VecDeque<InputId>,
    turns: HashSet<TurnId>,
    active_turn: Option<TurnId>,
    active_input: Option<InputId>,
    round: Option<u32>,
    last_response_finish: Option<ProviderFinish>,
    tools: HashMap<ToolCallId, ToolState>,
    interactions: HashMap<InteractionId, InteractionState>,
}

impl CommitState {
    pub(crate) fn apply(&mut self, fact: &SessionFact) -> Result<(), CommitStateError> {
        if !self.created && !matches!(fact.payload, FactPayload::SessionCreated(_)) {
            return Err(CommitStateError("session creation must be first"));
        }
        if self.deleted {
            return Err(CommitStateError("fact after session deletion"));
        }
        match &fact.payload {
            FactPayload::SessionCreated(_) => {
                if self.created || fact.fact_seq != 1 {
                    return Err(CommitStateError("duplicate or misplaced session creation"));
                }
                self.created = true;
            }
            FactPayload::InputAccepted(p) => {
                if self.inputs.len() >= MAX_TRACKED {
                    return Err(CommitStateError("input index capacity exceeded"));
                }
                if !self.inputs.insert(p.input_id.clone()) {
                    return Err(CommitStateError("duplicate input"));
                }
                self.pending.push_back(p.input_id.clone());
            }
            FactPayload::QueuedInputSettled(p) => {
                let Some(pos) = self.pending.iter().position(|id| id == &p.input_id) else {
                    return Err(CommitStateError(
                        "queued input is not pending or already started",
                    ));
                };
                self.pending.remove(pos);
            }
            FactPayload::TurnStarted(p) => {
                if self.active_turn.is_some() {
                    return Err(CommitStateError("turn already active"));
                }
                if self.pending.front() != Some(&p.input_id) {
                    return Err(CommitStateError("turn input is not next accepted input"));
                }
                if self.turns.len() >= MAX_TRACKED || !self.turns.insert(p.turn_id.clone()) {
                    return Err(CommitStateError(
                        "duplicate turn or turn index capacity exceeded",
                    ));
                }
                self.pending.pop_front();
                self.active_turn = Some(p.turn_id.clone());
                self.active_input = Some(p.input_id.clone());
                self.round = None;
                self.last_response_finish = None;
            }
            FactPayload::ModelRoundStarted(p) => {
                self.require_turn(&p.turn_id)?;
                if self
                    .last_response_finish
                    .is_some_and(|finish| !finish.is_complete())
                {
                    return Err(CommitStateError(
                        "cannot start model round after an incomplete provider response",
                    ));
                }
                if let Some(previous) = self.round {
                    if p.round
                        != previous
                            .checked_add(1)
                            .ok_or(CommitStateError("round exhausted"))?
                    {
                        return Err(CommitStateError("non-consecutive model round"));
                    }
                } else if p.round > 1 {
                    return Err(CommitStateError("invalid first model round"));
                }
                self.round = Some(p.round);
                self.last_response_finish = None;
            }
            FactPayload::AssistantBlockSealed(p) => self.require_turn(&p.turn_id)?,
            FactPayload::ToolCallDeclared(p) => {
                self.require_turn(&p.turn_id)?;
                if self.round.is_none() {
                    return Err(CommitStateError("tool declaration before model round"));
                }
                if self
                    .last_response_finish
                    .is_some_and(|finish| !finish.is_complete())
                {
                    return Err(CommitStateError(
                        "cannot declare tools after an incomplete provider response",
                    ));
                }
                if self.tools.len() >= MAX_TRACKED || self.tools.contains_key(&p.call_id) {
                    return Err(CommitStateError(
                        "duplicate tool call or tool index capacity exceeded",
                    ));
                }
                self.tools.insert(
                    p.call_id.clone(),
                    ToolState {
                        turn: p.turn_id.clone(),
                        tool_name: p.tool_name.clone(),
                        execution: None,
                        replay: None,
                        terminal: false,
                    },
                );
            }
            FactPayload::ToolIntent(p) => {
                if self.active_turn.as_ref() != fact.turn_id.as_ref() {
                    return Err(CommitStateError(
                        "tool intent does not belong to active turn",
                    ));
                }
                if self
                    .last_response_finish
                    .is_some_and(|finish| !finish.is_complete())
                {
                    return Err(CommitStateError(
                        "cannot execute tool intent after an incomplete provider response",
                    ));
                }
                let tool = self
                    .tools
                    .get_mut(&p.call_id)
                    .ok_or(CommitStateError("tool intent without declaration"))?;
                if fact.turn_id.as_ref() != Some(&tool.turn)
                    || tool.terminal
                    || tool.execution.is_some()
                {
                    return Err(CommitStateError("invalid or duplicate tool intent"));
                }
                if p.policy_decision.outcome != ToolIntentPolicyOutcome::Allow {
                    return Err(CommitStateError("non-allowed tool intent cannot execute"));
                }
                tool.execution = Some(p.execution_id.clone());
                tool.replay = Some(p.replay_capability.clone());
            }
            FactPayload::ToolFinished(p) => {
                if self.active_turn.as_ref() != fact.turn_id.as_ref() {
                    return Err(CommitStateError(
                        "tool terminal does not belong to active turn",
                    ));
                }
                let tool = self
                    .tools
                    .get_mut(&p.call_id)
                    .ok_or(CommitStateError("tool terminal without declaration"))?;
                if fact.turn_id.as_ref() != Some(&tool.turn)
                    || tool.terminal
                    || tool.execution != p.execution_id
                {
                    return Err(CommitStateError(
                        "tool terminal identity mismatch or duplicate",
                    ));
                }
                tool.terminal = true;
            }
            FactPayload::InteractionRequested(p) => {
                self.require_turn(&p.turn_id)?;
                if self.interactions.len() >= MAX_TRACKED
                    || self.interactions.contains_key(&p.interaction_id)
                {
                    return Err(CommitStateError(
                        "duplicate interaction or interaction index capacity exceeded",
                    ));
                }
                self.interactions.insert(
                    p.interaction_id.clone(),
                    InteractionState {
                        turn: p.turn_id.clone(),
                        call: p.call_id.clone(),
                        resolved: false,
                    },
                );
            }
            FactPayload::InteractionResolved(p) => {
                // CAS against the request revision: an answer is only valid for
                // the revision it was produced for, so a stale answer can never
                // resolve a newer request with the same identity.
                if p.resolution_seq != 1 {
                    return Err(CommitStateError(
                        "interaction answer carries a stale revision",
                    ));
                }
                self.resolve_interaction(&p.interaction_id, fact, true)?
            }
            FactPayload::InteractionExpired(p) => {
                self.resolve_interaction(&p.interaction_id, fact, false)?
            }
            FactPayload::TurnFinished(p) => self.finish_turn(&p.turn_id)?,
            FactPayload::TurnInterrupted(p) => self.finish_turn(&p.turn_id)?,
            FactPayload::ContextMessageCommitted(p) => {
                if matches!(
                    p.origin,
                    ContextMessageOrigin::Input
                        | ContextMessageOrigin::ProviderResponse
                        | ContextMessageOrigin::ToolResult
                ) {
                    let turn = fact
                        .turn_id
                        .as_ref()
                        .ok_or(CommitStateError("context message missing turn"))?;
                    self.require_turn(turn)?;
                }
            }
            FactPayload::ProviderResponseCommitted(p) => {
                self.require_turn(&p.turn_id)?;
                if self.round != Some(p.round) {
                    return Err(CommitStateError(
                        "provider response does not match the open model round",
                    ));
                }
                self.last_response_finish = Some(p.finish);
            }
            FactPayload::SessionDeleted(_) => {
                if self.active_turn.is_some() || !self.pending.is_empty() {
                    return Err(CommitStateError("session deletion with unsettled inputs"));
                }
                self.deleted = true;
            }
            FactPayload::SessionRecovered(p) => {
                if self.active_turn.is_some() {
                    return Err(CommitStateError(
                        "cannot seal session recovery while a turn is still active",
                    ));
                }
                if self.tools.values().any(|tool| !tool.terminal) {
                    return Err(CommitStateError(
                        "cannot seal session recovery with unsettled tool calls",
                    ));
                }
                if self
                    .interactions
                    .values()
                    .any(|interaction| !interaction.resolved)
                {
                    return Err(CommitStateError(
                        "cannot seal session recovery with unsettled interactions",
                    ));
                }
                if p.last_good_fact_seq >= fact.fact_seq {
                    return Err(CommitStateError(
                        "recovery last_good_fact_seq must precede recovery fact",
                    ));
                }
                for action in &p.actions {
                    match action {
                        RecoveryAction::TurnInterrupted { turn_id, .. } => {
                            if !self.turns.contains(turn_id)
                                || self.active_turn.as_ref() == Some(turn_id)
                            {
                                return Err(CommitStateError(
                                    "recovery turn_interrupted action does not match settled turn",
                                ));
                            }
                        }
                        RecoveryAction::ToolFinished { completion } => {
                            let Some(tool) = self.tools.get(&completion.call_id) else {
                                return Err(CommitStateError(
                                    "recovery tool_finished action references unknown call",
                                ));
                            };
                            if !tool.terminal || tool.execution != completion.execution_id {
                                return Err(CommitStateError(
                                    "recovery tool_finished action does not match settled tool",
                                ));
                            }
                        }
                        RecoveryAction::InteractionExpired { interaction_id, .. } => {
                            let Some(interaction) = self.interactions.get(interaction_id) else {
                                return Err(CommitStateError(
                                    "recovery interaction_expired action references unknown interaction",
                                ));
                            };
                            if !interaction.resolved {
                                return Err(CommitStateError(
                                    "recovery interaction_expired action references unresolved interaction",
                                ));
                            }
                        }
                        _ => {}
                    }
                }
            }
            FactPayload::CompactionApplied(_) => {
                return Err(CommitStateError("legacy compaction fact not supported"));
            }
            FactPayload::SubagentSpawned(_)
            | FactPayload::SubagentFinished(_)
            | FactPayload::InterAgentCommunication(_) => {
                return Err(CommitStateError(
                    "child delivery transitions not implemented for batch owner",
                ));
            }
            FactPayload::ContextCheckpointCommitted(_)
            | FactPayload::SessionMetadataChanged(_)
            | FactPayload::SessionTitleChanged(_)
            | FactPayload::WorkspaceResourceChanged(_)
            | FactPayload::DriverChanged(_) => {}
        }
        Ok(())
    }

    fn require_turn(&self, turn: &TurnId) -> Result<(), CommitStateError> {
        if self.active_turn.as_ref() == Some(turn) {
            Ok(())
        } else {
            Err(CommitStateError("fact does not belong to active turn"))
        }
    }

    fn resolve_interaction(
        &mut self,
        id: &InteractionId,
        fact: &SessionFact,
        answer: bool,
    ) -> Result<(), CommitStateError> {
        if self.active_turn.as_ref() != fact.turn_id.as_ref() {
            return Err(CommitStateError(
                "interaction does not belong to active turn",
            ));
        }
        let interaction = self
            .interactions
            .get_mut(id)
            .ok_or(CommitStateError("interaction terminal without request"))?;
        if interaction.resolved || fact.turn_id.as_ref() != Some(&interaction.turn) {
            return Err(CommitStateError(
                "interaction terminal identity mismatch or duplicate",
            ));
        }
        // An answer must target the call the request was raised for. An
        // approval can never be retargeted to another execution generation.
        if answer && interaction.call.as_ref() != fact.call_id.as_ref() {
            return Err(CommitStateError(
                "interaction answer targets a different call",
            ));
        }
        interaction.resolved = true;
        Ok(())
    }

    fn finish_turn(&mut self, turn: &TurnId) -> Result<(), CommitStateError> {
        self.require_turn(turn)?;
        if self
            .tools
            .values()
            .any(|tool| &tool.turn == turn && !tool.terminal)
            || self
                .interactions
                .values()
                .any(|interaction| &interaction.turn == turn && !interaction.resolved)
        {
            return Err(CommitStateError("turn has unsettled tools or interactions"));
        }
        self.active_turn = None;
        self.active_input = None;
        self.round = None;
        self.last_response_finish = None;
        Ok(())
    }

    pub(crate) fn active_input(&self) -> Option<&InputId> {
        self.active_input.as_ref()
    }

    pub(crate) fn obligations(&self) -> RecoveredObligations {
        let mut unsettled_tools: Vec<UnsettledTool> = self
            .tools
            .iter()
            .filter(|(_, state)| !state.terminal)
            .map(|(call_id, state)| UnsettledTool {
                call_id: call_id.clone(),
                turn_id: state.turn.clone(),
                tool_name: state.tool_name.clone(),
                execution_id: state.execution.clone(),
                replay_capability: state.replay.clone(),
            })
            .collect();
        unsettled_tools.sort_by(|a, b| a.call_id.as_str().cmp(b.call_id.as_str()));

        let mut unsettled_interactions: Vec<UnsettledInteraction> = self
            .interactions
            .iter()
            .filter(|(_, state)| !state.resolved)
            .map(|(interaction_id, state)| UnsettledInteraction {
                interaction_id: interaction_id.clone(),
                turn_id: state.turn.clone(),
                call_id: state.call.clone(),
            })
            .collect();
        unsettled_interactions
            .sort_by(|a, b| a.interaction_id.as_str().cmp(b.interaction_id.as_str()));

        RecoveredObligations {
            active_turn: self.active_turn.clone(),
            active_input: self.active_input.clone(),
            open_round: self.round,
            last_response_finish: self.last_response_finish,
            pending_inputs: self.pending.iter().cloned().collect(),
            unsettled_tools,
            unsettled_interactions,
        }
    }
}
