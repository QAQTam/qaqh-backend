mod openai;
mod anthropic;

pub use openai::chat_stream_openai;
pub use anthropic::chat_stream_anthropic;

use dsx_types::{Message, UsageInfo};

/// Minimal config needed by gateway (no process-level fields).
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub base_url: String,
    pub api_key: String,
}

/// Events emitted during API streaming.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    ContentDelta(String),
    ReasoningDelta(String),
    ToolCallProgress {
        name: String,
        args_so_far: String,
    },
    Done {
        raw_message: Message,
        usage: Option<UsageInfo>,
        stop_reason: Option<String>,
    },
    Error(String),
}
