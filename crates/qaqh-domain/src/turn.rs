//! Turn / round transcript model used by the v2 conversation projection.
//!
//! Extracted from the former `timeline` module: these types are the domain
//! shape of "one agent turn" (user message + model rounds + ordered blocks)
//! and are consumed by `qaqh_domain::state` and the ringing v2 projection —
//! they were never timeline-exclusive.

use serde::{Deserialize, Serialize};

use qaqh_types::ReasoningKind;

/// Tool call declared by the model, used in turn projections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallDef {
    pub id: String,
    pub name: String,
    /// Human-readable args summary (e.g. "foo.rs", "search pattern")
    pub args_display: String,
    /// Raw JSON arguments string
    pub args_json: String,
}

/// Tool execution result used in turn projections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultDef {
    pub tool_call_id: String,
    pub output: String,
    pub success: bool,
    /// 工具侧五态（Ok/Error/Partial/Cancelled/Backgrounded），历史归档无此
    /// 字段（serde default 兼容旧 journal）；缺失时 rebuild 按 success 二值回退。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<qaqh_types::ToolStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<FileSnapshotInfo>,
    /// 运行元数据（09-18 展示契约）。历史归档缺失时为空对象。
    #[serde(
        default,
        skip_serializing_if = "qaqh_types::ToolResultMetrics::is_empty"
    )]
    pub metrics: qaqh_types::ToolResultMetrics,
    /// Canonical display projection carried by typed tool results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<qaqh_types::ToolResultDisplay>,
    /// 结构化错误（rebuild 侧失败槽的单一事实源）。历史归档无此字段
    /// （serde default 兼容）；缺失时 rebuild 把 message 置空（不读 output），
    /// 由 client 显示裸 code。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<qaqh_types::ToolError>,
}

/// File metadata snapshot for rich rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSnapshotInfo {
    pub path: String,
    pub lines: u32,
    pub size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

/// One round of a turn (one API call).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundData {
    pub round_num: u32,
    #[serde(default)]
    pub is_final: bool,
    pub thinking: Option<String>,
    pub answer: Option<String>,
    pub tool_calls: Vec<ToolCallDef>,
    pub tool_results: Vec<ToolResultDef>,
    /// Ordered blocks preserving the LLM's output sequence (reasoning ↔ text ↔ tool).
    #[serde(default)]
    pub blocks: Vec<RoundBlock>,
}

/// One full turn (user message + all rounds).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnData {
    pub turn_id: String,
    pub user_text: String,
    pub rounds: Vec<RoundData>,
}

/// One block in a round, preserving the LLM's output order.
///
/// Blocks are streamed to the frontend in order so it can reconstruct
/// the exact sequence of reasoning → text → tool calls from the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RoundBlock {
    /// Model reasoning/thinking block (collapsible in UI).
    Reasoning {
        content: String,
        /// 思考类别。`None` = 未标注（重建自本字段引入前的历史）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<ReasoningKind>,
    },
    /// Plain text answer block.
    Text { content: String },
    /// A tool call the model wants to invoke.
    Tool { card: ToolCallDef },
    /// A server-side web search performed by the model's built-in tool
    /// (Responses API). Shown as a record line; the search itself ran on the
    /// provider, so there is no local tool card or result round-trip.
    WebSearch { action: String },
}
