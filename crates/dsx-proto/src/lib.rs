//! DSX IPC message protocol — shared frame definitions for all processes.
//!
//! Every frame is a single-line JSON object (`\n` delimited), tagged with `"type"`.
//!
//! ## Channels
//!
//! | Channel | Transport | Direction |
//! |---------|-----------|-----------|
//! | TUI ↔ Agent | stdin/stdout pipes | Bidirectional |
//! | Agent ↔ Tools | stdin/stdout pipes | Bidirectional |
//! | Agent → HP | TCP localhost | Bidirectional |
//!
//! ## Frame categories
//!
//! - `TuiToAgent` / `AgentToTui` — TUI ↔ Agent JSON-LP over pipes
//! - `AgentToTools` / `ToolsToAgent` — Agent ↔ Tools JSON-LP over pipes
//! - `AgentToHp` / `HpToAgent` — Agent ↔ HP JSON-LP over TCP

use serde::{Deserialize, Serialize};

// ── TUI ↔ Agent ────────────────────────────────────────────────────────

/// TUI → Agent frames (stdin pipe).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum TuiToAgent {
    #[serde(rename = "user_input")]
    UserInput { text: String },

    #[serde(rename = "tool_call")]
    ToolCall {
        id: String,
        name: String,
        action: String,
        args: serde_json::Value,
    },

    #[serde(rename = "set_phase")]
    SetPhase { phase: String },

    #[serde(rename = "tool_confirm")]
    ToolConfirm { id: String, approved: bool },

    #[serde(rename = "cancel")]
    Cancel,

    #[serde(rename = "shutdown")]
    Shutdown,
}

/// Agent → TUI frames (stdout pipe).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum AgentToTui {
    /// Streaming content delta (one token or small chunk).
    #[serde(rename = "content_delta")]
    ContentDelta {
        delta: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
    },

    /// Streaming tool progress.
    #[serde(rename = "tool_progress")]
    ToolProgress {
        id: String,
        content: String,
        stream_type: String,
    },

    /// Full API response (non-streaming fallback or final).
    #[serde(rename = "api_response")]
    ApiResponse {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<dsx_types::ToolCall>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<dsx_types::UsageInfo>,
    },

    #[serde(rename = "ask_user")]
    AskUser {
        id: String,
        question: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        options: Option<Vec<String>>,
    },

    #[serde(rename = "phase_changed")]
    PhaseChanged { phase: String },

    #[serde(rename = "tool_state")]
    ToolState {
        explored: bool,
        declared_files: Vec<String>,
        read_files: Vec<String>,
        written_this_turn: Vec<String>,
    },

    #[serde(rename = "status")]
    Status { message: String },

    /// Echo back a raw frame (passthrough mode, no HP).
    #[serde(rename = "echo")]
    Echo { data: serde_json::Value },

    /// End of a turn (agent ready for next input).
    #[serde(rename = "done")]
    Done,

    /// Error during processing.
    #[serde(rename = "error")]
    Error { message: String },

    /// Predicted KV cache hit rate (client-side estimate).
    #[serde(rename = "cache_prediction")]
    CachePrediction { hit_rate: f64 },

    /// Shutdown acknowledgement.
    #[serde(rename = "shutdown_ack")]
    ShutdownAck,
}

// ── Agent ↔ Tools ──────────────────────────────────────────────────────

/// Agent → Tools frames (stdin pipe).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum AgentToTools {
    #[serde(rename = "tools_init")]
    Init {
        allowed_tools: Vec<String>,
        session_seed: String,
        auto_mode: bool,
    },

    #[serde(rename = "tool_call_req")]
    CallReq {
        id: String,
        name: String,
        action: String,
        args: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
    },

    #[serde(rename = "tool_cancel")]
    Cancel {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },

    #[serde(rename = "tool_confirm_resp")]
    ConfirmResp {
        id: String,
        approved: bool,
    },

    #[serde(rename = "tools_shutdown")]
    Shutdown,
}

/// Tools → Agent frames (stdout pipe).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum ToolsToAgent {
    #[serde(rename = "tools_ready")]
    Ready {
        tools: Vec<dsx_types::ToolDef>,
    },

    #[serde(rename = "tool_progress")]
    Progress {
        id: String,
        content: String,
        stream_type: String,
    },

    /// Legacy text result (backward compatible).
    #[serde(rename = "tool_result")]
    Result {
        id: String,
        success: bool,
        content: String,
    },

    /// Structured result with is_error flag.
    #[serde(rename = "tool_result_message")]
    ToolResultMessage {
        id: String,
        name: String,
        action: String,
        success: bool,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },

    #[serde(rename = "tool_error")]
    ToolError {
        id: String,
        error: String,
        /// "UNKNOWN_TOOL" | "BLOCKED" | "TIMEOUT" | "PANIC" | "FORBIDDEN"
        code: String,
    },

    #[serde(rename = "tool_confirm_req")]
    ToolConfirmReq {
        id: String,
        tool_name: String,
        action: String,
        danger_level: String,
        prompt: String,
    },
}

// ── Agent ↔ HP ─────────────────────────────────────────────────────────

/// Agent → HP frames (TCP).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum AgentToHp {
    #[serde(rename = "register")]
    Register {
        kind: String,
        name: String,
        pid: u32,
    },

    #[serde(rename = "heartbeat")]
    Heartbeat { pid: u32 },

    #[serde(rename = "unregister")]
    Unregister { pid: u32 },

    #[serde(rename = "judge")]
    Judge,

    #[serde(rename = "query")]
    Query { pid: u32 },

    #[serde(rename = "list")]
    List,

    /// API chat request — forwarded to LLM provider.
    #[serde(rename = "api_chat")]
    ApiChat {
        model: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        system: Option<String>,
        messages: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_tokens: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tools: Option<serde_json::Value>,
    },
}

/// HP → Agent frames (TCP).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum HpToAgent {
    #[serde(rename = "ok")]
    Ok { message: String },

    #[serde(rename = "error")]
    Error { message: String },

    #[serde(rename = "verdicts")]
    Verdicts { data: serde_json::Value },

    #[serde(rename = "health")]
    Health { data: serde_json::Value },

    #[serde(rename = "process_list")]
    ProcessList { data: serde_json::Value },

    /// Streaming content delta from LLM.
    #[serde(rename = "content_delta")]
    ContentDelta {
        delta: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
    },

    /// Streaming tool call progress.
    #[serde(rename = "tool_progress")]
    ToolProgress {
        #[serde(default)]
        id: String,
        content: String,
        #[serde(default = "default_stream_type")]
        stream_type: String,
    },

    /// Final API response from LLM provider.
    #[serde(rename = "api_response")]
    ApiResponse {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        thinking_signature: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<dsx_types::UsageInfo>,
    },
}

fn default_stream_type() -> String { "progress".into() }

// ── Frame I/O helpers ──────────────────────────────────────────────────

use std::io::{self, BufRead, Write};

/// Read one JSON-LP line and deserialize into `T`.
pub fn read_frame<T: for<'de> Deserialize<'de>>(reader: &mut impl BufRead) -> io::Result<Option<T>> {
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    serde_json::from_str::<T>(trimmed)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Serialize `frame` and write as one JSON-LP line (append `\n` + flush).
pub fn write_frame(writer: &mut impl Write, frame: &impl Serialize) -> io::Result<()> {
    let json = serde_json::to_string(frame)?;
    writeln!(writer, "{}", json)?;
    writer.flush()?;
    Ok(())
}

/// Convenience: read a raw string line (unparsed JSON).
pub fn read_line(reader: &mut impl BufRead) -> Option<String> {
    let mut line = String::new();
    reader.read_line(&mut line).ok().filter(|n| *n > 0).map(|_| line.trim().to_string())
}

/// Convenience: write a raw string as a JSON-LP line.
pub fn write_line(writer: &mut impl Write, line: &str) {
    let _ = writeln!(writer, "{line}");
    let _ = writer.flush();
}
