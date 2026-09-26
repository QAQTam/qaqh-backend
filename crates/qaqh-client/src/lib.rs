//! QAQ-Harness Ringing V1/V2 daemon client (HTTP/SSE).
//!
//! Shared transport for the TUI and desktop shells: discovery, lease
//! negotiation/renewal, three SSE event channels and the per-session timeline
//! stream, plus commands, queries, bootstrap and graceful stop. The additive
//! [`v2`] surface exposes canonical cursors, typed projection events,
//! bootstrap/reset and driver commands for the beta cutover.
//!
//! The public API uses the canonical `qaqh-domain` and `qaqh-ringing`
//! contracts. HTTP/SSE JSON is decoded at this boundary and never becomes a
//! renderer-facing compatibility protocol.

pub mod client;
pub mod discovery;
pub mod endpoint;
pub mod error;
pub mod remote_path;
pub mod session;
mod sse_decoder;
pub mod timeline;
pub mod types;
pub mod v2;
pub mod v2_stream;

pub use client::{
    Client, ClientHandlers, ClientOptions, RemoteEndpoint, StopStatus, runtime_handle,
};
pub use discovery::{DaemonDiscovery, DiscoveryExt, ensure_daemon_running, read_discovery};
pub use endpoint::{ActionRequest, QueryRequest};
pub use error::{ClientError, Result};
/// 领域状态类型（`qaqh_domain::state`）。
///
/// 纯 v2 之后**不再**经 v1 bootstrap 的 `state` 访问器取用：v2 bootstrap 直接返回
/// typed 三频道快照（[`ClientV2Bootstrap`]）。这里保留导出只因为 v1 领域投影仍在
/// runtime 内部（orphan_seal / timeline）使用，壳层不应再依赖。
pub use qaqh_domain::state::{
    ControlState, ConversationState, InteractionKind, LastFailure, LastRound, PendingInteraction,
    RunningTool, ToolState,
};
pub use remote_path::{display_host, display_path, remote_path_from_display};
pub use session::{RingingSession, SessionState};
pub use timeline::TimelineStream;
pub use types::{
    AgentLifecycleState, AskAnswer, AskMode, AskResolution, CLIENT_SESSION_HEADER, Channel,
    CommandOptions, CompactStatus, ContentRef, ControlCommand, ControlEvent, ConversationCommand,
    ConversationEvent, ConversationInputPurpose, ConversationMode, DashboardDocument,
    DashboardTask, Delivery, DomainActivityState, DomainAskQuestion, DomainDashboardSnapshot,
    DomainError, DomainSessionState, ErrorScope, EventBatch, ImageBlock, MAX_SAFE_INTEGER,
    NoticeLevel, PermissionCategory, PermissionRisk, PlanReviewItem, ProviderToolState,
    RINGING_SCHEMA, RINGING_VERSION, ReconnectReason, RingingChannelSnapshot, RingingCommand,
    RingingCommandAck, RingingCommandAckStatus, RingingCommandState, RingingCommandStatus,
    RingingEvent, RingingEventEnvelope, RoundDeltaKind, SessionActivity, SessionListEntry,
    SessionMeta, SkillInfo, SkillRuntimeInfo, SkillsStatus, TimelineBlock, TimelineBlockKind,
    TimelineBlockState, TimelineEntry, TimelineEvent, TimelineFailure, TimelinePage,
    TimelinePathOp, TimelineRound, TimelineSnapshot, TimelineStatus, TimelineTool,
    TimelineToolBody, TimelineToolDisplay, TimelineToolHeader, TimelineToolMetrics,
    TimelineToolPermission, TimelineToolState, TimelineTurn, TimelineTurnState, ToolCommand,
    ToolContinuation, ToolError, ToolEvent, ToolImage, ToolModelPayload, ToolResult, ToolStatus,
    UsageInfo, is_safe_integer,
};
pub use v2::{
    CLIENT_V2_END_OF_FACT, ClientV2ActivityState, ClientV2ActorRef, ClientV2AskOutcome,
    ClientV2Bootstrap, ClientV2Capabilities, ClientV2CommandAck, ClientV2CommandResult,
    ClientV2CommandStatus, ClientV2ContentValue, ClientV2ControlDelta, ClientV2ControlState,
    ClientV2ConversationContextKind, ClientV2ConversationDelta, ClientV2ConversationState,
    ClientV2Cursor, ClientV2CursorToken, ClientV2DeleteReason, ClientV2Delivery,
    ClientV2DeltaInteractionKind, ClientV2DriverState, ClientV2Event, ClientV2EventEnvelope,
    ClientV2ExistingResult, ClientV2InteractionDecision, ClientV2InteractionExpiryReason,
    ClientV2InteractionId, ClientV2InteractionKind, ClientV2MetaDelta, ClientV2Payload,
    ClientV2PendingContentValue, ClientV2PendingInteraction, ClientV2PendingSet, ClientV2Reset,
    ClientV2ResetReason, ClientV2ResourceDelta, ClientV2SessionState, ClientV2StreamKey,
    ClientV2SubagentTerminalStatus, ClientV2Subscription, ClientV2SubscriptionEvent,
    ClientV2TaskBoardSnapshot, ClientV2TeamAgentResidency, ClientV2TeamAgentSnapshot,
    ClientV2TeamAgentStatus, ClientV2TeamBoardSnapshot, ClientV2TeamDelivery, ClientV2TeamDelta,
    ClientV2TeamInboxSummary, ClientV2TeamResponse, ClientV2TeamSnapshot, ClientV2TeamTaskSnapshot,
    ClientV2ToolCallId, ClientV2ToolState, ClientV2TurnId, ClientV2TurnTerminal,
    RINGING_V2_BASE_PATH, RINGING_V2_VERSION,
};
pub use v2_stream::{V2Stream, V2StreamHandlers, V2StreamStatus};
