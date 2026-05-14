//! Provider routing and request shaping.
//!
//! - `anthropic_url`: endpoint URL construction for different providers.
//! - `request_to_provider`: final HTTP body assembly (e.g. DeepSeek output_config injection).

use crate::types::*;

/// Smart URL construction — supports:
///   "https://api.deepseek.com"             → /anthropic/messages (DeepSeek)
///   "http://localhost:1234/v1"             → /v1/messages       (LM Studio / Anthropic native)
///   "http://localhost:1234/v1/messages"    → unchanged           (full endpoint)
///   "https://api.anthropic.com/v1/messages" → unchanged
pub(crate) fn anthropic_url(base: &str) -> String {
    let base = base.strip_suffix('/').unwrap_or(base);

    // Already a full endpoint path
    if base.ends_with("/messages") {
        return base.to_string();
    }

    // Extract origin and path
    let (origin, path) = match base.find("://") {
        Some(scheme_end) => {
            let rest = &base[scheme_end + 3..];
            match rest.find('/') {
                Some(path_start) => {
                    let origin = &base[..scheme_end + 3 + path_start];
                    let path = &base[scheme_end + 3 + path_start..];
                    (origin, path)
                }
                None => (base, ""),
            }
        }
        None => (base, ""),
    };

    // If path is /v1 or empty, append /messages
    // Otherwise assume non-standard path → append /anthropic/messages (DeepSeek)
    if path.is_empty() || path == "/v1" {
        format!("{}/v1/messages", origin)
    } else {
        format!("{}/anthropic/messages", origin)
    }
}

/// Build the final HTTP body JSON from the request.
///
/// Applies provider-specific adjustments on top of `AnthropicRequest`:
/// - DeepSeek: inject `output_config` (effort control).
pub fn request_to_provider(
    req_serde: &mut serde_json::Value,
    base_url: &str,
    effort: Option<&str>,
) {
    if let Some(e) = effort {
        if base_url.contains("deepseek.com") {
            req_serde["output_config"] = serde_json::json!({"effort": e});
        }
    }
}
