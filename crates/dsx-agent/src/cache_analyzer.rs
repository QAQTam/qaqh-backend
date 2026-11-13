//! Predicted KV cache hit rate analyzer.
//!
//! Compares consecutive API request payloads to estimate the common-prefix
//! cache hit rate that DeepSeek's KV cache will achieve.  Works at message
//! granularity — if the first K messages in the current request byte-match
//! the first K of the previous request, those K are considered cached.
//!
//! Cross-round predictions are exact (messages are byte-identical).
//! Within-round incremental annotation appends are slightly pessimistic
//! (the changed user message is counted as a miss, even though its prefix
//!  is still cached).

use crate::tokenizer;
use dsx_types::Message;

/// Snapshot of one API request for cache prediction.
pub struct RequestSnapshot {
    pub system: String,
    pub messages: Vec<Message>,
    /// Pre-computed message token counts (including overhead).
    pub msg_tokens: Vec<u32>,
    /// System prompt token count.
    pub sys_tokens: u32,
}

/// Result of a cache prediction.
#[derive(Debug, Clone, Copy)]
pub struct CacheReport {
    pub cache_hit_tokens: u32,
    pub cache_miss_tokens: u32,
    pub hit_rate: f64,
}

/// Tracks consecutive requests within a session and predicts cache hit rate.
pub struct CacheAnalyzer {
    prev: Option<RequestSnapshot>,
}

impl CacheAnalyzer {
    pub fn new() -> Self {
        Self { prev: None }
    }

    /// Record a new request and return the predicted cache report.
    ///
    /// `system` — the system prompt string (base prompt + tool help).
    /// `messages` — the full messages array as sent to the API.
    pub fn record(&mut self, system: &str, messages: &[Message]) -> CacheReport {
        // --- tokenise current request ---
        let sys_tokens = tokenizer::count_tokens(system);
        let msg_tokens: Vec<u32> = messages.iter().map(|m| tokenizer::count_message_tokens(m)).collect();
        let total = sys_tokens + msg_tokens.iter().copied().sum::<u32>();

        // --- compare with previous request ---
        let (hit, miss) = if let Some(ref prev) = self.prev {
            if prev.system == system {
                // System prompt is identical → fully cached.
                // Find how many leading messages byte-match.
                let common = prev
                    .messages
                    .iter()
                    .zip(messages.iter())
                    .take_while(|(a, b)| {
                        a.role == b.role
                            && a.content == b.content
                            && a.name == b.name
                            && a.tool_call_id == b.tool_call_id
                            && a.reasoning_content == b.reasoning_content
                            && a.thinking_signature == b.thinking_signature
                            && serde_json::to_string(&a.tool_calls).ok()
                                == serde_json::to_string(&b.tool_calls).ok()
                    })
                    .count();

                // Cached = system tokens + matching message tokens
                let cached_msg: u32 = msg_tokens[..common].iter().copied().sum();
                let hit = sys_tokens + cached_msg;
                (hit, total.saturating_sub(hit))
            } else {
                // System changed — nothing is cached (cold start).
                (0, total)
            }
        } else {
            // First request — no previous to compare with, everything is a miss.
            (0, total)
        };

        // --- store as previous ---
        self.prev = Some(RequestSnapshot {
            system: system.to_string(),
            messages: messages.to_vec(),
            msg_tokens,
            sys_tokens,
        });

        let hit_rate = if total > 0 {
            hit as f64 / total as f64
        } else {
            0.0
        };

        CacheReport {
            cache_hit_tokens: hit,
            cache_miss_tokens: miss,
            hit_rate,
        }
    }

    /// Reset the analyzer (e.g. on new session).
    pub fn reset(&mut self) {
        self.prev = None;
    }
}
