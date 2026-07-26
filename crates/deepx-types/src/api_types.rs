use serde::{Deserialize, Serialize};
use ts_rs::TS;

// ── Usage ──

/// Token usage information returned by the LLM API.
///
/// Captures both standard token counts and provider-specific fields
/// like cache hit/miss and reasoning tokens.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
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
}
