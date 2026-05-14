use crate::types::{AnthropicContent, AnthropicMessage, AnthropicUsage, Message, ToolCall, UsageInfo};

// ── Conversion: internal Message ↔ Anthropic API format ──

impl Message {
    /// Convert this assistant Message (with tool_calls) into an Anthropic
    /// message with Text + ToolUse content blocks.
    pub fn to_anthropic_assistant(&self) -> AnthropicMessage {
        let mut blocks = Vec::new();
        if let Some(ref reasoning) = self.reasoning_content {
            if !reasoning.is_empty() {
                blocks.push(AnthropicContent::Thinking {
                    thinking: reasoning.clone(),
                    signature: "dsx-session".into(),
                });
            }
        }
        if let Some(ref text) = self.content {
            if !text.is_empty() {
                blocks.push(AnthropicContent::Text { text: text.clone() });
            }
        }
        if let Some(ref tcs) = self.tool_calls {
            for tc in tcs {
                let input: serde_json::Value =
                    serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::Value::Null);
                blocks.push(AnthropicContent::ToolUse {
                    id: tc.id.clone(),
                    name: tc.function.name.clone(),
                    input,
                });
            }
        }
        AnthropicMessage {
            role: "assistant".into(),
            content: blocks,
        }
    }

    /// Convert a tool-result Message into an Anthropic user message
    /// with a ToolResult content block.
    pub fn to_anthropic_tool_result(&self) -> AnthropicMessage {
        let tc_id = self.tool_call_id.clone().unwrap_or_default();
        let result_text = self.content.clone().unwrap_or_default();
        let is_err = result_text.starts_with("[ERROR]") || result_text.starts_with("[FAIL]");
        AnthropicMessage {
            role: "user".into(),
            content: vec![AnthropicContent::ToolResult {
                tool_use_id: tc_id,
                content: result_text,
                is_error: if is_err { Some(true) } else { None },
            }],
        }
    }

    /// True if this is an assistant message containing tool_calls.
    pub fn has_tool_calls(&self) -> bool {
        self.tool_calls.as_ref().map_or(false, |v| !v.is_empty())
    }
}

impl AnthropicUsage {
    /// Convert to the internal UsageInfo format.
    pub fn to_usage_info(&self) -> UsageInfo {
        UsageInfo {
            prompt_tokens: self.input_tokens,
            completion_tokens: self.output_tokens,
            total_tokens: self.input_tokens + self.output_tokens,
            prompt_cache_hit_tokens: self.cache_read_input_tokens,
            prompt_cache_miss_tokens: self
                .input_tokens
                .saturating_sub(self.cache_read_input_tokens),
            completion_tokens_details: None,
        }
    }
}
