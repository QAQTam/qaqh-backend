//! Model context projection over an explicit, committed fact prefix.
//!
//! A caller provides committed facts and content; this module never reads the
//! legacy message store or a live UI timeline. Display blocks do not substitute
//! for explicit complete-message facts.

use std::collections::HashSet;

use qaqh_types::{ContentBlock, Message};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::session_fact_v2::{
    ContentRef, ContextMessageOrigin, FactPayload, LogId, ProviderFinish, SessionFact, SessionId,
};

/// The caller must use a content-addressed source scoped to the same session.
pub trait ContextBlobReader {
    type Error: std::fmt::Display;
    fn read(&self, content: &ContentRef, max_bytes: usize) -> Result<Vec<u8>, Self::Error>;
}

impl ContextBlobReader for crate::canonical::SessionBlobStore {
    type Error = std::io::Error;
    fn read(&self, content: &ContentRef, max_bytes: usize) -> Result<Vec<u8>, Self::Error> {
        self.get_bounded(content, max_bytes)
    }
}

/// Snapshot payload persisted as a content-addressed blob by a committed
/// context-checkpoint fact. Images must be self-contained: external image paths
/// are not a durable session-scoped reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextCheckpoint {
    pub session_id: SessionId,
    pub log_id: LogId,
    pub through_fact_seq: u64,
    pub messages: Vec<Message>,
}

#[derive(Debug, thiserror::Error)]
pub enum ContextProjectionError {
    #[error("invalid committed context prefix: {0}")]
    InvalidPrefix(String),
    #[error("context blob unavailable: {0}")]
    BlobUnavailable(String),
    #[error("context cannot be reconstructed from existing fact: {0}")]
    IncompleteFact(&'static str),
    #[error("context checkpoint is invalid: {0}")]
    InvalidCheckpoint(String),
    #[error("context exceeds configured budget")]
    BudgetExceeded,
    #[error("context input was not materialized before request")]
    MissingInput,
    #[error("provider round has no complete replayable response")]
    MissingResponse,
}

/// Explicit bounds apply before allocating the complete result.
#[derive(Debug, Clone, Copy)]
pub struct ContextBudget {
    pub max_messages: usize,
    pub max_payload_bytes: u64,
    pub max_checkpoint_bytes: usize,
}

/// A fixed-prefix projector; mutation is staged by the caller until commit.
#[derive(Clone)]
pub struct ContextProjector {
    session_id: SessionId,
    log_id: LogId,
    head: u64,
    messages: Vec<Message>,
    payload_bytes: u64,
    budget: ContextBudget,
    pending_input: Option<crate::session_fact_v2::TurnId>,
    pending_response: Option<crate::session_fact_v2::TurnId>,
    /// Turn whose model response was recorded incomplete. It must be
    /// interrupted explicitly; it can never start another round.
    damaged_turn: Option<crate::session_fact_v2::TurnId>,
}

impl ContextProjector {
    /// Establish an empty context at sequence zero. Prefix messages must be
    /// supplied through committed context facts; nothing is discovered live.
    pub fn empty(session_id: SessionId, log_id: LogId, budget: ContextBudget) -> Self {
        Self {
            session_id,
            log_id,
            head: 0,
            messages: Vec::new(),
            payload_bytes: 0,
            budget,
            pending_input: None,
            pending_response: None,
            damaged_turn: None,
        }
    }

    pub fn head(&self) -> u64 {
        self.head
    }
    /// Apply one committed fact. A failing fact never advances the head.
    /// Model input comes solely from explicit complete-message facts; other
    /// business facts are not interpreted as partial context messages.
    pub fn apply<R: ContextBlobReader>(
        &mut self,
        fact: &SessionFact,
        blobs: &R,
    ) -> Result<(), ContextProjectionError> {
        if fact.session_id != self.session_id
            || fact.log_id != self.log_id
            || fact.fact_seq != self.head.checked_add(1).unwrap_or(0)
        {
            return Err(ContextProjectionError::InvalidPrefix(
                "identity or sequence mismatch".into(),
            ));
        }
        fact.validate()
            .map_err(|e| ContextProjectionError::InvalidPrefix(e.to_string()))?;
        match &fact.payload {
            FactPayload::ContextMessageCommitted(payload) => {
                let bytes = blobs
                    .read(&payload.message_ref, self.budget.max_checkpoint_bytes)
                    .map_err(|e| ContextProjectionError::BlobUnavailable(e.to_string()))?;
                verify_blob(&payload.message_ref, &bytes)?;
                if bytes.len() > self.budget.max_checkpoint_bytes {
                    return Err(ContextProjectionError::BudgetExceeded);
                }
                let message: Message = serde_json::from_slice(&bytes)
                    .map_err(|e| ContextProjectionError::InvalidCheckpoint(e.to_string()))?;
                validate_message(&message)?;
                let role_matches = match payload.origin {
                    ContextMessageOrigin::FrozenPrefix => {
                        matches!(message.role.as_str(), "system" | "developer")
                    }
                    ContextMessageOrigin::Input => message.role == "user",
                    ContextMessageOrigin::ProviderResponse => message.role == "assistant",
                    ContextMessageOrigin::ToolResult => {
                        matches!(message.role.as_str(), "tool" | "user")
                    }
                    ContextMessageOrigin::Feature => {
                        matches!(message.role.as_str(), "system" | "developer" | "user")
                    }
                };
                if !role_matches {
                    return Err(ContextProjectionError::InvalidCheckpoint(
                        "message role does not match origin".into(),
                    ));
                }
                if matches!(payload.origin, ContextMessageOrigin::Input)
                    && (self.pending_input.is_none() || self.pending_input != fact.turn_id)
                {
                    return Err(ContextProjectionError::InvalidCheckpoint(
                        "input message without started turn".into(),
                    ));
                }
                if matches!(payload.origin, ContextMessageOrigin::ProviderResponse)
                    && (self.pending_response.is_none() || self.pending_response != fact.turn_id)
                {
                    return Err(ContextProjectionError::InvalidCheckpoint(
                        "provider response without matching model round".into(),
                    ));
                }
                let payload_bytes = message.memory_usage_estimate().payload_bytes;
                if self.messages.len() >= self.budget.max_messages
                    || self.payload_bytes.saturating_add(payload_bytes)
                        > self.budget.max_payload_bytes
                {
                    return Err(ContextProjectionError::BudgetExceeded);
                }
                self.messages.push(message);
                self.payload_bytes += payload_bytes;
                if matches!(payload.origin, ContextMessageOrigin::Input) {
                    self.pending_input = None;
                }
                if matches!(payload.origin, ContextMessageOrigin::ProviderResponse) {
                    self.pending_response = None;
                }
            }
            FactPayload::ContextCheckpointCommitted(payload) => {
                if self.pending_input.is_some() {
                    return Err(ContextProjectionError::MissingInput);
                }
                if self.pending_response.is_some() {
                    return Err(ContextProjectionError::MissingResponse);
                }
                if payload.replaces_through_fact_seq != self.head {
                    return Err(ContextProjectionError::InvalidCheckpoint(
                        "checkpoint must replace current prefix".into(),
                    ));
                }
                let bytes = blobs
                    .read(&payload.checkpoint_ref, self.budget.max_checkpoint_bytes)
                    .map_err(|e| ContextProjectionError::BlobUnavailable(e.to_string()))?;
                verify_blob(&payload.checkpoint_ref, &bytes)?;
                let messages = decode_checkpoint(
                    &bytes,
                    &self.session_id,
                    &self.log_id,
                    self.head,
                    self.budget,
                )?;
                self.payload_bytes = messages
                    .iter()
                    .map(|m| m.memory_usage_estimate().payload_bytes)
                    .sum();
                self.messages = messages;
            }
            FactPayload::CompactionApplied(_) => {
                return Err(ContextProjectionError::IncompleteFact(
                    "legacy compaction fact does not include a context checkpoint",
                ));
            }
            FactPayload::ProviderResponseCommitted(payload) => {
                let pending =
                    self.pending_response
                        .as_ref()
                        .ok_or(ContextProjectionError::InvalidPrefix(
                            "provider response without a pending model round".into(),
                        ))?;
                if pending != &payload.turn_id {
                    return Err(ContextProjectionError::InvalidPrefix(
                        "provider response does not match the pending round".into(),
                    ));
                }
                if !payload.finish.is_complete() {
                    // Keep incomplete evidence out of the replayable view and
                    // block further rounds of this turn until it is interrupted.
                    self.pending_response = None;
                    self.damaged_turn = Some(payload.turn_id.clone());
                    self.head = fact.fact_seq;
                    return Ok(());
                }
                let bytes = blobs
                    .read(&payload.message_ref, self.budget.max_checkpoint_bytes)
                    .map_err(|e| ContextProjectionError::BlobUnavailable(e.to_string()))?;
                verify_blob(&payload.message_ref, &bytes)?;
                let mut message: Message = serde_json::from_slice(&bytes)
                    .map_err(|e| ContextProjectionError::InvalidCheckpoint(e.to_string()))?;
                if message.role != "assistant" {
                    return Err(ContextProjectionError::InvalidCheckpoint(
                        "provider response is not an assistant message".into(),
                    ));
                }
                for opaque_ref in &payload.opaque_refs {
                    let opaque_bytes = blobs
                        .read(opaque_ref, self.budget.max_checkpoint_bytes)
                        .map_err(|e| ContextProjectionError::BlobUnavailable(e.to_string()))?;
                    verify_blob(opaque_ref, &opaque_bytes)?;
                    let item: serde_json::Value = serde_json::from_slice(&opaque_bytes)
                        .map_err(|e| ContextProjectionError::InvalidCheckpoint(e.to_string()))?;
                    message
                        .content
                        .push(ContentBlock::ResponseOutputItem { item });
                }
                if message.content.iter().any(|block| {
                    block.is_hanging_tool_use()
                        || matches!(block, ContentBlock::ToolUse { input, .. } if input.is_null())
                }) {
                    return Err(ContextProjectionError::InvalidCheckpoint(
                        "provider response contains an incomplete tool call".into(),
                    ));
                }
                let declares_tools = message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolUse { .. }));
                if payload.finish == ProviderFinish::ToolUse && !declares_tools {
                    return Err(ContextProjectionError::InvalidCheckpoint(
                        "tool_use finish requires at least one tool call block".into(),
                    ));
                }
                if payload.finish == ProviderFinish::Stop && declares_tools {
                    return Err(ContextProjectionError::InvalidCheckpoint(
                        "stop finish must not declare tool call blocks".into(),
                    ));
                }
                validate_message(&message)?;
                let payload_bytes = message.memory_usage_estimate().payload_bytes;
                if self.messages.len() >= self.budget.max_messages
                    || self.payload_bytes.saturating_add(payload_bytes)
                        > self.budget.max_payload_bytes
                {
                    return Err(ContextProjectionError::BudgetExceeded);
                }
                self.messages.push(message);
                self.payload_bytes += payload_bytes;
                self.pending_response = None;
            }
            FactPayload::TurnStarted(payload) => {
                if self.pending_input.is_some() || self.pending_response.is_some() {
                    return Err(ContextProjectionError::InvalidPrefix(
                        "turn started before prior input materialization".into(),
                    ));
                }
                self.pending_input = Some(payload.turn_id.clone());
            }
            FactPayload::ModelRoundStarted(payload) => {
                if self.pending_input.is_some() {
                    return Err(ContextProjectionError::MissingInput);
                }
                if self.pending_response.is_some() {
                    return Err(ContextProjectionError::MissingResponse);
                }
                if self.damaged_turn.as_ref() == Some(&payload.turn_id) {
                    return Err(ContextProjectionError::IncompleteFact(
                        "turn has an incomplete provider response; interrupt it first",
                    ));
                }
                validate_complete_sequence(&self.messages)?;
                self.pending_response = Some(payload.turn_id.clone());
            }
            FactPayload::TurnFinished(payload) => {
                if self.pending_input.is_some() || self.pending_response.is_some() {
                    return Err(ContextProjectionError::InvalidPrefix(
                        "turn has missing replayable input or uncertain provider response".into(),
                    ));
                }
                if self.damaged_turn.as_ref() == Some(&payload.turn_id) {
                    return Err(ContextProjectionError::IncompleteFact(
                        "incomplete provider response requires an explicit interruption",
                    ));
                }
            }
            FactPayload::TurnInterrupted(payload) => {
                if self
                    .pending_input
                    .as_ref()
                    .is_some_and(|turn| turn != &payload.turn_id)
                    || self
                        .pending_response
                        .as_ref()
                        .is_some_and(|turn| turn != &payload.turn_id)
                    || self
                        .damaged_turn
                        .as_ref()
                        .is_some_and(|turn| turn != &payload.turn_id)
                {
                    return Err(ContextProjectionError::InvalidPrefix(
                        "interrupted turn does not match open context turn".into(),
                    ));
                }
                validate_complete_sequence(&self.messages)?;
                self.pending_input = None;
                self.pending_response = None;
                self.damaged_turn = None;
            }
            FactPayload::SessionCreated(_)
            | FactPayload::InputAccepted(_)
            | FactPayload::QueuedInputSettled(_)
            | FactPayload::AssistantBlockSealed(_)
            | FactPayload::ToolCallDeclared(_)
            | FactPayload::ToolIntent(_)
            | FactPayload::ToolFinished(_)
            | FactPayload::InteractionRequested(_)
            | FactPayload::InteractionResolved(_)
            | FactPayload::InteractionExpired(_)
            | FactPayload::SessionRecovered(_)
            | FactPayload::SessionMetadataChanged(_)
            | FactPayload::SessionTitleChanged(_)
            | FactPayload::SessionDeleted(_)
            | FactPayload::WorkspaceResourceChanged(_)
            | FactPayload::SubagentSpawned(_)
            | FactPayload::SubagentFinished(_)
            | FactPayload::InterAgentCommunication(_)
            | FactPayload::DriverChanged(_) => {}
        }
        self.head = fact.fact_seq;
        Ok(())
    }

    /// A request may only use a complete tool-call/result sequence.
    pub fn request_messages(&self) -> Result<&[Message], ContextProjectionError> {
        if self.pending_input.is_some() {
            return Err(ContextProjectionError::MissingInput);
        }
        if self.pending_response.is_some() {
            return Err(ContextProjectionError::MissingResponse);
        }
        validate_complete_sequence(&self.messages)?;
        Ok(&self.messages)
    }
}

fn verify_blob(reference: &ContentRef, bytes: &[u8]) -> Result<(), ContextProjectionError> {
    let hash = format!("sha256:{}", hex::encode(Sha256::digest(bytes)));
    if reference.hash().as_str() != hash {
        return Err(ContextProjectionError::BlobUnavailable(
            "content hash mismatch".into(),
        ));
    }
    Ok(())
}

/// Validate a checkpoint before using it at a separately verified committed head.
/// This function deliberately does not assert that the reference itself was
/// committed; the caller must establish that from a context-checkpoint fact.
pub fn decode_checkpoint(
    bytes: &[u8],
    session_id: &SessionId,
    log_id: &LogId,
    through_fact_seq: u64,
    budget: ContextBudget,
) -> Result<Vec<Message>, ContextProjectionError> {
    if bytes.len() > budget.max_checkpoint_bytes {
        return Err(ContextProjectionError::BudgetExceeded);
    }
    let checkpoint: ContextCheckpoint = serde_json::from_slice(bytes)
        .map_err(|e| ContextProjectionError::InvalidCheckpoint(e.to_string()))?;
    if &checkpoint.session_id != session_id
        || &checkpoint.log_id != log_id
        || checkpoint.through_fact_seq != through_fact_seq
    {
        return Err(ContextProjectionError::InvalidCheckpoint(
            "identity or head mismatch".into(),
        ));
    }
    if checkpoint.messages.len() > budget.max_messages {
        return Err(ContextProjectionError::BudgetExceeded);
    }
    let mut bytes = 0u64;
    for message in &checkpoint.messages {
        validate_message(message)?;
        bytes = bytes.saturating_add(message.memory_usage_estimate().payload_bytes);
    }
    validate_complete_sequence(&checkpoint.messages)?;
    if bytes > budget.max_payload_bytes {
        return Err(ContextProjectionError::BudgetExceeded);
    }
    Ok(checkpoint.messages)
}

fn validate_message(message: &Message) -> Result<(), ContextProjectionError> {
    if !matches!(
        message.role.as_str(),
        "system" | "developer" | "user" | "assistant" | "tool"
    ) {
        return Err(ContextProjectionError::InvalidCheckpoint(
            "unknown message role".into(),
        ));
    }
    if message
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::ImageRef { .. }))
    {
        return Err(ContextProjectionError::InvalidCheckpoint(
            "unscoped image reference".into(),
        ));
    }
    if message.content.iter().any(|block| {
        matches!(
            block,
            ContentBlock::Image { mime_type, data } if mime_type.is_empty() || data.is_empty()
        )
    }) {
        return Err(ContextProjectionError::InvalidCheckpoint(
            "empty image mime_type or data".into(),
        ));
    }
    Ok(())
}

fn validate_complete_sequence(messages: &[Message]) -> Result<(), ContextProjectionError> {
    let mut pending_calls = HashSet::new();
    // Duplicate ids are only illegal *within one model response*. Providers
    // reuse ids such as `call_1` across turns, so the ledger cannot be global:
    // a later response legitimately hands out an id an earlier one used.
    let mut seen_calls = HashSet::new();
    for message in messages {
        if !pending_calls.is_empty()
            && message.content.iter().any(|block| {
                !matches!(
                    block,
                    ContentBlock::ToolResult { .. } | ContentBlock::Image { .. }
                )
            })
        {
            return Err(ContextProjectionError::InvalidCheckpoint(
                "message interrupted outstanding tool results".into(),
            ));
        }
        if message.role == "assistant" {
            seen_calls.clear();
        }
        for block in &message.content {
            match block {
                ContentBlock::ToolUse { id, name, .. } => {
                    if message.role != "assistant"
                        || id.is_empty()
                        || name.is_empty()
                        || !seen_calls.insert(id.clone())
                    {
                        return Err(ContextProjectionError::InvalidCheckpoint(
                            "invalid or duplicate tool call".into(),
                        ));
                    }
                    pending_calls.insert(id.clone());
                }
                ContentBlock::ToolResult { tool_use_id, .. } => {
                    if !matches!(message.role.as_str(), "tool" | "user") {
                        return Err(ContextProjectionError::InvalidCheckpoint(
                            "tool result has invalid role".into(),
                        ));
                    }
                    if !pending_calls.remove(tool_use_id) {
                        return Err(ContextProjectionError::InvalidCheckpoint(
                            "unmatched tool result".into(),
                        ));
                    }
                }
                _ => {}
            }
        }
    }
    if !pending_calls.is_empty() {
        return Err(ContextProjectionError::InvalidCheckpoint(
            "unfinished tool calls".into(),
        ));
    }
    Ok(())
}
