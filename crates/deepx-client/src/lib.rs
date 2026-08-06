//! DeepX Ringing V1 daemon client (HTTP/SSE).
//!
//! Shared transport for the TUI and desktop shells: discovery, lease
//! negotiation/renewal, three SSE event channels and the per-session timeline
//! stream, plus commands, queries, bootstrap and graceful stop.
//!
//! Contract mirrors the Ringing V1 reference implementation (the original
//! Electron `apps/desktop/electron/controlClient.ts` / `ringingClient.ts`
//! were removed; this crate is now the single reference) and
//! `docs/backend-dataflow/protocol-anchor.md`.

pub mod client;
pub mod discovery;
pub mod error;
pub mod session;
pub mod sse;
pub mod timeline;
pub mod types;

pub use client::{runtime_handle, Client, ClientHandlers, ClientOptions, StopStatus};
pub use types::ResetRequired;
pub use discovery::{ensure_daemon_running, read_discovery, DaemonDiscovery};
pub use error::{ClientError, Result};
pub use session::{RingingSession, SessionState};
pub use timeline::TimelineStream;
pub use types::{
    Channel, ChannelStatus, EventBatch, RingingEventEnvelope, TimelineEntry, TimelineStatus,
};
