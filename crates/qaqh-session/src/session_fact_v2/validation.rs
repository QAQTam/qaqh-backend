use std::fmt;

use super::types::{
    ActorRef, ContentHash, ContentRef, FactPayload, FactSchema, MAX_SAFE_FACT_SEQ, RecoveryAction,
    RecoveryOutcome, RecoveryRef, RecoveryToolCompletion, SessionFact, SessionMetadataPatch,
    ToolError, ToolIntentPolicyOutcome, ToolMetrics, ToolReplayCapability, ToolTerminalStatus,
    TurnError, TurnTerminal,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    InvalidSchema {
        field: &'static str,
    },
    InvalidFactSeq {
        fact_seq: u64,
    },
    InvalidId {
        field: &'static str,
        value: String,
    },
    InvalidField {
        field: &'static str,
        message: String,
    },
    EnvelopeMismatch {
        field: &'static str,
    },
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSchema { field } => {
                write!(formatter, "invalid session-fact schema field: {field}")
            }
            Self::InvalidFactSeq { fact_seq } => {
                write!(
                    formatter,
                    "fact_seq must be in 1..={MAX_SAFE_FACT_SEQ}, got {fact_seq}"
                )
            }
            Self::InvalidId { field, value } => {
                write!(formatter, "invalid {field}: {value}")
            }
            Self::InvalidField { field, message } => {
                write!(formatter, "invalid {field}: {message}")
            }
            Self::EnvelopeMismatch { field } => {
                write!(formatter, "envelope does not match payload field: {field}")
            }
        }
    }
}

impl std::error::Error for ValidationError {}

impl FactSchema {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.name != super::types::SESSION_FACT_SCHEMA_NAME {
            return Err(ValidationError::InvalidSchema { field: "name" });
        }
        if self.version != super::types::SESSION_FACT_SCHEMA_VERSION {
            return Err(ValidationError::InvalidSchema { field: "version" });
        }
        if self.payload_version != super::types::SESSION_FACT_PAYLOAD_VERSION {
            return Err(ValidationError::InvalidSchema {
                field: "payload_version",
            });
        }
        Ok(())
    }
}

impl SessionFact {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.schema.validate()?;
        validate_fact_seq("fact_seq", self.fact_seq)?;
        validate_uuid_v7("session_id", self.session_id.as_str())?;
        validate_uuid_v7("log_id", self.log_id.as_str())?;
        validate_ulid("event_id", self.event_id.as_str())?;
        if let Some(causation_id) = &self.causation_id {
            validate_ulid("causation_id", causation_id.as_str())?;
        }
        if let Some(turn_id) = &self.turn_id {
            validate_prefixed_ulid("turn_id", turn_id.as_str(), "turn_")?;
        }
        if let Some(call_id) = &self.call_id {
            validate_prefixed_ulid("call_id", call_id.as_str(), "call_")?;
        }
        if let Some(interaction_id) = &self.interaction_id {
            validate_prefixed_ulid("interaction_id", interaction_id.as_str(), "int_")?;
        }

        self.payload.validate()?;
        self.validate_envelope_links()
    }

    fn validate_envelope_links(&self) -> Result<(), ValidationError> {
        match &self.payload {
            FactPayload::SessionCreated(payload) => {
                if payload.created_at_ms != self.ts_ms {
                    return Err(ValidationError::EnvelopeMismatch {
                        field: "created_at_ms",
                    });
                }
            }
            FactPayload::TurnStarted(payload) => {
                require_turn_id(self.turn_id.as_ref(), &payload.turn_id)?;
            }
            FactPayload::ModelRoundStarted(payload) => {
                require_turn_id(self.turn_id.as_ref(), &payload.turn_id)?;
            }
            FactPayload::AssistantBlockSealed(payload) => {
                require_turn_id(self.turn_id.as_ref(), &payload.turn_id)?;
            }
            FactPayload::ToolCallDeclared(payload) => {
                require_turn_id(self.turn_id.as_ref(), &payload.turn_id)?;
                require_call_id(self.call_id.as_ref(), &payload.call_id)?;
            }
            FactPayload::ToolIntent(payload) => {
                require_call_id(self.call_id.as_ref(), &payload.call_id)?;
            }
            FactPayload::ToolFinished(payload) => {
                require_call_id(self.call_id.as_ref(), &payload.call_id)?;
            }
            FactPayload::InteractionRequested(payload) => {
                require_interaction_id(self.interaction_id.as_ref(), &payload.interaction_id)?;
                require_turn_id(self.turn_id.as_ref(), &payload.turn_id)?;
                if let (Some(envelope), Some(payload)) = (&self.call_id, &payload.call_id)
                    && envelope != payload
                {
                    return Err(ValidationError::EnvelopeMismatch { field: "call_id" });
                }
            }
            FactPayload::InteractionResolved(payload) => {
                require_interaction_id(self.interaction_id.as_ref(), &payload.interaction_id)?;
            }
            FactPayload::InteractionExpired(payload) => {
                require_interaction_id(self.interaction_id.as_ref(), &payload.interaction_id)?;
            }
            // Driver seat changes carry no turn/call/interaction envelope link.
            FactPayload::DriverChanged(_) => {}
            FactPayload::TurnFinished(payload) => {
                require_turn_id(self.turn_id.as_ref(), &payload.turn_id)?;
            }
            FactPayload::TurnInterrupted(payload) => {
                require_turn_id(self.turn_id.as_ref(), &payload.turn_id)?;
            }
            FactPayload::WorkspaceResourceChanged(payload) => {
                if let (Some(envelope), Some(payload)) = (&self.call_id, &payload.source_call_id)
                    && envelope != payload
                {
                    return Err(ValidationError::EnvelopeMismatch { field: "call_id" });
                }
            }
            FactPayload::SubagentSpawned(payload) => {
                if let Some(envelope) = &self.call_id
                    && envelope != &payload.parent_call_id
                {
                    return Err(ValidationError::EnvelopeMismatch { field: "call_id" });
                }
            }
            FactPayload::SubagentFinished(payload) => {
                if let Some(envelope) = &self.call_id
                    && envelope != &payload.parent_call_id
                {
                    return Err(ValidationError::EnvelopeMismatch { field: "call_id" });
                }
            }
            FactPayload::InputAccepted(_)
            | FactPayload::SessionRecovered(_)
            | FactPayload::CompactionApplied(_)
            | FactPayload::SessionMetadataChanged(_)
            | FactPayload::SessionTitleChanged(_)
            | FactPayload::SessionDeleted(_) => {}
        }
        Ok(())
    }
}

impl FactPayload {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::SessionCreated(payload) => {
                validate_absolute_cwd("cwd", &payload.cwd)?;
                validate_non_empty("model", &payload.model)?;
                if let Some(parent_session_id) = &payload.parent_session_id {
                    validate_uuid_v7("parent_session_id", parent_session_id.as_str())?;
                }
            }
            Self::InputAccepted(payload) => {
                validate_prefixed_ulid("input_id", payload.input_id.as_str(), "input_")?;
                match (&payload.content_ref, &payload.inline_text) {
                    (Some(content_ref), None) => validate_content_ref("content_ref", content_ref)?,
                    (None, Some(inline_text)) => {
                        validate_byte_limit("inline_text", inline_text, 8 * 1024)?;
                    }
                    (Some(_), Some(_)) => {
                        return Err(ValidationError::InvalidField {
                            field: "content_ref",
                            message: "content_ref and inline_text are mutually exclusive"
                                .to_owned(),
                        });
                    }
                    (None, None) => {
                        return Err(ValidationError::InvalidField {
                            field: "content_ref",
                            message: "exactly one of content_ref or inline_text is required"
                                .to_owned(),
                        });
                    }
                }
                for attachment in &payload.attachments {
                    validate_content_ref("attachments", attachment)?;
                }
                validate_actor_ref("actor", &payload.actor)?;
            }
            Self::TurnStarted(payload) => {
                validate_prefixed_ulid("turn_id", payload.turn_id.as_str(), "turn_")?;
                validate_prefixed_ulid("input_id", payload.input_id.as_str(), "input_")?;
                if let Some(recovery_ref) = &payload.recovery_ref {
                    validate_recovery_ref("recovery_ref", recovery_ref)?;
                }
            }
            Self::ModelRoundStarted(payload) => {
                validate_prefixed_ulid("turn_id", payload.turn_id.as_str(), "turn_")?;
                validate_content_hash("request_hash", &payload.request_hash)?;
            }
            Self::AssistantBlockSealed(payload) => {
                validate_prefixed_ulid("turn_id", payload.turn_id.as_str(), "turn_")?;
                validate_prefixed_ulid("block_id", payload.block_id.as_str(), "block_")?;
                validate_content_ref("content_ref", &payload.content_ref)?;
                validate_non_empty("model", &payload.model)?;
            }
            Self::ToolCallDeclared(payload) => {
                validate_prefixed_ulid("turn_id", payload.turn_id.as_str(), "turn_")?;
                validate_prefixed_ulid("call_id", payload.call_id.as_str(), "call_")?;
                validate_non_empty("tool_name", &payload.tool_name)?;
                validate_content_ref("args_ref", &payload.args_ref)?;
                validate_content_hash("args_hash", &payload.args_hash)?;
                if payload.args_hash != payload.args_ref.0 {
                    return Err(ValidationError::InvalidField {
                        field: "args_hash",
                        message: "must equal args_ref hash".to_owned(),
                    });
                }
            }
            Self::ToolIntent(payload) => validate_tool_intent(payload)?,
            Self::ToolFinished(payload) => validate_tool_finished(payload)?,
            Self::InteractionRequested(payload) => {
                validate_prefixed_ulid("interaction_id", payload.interaction_id.as_str(), "int_")?;
                if let Some(call_id) = &payload.call_id {
                    validate_prefixed_ulid("call_id", call_id.as_str(), "call_")?;
                }
                if payload.kind == super::types::InteractionKind::Permission
                    && payload.call_id.is_none()
                {
                    return Err(ValidationError::InvalidField {
                        field: "call_id",
                        message: "permission interaction requires a call_id".to_owned(),
                    });
                }
                validate_prefixed_ulid("turn_id", payload.turn_id.as_str(), "turn_")?;
                validate_content_ref("request_ref", &payload.request_ref)?;
                if let Some(expires_at_ms) = payload.expires_at_ms
                    && expires_at_ms < payload.requested_at_ms
                {
                    return Err(ValidationError::InvalidField {
                        field: "expires_at_ms",
                        message: "must not precede requested_at_ms".to_owned(),
                    });
                }
            }
            Self::InteractionResolved(payload) => {
                validate_prefixed_ulid("interaction_id", payload.interaction_id.as_str(), "int_")?;
                validate_content_ref("decision_ref", &payload.decision_ref)?;
                validate_actor_ref("resolved_by", &payload.resolved_by)?;
                if payload.resolution_seq != 1 {
                    return Err(ValidationError::InvalidField {
                        field: "resolution_seq",
                        message: "must be 1 for the first answer".to_owned(),
                    });
                }
            }
            Self::DriverChanged(payload) => {
                if let Some(holder) = &payload.holder {
                    validate_non_empty("holder", holder)?;
                }
                if payload.driver_epoch == 0 || payload.driver_epoch > MAX_SAFE_FACT_SEQ {
                    return Err(ValidationError::InvalidField {
                        field: "driver_epoch",
                        message: format!(
                            "driver_epoch must be in 1..={MAX_SAFE_FACT_SEQ}, got {}",
                            payload.driver_epoch
                        ),
                    });
                }
            }
            Self::InteractionExpired(payload) => {
                validate_prefixed_ulid("interaction_id", payload.interaction_id.as_str(), "int_")?;
                if let Some(recovery_ref) = &payload.recovery_ref {
                    validate_recovery_ref("recovery_ref", recovery_ref)?;
                }
            }
            Self::TurnFinished(payload) => {
                validate_prefixed_ulid("turn_id", payload.turn_id.as_str(), "turn_")?;
                if let Some(error) = &payload.error {
                    validate_turn_error("error", error)?;
                }
                if payload.terminal == TurnTerminal::Failed && payload.error.is_none() {
                    return Err(ValidationError::InvalidField {
                        field: "error",
                        message: "failed turn requires an error".to_owned(),
                    });
                }
            }
            Self::TurnInterrupted(payload) => {
                validate_prefixed_ulid("turn_id", payload.turn_id.as_str(), "turn_")?;
                validate_fact_seq("last_fact_seq", payload.last_fact_seq)?;
                validate_recovery_ref("recovery_ref", &payload.recovery_ref)?;
            }
            Self::SessionRecovered(payload) => {
                validate_prefixed_ulid("recovery_id", payload.recovery_id.as_str(), "recovery_")?;
                validate_ulid("recovery_event_id", payload.recovery_event_id.as_str())?;
                validate_content_hash(
                    "recovery_input_fingerprint",
                    &payload.recovery_input_fingerprint,
                )?;
                validate_fact_seq("last_good_fact_seq", payload.last_good_fact_seq)?;
                for action in &payload.actions {
                    action.validate()?;
                }
                if payload.outcome == RecoveryOutcome::Tombstone && payload.torn_tail {
                    return Err(ValidationError::InvalidField {
                        field: "torn_tail",
                        message: "tombstone recovery cannot claim a torn tail".to_owned(),
                    });
                }
            }
            Self::CompactionApplied(payload) => {
                validate_prefixed_ulid("checkpoint_id", payload.checkpoint_id.as_str(), "ckpt_")?;
                validate_fact_seq(
                    "replaces_through_fact_seq",
                    payload.replaces_through_fact_seq,
                )?;
                validate_content_ref("summary_ref", &payload.summary_ref)?;
            }
            Self::SessionMetadataChanged(payload) => {
                validate_metadata_patch(&payload.patch)?;
            }
            Self::SessionTitleChanged(payload) => {
                validate_byte_limit("title", &payload.title, 4 * 1024)?;
            }
            Self::SessionDeleted(payload) => {
                if let Some(purge_after_ms) = payload.purge_after_ms
                    && purge_after_ms < payload.tombstone_at_ms
                {
                    return Err(ValidationError::InvalidField {
                        field: "purge_after_ms",
                        message: "must not precede tombstone_at_ms".to_owned(),
                    });
                }
            }
            Self::WorkspaceResourceChanged(payload) => {
                validate_prefixed_ulid("resource_id", payload.resource_id.as_str(), "res_")?;
                if let Some(source_call_id) = &payload.source_call_id {
                    validate_prefixed_ulid("source_call_id", source_call_id.as_str(), "call_")?;
                }
                validate_content_ref("summary_ref", &payload.summary_ref)?;
            }
            Self::SubagentSpawned(payload) => {
                validate_uuid_v7("child_session_id", payload.child_session_id.as_str())?;
                validate_prefixed_ulid("parent_call_id", payload.parent_call_id.as_str(), "call_")?;
                match (&payload.parent_agent_path, &payload.child_agent_path) {
                    (None, None) => {}
                    (Some(parent), Some(child)) => {
                        if parent.namespace() != super::agent::AgentNamespace::Root
                            || child.namespace() != super::agent::AgentNamespace::Root
                        {
                            return Err(ValidationError::InvalidField {
                                field: "agent_path",
                                message: "subagent paths must belong to the /root namespace"
                                    .to_owned(),
                            });
                        }
                        if child.parent().as_ref() != Some(parent) {
                            return Err(ValidationError::InvalidField {
                                field: "child_agent_path",
                                message: "child path must be a direct child of parent path"
                                    .to_owned(),
                            });
                        }
                    }
                    _ => {
                        return Err(ValidationError::InvalidField {
                            field: "agent_path",
                            message:
                                "parent_agent_path and child_agent_path must be present together"
                                    .to_owned(),
                        });
                    }
                }
            }
            Self::SubagentFinished(payload) => {
                validate_uuid_v7("child_session_id", payload.child_session_id.as_str())?;
                validate_prefixed_ulid("parent_call_id", payload.parent_call_id.as_str(), "call_")?;
                if let Some(result_ref) = &payload.result_ref {
                    validate_content_ref("result_ref", result_ref)?;
                }
                if let Some(recovery_ref) = &payload.recovery_ref {
                    validate_recovery_ref("recovery_ref", recovery_ref)?;
                }
            }
        }
        Ok(())
    }
}

impl RecoveryAction {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::TurnInterrupted {
                turn_id,
                last_fact_seq,
            } => {
                validate_prefixed_ulid("turn_id", turn_id.as_str(), "turn_")?;
                validate_fact_seq("last_fact_seq", *last_fact_seq)?;
            }
            Self::TurnStarted {
                turn_id,
                input_id,
                recovery_ref,
                ..
            } => {
                validate_prefixed_ulid("turn_id", turn_id.as_str(), "turn_")?;
                validate_prefixed_ulid("input_id", input_id.as_str(), "input_")?;
                validate_recovery_ref("recovery_ref", recovery_ref)?;
            }
            Self::ToolFinished { completion } => validate_recovery_tool_completion(completion)?,
            Self::InteractionExpired { interaction_id, .. } => {
                validate_prefixed_ulid("interaction_id", interaction_id.as_str(), "int_")?;
            }
            Self::SubagentFinished {
                child_session_id,
                child_log_id,
                terminal_fact_seq,
                terminal_event_id,
                parent_call_id,
                result_ref,
                recovery_ref,
                ..
            } => {
                validate_uuid_v7("child_session_id", child_session_id.as_str())?;
                validate_uuid_v7("child_log_id", child_log_id.as_str())?;
                validate_fact_seq("terminal_fact_seq", *terminal_fact_seq)?;
                validate_ulid("terminal_event_id", terminal_event_id.as_str())?;
                validate_prefixed_ulid("parent_call_id", parent_call_id.as_str(), "call_")?;
                if let Some(result_ref) = result_ref {
                    validate_content_ref("result_ref", result_ref)?;
                }
                validate_recovery_ref("recovery_ref", recovery_ref)?;
            }
            Self::UpgradeSuperseded {
                previous_recovery_id,
            } => {
                validate_prefixed_ulid(
                    "previous_recovery_id",
                    previous_recovery_id.as_str(),
                    "recovery_",
                )?;
            }
            Self::CommitRepaired {
                committed_fact_seq, ..
            } => {
                validate_fact_seq("committed_fact_seq", *committed_fact_seq)?;
            }
            Self::MoveTornTail { bytes_hash, .. } => {
                validate_content_hash("bytes_hash", bytes_hash)?;
            }
            Self::ProjectionRebuilt {
                through_fact_seq, ..
            } => {
                validate_fact_seq("through_fact_seq", *through_fact_seq)?;
            }
        }
        Ok(())
    }
}

fn validate_tool_intent(payload: &super::types::ToolIntent) -> Result<(), ValidationError> {
    validate_prefixed_ulid("call_id", payload.call_id.as_str(), "call_")?;
    validate_prefixed_ulid("execution_id", payload.execution_id.as_str(), "exec_")?;
    validate_content_hash("sandbox_spec_hash", &payload.sandbox_spec_hash)?;
    match &payload.replay_capability {
        ToolReplayCapability::NoReplay => {
            if payload.idempotency_key.is_some() {
                return Err(ValidationError::InvalidField {
                    field: "idempotency_key",
                    message: "must be absent for no_replay".to_owned(),
                });
            }
        }
        ToolReplayCapability::IdempotentReplay => {
            if payload.idempotency_key.is_none() {
                return Err(ValidationError::InvalidField {
                    field: "idempotency_key",
                    message: "required for idempotent_replay".to_owned(),
                });
            }
        }
        ToolReplayCapability::Reconcile { probe_ref } => {
            validate_content_ref("replay_capability.probe_ref", probe_ref)?;
        }
    }

    match (&payload.effective_args_ref, &payload.effective_args_hash) {
        (Some(effective_args_ref), Some(effective_args_hash)) => {
            validate_content_ref("effective_args_ref", effective_args_ref)?;
            validate_content_hash("effective_args_hash", effective_args_hash)?;
            if effective_args_hash != &effective_args_ref.0 {
                return Err(ValidationError::InvalidField {
                    field: "effective_args_hash",
                    message: "must equal effective_args_ref hash".to_owned(),
                });
            }
        }
        (None, None) => {
            if payload.policy_decision.outcome == ToolIntentPolicyOutcome::Amend {
                return Err(ValidationError::InvalidField {
                    field: "effective_args_ref",
                    message: "amend requires effective args".to_owned(),
                });
            }
        }
        _ => {
            return Err(ValidationError::InvalidField {
                field: "effective_args_ref",
                message: "effective_args_ref and effective_args_hash must both be present"
                    .to_owned(),
            });
        }
    }

    if let Some(reason_ref) = &payload.policy_decision.reason_ref {
        validate_content_ref("policy_decision.reason_ref", reason_ref)?;
    }
    Ok(())
}

fn validate_tool_finished(payload: &super::types::ToolFinished) -> Result<(), ValidationError> {
    validate_prefixed_ulid("call_id", payload.call_id.as_str(), "call_")?;
    if let Some(execution_id) = &payload.execution_id {
        validate_prefixed_ulid("execution_id", execution_id.as_str(), "exec_")?;
    } else if !matches!(
        payload.terminal_status,
        ToolTerminalStatus::Denied | ToolTerminalStatus::Cancelled
    ) {
        return Err(ValidationError::InvalidField {
            field: "execution_id",
            message: "is only optional for denied/cancelled terminal states".to_owned(),
        });
    }
    if let Some(output_ref) = &payload.output_ref {
        validate_content_ref("output_ref", output_ref)?;
    }
    if let Some(error) = &payload.error {
        validate_tool_error("error", error)?;
    }
    validate_tool_metrics("metrics", &payload.metrics)?;
    if let Some(evidence_ref) = &payload.evidence_ref {
        validate_content_ref("evidence_ref", evidence_ref)?;
    }
    if let Some(evidence_fact_seq) = payload.evidence_fact_seq {
        validate_fact_seq("evidence_fact_seq", evidence_fact_seq)?;
    }
    if let Some(evidence_event_id) = &payload.evidence_event_id {
        validate_ulid("evidence_event_id", evidence_event_id.as_str())?;
    }
    validate_reconciled_evidence(
        payload.reconciled,
        payload.evidence_ref.as_ref(),
        payload.evidence_fact_seq,
        payload.evidence_event_id.as_ref(),
    )?;
    if let Some(recovery_ref) = &payload.recovery_ref {
        validate_recovery_ref("recovery_ref", recovery_ref)?;
    }
    Ok(())
}

fn validate_recovery_tool_completion(
    completion: &RecoveryToolCompletion,
) -> Result<(), ValidationError> {
    if completion.terminal_status == ToolTerminalStatus::Backgrounded {
        return Err(ValidationError::InvalidField {
            field: "terminal_status",
            message: "backgrounded is not valid for recovery completion".to_owned(),
        });
    }
    validate_prefixed_ulid("call_id", completion.call_id.as_str(), "call_")?;
    if let Some(execution_id) = &completion.execution_id {
        validate_prefixed_ulid("execution_id", execution_id.as_str(), "exec_")?;
    } else if !matches!(
        completion.terminal_status,
        ToolTerminalStatus::Denied | ToolTerminalStatus::Cancelled
    ) {
        return Err(ValidationError::InvalidField {
            field: "execution_id",
            message: "is only optional for denied/cancelled terminal states".to_owned(),
        });
    }
    if let Some(output_ref) = &completion.output_ref {
        validate_content_ref("output_ref", output_ref)?;
    }
    if let Some(error) = &completion.error {
        validate_tool_error("error", error)?;
    }
    validate_tool_metrics("metrics", &completion.metrics)?;
    validate_recovery_ref("recovery_ref", &completion.recovery_ref)?;
    if let Some(evidence_ref) = &completion.evidence_ref {
        validate_content_ref("evidence_ref", evidence_ref)?;
    }
    if let Some(evidence_fact_seq) = completion.evidence_fact_seq {
        validate_fact_seq("evidence_fact_seq", evidence_fact_seq)?;
    }
    if let Some(evidence_event_id) = &completion.evidence_event_id {
        validate_ulid("evidence_event_id", evidence_event_id.as_str())?;
    }
    validate_reconciled_evidence(
        completion.reconciled,
        completion.evidence_ref.as_ref(),
        completion.evidence_fact_seq,
        completion.evidence_event_id.as_ref(),
    )?;
    Ok(())
}

fn validate_reconciled_evidence(
    reconciled: bool,
    evidence_ref: Option<&ContentRef>,
    evidence_fact_seq: Option<u64>,
    evidence_event_id: Option<&super::types::EventId>,
) -> Result<(), ValidationError> {
    if evidence_fact_seq.is_some() != evidence_event_id.is_some() {
        return Err(ValidationError::InvalidField {
            field: "evidence_fact_seq",
            message: "evidence_fact_seq and evidence_event_id must be present together".to_owned(),
        });
    }
    if reconciled && evidence_ref.is_none() && evidence_fact_seq.is_none() {
        return Err(ValidationError::InvalidField {
            field: "reconciled",
            message: "true requires evidence_ref or evidence_fact_seq + evidence_event_id"
                .to_owned(),
        });
    }
    Ok(())
}

fn validate_recovery_ref(
    field: &'static str,
    recovery_ref: &RecoveryRef,
) -> Result<(), ValidationError> {
    validate_prefixed_ulid(field, recovery_ref.recovery_id.as_str(), "recovery_")?;
    validate_ulid("recovery_event_id", recovery_ref.recovery_event_id.as_str())?;
    validate_content_hash(
        "recovery_input_fingerprint",
        &recovery_ref.recovery_input_fingerprint,
    )
}

fn validate_metadata_patch(patch: &SessionMetadataPatch) -> Result<(), ValidationError> {
    let empty = patch.cwd.is_none()
        && patch.model.is_none()
        && patch.archived.is_none()
        && patch.search_visibility.is_none()
        && patch.parent_session_id.is_none()
        && patch.schema_caps.is_none();
    if empty {
        return Err(ValidationError::InvalidField {
            field: "patch",
            message: "at least one field must be present".to_owned(),
        });
    }
    if let Some(parent_session_id) = &patch.parent_session_id {
        validate_uuid_v7("patch.parent_session_id", parent_session_id.as_str())?;
    }
    Ok(())
}

fn validate_actor_ref(field: &'static str, actor: &ActorRef) -> Result<(), ValidationError> {
    validate_non_empty(&format!("{field}.id"), &actor.id)?;
    if let Some(display_name) = &actor.display_name {
        validate_byte_limit("actor.display_name", display_name, 64 * 1024)?;
    }
    Ok(())
}

fn validate_tool_metrics(
    field: &'static str,
    metrics: &ToolMetrics,
) -> Result<(), ValidationError> {
    if metrics.retry_count != 0 {
        return Err(ValidationError::InvalidField {
            field: "metrics.retry_count",
            message: "must be 0 in v2.0".to_owned(),
        });
    }
    if metrics.finished_at_ms < metrics.started_at_ms {
        return Err(ValidationError::InvalidField {
            field: "metrics.finished_at_ms",
            message: "must not precede started_at_ms".to_owned(),
        });
    }
    if field.is_empty() {
        return Err(ValidationError::InvalidField {
            field: "metrics",
            message: "missing metrics context".to_owned(),
        });
    }
    Ok(())
}

fn validate_tool_error(field: &'static str, error: &ToolError) -> Result<(), ValidationError> {
    validate_error_code(&format!("{field}.code"), &error.code)?;
    if let Some(details_ref) = &error.details_ref {
        validate_content_ref("details_ref", details_ref)?;
    }
    Ok(())
}

fn validate_turn_error(field: &'static str, error: &TurnError) -> Result<(), ValidationError> {
    validate_error_code(&format!("{field}.code"), &error.code)?;
    if let Some(details_ref) = &error.details_ref {
        validate_content_ref("details_ref", details_ref)?;
    }
    Ok(())
}

fn validate_error_code(field: &str, value: &str) -> Result<(), ValidationError> {
    validate_non_empty(field, value)?;
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(ValidationError::InvalidField {
            field: "error.code",
            message: "must be stable snake_case ASCII".to_owned(),
        });
    }
    Ok(())
}

fn validate_absolute_cwd(field: &'static str, value: &str) -> Result<(), ValidationError> {
    validate_non_empty(field, value)?;
    let is_unix_absolute = value.starts_with('/');
    let is_windows_drive_absolute = value.len() >= 3
        && value.as_bytes()[0].is_ascii_alphabetic()
        && value.as_bytes()[1] == b':'
        && matches!(value.as_bytes()[2], b'\\' | b'/');
    let is_windows_unc_absolute = value.starts_with(r"\\");
    if !is_unix_absolute && !is_windows_drive_absolute && !is_windows_unc_absolute {
        return Err(ValidationError::InvalidField {
            field,
            message: "must be an absolute path".to_owned(),
        });
    }
    Ok(())
}

fn validate_byte_limit(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), ValidationError> {
    if value.len() > max_bytes {
        return Err(ValidationError::InvalidField {
            field,
            message: format!("must be at most {max_bytes} UTF-8 bytes"),
        });
    }
    Ok(())
}

fn validate_non_empty(field: &str, value: &str) -> Result<(), ValidationError> {
    if value.is_empty() {
        return Err(ValidationError::InvalidField {
            field: "string",
            message: format!("{field} must not be empty"),
        });
    }
    Ok(())
}

fn validate_fact_seq(field: &'static str, fact_seq: u64) -> Result<(), ValidationError> {
    if fact_seq == 0 || fact_seq > MAX_SAFE_FACT_SEQ {
        return Err(ValidationError::InvalidFactSeq { fact_seq });
    }
    if field.is_empty() {
        return Err(ValidationError::InvalidField {
            field,
            message: "missing field name".to_owned(),
        });
    }
    Ok(())
}

fn validate_content_ref(field: &'static str, value: &ContentRef) -> Result<(), ValidationError> {
    validate_content_hash(field, &value.0)
}

fn validate_content_hash(field: &'static str, value: &ContentHash) -> Result<(), ValidationError> {
    let Some(hash) = value.as_str().strip_prefix("sha256:") else {
        return Err(ValidationError::InvalidId {
            field,
            value: value.0.clone(),
        });
    };
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ValidationError::InvalidId {
            field,
            value: value.0.clone(),
        });
    }
    Ok(())
}

fn validate_uuid_v7(field: &'static str, value: &str) -> Result<(), ValidationError> {
    let bytes = value.as_bytes();
    let valid = bytes.len() == 36
        && bytes[8] == b'-'
        && bytes[13] == b'-'
        && bytes[18] == b'-'
        && bytes[23] == b'-'
        && bytes[14] == b'7'
        && matches!(bytes[19], b'8' | b'9' | b'a' | b'b')
        && bytes.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()
            }
        });
    if !valid {
        return Err(ValidationError::InvalidId {
            field,
            value: value.to_owned(),
        });
    }
    Ok(())
}

fn validate_ulid(field: &'static str, value: &str) -> Result<(), ValidationError> {
    let valid = value.len() == 26
        && value.bytes().all(|byte| {
            matches!(
                byte,
                b'0'..=b'9'
                    | b'A'..=b'H'
                    | b'J'
                    | b'K'
                    | b'M'
                    | b'N'
                    | b'P'..=b'T'
                    | b'V'..=b'Z'
            )
        });
    if !valid {
        return Err(ValidationError::InvalidId {
            field,
            value: value.to_owned(),
        });
    }
    Ok(())
}

fn validate_prefixed_ulid(
    field: &'static str,
    value: &str,
    prefix: &str,
) -> Result<(), ValidationError> {
    let Some(ulid) = value.strip_prefix(prefix) else {
        return Err(ValidationError::InvalidId {
            field,
            value: value.to_owned(),
        });
    };
    validate_ulid(field, ulid)
}

fn require_turn_id(
    envelope: Option<&super::types::TurnId>,
    payload: &super::types::TurnId,
) -> Result<(), ValidationError> {
    if envelope != Some(payload) {
        return Err(ValidationError::EnvelopeMismatch { field: "turn_id" });
    }
    Ok(())
}

fn require_call_id(
    envelope: Option<&super::types::ToolCallId>,
    payload: &super::types::ToolCallId,
) -> Result<(), ValidationError> {
    if envelope != Some(payload) {
        return Err(ValidationError::EnvelopeMismatch { field: "call_id" });
    }
    Ok(())
}

fn require_interaction_id(
    envelope: Option<&super::types::InteractionId>,
    payload: &super::types::InteractionId,
) -> Result<(), ValidationError> {
    if envelope != Some(payload) {
        return Err(ValidationError::EnvelopeMismatch {
            field: "interaction_id",
        });
    }
    Ok(())
}
