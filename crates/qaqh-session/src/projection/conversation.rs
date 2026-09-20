//! Rebuildable conversation projection for model context and gate input.

use serde::{Deserialize, Serialize};

use crate::session_fact_v2::{
    ActorRef, AssistantBlockKind, BlockId, CheckpointId, ContentHash, ContentRef, ContentValue,
    ConversationDelta, FactPayload, InputAccepted, InputId, InputKind, InputPurpose,
    InterruptReason, RecoveryRef, SessionFact, SessionId, ToolCallDeclared, ToolCallId, ToolError,
    ToolFinished, ToolMetrics, ToolTerminalStatus, TurnError, TurnFinished, TurnId,
    TurnInterrupted, TurnMode, TurnStarted, TurnTerminal,
};

use super::Projection;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationInputState {
    pub input_id: InputId,
    pub input_kind: InputKind,
    pub input_purpose: InputPurpose,
    pub content: ContentValue,
    pub attachments: Vec<ContentRef>,
    pub actor: ActorRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationAssistantBlockState {
    pub turn_id: TurnId,
    pub block_id: BlockId,
    pub block_kind: AssistantBlockKind,
    pub content: ContentValue,
    pub model: String,
    pub usage: Option<qaqh_types::UsageInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationToolResultState {
    pub terminal_status: ToolTerminalStatus,
    pub output: Option<ContentValue>,
    pub error: Option<ToolError>,
    pub metrics: ToolMetrics,
    pub reconciled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationToolCallState {
    pub call_id: ToolCallId,
    pub turn_id: Option<TurnId>,
    pub tool_name: Option<String>,
    pub args: Option<ContentValue>,
    pub args_hash: Option<ContentHash>,
    pub result: Option<ConversationToolResultState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationCompactionState {
    pub checkpoint_id: CheckpointId,
    pub replaces_through_fact_seq: u64,
    pub summary: ContentValue,
    pub context_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ConversationContextKind {
    Input(ConversationInputState),
    AssistantBlock(ConversationAssistantBlockState),
    ToolCall(ConversationToolCallState),
    Compaction(ConversationCompactionState),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationContextEntry {
    pub source_fact_seq: u64,
    pub kind: ConversationContextKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ConversationTurnOutcome {
    Finished {
        terminal: TurnTerminal,
        usage: Option<qaqh_types::UsageInfo>,
        error: Option<TurnError>,
    },
    Interrupted {
        reason: InterruptReason,
        last_fact_seq: u64,
        recovery_ref: RecoveryRef,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationTurnState {
    pub turn_id: TurnId,
    pub input_id: Option<InputId>,
    pub mode: Option<TurnMode>,
    pub recovery_ref: Option<RecoveryRef>,
    pub outcome: Option<ConversationTurnOutcome>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationSnapshot {
    pub session_id: Option<SessionId>,
    pub turns: Vec<ConversationTurnState>,
    pub context: Vec<ConversationContextEntry>,
    pub compaction: Option<ConversationCompactionState>,
    pub current_turn_id: Option<TurnId>,
    pub revision: u64,
    pub last_fact_seq: u64,
}

#[derive(Debug, Default)]
pub struct ConversationProjection {
    snapshot: ConversationSnapshot,
}

impl Projection for ConversationProjection {
    type Snapshot = ConversationSnapshot;
    type Delta = ConversationDelta;

    fn apply(&mut self, fact: &SessionFact) -> Option<Self::Delta> {
        self.snapshot.last_fact_seq = fact.fact_seq;
        if self.snapshot.session_id.is_none() {
            self.snapshot.session_id = Some(fact.session_id.clone());
        }

        match &fact.payload {
            FactPayload::InputAccepted(payload) => {
                let state = ConversationInputState {
                    input_id: payload.input_id.clone(),
                    input_kind: payload.input_kind,
                    input_purpose: payload.input_purpose,
                    content: input_content(payload),
                    attachments: payload.attachments.clone(),
                    actor: payload.actor.clone(),
                };
                self.upsert_input(fact.fact_seq, state.clone());
                Some(ConversationDelta::InputAccepted {
                    revision: self.next_revision(),
                    input_id: payload.input_id.clone(),
                    input_kind: payload.input_kind,
                    input_purpose: payload.input_purpose,
                    content: state.content,
                    attachments: payload.attachments.clone(),
                    actor: payload.actor.clone(),
                })
            }
            FactPayload::TurnStarted(payload) => {
                self.apply_turn_started(payload);
                Some(ConversationDelta::TurnStarted {
                    revision: self.next_revision(),
                    turn_id: payload.turn_id.clone(),
                    input_id: payload.input_id.clone(),
                    mode: payload.mode,
                })
            }
            FactPayload::AssistantBlockSealed(payload) => {
                let state = ConversationAssistantBlockState {
                    turn_id: payload.turn_id.clone(),
                    block_id: payload.block_id.clone(),
                    block_kind: payload.kind,
                    content: content_ref_value(payload.content_ref.clone()),
                    model: payload.model.clone(),
                    usage: payload.usage.clone(),
                };
                self.upsert_assistant_block(fact.fact_seq, state);
                Some(ConversationDelta::AssistantBlockSealed {
                    revision: self.next_revision(),
                    turn_id: payload.turn_id.clone(),
                    block_id: payload.block_id.clone(),
                    block_kind: payload.kind,
                    content: content_ref_value(payload.content_ref.clone()),
                    model: payload.model.clone(),
                    usage: payload.usage.clone(),
                })
            }
            FactPayload::ToolCallDeclared(payload) => {
                self.apply_tool_call_declared(fact.fact_seq, payload);
                Some(ConversationDelta::ToolCallDeclared {
                    revision: self.next_revision(),
                    turn_id: payload.turn_id.clone(),
                    call_id: payload.call_id.clone(),
                    tool_name: payload.tool_name.clone(),
                    args: content_ref_value(payload.args_ref.clone()),
                    args_hash: payload.args_hash.clone(),
                })
            }
            FactPayload::ToolFinished(payload) => {
                self.apply_tool_finished(fact.fact_seq, fact.turn_id.as_ref(), payload);
                Some(ConversationDelta::ToolFinished {
                    revision: self.next_revision(),
                    call_id: payload.call_id.clone(),
                    terminal_status: payload.terminal_status,
                    output: payload.output_ref.clone().map(content_ref_value),
                    error: payload.error.clone(),
                    metrics: payload.metrics.clone(),
                    reconciled: payload.reconciled,
                })
            }
            FactPayload::TurnFinished(payload) => {
                self.apply_turn_finished(&payload.turn_id, payload);
                Some(ConversationDelta::TurnFinished {
                    revision: self.next_revision(),
                    turn_id: payload.turn_id.clone(),
                    terminal: payload.terminal,
                    usage: payload.usage.clone(),
                    error: payload.error.clone(),
                })
            }
            FactPayload::TurnInterrupted(payload) => {
                self.apply_turn_interrupted(&payload.turn_id, payload);
                Some(ConversationDelta::TurnInterrupted {
                    revision: self.next_revision(),
                    turn_id: payload.turn_id.clone(),
                    reason: payload.reason,
                    last_fact_seq: payload.last_fact_seq,
                    recovery_ref: payload.recovery_ref.clone(),
                })
            }
            FactPayload::CompactionApplied(payload) => {
                let state = ConversationCompactionState {
                    checkpoint_id: payload.checkpoint_id.clone(),
                    replaces_through_fact_seq: payload.replaces_through_fact_seq,
                    summary: content_ref_value(payload.summary_ref.clone()),
                    context_revision: payload.context_revision,
                };
                self.apply_compaction(fact.fact_seq, state.clone());
                Some(ConversationDelta::CompactionApplied {
                    revision: self.next_revision(),
                    checkpoint_id: payload.checkpoint_id.clone(),
                    replaces_through_fact_seq: payload.replaces_through_fact_seq,
                    summary: state.summary,
                    context_revision: payload.context_revision,
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

impl ConversationProjection {
    fn apply_turn_started(&mut self, payload: &TurnStarted) {
        self.snapshot.current_turn_id = Some(payload.turn_id.clone());
        if let Some(turn) = self.turn_mut(&payload.turn_id) {
            turn.input_id = Some(payload.input_id.clone());
            turn.mode = Some(payload.mode);
            turn.recovery_ref = payload.recovery_ref.clone();
        } else {
            self.snapshot.turns.push(ConversationTurnState {
                turn_id: payload.turn_id.clone(),
                input_id: Some(payload.input_id.clone()),
                mode: Some(payload.mode),
                recovery_ref: payload.recovery_ref.clone(),
                outcome: None,
            });
        }
    }

    fn apply_turn_finished(&mut self, turn_id: &TurnId, payload: &TurnFinished) {
        if self.snapshot.current_turn_id.as_ref() == Some(turn_id) {
            self.snapshot.current_turn_id = None;
        }
        let outcome = ConversationTurnOutcome::Finished {
            terminal: payload.terminal,
            usage: payload.usage.clone(),
            error: payload.error.clone(),
        };
        if let Some(turn) = self.turn_mut(turn_id) {
            turn.outcome = Some(outcome);
        } else {
            self.snapshot.turns.push(ConversationTurnState {
                turn_id: turn_id.clone(),
                input_id: None,
                mode: None,
                recovery_ref: None,
                outcome: Some(outcome),
            });
        }
    }

    fn apply_turn_interrupted(&mut self, turn_id: &TurnId, payload: &TurnInterrupted) {
        if self.snapshot.current_turn_id.as_ref() == Some(turn_id) {
            self.snapshot.current_turn_id = None;
        }
        let outcome = ConversationTurnOutcome::Interrupted {
            reason: payload.reason,
            last_fact_seq: payload.last_fact_seq,
            recovery_ref: payload.recovery_ref.clone(),
        };
        if let Some(turn) = self.turn_mut(turn_id) {
            turn.outcome = Some(outcome);
        } else {
            self.snapshot.turns.push(ConversationTurnState {
                turn_id: turn_id.clone(),
                input_id: None,
                mode: None,
                recovery_ref: None,
                outcome: Some(outcome),
            });
        }
    }

    fn apply_tool_call_declared(&mut self, source_fact_seq: u64, payload: &ToolCallDeclared) {
        let state = self.tool_call_mut(source_fact_seq, &payload.call_id);
        state.turn_id = Some(payload.turn_id.clone());
        state.tool_name = Some(payload.tool_name.clone());
        state.args = Some(content_ref_value(payload.args_ref.clone()));
        state.args_hash = Some(payload.args_hash.clone());
    }

    fn apply_tool_finished(
        &mut self,
        source_fact_seq: u64,
        envelope_turn_id: Option<&TurnId>,
        payload: &ToolFinished,
    ) {
        let state = self.tool_call_mut(source_fact_seq, &payload.call_id);
        if state.turn_id.is_none() {
            state.turn_id = envelope_turn_id.cloned();
        }
        state.result = Some(ConversationToolResultState {
            terminal_status: payload.terminal_status,
            output: payload.output_ref.clone().map(content_ref_value),
            error: payload.error.clone(),
            metrics: payload.metrics.clone(),
            reconciled: payload.reconciled,
        });
    }

    fn apply_compaction(&mut self, source_fact_seq: u64, state: ConversationCompactionState) {
        self.snapshot
            .context
            .retain(|entry| entry.source_fact_seq > state.replaces_through_fact_seq);
        self.snapshot.compaction = Some(state.clone());
        self.snapshot.context.push(ConversationContextEntry {
            source_fact_seq,
            kind: ConversationContextKind::Compaction(state),
        });
    }

    fn upsert_input(&mut self, source_fact_seq: u64, state: ConversationInputState) {
        if let Some(index) = self.snapshot.context.iter().position(|entry| {
            matches!(
                &entry.kind,
                ConversationContextKind::Input(existing)
                    if existing.input_id == state.input_id
            )
        }) {
            let entry = &mut self.snapshot.context[index];
            entry.source_fact_seq = source_fact_seq;
            entry.kind = ConversationContextKind::Input(state);
        } else {
            self.snapshot.context.push(ConversationContextEntry {
                source_fact_seq,
                kind: ConversationContextKind::Input(state),
            });
        }
    }

    fn upsert_assistant_block(
        &mut self,
        source_fact_seq: u64,
        state: ConversationAssistantBlockState,
    ) {
        if let Some(index) = self.snapshot.context.iter().position(|entry| {
            matches!(
                &entry.kind,
                ConversationContextKind::AssistantBlock(existing)
                    if existing.block_id == state.block_id
            )
        }) {
            let entry = &mut self.snapshot.context[index];
            entry.source_fact_seq = source_fact_seq;
            entry.kind = ConversationContextKind::AssistantBlock(state);
        } else {
            self.snapshot.context.push(ConversationContextEntry {
                source_fact_seq,
                kind: ConversationContextKind::AssistantBlock(state),
            });
        }
    }

    fn tool_call_mut(
        &mut self,
        source_fact_seq: u64,
        call_id: &ToolCallId,
    ) -> &mut ConversationToolCallState {
        let existing = self.snapshot.context.iter().position(|entry| {
            matches!(
                &entry.kind,
                ConversationContextKind::ToolCall(state) if &state.call_id == call_id
            )
        });
        let index = if let Some(index) = existing {
            index
        } else {
            self.snapshot.context.push(ConversationContextEntry {
                source_fact_seq,
                kind: ConversationContextKind::ToolCall(ConversationToolCallState {
                    call_id: call_id.clone(),
                    turn_id: None,
                    tool_name: None,
                    args: None,
                    args_hash: None,
                    result: None,
                }),
            });
            self.snapshot.context.len() - 1
        };
        self.snapshot.context[index].source_fact_seq = source_fact_seq;
        match &mut self.snapshot.context[index].kind {
            ConversationContextKind::ToolCall(state) => state,
            _ => unreachable!("tool call entry must contain tool call state"),
        }
    }

    fn turn_mut(&mut self, turn_id: &TurnId) -> Option<&mut ConversationTurnState> {
        self.snapshot
            .turns
            .iter_mut()
            .find(|turn| &turn.turn_id == turn_id)
    }

    fn next_revision(&mut self) -> u64 {
        self.snapshot.revision = self.snapshot.revision.saturating_add(1);
        self.snapshot.revision
    }
}

fn input_content(payload: &InputAccepted) -> ContentValue {
    if let Some(content_ref) = &payload.content_ref {
        return content_ref_value(content_ref.clone());
    }
    ContentValue::Inline {
        text: payload.inline_text.clone().unwrap_or_default(),
    }
}

fn content_ref_value(content_ref: ContentRef) -> ContentValue {
    ContentValue::Ref { content_ref }
}
