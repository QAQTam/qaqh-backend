//! Typed server-side projection events.
//!
//! These types are the boundary between canonical facts and wire adapters.
//! They intentionally contain no provider JSON or UI-only state.

use qaqh_domain::RingingChannel;
use qaqh_types::UsageInfo;
use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

use super::agent::AgentPath;
use super::projection::{END_OF_FACT, MAX_RELIABLE_PROJECTION_INDEX, ProjectionIndex};
use super::types::{
    ActivityState, ActorRef, AssistantBlockKind, CheckpointId, ContentHash, ContentRef,
    ContentUnavailable, DeleteReason, EventId, ExecutionId, InputId, InputKind, InputPurpose,
    InterAgentCommunication, InterAgentDelivery, InteractionDecision, InteractionExpiryReason,
    InteractionId, InteractionKind, InterruptReason, LogId, MAX_SAFE_FACT_SEQ, MessageId,
    PolicyDecisionRef, ProjectionSlot, RecoveryAction, RecoveryOutcome, RecoveryRef, ResourceId,
    ResourceKind, SessionFact, SessionId, SessionMetadataPatch, SideEffectClass,
    SubagentTerminalStatus, TitleSource, ToolCallId, ToolError, ToolMetrics, ToolReplayCapability,
    ToolTerminalStatus, TurnError, TurnId, TurnMode, TurnTerminal,
};
use super::validation::ValidationError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ReliableCursor {
    pub log_id: LogId,
    pub fact_seq: u64,
    /// `0..=65534` is a reliable projection; `65535` is reserved for snapshot
    /// `END_OF_FACT`.
    pub projection_index: ProjectionIndex,
}

impl ReliableCursor {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.fact_seq == 0 || self.fact_seq > MAX_SAFE_FACT_SEQ {
            return Err(ValidationError::InvalidFactSeq {
                fact_seq: self.fact_seq,
            });
        }
        if self.projection_index > MAX_RELIABLE_PROJECTION_INDEX {
            return Err(ValidationError::InvalidField {
                field: "projection_index",
                message: "reliable projection index must be <= 65534".into(),
            });
        }
        Ok(())
    }

    /// Snapshot cursors use `u16::MAX` as the `END_OF_FACT` sentinel.
    pub fn validate_snapshot_cursor(&self) -> Result<(), ValidationError> {
        if self.fact_seq == 0 || self.fact_seq > MAX_SAFE_FACT_SEQ {
            return Err(ValidationError::InvalidFactSeq {
                fact_seq: self.fact_seq,
            });
        }
        if self.projection_index != END_OF_FACT {
            return Err(ValidationError::InvalidField {
                field: "projection_index",
                message: "snapshot cursor projection_index must be END_OF_FACT".into(),
            });
        }
        Ok(())
    }

    /// Compare two cursors only when they belong to the same canonical log.
    pub fn is_after(&self, other: &Self) -> Result<bool, ValidationError> {
        self.validate()?;
        other.validate()?;
        if self.log_id != other.log_id {
            return Err(ValidationError::InvalidField {
                field: "log_id",
                message: "reliable cursors from different logs are not comparable".into(),
            });
        }
        Ok((self.fact_seq, self.projection_index) > (other.fact_seq, other.projection_index))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "ts",
    derive(TS),
    ts(export, export_to = "qaqh/", rename = "SessionDelivery")
)]
pub enum Delivery {
    Reliable { cursor: ReliableCursor },
    Replaceable { revision: u64 },
    Ephemeral,
}

impl Delivery {
    pub fn reliable_cursor(&self) -> Option<&ReliableCursor> {
        match self {
            Self::Reliable { cursor } => Some(cursor),
            Self::Replaceable { .. } | Self::Ephemeral => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum ResetReason {
    CursorExpired,
    LogIdMismatch,
    UnknownFact,
    UpgradeRequired,
    ReplayOverflow,
    EpochMismatch,
    CrossSession,
    SnapshotMissing,
    SnapshotExpired,
    SnapshotHashMismatch,
    StaleWriter,
    ContentQuotaExceeded,
    PerConnectionOverflow,
    ProgressBufferOverflow,
    ActorMailboxOverflow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ResetRequired {
    pub log_id: LogId,
    pub snapshot_cursor: Option<ReliableCursor>,
    pub reason: ResetReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum StreamKey {
    Channel(RingingChannel),
    Resource { kind: ResourceKind, id: ResourceId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ProjectionEvent {
    pub event_id: EventId,
    pub source_fact_seq: u64,
    pub source_event_id: EventId,
    /// Causal source copied from the canonical fact (for example a command id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<EventId>,
    /// Wall-clock commit time of the source fact (C3). Synthetic events with
    /// no source fact (ephemeral team overlays) carry `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_ms: Option<i64>,
    pub stream_key: StreamKey,
    pub delivery: Delivery,
    /// Reliable must be `Some` and match the delivery cursor's slot.
    pub projection_slot: Option<ProjectionSlot>,
    /// Reliable must be `Some` and equal the delivery cursor's index.
    pub projection_index: Option<ProjectionIndex>,
    pub payload: ProjectionPayload,
}

impl ProjectionEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.source_fact_seq == 0 || self.source_fact_seq > MAX_SAFE_FACT_SEQ {
            return Err(ValidationError::InvalidFactSeq {
                fact_seq: self.source_fact_seq,
            });
        }

        match &self.delivery {
            Delivery::Reliable { cursor } => {
                cursor.validate()?;
                if cursor.fact_seq != self.source_fact_seq {
                    return Err(ValidationError::EnvelopeMismatch {
                        field: "delivery.cursor.fact_seq",
                    });
                }
                let slot = self.projection_slot.ok_or(ValidationError::InvalidField {
                    field: "projection_slot",
                    message: "reliable projection event requires a projection slot".into(),
                })?;
                let index = self.projection_index.ok_or(ValidationError::InvalidField {
                    field: "projection_index",
                    message: "reliable projection event requires a projection index".into(),
                })?;
                if index != slot.as_u16() {
                    return Err(ValidationError::InvalidField {
                        field: "projection_index",
                        message: "projection index must equal the projection slot repr".into(),
                    });
                }
                let payload_slot =
                    self.payload
                        .reliable_slot()
                        .ok_or(ValidationError::InvalidField {
                            field: "payload",
                            message: "payload does not participate in the reliable cursor".into(),
                        })?;
                if slot != payload_slot {
                    return Err(ValidationError::InvalidField {
                        field: "projection_slot",
                        message: "projection slot must match the typed payload family".into(),
                    });
                }
                if cursor.projection_index != index {
                    return Err(ValidationError::EnvelopeMismatch {
                        field: "delivery.cursor.projection_index",
                    });
                }
            }
            Delivery::Replaceable { .. } | Delivery::Ephemeral => {
                if self.projection_slot.is_some() || self.projection_index.is_some() {
                    return Err(ValidationError::InvalidField {
                        field: "projection_index",
                        message: "replaceable and ephemeral events must not carry a canonical projection index".into(),
                    });
                }
            }
        }
        Ok(())
    }

    pub fn reliable(
        event_id: EventId,
        source_fact: &SessionFact,
        stream_key: StreamKey,
        projection_slot: ProjectionSlot,
        payload: ProjectionPayload,
    ) -> Result<Self, ValidationError> {
        let projection_index = projection_slot.as_u16();
        let event = Self {
            event_id,
            source_fact_seq: source_fact.fact_seq,
            source_event_id: source_fact.event_id.clone(),
            causation_id: source_fact.causation_id.clone(),
            ts_ms: Some(source_fact.ts_ms),
            stream_key,
            delivery: Delivery::Reliable {
                cursor: ReliableCursor {
                    log_id: source_fact.log_id.clone(),
                    fact_seq: source_fact.fact_seq,
                    projection_index,
                },
            },
            projection_slot: Some(projection_slot),
            projection_index: Some(projection_index),
            payload,
        };
        event.validate()?;
        Ok(event)
    }

    pub fn replaceable(
        event_id: EventId,
        source_fact: &SessionFact,
        stream_key: StreamKey,
        revision: u64,
        payload: ProjectionPayload,
    ) -> Result<Self, ValidationError> {
        let event = Self {
            event_id,
            source_fact_seq: source_fact.fact_seq,
            source_event_id: source_fact.event_id.clone(),
            causation_id: source_fact.causation_id.clone(),
            ts_ms: Some(source_fact.ts_ms),
            stream_key,
            delivery: Delivery::Replaceable { revision },
            projection_slot: None,
            projection_index: None,
            payload,
        };
        event.validate()?;
        Ok(event)
    }

    pub fn ephemeral(
        event_id: EventId,
        source_fact: &SessionFact,
        stream_key: StreamKey,
        payload: ProjectionPayload,
    ) -> Result<Self, ValidationError> {
        let event = Self {
            event_id,
            source_fact_seq: source_fact.fact_seq,
            source_event_id: source_fact.event_id.clone(),
            causation_id: source_fact.causation_id.clone(),
            ts_ms: Some(source_fact.ts_ms),
            stream_key,
            delivery: Delivery::Ephemeral,
            projection_slot: None,
            projection_index: None,
            payload,
        };
        event.validate()?;
        Ok(event)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum ProjectionPayload {
    ConversationDelta(ConversationDelta),
    TimelineDelta(TimelineDelta),
    ControlDelta(ControlDelta),
    ResourceDelta(ResourceDelta),
    MetaDelta(MetaDelta),
    MailboxDelta(MailboxDelta),
    TeamDelta(TeamDelta),
    AuditRef(AuditRef),
    Unknown(UnknownProjection),
}

impl ProjectionPayload {
    /// Projection revision carried by every reducer-facing delta.
    pub fn revision(&self) -> Option<u64> {
        match self {
            Self::AuditRef(_) | Self::Unknown(_) => None,
            _ => serde_json::to_value(self)
                .ok()?
                .get("data")?
                .get("data")?
                .get("revision")?
                .as_u64(),
        }
    }

    fn reliable_slot(&self) -> Option<ProjectionSlot> {
        match self {
            Self::ConversationDelta(_) => Some(ProjectionSlot::Conversation),
            Self::TimelineDelta(_) => Some(ProjectionSlot::Timeline),
            Self::ControlDelta(_) => Some(ProjectionSlot::Control),
            Self::ResourceDelta(_) => Some(ProjectionSlot::Resources),
            Self::MetaDelta(_) => Some(ProjectionSlot::Meta),
            Self::MailboxDelta(_) => Some(ProjectionSlot::Mailbox),
            Self::TeamDelta(_) => Some(ProjectionSlot::Team),
            Self::AuditRef(_) | Self::Unknown(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum ContentValue {
    Inline { text: String },
    Ref { content_ref: ContentRef },
    Unavailable(ContentUnavailable),
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxMessageState {
    Queued,
    Delivered,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxMessage {
    pub communication: InterAgentCommunication,
    pub fact_seq: u64,
    pub state: MailboxMessageState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_fact_seq: Option<u64>,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamAgentStatus {
    PendingInit,
    Running,
    WaitingUser,
    Interrupted,
    Completed,
    Errored,
    Shutdown,
    NotFound,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamAgentResidency {
    Loaded,
    Unloaded,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamAgentSnapshot {
    pub agent_id: SessionId,
    pub agent_path: AgentPath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub status: TeamAgentStatus,
    pub residency: TeamAgentResidency,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_path: Option<AgentPath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_task_id: Option<String>,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamInboxSummary {
    pub message_id: MessageId,
    pub author: AgentPath,
    pub recipient: AgentPath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub delivery: InterAgentDelivery,
    pub created_at_ms: i64,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamTaskArtifact {
    pub content_ref: ContentRef,
    pub media_type: String,
    pub added_at_ms: i64,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamTaskSnapshot {
    pub task_id: String,
    pub title: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<AgentPath>,
    pub claim_epoch: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<TeamTaskArtifact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_ref: Option<ContentRef>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamBoardChannel {
    pub channel_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub created_by: AgentPath,
    pub created_at_ms: i64,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamBoardThread {
    pub thread_id: String,
    pub channel_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub created_by: AgentPath,
    pub created_at_ms: i64,
    pub post_count: u64,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamBoardPost {
    pub post_id: String,
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub author: AgentPath,
    pub body: String,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TeamBoardSubscriptionTarget {
    Channel { channel_id: String },
    Thread { thread_id: String },
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamBoardSubscription {
    pub target: TeamBoardSubscriptionTarget,
    pub subscriber: AgentPath,
    pub subscribed: bool,
    pub updated_at_ms: i64,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamBoardSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board_id: Option<SessionId>,
    pub revision: u64,
    pub last_fact_seq: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<TeamBoardChannel>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub threads: Vec<TeamBoardThread>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub posts: Vec<TeamBoardPost>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscriptions: Vec<TeamBoardSubscription>,
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum TeamDelta {
    AgentJoined {
        revision: u64,
        agent: Box<TeamAgentSnapshot>,
    },
    AgentStatusChanged {
        revision: u64,
        agent_id: SessionId,
        status: TeamAgentStatus,
    },
    AgentResidencyChanged {
        revision: u64,
        agent_id: SessionId,
        residency: TeamAgentResidency,
    },
    AgentMessageQueued {
        revision: u64,
        message: Box<TeamInboxSummary>,
    },
    AgentMessageDelivered {
        revision: u64,
        message_id: MessageId,
    },
    AgentInterrupted {
        revision: u64,
        agent_id: SessionId,
    },
    AgentCompleted {
        revision: u64,
        agent_id: SessionId,
        status: TeamAgentStatus,
    },
    TaskChanged {
        revision: u64,
        task: Box<TeamTaskSnapshot>,
    },
    BoardChanged {
        revision: u64,
        board: Box<TeamBoardSnapshot>,
    },
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum MailboxDelta {
    Queued {
        revision: u64,
        message: Box<MailboxMessage>,
    },
    Delivered {
        revision: u64,
        message_id: MessageId,
        delivered_fact_seq: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum ConversationDelta {
    InputAccepted {
        revision: u64,
        input_id: InputId,
        input_kind: InputKind,
        input_purpose: InputPurpose,
        content: ContentValue,
        attachments: Vec<ContentRef>,
        actor: ActorRef,
    },
    TurnStarted {
        revision: u64,
        turn_id: TurnId,
        input_id: InputId,
        mode: TurnMode,
    },
    AssistantBlockSealed {
        revision: u64,
        turn_id: TurnId,
        block_id: super::types::BlockId,
        block_kind: AssistantBlockKind,
        content: ContentValue,
        model: String,
        usage: Option<UsageInfo>,
    },
    ToolCallDeclared {
        revision: u64,
        turn_id: TurnId,
        call_id: ToolCallId,
        tool_name: String,
        args: ContentValue,
        args_hash: ContentHash,
    },
    ToolFinished {
        revision: u64,
        call_id: ToolCallId,
        terminal_status: ToolTerminalStatus,
        output: Option<ContentValue>,
        error: Option<ToolError>,
        metrics: ToolMetrics,
        reconciled: bool,
    },
    TurnFinished {
        revision: u64,
        turn_id: TurnId,
        terminal: TurnTerminal,
        usage: Option<UsageInfo>,
        error: Option<TurnError>,
    },
    TurnInterrupted {
        revision: u64,
        turn_id: TurnId,
        reason: InterruptReason,
        last_fact_seq: u64,
        recovery_ref: RecoveryRef,
    },
    CompactionApplied {
        revision: u64,
        checkpoint_id: CheckpointId,
        replaces_through_fact_seq: u64,
        summary: ContentValue,
        context_revision: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum TimelineDelta {
    Input {
        revision: u64,
        input_id: InputId,
        content: ContentValue,
    },
    Block {
        revision: u64,
        block_id: super::types::BlockId,
        block_kind: AssistantBlockKind,
        content: ContentValue,
    },
    ToolCall {
        revision: u64,
        call_id: ToolCallId,
        tool_name: String,
        args: ContentValue,
    },
    ToolResult {
        revision: u64,
        call_id: ToolCallId,
        terminal_status: ToolTerminalStatus,
        output: Option<ContentValue>,
        error: Option<ToolError>,
    },
    TurnFinished {
        revision: u64,
        turn_id: TurnId,
        terminal: TurnTerminal,
    },
    TurnInterrupted {
        revision: u64,
        turn_id: TurnId,
        reason: InterruptReason,
    },
    Compaction {
        revision: u64,
        checkpoint_id: CheckpointId,
        summary: ContentValue,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum ControlDelta {
    SessionCreated {
        revision: u64,
        session_id: SessionId,
        cwd: String,
        model: String,
        schema_caps: Vec<String>,
    },
    Round {
        revision: u64,
        turn_id: TurnId,
        round: u32,
        request_hash: ContentHash,
        context_revision: u64,
    },
    Activity {
        revision: u64,
        turn_id: Option<TurnId>,
        call_id: Option<ToolCallId>,
        state: ActivityState,
    },
    ToolIntent {
        revision: u64,
        call_id: ToolCallId,
        execution_id: ExecutionId,
        policy_decision: PolicyDecisionRef,
        replay_capability: ToolReplayCapability,
        side_effect_class: SideEffectClass,
        intent_at_ms: i64,
    },
    ToolFinished {
        revision: u64,
        call_id: ToolCallId,
        execution_id: Option<ExecutionId>,
        terminal_status: ToolTerminalStatus,
        output: Option<ContentValue>,
        error: Option<ToolError>,
        metrics: ToolMetrics,
        reconciled: bool,
    },
    InteractionRequested {
        revision: u64,
        interaction_id: InteractionId,
        call_id: Option<ToolCallId>,
        kind: InteractionKind,
        request: ContentValue,
        expires_at_ms: Option<i64>,
    },
    InteractionResolved {
        revision: u64,
        interaction_id: InteractionId,
        decision: ContentValue,
        /// Structured verdict; `None` for legacy facts.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verdict: Option<InteractionDecision>,
        resolved_by: ActorRef,
        resolution_seq: u64,
    },
    InteractionExpired {
        revision: u64,
        interaction_id: InteractionId,
        reason: InteractionExpiryReason,
    },
    /// Driver seat handover (`holder = None` = released).
    DriverChanged {
        revision: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        holder: Option<String>,
        driver_epoch: u64,
    },
    SessionRecovered {
        revision: u64,
        outcome: RecoveryOutcome,
        recovery_ref: RecoveryRef,
        actions: Vec<RecoveryAction>,
    },
    SubagentSpawned {
        revision: u64,
        child_session_id: SessionId,
        parent_call_id: ToolCallId,
        role: Option<String>,
    },
    SubagentFinished {
        revision: u64,
        child_session_id: SessionId,
        parent_call_id: ToolCallId,
        status: SubagentTerminalStatus,
        result: Option<ContentValue>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum ResourceDelta {
    WorkspaceResourceChanged {
        revision: u64,
        resource_kind: ResourceKind,
        resource_id: ResourceId,
        source_call_id: Option<ToolCallId>,
        summary: ContentValue,
        deleted: bool,
    },
    GraphEdge {
        revision: u64,
        child_session_id: SessionId,
        parent_call_id: ToolCallId,
        status: Option<SubagentTerminalStatus>,
    },
}

#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum MetaDelta {
    Created {
        revision: u64,
        cwd: String,
        model: String,
        parent_session_id: Option<SessionId>,
        schema_caps: Vec<String>,
    },
    MetadataChanged {
        revision: u64,
        patch: SessionMetadataPatch,
    },
    TitleChanged {
        revision: u64,
        title: String,
        source: TitleSource,
    },
    Deleted {
        revision: u64,
        tombstone_at_ms: i64,
        reason: DeleteReason,
        purge_after_ms: Option<i64>,
    },
    ContextRevision {
        revision: u64,
        checkpoint_id: CheckpointId,
        context_revision: u64,
    },
    Recovered {
        revision: u64,
        outcome: RecoveryOutcome,
        recovery_ref: RecoveryRef,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct AuditRef {
    pub audit_seq: u64,
    pub audit_hash: ContentHash,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct UnknownProjection {
    pub raw_ref: ContentRef,
}
