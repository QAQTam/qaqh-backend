//! Rebuildable transcript projection for TUI and Web timeline consumers.

use serde::{Deserialize, Serialize};

use crate::session_fact_v2::{
    AssistantBlockKind, BlockId, CheckpointId, ContentRef, ContentValue, FactPayload,
    InputAccepted, InputId, InterruptReason, SessionFact, SessionId, TimelineDelta,
    ToolCallDeclared, ToolCallId, ToolError, ToolFinished, ToolTerminalStatus, TurnId,
    TurnTerminal,
};

use super::Projection;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineInputState {
    pub input_id: InputId,
    pub content: ContentValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineAssistantBlockState {
    pub block_id: BlockId,
    pub block_kind: AssistantBlockKind,
    pub content: ContentValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineToolResultState {
    pub terminal_status: ToolTerminalStatus,
    pub output: Option<ContentValue>,
    pub error: Option<ToolError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineToolCallState {
    pub call_id: ToolCallId,
    pub turn_id: Option<TurnId>,
    pub tool_name: Option<String>,
    pub args: Option<ContentValue>,
    pub result: Option<TimelineToolResultState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineTurnTerminalState {
    pub turn_id: TurnId,
    pub terminal: TurnTerminal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineTurnInterruptedState {
    pub turn_id: TurnId,
    pub reason: InterruptReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineCompactionState {
    pub checkpoint_id: CheckpointId,
    pub summary: ContentValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum TimelineEntryKind {
    Input(TimelineInputState),
    AssistantBlock(TimelineAssistantBlockState),
    ToolCall(TimelineToolCallState),
    TurnFinished(TimelineTurnTerminalState),
    TurnInterrupted(TimelineTurnInterruptedState),
    Compaction(TimelineCompactionState),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineEntry {
    pub source_fact_seq: u64,
    pub kind: TimelineEntryKind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineSnapshot {
    pub session_id: Option<SessionId>,
    pub entries: Vec<TimelineEntry>,
    pub revision: u64,
    pub last_fact_seq: u64,
}

#[derive(Debug, Default)]
pub struct TimelineProjection {
    snapshot: TimelineSnapshot,
}

impl Projection for TimelineProjection {
    type Snapshot = TimelineSnapshot;
    type Delta = TimelineDelta;

    fn apply(&mut self, fact: &SessionFact) -> Option<Self::Delta> {
        self.snapshot.last_fact_seq = fact.fact_seq;
        if self.snapshot.session_id.is_none() {
            self.snapshot.session_id = Some(fact.session_id.clone());
        }

        match &fact.payload {
            FactPayload::InputAccepted(payload) => {
                let content = input_content(payload);
                self.upsert_input(fact.fact_seq, payload.input_id.clone(), content.clone());
                Some(TimelineDelta::Input {
                    revision: self.next_revision(),
                    input_id: payload.input_id.clone(),
                    content,
                })
            }
            FactPayload::AssistantBlockSealed(payload) => {
                let content = content_ref_value(payload.content_ref.clone());
                self.upsert_assistant_block(
                    fact.fact_seq,
                    payload.block_id.clone(),
                    payload.kind,
                    content.clone(),
                );
                Some(TimelineDelta::Block {
                    revision: self.next_revision(),
                    block_id: payload.block_id.clone(),
                    block_kind: payload.kind,
                    content,
                })
            }
            FactPayload::ToolCallDeclared(payload) => {
                self.apply_tool_call_declared(fact.fact_seq, payload);
                Some(TimelineDelta::ToolCall {
                    revision: self.next_revision(),
                    call_id: payload.call_id.clone(),
                    tool_name: payload.tool_name.clone(),
                    args: content_ref_value(payload.args_ref.clone()),
                })
            }
            FactPayload::ToolFinished(payload) => {
                self.apply_tool_finished(fact.fact_seq, fact.turn_id.as_ref(), payload);
                Some(TimelineDelta::ToolResult {
                    revision: self.next_revision(),
                    call_id: payload.call_id.clone(),
                    terminal_status: payload.terminal_status,
                    output: payload.output_ref.clone().map(content_ref_value),
                    error: payload.error.clone(),
                })
            }
            FactPayload::TurnFinished(payload) => {
                self.upsert_turn_finished(fact.fact_seq, payload.turn_id.clone(), payload.terminal);
                Some(TimelineDelta::TurnFinished {
                    revision: self.next_revision(),
                    turn_id: payload.turn_id.clone(),
                    terminal: payload.terminal,
                })
            }
            FactPayload::TurnInterrupted(payload) => {
                self.upsert_turn_interrupted(
                    fact.fact_seq,
                    payload.turn_id.clone(),
                    payload.reason,
                );
                Some(TimelineDelta::TurnInterrupted {
                    revision: self.next_revision(),
                    turn_id: payload.turn_id.clone(),
                    reason: payload.reason,
                })
            }
            FactPayload::CompactionApplied(payload) => {
                let state = TimelineCompactionState {
                    checkpoint_id: payload.checkpoint_id.clone(),
                    summary: content_ref_value(payload.summary_ref.clone()),
                };
                self.upsert_compaction(fact.fact_seq, state.clone());
                Some(TimelineDelta::Compaction {
                    revision: self.next_revision(),
                    checkpoint_id: payload.checkpoint_id.clone(),
                    summary: state.summary,
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

impl TimelineProjection {
    fn apply_tool_call_declared(&mut self, source_fact_seq: u64, payload: &ToolCallDeclared) {
        let state = self.tool_call_mut(source_fact_seq, &payload.call_id);
        state.turn_id = Some(payload.turn_id.clone());
        state.tool_name = Some(payload.tool_name.clone());
        state.args = Some(content_ref_value(payload.args_ref.clone()));
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
        state.result = Some(TimelineToolResultState {
            terminal_status: payload.terminal_status,
            output: payload.output_ref.clone().map(content_ref_value),
            error: payload.error.clone(),
        });
    }

    fn upsert_input(&mut self, source_fact_seq: u64, input_id: InputId, content: ContentValue) {
        if let Some(index) = self.snapshot.entries.iter().position(|entry| {
            matches!(
                &entry.kind,
                TimelineEntryKind::Input(state) if state.input_id == input_id
            )
        }) {
            let entry = &mut self.snapshot.entries[index];
            entry.source_fact_seq = source_fact_seq;
            entry.kind = TimelineEntryKind::Input(TimelineInputState { input_id, content });
        } else {
            self.snapshot.entries.push(TimelineEntry {
                source_fact_seq,
                kind: TimelineEntryKind::Input(TimelineInputState { input_id, content }),
            });
        }
    }

    fn upsert_assistant_block(
        &mut self,
        source_fact_seq: u64,
        block_id: BlockId,
        block_kind: AssistantBlockKind,
        content: ContentValue,
    ) {
        if let Some(index) = self.snapshot.entries.iter().position(|entry| {
            matches!(
                &entry.kind,
                TimelineEntryKind::AssistantBlock(state) if state.block_id == block_id
            )
        }) {
            let entry = &mut self.snapshot.entries[index];
            entry.source_fact_seq = source_fact_seq;
            entry.kind = TimelineEntryKind::AssistantBlock(TimelineAssistantBlockState {
                block_id,
                block_kind,
                content,
            });
        } else {
            self.snapshot.entries.push(TimelineEntry {
                source_fact_seq,
                kind: TimelineEntryKind::AssistantBlock(TimelineAssistantBlockState {
                    block_id,
                    block_kind,
                    content,
                }),
            });
        }
    }

    fn upsert_turn_finished(
        &mut self,
        source_fact_seq: u64,
        turn_id: TurnId,
        terminal: TurnTerminal,
    ) {
        if let Some(index) = self
            .snapshot
            .entries
            .iter()
            .position(|entry| match &entry.kind {
                TimelineEntryKind::TurnFinished(state) => state.turn_id == turn_id,
                TimelineEntryKind::TurnInterrupted(state) => state.turn_id == turn_id,
                _ => false,
            })
        {
            let entry = &mut self.snapshot.entries[index];
            entry.source_fact_seq = source_fact_seq;
            entry.kind =
                TimelineEntryKind::TurnFinished(TimelineTurnTerminalState { turn_id, terminal });
        } else {
            self.snapshot.entries.push(TimelineEntry {
                source_fact_seq,
                kind: TimelineEntryKind::TurnFinished(TimelineTurnTerminalState {
                    turn_id,
                    terminal,
                }),
            });
        }
    }

    fn upsert_turn_interrupted(
        &mut self,
        source_fact_seq: u64,
        turn_id: TurnId,
        reason: InterruptReason,
    ) {
        if let Some(index) = self
            .snapshot
            .entries
            .iter()
            .position(|entry| match &entry.kind {
                TimelineEntryKind::TurnFinished(state) => state.turn_id == turn_id,
                TimelineEntryKind::TurnInterrupted(state) => state.turn_id == turn_id,
                _ => false,
            })
        {
            let entry = &mut self.snapshot.entries[index];
            entry.source_fact_seq = source_fact_seq;
            entry.kind = TimelineEntryKind::TurnInterrupted(TimelineTurnInterruptedState {
                turn_id,
                reason,
            });
        } else {
            self.snapshot.entries.push(TimelineEntry {
                source_fact_seq,
                kind: TimelineEntryKind::TurnInterrupted(TimelineTurnInterruptedState {
                    turn_id,
                    reason,
                }),
            });
        }
    }

    fn upsert_compaction(&mut self, source_fact_seq: u64, state: TimelineCompactionState) {
        if let Some(index) = self.snapshot.entries.iter().position(|entry| {
            matches!(
                &entry.kind,
                TimelineEntryKind::Compaction(existing)
                    if existing.checkpoint_id == state.checkpoint_id
            )
        }) {
            let entry = &mut self.snapshot.entries[index];
            entry.source_fact_seq = source_fact_seq;
            entry.kind = TimelineEntryKind::Compaction(state);
        } else {
            self.snapshot.entries.push(TimelineEntry {
                source_fact_seq,
                kind: TimelineEntryKind::Compaction(state),
            });
        }
    }

    fn tool_call_mut(
        &mut self,
        source_fact_seq: u64,
        call_id: &ToolCallId,
    ) -> &mut TimelineToolCallState {
        let existing = self.snapshot.entries.iter().position(|entry| {
            matches!(
                &entry.kind,
                TimelineEntryKind::ToolCall(state) if &state.call_id == call_id
            )
        });
        let index = if let Some(index) = existing {
            index
        } else {
            self.snapshot.entries.push(TimelineEntry {
                source_fact_seq,
                kind: TimelineEntryKind::ToolCall(TimelineToolCallState {
                    call_id: call_id.clone(),
                    turn_id: None,
                    tool_name: None,
                    args: None,
                    result: None,
                }),
            });
            self.snapshot.entries.len() - 1
        };
        self.snapshot.entries[index].source_fact_seq = source_fact_seq;
        match &mut self.snapshot.entries[index].kind {
            TimelineEntryKind::ToolCall(state) => state,
            _ => unreachable!("tool call entry must contain tool call state"),
        }
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
