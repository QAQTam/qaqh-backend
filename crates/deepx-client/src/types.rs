//! Ringing V1 wire types (M4 slim envelopes).
//!
//! Contract source: `docs/backend-dataflow/protocol-anchor.md` and the
//! Ringing V1 client in `crates/deepx-client` (the original Electron
//! reference implementation `apps/desktop/electron/` was removed).

use serde::{Deserialize, Serialize};

/// Ringing event channels. Three independent SSE streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    Control,
    Conversation,
    Tool,
}

impl Channel {
    pub const ALL: [Channel; 3] = [Channel::Control, Channel::Conversation, Channel::Tool];

    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Control => "control",
            Channel::Conversation => "conversation",
            Channel::Tool => "tool",
        }
    }
}

/// Per-channel SSE connection state (mirrors `ChannelStatus` in TS).
#[derive(Debug, Clone)]
pub enum ChannelStatus {
    Connecting,
    Open { server_epoch: String, cursor: u64 },
    Reconnecting { retry_ms: u64, last_cursor: u64 },
    Closed { reason: String },
}

/// Single Ringing event envelope (M4: schema/version/channel/server_epoch removed).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RingingEventEnvelope {
    pub seed: String,
    pub event_id: String,
    pub stream_seq: u64,
    pub channel_seq: u64,
    pub session_seq: u64,
    #[serde(default)]
    pub state_revision: Option<u64>,
    pub event: serde_json::Value,
}

/// Batch delivered to the shell: a single envelope wrapped in transport context.
/// Kept structurally compatible with the TS `RingingEventBatch`.
#[derive(Debug, Clone)]
pub struct EventBatch {
    pub channel: Channel,
    pub seed: String,
    pub server_epoch: String,
    pub from_stream_seq: u64,
    pub to_stream_seq: u64,
    pub envelopes: Vec<RingingEventEnvelope>,
}

/// Payload of the special `ringing.reset_required` SSE event.
#[derive(Debug, Clone, Deserialize)]
pub struct ResetRequired {
    pub channel: String,
    /// Session that needs a fresh snapshot (mirrors TS `RingingResetRequired`).
    pub seed: String,
    /// Earliest stream_seq the server can still replay for seed+channel.
    pub earliest_available_seq: u64,
    pub reason: String,
}

/// One entry from the per-session timeline stream.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TimelineEntry {
    pub timeline_seq: u64,
    #[serde(flatten)]
    pub kind: serde_json::Value,
}

/// Per-session timeline connection state (mirrors `TimelineStatus` in TS).
#[derive(Debug, Clone)]
pub enum TimelineStatus {
    Connecting { seed: String },
    Open { seed: String, server_epoch: String, cursor: u64 },
    Reconnecting { seed: String, retry_ms: u64, cursor: u64 },
    Closed { seed: String, reason: String },
}

/// Ringing V1 timeline SSE frame — validated before dispatch.
#[derive(Debug, Clone, Deserialize)]
pub struct TimelineSseFrame {
    pub schema: String,
    pub version: u64,
    pub server_epoch: String,
    pub seed: String,
    pub entry: TimelineEntry,
}

/// Parsed SSE frame (a block of `key: value` lines separated by blank lines).
#[derive(Debug, Clone, Default)]
pub struct SseFrame {
    pub id: String,
    pub event_type: String,
    pub data: String,
}

/// Parse one SSE frame. Comment lines (`: keepalive`) are skipped; `data:`
/// lines accumulate with a single trailing newline per line (trimmed by caller).
pub fn parse_sse_frame(frame: &str) -> SseFrame {
    let mut parsed = SseFrame::default();
    for line in frame.split('\n') {
        if let Some(rest) = line.strip_prefix(':') {
            let _ = rest; // comment / keepalive
            continue;
        }
        if let Some(id) = line.strip_prefix("id:") {
            parsed.id = id.trim().to_string();
        } else if let Some(event) = line.strip_prefix("event:") {
            parsed.event_type = event.trim().to_string();
        } else if let Some(data) = line.strip_prefix("data:") {
            if !parsed.data.is_empty() {
                parsed.data.push('\n');
            }
            parsed.data.push_str(data.trim());
        }
    }
    parsed
}

/// Extract the stream sequence from an SSE `id: <epoch>:<channel>:<seq>`.
/// Returns `None` when the id does not match the given channel or the seq is invalid.
pub fn cursor_from_sse_id(id: &str, channel: Channel) -> Option<u64> {
    let mut parts = id.split(':');
    let epoch = parts.next()?;
    let chan = parts.next()?;
    let seq = parts.next()?;
    if epoch.is_empty() || chan != channel.as_str() || parts.next().is_some() {
        return None;
    }
    seq.parse::<u64>().ok()
}

/// Validate an M4 envelope shape. Returns `Ok(())` when the envelope can be
/// accepted; errors are surfaced as protocol violations.
pub fn validate_envelope(envelope: &RingingEventEnvelope, channel: Channel) -> Result<(), String> {
    if envelope.seed.is_empty() {
        return Err("envelope seed is empty".into());
    }
    if envelope.event_id.is_empty() {
        return Err("envelope event_id is empty".into());
    }
    let event_channel = envelope
        .event
        .get("channel")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !event_channel.is_empty() && event_channel != channel.as_str() {
        return Err(format!(
            "envelope channel {event_channel:?} != connection channel {:?}",
            channel.as_str()
        ));
    }
    Ok(())
}

/// `{ schema, version, channel, seed, server_epoch, from_stream_seq, to_stream_seq, envelopes }`
/// — the batch shape forwarded to renderers. Built from a validated envelope.
pub fn envelope_to_batch(
    channel: Channel,
    envelope: RingingEventEnvelope,
    server_epoch: String,
) -> EventBatch {
    let seq = envelope.stream_seq;
    EventBatch {
        channel,
        seed: envelope.seed.clone(),
        server_epoch,
        from_stream_seq: seq,
        to_stream_seq: seq,
        envelopes: vec![envelope],
    }
}

/// Negotiation request body for `POST /ringing/v1/clients/open`.
#[derive(Debug, Serialize)]
pub struct OpenRequest {
    pub schema: &'static str,
    pub version: u32,
    pub client_instance_id: String,
    pub capabilities: Vec<&'static str>,
}

/// Negotiation response from the daemon.
#[derive(Debug, Deserialize)]
pub struct OpenResponse {
    pub accepted: bool,
    pub client_session_id: String,
    pub server_epoch: String,
    pub lease_ttl_ms: u64,
    pub renew_interval_ms: u64,
}

/// Command envelope for `POST /ringing/v1/commands/{channel}`.
#[derive(Debug, Serialize)]
pub struct CommandRequest {
    pub schema: &'static str,
    pub version: u32,
    pub channel: &'static str,
    pub command_id: String,
    pub client_instance_id: String,
    pub client_session_id: String,
    pub seed: Option<String>,
    pub expected_revision: Option<u64>,
    pub command: serde_json::Value,
}

/// Command receipt queried via `GET /ringing/v1/commands/{id}`.
#[derive(Debug, Deserialize)]
pub struct CommandReceipt {
    pub state: String,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub command_id: Option<String>,
}

/// Uploaded attachment reference (`POST /ringing/v1/content` response).
#[derive(Debug, Clone, Deserialize)]
pub struct ContentRef {
    pub content_id: String,
    pub media_type: String,
    pub sha256: String,
    pub truncated: bool,
}
