// ── Type definitions for DSX core types ──
//
// All type definitions are split across sub-modules below.
// This file re-exports every public symbol so consumers can
// use `crate::types::TypeName` without caring about sub-module layout.

// ── Sub-module declarations (each file = one logical group) ──

pub mod message;
pub mod safety;
pub mod tool_def;
pub mod state;
pub mod config;
pub mod session;
pub mod anthropic;
pub mod api_types;


// Unified arg parsing (shared across dsx-agent, dsx-tools)
pub mod arg;

// Platform-specific utilities
pub mod platform;

// Crates contributed by A03 (shared utilities)
pub mod error;
pub mod serde;
pub mod token;

// ── Re-exports: flat public API ──

pub use message::{Message, ToolCall, FunctionCall};
pub use safety::SafetyLevel;
pub use tool_def::{ToolDef, ToolFunction};
pub use state::{TaskPhase, DebugLevel, RouterCommand};
pub use config::{PersistentConfig, PhasePerfConfig, ProfileConfig, UserPreferences, default_phase_configs};
pub use session::{SessionFile, SessionMeta, StreamState};
pub use anthropic::{
    AnthropicCacheControl, AnthropicSystemBlock,
    AnthropicContent, AnthropicMessage, AnthropicTool, AnthropicThinking, AnthropicRequest,
    AnthropicEventMessage, AnthropicContentBlockStart, AnthropicDelta, AnthropicMessageDelta,
    AnthropicUsage, AnthropicStreamEvent,
};
pub use api_types::{UsageInfo, TokenDetails, ModelInfo, ModelList, BalanceInfo, BalanceEntry};

// ── Unified arg parsers ──
pub use arg::{
    parse_arg, parse_arg_or, parse_opt, parse_opt_u64, tool_action, parse_file_arg, parse_cmd_arg,
};

// ── A03: shared utilities ──
pub use error::{DsxError, IpcError, ApiError, ToolError, HealthError};
pub use token::{
    TokenCount, TokenBreakdown,
    estimate_messages_tokens, count_tokens, format_tokens, context_usage_ratio,
};
pub use serde::{SerdeError, encode_msg, encode_msg_with_max, decode_msg, try_decode_msg, MAX_FRAME_SIZE};
