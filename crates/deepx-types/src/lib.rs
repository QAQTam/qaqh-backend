// ── Type definitions for deepx core types ──
//
// All type definitions are split across sub-modules below.
// This file re-exports every public symbol so consumers can
// use `deepx_types::TypeName` without caring about sub-module layout.

// ── Sub-module declarations (each file = one logical group) ──

pub mod api_types;
pub mod config;
pub mod message;
pub mod provider;
pub mod session;
pub mod state;
pub mod tool_def;

// Unified arg parsing (shared across dsx-agent, dsx-tools)
pub mod arg;

// Platform-specific utilities
pub mod platform;

pub mod token;

// ── Re-exports: flat public API ──

pub use api_types::UsageInfo;
pub use config::{
    BalanceInfo, ConfigStore, PersistentConfig, PersistentDatabaseConfig, PersistentSubagentConfig,
    ProfileConfig,
};
pub use message::{ContentBlock, FunctionCall, Message, ToolCall};
pub use provider::{CacheTokenField, EndpointSpec, ProviderSpec, ThinkingParamMode, UserSendMode};
pub use session::{SessionMeta, SkillSessionEntry, SkillSessionEntryState, SkillSessionStateV2};
pub use state::DebugLevel;
pub use tool_def::{ToolDef, ToolFunction};

// ── Unified arg parsers ──
pub use arg::{
    parse_arg, parse_arg_or, parse_cmd_arg, parse_file_arg, parse_opt, parse_opt_u64, tool_action,
};

// ── Shared utilities ──
pub use token::{TokenBreakdown, count_tokens, init_tokenizer};
