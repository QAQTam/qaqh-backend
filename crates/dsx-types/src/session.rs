use serde::{Deserialize, Serialize};
use crate::{Message, TaskPhase};

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
    // Phase 4: SemanticMemory lives in dsx-agent. Use serde_json::Value for cross-crate transport.
    pub semantic_memory: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_phase: Option<TaskPhase>,
}
