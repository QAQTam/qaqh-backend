use serde::{Deserialize, Serialize};

// ── OpenAI-native content blocks ──

/// Content block within a message, matching OpenAI / DeepSeek Chat Completions API.
///
/// Messages use content blocks instead of flat strings to support mixed text +
/// tool call + tool result + reasoning content within a single turn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ContentBlock {
    /// Plain text content from the model or user.
    #[serde(rename = "text")]
    Text { text: String },
    /// Model reasoning/thinking output (shown as collapsible in UI).
    /// Separate from `Text` so the frontend can style reasoning differently.
    #[serde(rename = "reasoning")]
    Reasoning { reasoning: String },
    /// A tool call the model wants to execute.
    /// Includes the tool name, input arguments, and a unique call ID for tracking.
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Result of executing a previously requested tool call.
    /// Fed back to the model as context for the next inference step.
    #[serde(rename = "tool_result")]
    ToolResult {
        /// Matches the `id` from the corresponding `ToolUse` block.
        tool_use_id: String,
        /// The tool's output (may be truncated).
        content: String,
        /// Whether the tool execution succeeded.
        #[serde(default)]
        success: bool,
    },
    /// An image for multimodal understanding (user messages only).
    /// `mime_type` is the MIME type (e.g. "image/png", "image/jpeg").
    /// `data` is the base64-encoded image data (without the `data:...;base64,` prefix).
    #[serde(rename = "image")]
    Image {
        /// MIME type of the image (e.g. "image/png", "image/jpeg").
        mime_type: String,
        /// Base64-encoded image data (raw, without the data URI prefix).
        data: String,
    },
}

impl ContentBlock {
    /// Convenience constructor for a text content block.
    pub fn text(text: &str) -> Self {
        ContentBlock::Text {
            text: text.to_string(),
        }
    }

    /// Convenience constructor for an image content block.
    pub fn image(mime_type: &str, base64_data: &str) -> Self {
        ContentBlock::Image {
            mime_type: mime_type.to_string(),
            data: base64_data.to_string(),
        }
    }
}

// ── Messages ──

/// A conversation message using OpenAI-native content-block format.
///
/// Roles:
/// - `"user"` — contains `Text`, `Image`, and/or `ToolResult` blocks
/// - `"assistant"` — contains `Text`, `Reasoning`, and/or `ToolUse` blocks
/// - `"system"` — system-level context and instructions
/// - `"tool"` — tool execution results
///
/// The optional `name` field distinguishes same-role participants
/// (e.g. `name="docs"` for injected document context, `name="code"` for
/// code snippets). It maps to OpenAI's `name` parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Monotonic per-session message ID for ordering and dedup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg_id: Option<u64>,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// Create a system message with a single text block.
    pub fn system(content: &str) -> Self {
        Self {
            msg_id: None,
            role: "system".into(),
            name: None,
            content: vec![ContentBlock::text(content)],
        }
    }
    /// Create a user message with a single text block.
    pub fn user(content: &str) -> Self {
        Self {
            msg_id: None,
            role: "user".into(),
            name: None,
            content: vec![ContentBlock::text(content)],
        }
    }
    /// Create a tool result message, feeding tool output back to the model.
    pub fn tool(tool_call_id: &str, result: &str, success: bool) -> Self {
        Self {
            msg_id: None,
            role: "tool".into(),
            name: None,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: tool_call_id.into(),
                content: result.into(),
                success,
            }],
        }
    }
}

// ── Tool Call (kept for IPC, XML/DSML parsing, and backward compat) ──

/// A tool call invocation, used in JSON-based tool call protocols.
///
/// Note: new code prefers `ContentBlock::ToolUse` for OpenAI-native format.
/// `ToolCall` remains for XML/DSML parsing and backward compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique call identifier for tracking and result matching.
    pub id: String,
    /// Always `"function"` for function-call-style tools.
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

/// The function details within a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    /// Name of the tool to invoke (e.g. "read", "exec").
    pub name: String,
    /// JSON-encoded arguments string.
    pub arguments: String,
}
