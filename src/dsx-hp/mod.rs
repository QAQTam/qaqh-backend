//! API layer: stream events, HTTP client, SSE parsing, format conversion.

mod client;
mod converter;

pub use client::{chat_stream, get_balance, list_models};
pub use converter::convert_messages;

use crate::types::{Message, UsageInfo};

/// Internal accumulator for building tool calls from streaming deltas
struct AccumulatedToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Events emitted during streaming + UI events
#[derive(Debug, Clone)]
pub enum StreamEvent {
    // Streaming events
    ContentDelta(String),
    ReasoningDelta(String),
    ToolCallProgress { name: String, args_so_far: String },
    Done { raw_message: Message, usage: Option<UsageInfo>, stop_reason: Option<String> },
    Error(String),
    // UI events (non-streaming)
    BalanceResult(String),
    #[allow(dead_code)]
    ModelListResult(Vec<crate::types::ModelInfo>),
    /// Result of background sudo execution: Vec<(tool_call_id, wrapped_result)>
    SudoDone(Vec<(String, String)>),
    /// Real-time line output from a long-running exec command
    ExecProgress(String),
    /// PID of a spawned executor — used for cancellation
    ExecStarted(String, u32),
    /// Final result of a long-running exec: (tool_call_id, wrapped_result)
    ExecDone(String, String),
}
