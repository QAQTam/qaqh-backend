//! Agent ↔ Tools frame definitions (direct call, in-process).
//!
//! Tool execution now runs in-process via `dsx_tools::ToolManager`.
//! These frame types are retained for typed request/response routing.

use serde::{Deserialize, Serialize};

/// Agent → Tools request frames.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum AgentToTools {
    #[serde(rename = "tools_init")]
    Init {
        allowed_tools: Vec<String>,
        session_seed: String,
    },

    #[serde(rename = "tool_call_req")]
    CallReq {
        id: String,
        name: String,
        action: String,
        args: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
    },

    #[serde(rename = "tool_cancel")]
    Cancel {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },

    #[serde(rename = "tools_shutdown")]
    Shutdown,
}

/// Tools → Agent response frames.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum ToolsToAgent {
    #[serde(rename = "tools_ready")]
    Ready {
        tools: Vec<dsx_types::ToolDef>,
    },

    #[serde(rename = "tool_result_message")]
    ToolResultMessage {  ⋮
// __GAP__ lines 51-55 not recovered
    },

    #[serde(rename = "tool_error")]
    ToolError {
        id: String,
        error: String,
        /// "UNKNOWN_TOOL" | "BLOCKED" | "FORBIDDEN" | "IPC_ERROR"
        code: String,
    },
}[HINT] Use start_line=N end_line=N to read specific lines.