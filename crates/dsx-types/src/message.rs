use serde::{Deserialize, Serialize};

// ── Messages ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Anthropic API thinking signature — MUST be preserved and passed
    /// back in subsequent requests when thinking mode is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_signature: Option<String>,
}

#[allow(dead_code)]
impl Message {
    pub fn system(content: &str) -> Self {
        Self {
            role: "system".into(), content: Some(content.into()),
            name: None, tool_calls: None, tool_call_id: None,
            reasoning_content: None, thinking_signature: None,
        }
    }
    pub fn user(content: &str) -> Self {
        Self {
            role: "user".into(), content: Some(content.into()),
            name: None, tool_calls: None, tool_call_id: None,
            reasoning_content: None, thinking_signature: None,
        }
    }
    pub fn assistant_empty() -> Self {
        Self {
            role: "assistant".into(), content: None,
            name: None, tool_calls: None, tool_call_id: None,
            reasoning_content: None, thinking_signature: None,
        }
    }
    pub fn tool(tool_call_id: &str, result: &str) -> Self {
        Self {
            role: "tool".into(), content: Some(result.into()),
            name: None, tool_calls: None, tool_call_id: Some(tool_call_id.into()),
            reasoning_content: None, thinking_signature: None,
        }
    }
}

// ── Tool Call (from model) ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String, // JSON string
}

#[allow(dead_code)]
impl ToolCall {
    pub fn try_parse_args<T: serde::de::DeserializeOwned>(&self) -> Option<T> {
        serde_json::from_str(&self.function.arguments).ok()
    }
}
