use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Safety levels for tool execution ──

#[derive(Debug, Clone, PartialEq)]
pub enum SafetyLevel {
    Safe,     // auto-execute (read, list, search, git-status)
    Confirm,  // ask user (write, edit, git-commit, curl, build)
    Danger,   // ask user with warning (sudo, rm, chmod, dd)
}

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

// ── Chat API types (kept for future streaming support) ──

#[allow(dead_code)]
#[derive(Debug, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: ToolFunction,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolFunction {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

// ── Stream Delta (for future streaming support) ──

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct StreamChunk {
    #[serde(default)]
    pub id: Option<String>,
    pub choices: Vec<StreamChoice>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub created: Option<u64>,
    #[serde(default)]
    pub system_fingerprint: Option<String>,
    pub usage: Option<UsageInfo>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct StreamChoice {
    pub delta: StreamDelta,
    pub index: i32,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct StreamDelta {
    pub role: Option<String>,
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<StreamToolCallDelta>>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct StreamToolCallDelta {
    pub index: i32,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "type", default)]
    pub call_type: Option<String>,
    #[serde(default)]
    pub function: Option<StreamFunctionDelta>,
}

#[allow(dead_code)]
#[derive(Debug, Default, Deserialize)]
pub struct StreamFunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

// ── Non-streaming response (for deserialization in streaming wrapper) ──

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct ChatResponse {
    pub choices: Vec<ResponseChoice>,
    pub usage: Option<UsageInfo>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct ResponseChoice {
    pub message: ResponseMessage,
    pub finish_reason: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct ResponseMessage {
    #[allow(dead_code)]
    pub role: Option<String>,
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCall>>,
}

// ── Usage ──

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct UsageInfo {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(default)]
    pub prompt_cache_hit_tokens: u32,
    #[serde(default)]
    pub prompt_cache_miss_tokens: u32,
    #[serde(default)]
    pub completion_tokens_details: Option<TokenDetails>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct TokenDetails {
    #[serde(default)]
    pub reasoning_tokens: u32,
}

// ── Model list ──

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct ModelList {
    pub data: Vec<ModelInfo>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub owned_by: String,
}

// ── Balance ──

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct BalanceInfo {
    pub is_available: bool,
    pub balance_infos: Vec<BalanceEntry>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct BalanceEntry {
    pub currency: String,
    pub total_balance: String,
    pub granted_balance: String,
    pub topped_up_balance: String,
}

// ── Config persistence ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistentConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_lang: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profiles: Option<HashMap<String, ProfileConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferences: Option<UserPreferences>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_mode: Option<bool>,
}

// ── Profile / Preferences ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileConfig {
    pub model: String,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub context_limit: u32,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_lang")]
    pub prompt_lang: String,
}

fn default_base_url() -> String { "https://api.deepseek.com".into() }
fn default_lang() -> String { "en".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPreferences {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_compact: Option<bool>,
}

// ── Agent phase (detected from reasoning chain) ──

#[derive(Debug, Clone, PartialEq)]
pub enum AgentPhase {
    Idle,        // waiting for user input
    Analyzing,   // understanding the problem
    Planning,    // outlining approach
    Executing,   // tool calls in progress
    Verifying,   // checking tool results
    Responding,  // generating final answer
}

// ── Task phase (AI-declared via status tool, drives auto-mode routing) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskPhase {
    Chat,
    Explore,
    Plan,
    Coding,
    Debug,
}

impl Default for TaskPhase {
    fn default() -> Self { TaskPhase::Coding }
}

impl TaskPhase {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "plan" => TaskPhase::Plan,
            "explore" | "discover" | "map" => TaskPhase::Explore,
            "coding" | "code" => TaskPhase::Coding,
            "debug" => TaskPhase::Debug,
            _ => TaskPhase::Coding,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DebugLevel {
    Low,
    Medium,
    High,
}

impl Default for DebugLevel {
    fn default() -> Self { DebugLevel::Medium }
}

impl DebugLevel {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "low" => DebugLevel::Low,
            "high" => DebugLevel::High,
            _ => DebugLevel::Medium,
        }
    }
}

// ── Session persistence ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamState {
    pub mode: String,
    pub stream_content: String,
    pub stream_reasoning: String,
    pub stream_tool_progress: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub seed: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub message_count: usize,
    pub last_summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFile {
    pub seed: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub messages: Vec<Message>,
    pub last_summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_state: Option<StreamState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_memory: Option<crate::memory::SemanticMemory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_phase: Option<TaskPhase>,
}

// ── Decode helper ──

#[allow(dead_code)]
impl ToolCall {
    pub fn try_parse_args<T: serde::de::DeserializeOwned>(&self) -> Option<T> {
        serde_json::from_str(&self.function.arguments).ok()
    }
}

// ── Anthropic API types ──

/// Content block within an Anthropic message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AnthropicContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        signature: String,
    },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

/// An Anthropic-format message (role + content blocks).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: Vec<AnthropicContent>,
}

/// Anthropic tool definition (input_schema instead of parameters).
#[derive(Debug, Clone, Serialize)]
pub struct AnthropicTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Thinking configuration for Anthropic requests.
#[derive(Debug, Clone, Serialize)]
pub struct AnthropicThinking {
    #[serde(rename = "type")]
    pub thinking_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

/// Top-level Anthropic Messages API request body.
///
/// Contains every field from the native Anthropic Messages API spec.
/// Fields the provider doesn't support are omitted via `skip_serializing_if = None`.
#[derive(Debug, Serialize)]
pub struct AnthropicRequest {
    // ── Stable fields (cache prefix) ──
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<AnthropicMessage>,

    // ── Variable fields ──
    pub model: String,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<AnthropicThinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,

    // ── Extended fields (native Anthropic, may be ignored by providers) ──
    /// Top-k sampling — only supported by some providers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,

    /// Custom stop sequences.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,

    /// User metadata (forwarded to provider, useful for abuse tracking).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

// ── Anthropic SSE streaming event types ──

/// The `message` field inside a `message_start` event.
#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicEventMessage {
    pub id: String,
    pub role: String,
    pub model: String,
}

/// The content_block in a `content_block_start` event.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum AnthropicContentBlockStart {
    #[serde(rename = "thinking")]
    Thinking { thinking: String, signature: String },
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String, input: serde_json::Value },
}

/// Delta in a `content_block_delta` event.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum AnthropicDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { thinking: String },
    #[serde(rename = "signature_delta")]
    SignatureDelta { signature: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
}

/// The delta in a `message_delta` event.
#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicMessageDelta {
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
}

/// Anthropic usage info (different field names from OpenAI).
#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicUsage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    #[serde(default)]
    pub cache_read_input_tokens: u32,
}

/// Top-level SSE event from the Anthropic stream.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum AnthropicStreamEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: AnthropicEventMessage },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: usize,
        content_block: AnthropicContentBlockStart,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta {
        index: usize,
        delta: AnthropicDelta,
    },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: usize },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: AnthropicMessageDelta,
        usage: Option<AnthropicUsage>,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "ping")]
    Ping,
}

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
