//! DeepX Ringing V1 daemon client (HTTP/SSE).
//!
//! Shared transport for the TUI and desktop shells: discovery, lease
//! negotiation/renewal, three SSE event channels and the per-session timeline
//! stream, plus commands, queries, bootstrap and graceful stop.
//!
//! The public API uses the canonical `deepx-domain` and `deepx-ringing`
//! contracts. HTTP/SSE JSON is decoded at this boundary and never becomes a
//! renderer-facing compatibility protocol.

pub mod client;
pub mod discovery;
pub mod endpoint;
pub mod error;
pub mod session;
pub mod sse;
pub mod timeline;
pub mod types;

pub use client::{Client, ClientHandlers, ClientOptions, StopStatus, runtime_handle};
pub use discovery::{DaemonDiscovery, ensure_daemon_running, read_discovery};
pub use endpoint::{ActionRequest, QueryRequest};
pub use error::{ClientError, Result};
pub use session::{RingingSession, SessionState};
pub use timeline::TimelineStream;
pub use types::ResetRequired;
pub use types::{
    AskAnswer, Channel, ChannelStatus, CommandOptions, ContentRef, ControlCommand, ControlEvent,
    ConversationCommand, ConversationEvent, ConversationMode, DomainActivityState,
    DomainAskQuestion, DomainDashboardSnapshot, DomainSessionState, EventBatch, PermissionCategory,
    PermissionRisk, ProviderToolState, RingingCommand, RingingCommandAck, RingingCommandState,
    RingingCommandStatus, RingingEvent, RingingEventEnvelope, RoundDeltaKind, SkillInfo,
    SkillRuntimeInfo, TimelineBlockKind, TimelineEntry, TimelinePage, TimelineSnapshot,
    TimelineStatus, TimelineTool, TimelineToolState, TimelineTurnState, TodoItem, ToolCommand,
    ToolEvent,
};
