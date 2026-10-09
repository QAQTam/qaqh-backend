use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

// ── Usage ──

/// Token usage information returned by the LLM API.
///
/// Captures both standard token counts and provider-specific fields
/// like cache hit/miss and reasoning tokens.
// `Eq` 保留：本类型嵌在 `qaqh-session` 的事实/投影类型里，那些类型靠 `Eq` 做
// 幂等比较（`UsageInfo` 一旦失去 `Eq`，5 个事实类型跟着失去）。因此 `extras`
// 只收整数——provider 在 usage 里报的都是整数计数器，非整数一律丢弃。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct UsageInfo {
    /// Tokens consumed by the input (prompt + conversation history).
    pub prompt_tokens: u32,
    /// Tokens generated in the model's response.
    pub completion_tokens: u32,
    /// Sum of prompt_tokens + completion_tokens.
    pub total_tokens: u32,
    /// Cached prompt tokens that were served from cache (DeepSeek).
    #[serde(default)]
    pub prompt_cache_hit_tokens: u32,
    /// Prompt tokens that missed the cache and were computed fresh.
    #[serde(default)]
    pub prompt_cache_miss_tokens: u32,
    /// Tokens consumed by internal reasoning/thinking (DeepSeek R1, etc.).
    #[serde(default)]
    pub reasoning_tokens: u32,
    /// Whether the provider actually returned cache usage fields. This keeps a
    /// genuine zero-percent hit rate distinct from unsupported/missing data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_usage_reported: Option<bool>,
    /// Provider-native integer fields that have no neutral name yet
    /// (`credit`, `completion_thinking_tokens`, …). Captured once at the gate
    /// (`qaqh_gate::usage`), display-only, and empty for most endpoints.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extras: BTreeMap<String, i64>,
}
