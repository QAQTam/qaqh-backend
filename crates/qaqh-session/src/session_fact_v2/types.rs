//! Canonical `session-fact/v2` types.
//!
//! This module is intentionally storage-free: it defines the durable wire
//! shape and the pure validation surface. Session-store and runtime wiring
//! live outside this module.

pub use qaqh_types::UsageInfo;
use serde::{Deserialize, Serialize};

pub const SESSION_FACT_SCHEMA: &str = "qaqh.session-fact/v2";
pub const SESSION_FACT_SCHEMA_NAME: &str = "qaqh.session-fact";
pub const SESSION_FACT_SCHEMA_VERSION: u16 = 2;
pub const SESSION_FACT_PAYLOAD_VERSION: u16 = 2;
pub const MAX_SAFE_FACT_SEQ: u64 = (1_u64 << 53) - 1;

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

string_id!(SessionId);
string_id!(LogId);
string_id!(EventId);
string_id!(InputId);
string_id!(TurnId);
string_id!(ToolCallId);
string_id!(ExecutionId);
string_id!(InteractionId);
string_id!(BlockId);
string_id!(CheckpointId);
string_id!(RecoveryId);
string_id!(ResourceId);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentHash(pub String);

impl ContentHash {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentRef(pub ContentHash);

impl ContentRef {
    pub fn new(hash: ContentHash) -> Self {
        Self(hash)
    }

    pub fn hash(&self) -> &ContentHash {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentUnavailableReason {
    GarbageCollected,
    Missing,
    HashMismatch,
    Offloaded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentUnavailable {
    pub content_ref: ContentRef,
    pub reason: ContentUnavailableReason,
    pub observed_at_logical_ms: i64,
    pub source_fact_seq: Option<u64>,
    pub gc_event_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactSchema {
    pub name: String,
    pub version: u16,
    pub payload_version: u16,
}

impl FactSchema {
    pub fn v2() -> Self {
        Self {
            name: SESSION_FACT_SCHEMA_NAME.to_owned(),
            version: SESSION_FACT_SCHEMA_VERSION,
            payload_version: SESSION_FACT_PAYLOAD_VERSION,
        }
    }
}

impl Default for FactSchema {
    fn default() -> Self {
        Self::v2()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFact {
    pub schema: FactSchema,
    pub session_id: SessionId,
    pub log_id: LogId,
    pub fact_seq: u64,
    pub event_id: EventId,
    pub ts_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<EventId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<ToolCallId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction_id: Option<InteractionId>,
    pub payload: FactPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum FactPayload {
    SessionCreated(SessionCreated),
    InputAccepted(InputAccepted),
    TurnStarted(TurnStarted),
    ModelRoundStarted(ModelRoundStarted),
    AssistantBlockSealed(AssistantBlockSealed),
    ToolCallDeclared(ToolCallDeclared),
    ToolIntent(ToolIntent),
    ToolFinished(ToolFinished),
    InteractionRequested(InteractionRequested),
    InteractionResolved(InteractionResolved),
    InteractionExpired(InteractionExpired),
    TurnFinished(TurnFinished),
    TurnInterrupted(TurnInterrupted),
    SessionRecovered(SessionRecovered),
    CompactionApplied(CompactionApplied),
    SessionMetadataChanged(SessionMetadataChanged),
    SessionTitleChanged(SessionTitleChanged),
    SessionDeleted(SessionDeleted),
    WorkspaceResourceChanged(WorkspaceResourceChanged),
    SubagentSpawned(SubagentSpawned),
    SubagentFinished(SubagentFinished),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCreated {
    pub created_at_ms: i64,
    pub cwd: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<SessionId>,
    pub schema_caps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputAccepted {
    pub input_id: InputId,
    pub input_kind: InputKind,
    pub input_purpose: InputPurpose,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_ref: Option<ContentRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_text: Option<String>,
    pub attachments: Vec<ContentRef>,
    pub actor: ActorRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnStarted {
    pub turn_id: TurnId,
    pub input_id: InputId,
    pub mode: TurnMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_ref: Option<RecoveryRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRoundStarted {
    pub turn_id: TurnId,
    pub round: u32,
    pub request_hash: ContentHash,
    pub context_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantBlockSealed {
    pub turn_id: TurnId,
    pub block_id: BlockId,
    pub kind: AssistantBlockKind,
    pub content_ref: ContentRef,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallDeclared {
    pub turn_id: TurnId,
    pub call_id: ToolCallId,
    pub tool_name: String,
    pub args_ref: ContentRef,
    pub args_hash: ContentHash,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolIntent {
    pub call_id: ToolCallId,
    pub execution_id: ExecutionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub replay_capability: ToolReplayCapability,
    pub policy_decision: PolicyDecisionRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_args_ref: Option<ContentRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_args_hash: Option<ContentHash>,
    pub sandbox_spec_hash: ContentHash,
    pub side_effect_class: SideEffectClass,
    pub intent_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolFinished {
    pub call_id: ToolCallId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<ExecutionId>,
    pub terminal_status: ToolTerminalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_ref: Option<ContentRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolError>,
    pub metrics: ToolMetrics,
    pub reconciled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_ref: Option<ContentRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_fact_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_event_id: Option<EventId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_ref: Option<RecoveryRef>,
    pub finished_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionRequested {
    pub interaction_id: InteractionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<ToolCallId>,
    pub turn_id: TurnId,
    pub kind: InteractionKind,
    pub request_ref: ContentRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
    pub requested_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionResolved {
    pub interaction_id: InteractionId,
    pub decision_ref: ContentRef,
    pub resolved_by: ActorRef,
    pub resolution_seq: u64,
    pub resolved_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionExpired {
    pub interaction_id: InteractionId,
    pub reason: InteractionExpiryReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_ref: Option<RecoveryRef>,
    pub expired_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnFinished {
    pub turn_id: TurnId,
    pub terminal: TurnTerminal,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<TurnError>,
    pub finished_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnInterrupted {
    pub turn_id: TurnId,
    pub reason: InterruptReason,
    pub last_fact_seq: u64,
    pub recovery_ref: RecoveryRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecovered {
    pub recovery_id: RecoveryId,
    pub recovery_event_id: EventId,
    pub recovery_input_fingerprint: ContentHash,
    pub outcome: RecoveryOutcome,
    pub last_good_fact_seq: u64,
    pub torn_tail: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub torn_bytes: Option<u64>,
    pub actions: Vec<RecoveryAction>,
    pub recovered_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionApplied {
    pub checkpoint_id: CheckpointId,
    pub replaces_through_fact_seq: u64,
    pub summary_ref: ContentRef,
    pub context_revision: u64,
    pub applied_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetadataChanged {
    pub patch: SessionMetadataPatch,
    pub source: MetadataSource,
    pub changed_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTitleChanged {
    pub title: String,
    pub source: TitleSource,
    pub changed_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDeleted {
    pub tombstone_at_ms: i64,
    pub reason: DeleteReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purge_after_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceResourceChanged {
    pub resource_kind: ResourceKind,
    pub resource_id: ResourceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_call_id: Option<ToolCallId>,
    pub revision: u64,
    pub summary_ref: ContentRef,
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentSpawned {
    pub child_session_id: SessionId,
    pub parent_call_id: ToolCallId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub spawned_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentFinished {
    pub child_session_id: SessionId,
    pub parent_call_id: ToolCallId,
    pub status: SubagentTerminalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_ref: Option<ContentRef>,
    pub finished_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_ref: Option<RecoveryRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryRef {
    pub recovery_id: RecoveryId,
    pub recovery_event_id: EventId,
    pub recovery_input_fingerprint: ContentHash,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetadataPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_visibility: Option<SearchVisibility>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_caps: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorRef {
    pub kind: ActorKind,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecisionRef {
    pub outcome: ToolIntentPolicyOutcome,
    pub rule_id: String,
    pub decided_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_ref: Option<ContentRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolMetrics {
    pub started_at_ms: i64,
    pub finished_at_ms: i64,
    pub retry_count: u32,
    pub output_bytes: u64,
    pub progress_bytes_total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details_ref: Option<ContentRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details_ref: Option<ContentRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryToolCompletion {
    pub call_id: ToolCallId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<ExecutionId>,
    pub terminal_status: ToolTerminalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_ref: Option<ContentRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolError>,
    pub metrics: ToolMetrics,
    pub reconciled: bool,
    pub recovery_ref: RecoveryRef,
    pub finished_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_ref: Option<ContentRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_fact_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_event_id: Option<EventId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecoveryAction {
    TurnInterrupted {
        turn_id: TurnId,
        last_fact_seq: u64,
    },
    TurnStarted {
        turn_id: TurnId,
        input_id: InputId,
        mode: TurnMode,
        recovery_ref: RecoveryRef,
    },
    ToolFinished {
        completion: RecoveryToolCompletion,
    },
    InteractionExpired {
        interaction_id: InteractionId,
        reason: InteractionExpiryReason,
    },
    SubagentFinished {
        child_session_id: SessionId,
        child_log_id: LogId,
        terminal_fact_seq: u64,
        terminal_event_id: EventId,
        parent_call_id: ToolCallId,
        status: SubagentTerminalStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result_ref: Option<ContentRef>,
        finished_at_ms: i64,
        recovery_ref: RecoveryRef,
    },
    UpgradeSuperseded {
        previous_recovery_id: RecoveryId,
    },
    CommitRepaired {
        previous_commit_generation: u64,
        committed_fact_seq: u64,
        committed_offset: u64,
    },
    MoveTornTail {
        from: String,
        to: String,
        bytes: u64,
        bytes_hash: ContentHash,
    },
    ProjectionRebuilt {
        projection: ProjectionSlot,
        through_fact_seq: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundedText {
    pub text: String,
    pub original_chars: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListOutput<T> {
    pub items: Vec<T>,
    pub total: u32,
    pub returned: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    User,
    Api,
    System,
    Agent,
    Subagent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    UserText,
    Command,
    Approval,
    Resume,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputPurpose {
    TriggerTurn,
    QueueOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnMode {
    Normal,
    Plan,
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistantBlockKind {
    Reasoning,
    Answer,
    ToolCall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolIntentPolicyOutcome {
    Allow,
    Ask,
    Amend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffectClass {
    ReadOnly,
    WorkspaceWrite,
    Process,
    Network,
    External,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityState {
    #[default]
    Idle,
    Running,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolReplayCapability {
    NoReplay,
    IdempotentReplay,
    Reconcile { probe_ref: ContentRef },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolTerminalStatus {
    Succeeded,
    Failed,
    Partial,
    Cancelled,
    TimedOut,
    Backgrounded,
    Indeterminate,
    Denied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionKind {
    Ask,
    Plan,
    Permission,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionExpiryReason {
    Timeout,
    SessionClosed,
    RestartPolicy,
    TurnCancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnTerminal {
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptReason {
    Crash,
    Restart,
    CancelBeforeSeal,
    UnknownFact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataSource {
    User,
    Api,
    System,
    Migration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleSource {
    User,
    Auto,
    Migration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeleteReason {
    User,
    Api,
    Retention,
    Rollback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Todo,
    Skill,
    Plan,
    Activity,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentTerminalStatus {
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryOutcome {
    Writable,
    CommitRecoveryRequired,
    ReadOnlyUpgradeRequired,
    Tombstone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchVisibility {
    Visible,
    Hidden,
}

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionSlot {
    Conversation = 0,
    Timeline = 1,
    Control = 2,
    Resources = 3,
    Meta = 4,
}
