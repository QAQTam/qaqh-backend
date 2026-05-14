//! Message format conversion: internal Message → Anthropic API format, URL construction.

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

/// Convert internal messages to Anthropic API format.
/// Leading system messages are concatenated into a single system prompt.
/// Tool-result messages are merged into the preceding assistant turn.
pub fn convert_messages(msgs: &[Message]) -> (Option<String>, Vec<AnthropicMessage>) {
    // Collect all leading system messages and concatenate them
    let mut system_parts = Vec::new();
    let mut user_start = 0;
    for m in msgs {
        if m.role == "system" {
            if let Some(ref c) = m.content {
                system_parts.push(c.clone());
            }
            user_start += 1;
        } else {
            break;
        }
    }
    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };

    let mut out = Vec::new();
    let mut i = user_start;

    while i < msgs.len() {
        match msgs[i].role.as_str() {
            "user" => {
                let text = msgs[i].content.clone().unwrap_or_default();
                out.push(AnthropicMessage {
                    role: "user".into(),
                    content: vec![AnthropicContent::Text { text }],
                });
                i += 1;
            }
            "assistant" => {
                let mut blocks = Vec::new();
                if let Some(ref r) = msgs[i].reasoning_content {
                    if !r.is_empty() {
                        blocks.push(AnthropicContent::Thinking {
                            thinking: r.clone(),
                            signature: msgs[i]
                                .thinking_signature
                                .clone()
                                .unwrap_or_else(|| "dsx-session".into()),
                        });
                    }
                }
                if let Some(ref text) = msgs[i].content {
                    if !text.is_empty() {
                        blocks.push(AnthropicContent::Text { text: text.clone() });
                    }
                }
                if let Some(ref tcs) = msgs[i].tool_calls {
                    for tc in tcs {
                        let input: serde_json::Value =
                            serde_json::from_str(&tc.function.arguments)
                                .unwrap_or(serde_json::Value::Null);
                        blocks.push(AnthropicContent::ToolUse {
                            id: tc.id.clone(),
                            name: tc.function.name.clone(),
                            input,
                        });
                    }
                }
                out.push(AnthropicMessage {
                    role: "assistant".into(),
                    content: blocks,
                });
                i += 1;

                // Collect immediately-following tool results into a single user message
                let mut tool_results = Vec::new();
                while i < msgs.len() && msgs[i].role == "tool" {
                    let tc_id = msgs[i].tool_call_id.clone().unwrap_or_default();
                    let result_text = msgs[i].content.clone().unwrap_or_default();
                    let is_err = result_text.starts_with("[ERROR]")
                        || result_text.starts_with("[FAIL]");
                    tool_results.push(AnthropicContent::ToolResult {
                        tool_use_id: tc_id,
                        content: result_text,
                        is_error: if is_err { Some(true) } else { None },
                    });
                    i += 1;
                }
                if !tool_results.is_empty() {
                    out.push(AnthropicMessage {
                        role: "user".into(),
                        content: tool_results,
                    });
                }
            }
            "tool" => {
                // Standalone tool result — shouldn't normally happen, but handle gracefully
                let tc_id = msgs[i].tool_call_id.clone().unwrap_or_default();
                let result_text = msgs[i].content.clone().unwrap_or_default();
                let is_err =
                    result_text.starts_with("[ERROR]") || result_text.starts_with("[FAIL]");
                out.push(AnthropicMessage {
                    role: "user".into(),
                    content: vec![AnthropicContent::ToolResult {
                        tool_use_id: tc_id,
                        content: result_text,
                        is_error: if is_err { Some(true) } else { None },
                    }],
                });
                i += 1;
            }
            _ => i += 1,
        }
    }

    (system, out)
}
