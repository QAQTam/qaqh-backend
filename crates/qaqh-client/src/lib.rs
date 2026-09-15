//! QAQ-Harness Ringing V1 daemon client (HTTP/SSE).
//!
//! Shared transport for the TUI and desktop shells: discovery, lease
//! negotiation/renewal, three SSE event channels and the per-session timeline
//! stream, plus commands, queries, bootstrap and graceful stop.
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
pub mod sse;
mod sse_decoder;
pub mod timeline;
pub mod types;

pub use client::{
    Client, ClientHandlers, ClientOptions, RemoteEndpoint, StopStatus, runtime_handle,
};
pub use discovery::{DaemonDiscovery, DiscoveryExt, ensure_daemon_running, read_discovery};
pub use endpoint::{ActionRequest, QueryRequest};
pub use error::{ClientError, Result};
pub use remote_path::{display_host, display_path, remote_path_from_display};
pub use session::{RingingSession, SessionState};
pub use timeline::TimelineStream;
pub use types::ResetRequired;
/// 三频道快照 `state` 的类型化视图（前端契约 G1）。三端共用，替代各自手解
/// `serde_json::Value`——经 [`RingingSessionBootstrap`] 的同名访问器取用。
pub use qaqh_domain::state::{
    ControlState, ConversationState, InteractionKind, LastFailure, LastRound, PendingInteraction,
    RunningTool, ToolState,
};
pub use types::{
    AgentLifecycleState, AskAnswer, AskMode, AskResolution, CLIENT_SESSION_HEADER, Channel,
    ChannelStatus, CommandOptions, CompactStatus, ContentRef, ControlCommand, ControlEvent,
    ConversationCommand, ConversationEvent, ConversationMode, DashboardDocument, DashboardTask,
    Delivery, DomainActivityState, DomainAskQuestion, DomainDashboardSnapshot, DomainError,
    DomainSessionState, ErrorScope, EventBatch, ImageBlock, MAX_SAFE_INTEGER, NoticeLevel,
    PermissionCategory, PermissionRisk, ProviderToolState, RINGING_SCHEMA, RINGING_VERSION,
    RingingChannelSnapshot, RingingCommand, RingingCommandAck, RingingCommandAckStatus,
    RingingCommandState, RingingCommandStatus, RingingEvent, RingingEventEnvelope,
    RingingSessionBootstrap, RoundDeltaKind, SkillInfo, SkillRuntimeInfo, SkillsStatus,
    TimelineBlock, TimelineBlockKind, TimelineBlockState, TimelineEntry, TimelineEvent,
    TimelineFailure, TimelinePage, TimelineRound, TimelineSnapshot, TimelineStatus, TimelineTool,
    TimelineToolPermission, TimelineToolState, TimelineTurn, TimelineTurnState, TodoItem,
    ToolCommand, ToolContinuation, ToolError, ToolEvent, ToolImage, ToolModelPayload, ToolResult,
    ToolStatus, UsageInfo, is_safe_integer,
};
