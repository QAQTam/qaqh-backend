//! Native, ordered transcript model.
//!
//! This is deliberately independent of `Agent2Ui`: a timeline is the
//! authoritative representation of what a desktop transcript displays, not a
//! projection of a legacy message protocol.

use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

/// A display block in one model round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum TimelineBlockKind {
    Reasoning,
    Text,
    Tool,
    Notice,
}

/// Lifecycle of a display block. Markdown is rendered only after `Sealed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum TimelineBlockState {
    Open,
    Sealed,
}

/// State updates for a tool block; all updates retain the block's position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum TimelineToolState {
    Prepared,
    Running,
    Succeeded,
    Failed,
    /// 工具被用户/系统取消（ToolStatus::Cancelled）。终态：不是失败——
    /// 前端应区分「失败（有错误输出）」与「取消（无输出或被中断）」。
    Cancelled,
    /// 工具转入后台继续运行（ToolStatus::Backgrounded）。终态：调用已
    /// 返回（副作用已发生），但进程/任务仍在输出，可经后续查询跟进。
    Backgrounded,
}

impl From<qaqh_types::ToolStatus> for TimelineToolState {
    fn from(status: qaqh_types::ToolStatus) -> Self {
        match status {
            qaqh_types::ToolStatus::Ok => Self::Succeeded,
            qaqh_types::ToolStatus::Error | qaqh_types::ToolStatus::Partial => Self::Failed,
            qaqh_types::ToolStatus::Cancelled => Self::Cancelled,
            qaqh_types::ToolStatus::Backgrounded => Self::Backgrounded,
        }
    }
}

/// Terminal state of a transcript turn. This is distinct from block sealing:
/// a cancelled or failed turn may have valid, already-sealed Markdown blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum TimelineTurnState {
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// Sanitised failure information retained with a transcript terminal event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TimelineFailure {
    pub code: String,
    pub message: String,
}

/// Tool permission data belongs to the transcript tool block, while the
/// interaction request/response lifecycle stays on the native control plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TimelineToolPermission {
    pub reason: String,
    pub paths: Vec<String>,
    pub category: String,
    pub level: u8,
    pub risk: String,
    pub consequence: String,
}

/// 类型化展示投影（09-18 跨仓展示契约 §3.3）。
///
/// 全部字段可选、可忽略；未知 header/body 变体解析为 `Unknown`，
/// 旧 client 不会因为新变体丢整块。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TimelineToolDisplay {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<TimelineToolHeader>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<TimelineToolBody>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<TimelineToolMetrics>,
    /// 结构化终态（#336 P2）；旧 client 忽略后仍可读 summary/body。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<qaqh_types::ToolResultDisplayOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimelineToolHeader {
    Path {
        path: String,
        op: TimelinePathOp,
    },
    Shell {
        command: String,
    },
    Query {
        query: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
    },
    Other {
        label: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum TimelinePathOp {
    Read,
    Write,
    Edit,
    List,
    Patch,
    Delete,
    Unknown,
}

impl<'de> Deserialize<'de> for TimelinePathOp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Ok(match raw.as_str() {
            "read" => Self::Read,
            "write" => Self::Write,
            "edit" => Self::Edit,
            "list" => Self::List,
            "patch" => Self::Patch,
            "delete" => Self::Delete,
            _ => Self::Unknown,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimelineToolBody {
    None,
    Text {
        text: String,
        #[serde(default)]
        truncated: bool,
    },
    Diff {
        unified: String,
        #[serde(default)]
        files: Vec<String>,
    },
    Shell {
        output: String,
        /// 字段必须存在；前台 completed 必须为 Some，backgrounded/cancelled 允许 None。
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        truncated: bool,
    },
    /// v2：stdout / stderr 分离展示；旧 client 遇到未知变体应回退旧字段。
    Streams {
        stdout: String,
        stderr: String,
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        truncated: bool,
        #[serde(default)]
        interleaved: bool,
    },
    Subagent {
        name: String,
        #[serde(rename = "session_id", alias = "seed")]
        session_id: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TimelineToolMetrics {
    pub elapsed_ms: u64,
    #[serde(default)]
    pub output_bytes: u64,
    #[serde(default)]
    pub retry_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_tool_name: Option<String>,
    #[serde(default)]
    pub user_initiated: bool,
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

/// Immutable identity and mutable presentation state for one tool block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TimelineTool {
    pub tool_call_id: String,
    pub name: String,
    pub state: TimelineToolState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Original structured arguments as supplied by the tool producer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_json: Option<String>,
    /// 保留下来的工具输出片段。
    ///
    /// ⚠ 这不是"完整输出"，也**不是**靠 `output_ref` 补齐的：标准模式下模型
    /// 文本先被 `TOOL_MODEL_MAX_CHARS`（24K 字符）封顶，而内容外置的阈值是
    /// 10 MiB，二者差两个数量级——外置路径在标准模式下永远不会触发。因此
    /// `output` 就是前端能拿到的全部；想看更多应让模型用更窄的参数重调工具
    /// （截断标记里也是这么提示模型的），而不是期待一个 content 下载端点。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Display-plane unified diff (file-mutation tools). Never projected to the
    /// model; consumed by the transcript renderer (diff drawer / tool cards).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub progress: String,
    /// True once the writer discarded an older prefix of `progress`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub progress_truncated: bool,
    /// 进度流标识（09-18 契约 §5.1）："stdout" | "stderr" | "mixed"。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_stream: Option<String>,
    /// 本次调用累计观测字节（emitted + dropped，含被尾部裁剪的部分）。
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub progress_bytes_total: u64,
    /// 类型化展示投影；缺失时 client 完整回退旧字段（H16）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<TimelineToolDisplay>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<TimelineFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<TimelineToolPermission>,
}

/// Fully materialized display block saved in timeline snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TimelineBlock {
    pub block_id: String,
    /// Stable order within one round. It never changes when the block updates.
    pub block_order: u32,
    pub kind: TimelineBlockKind,
    pub state: TimelineBlockState,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<TimelineTool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TimelineRound {
    pub round_num: u32,
    pub sealed: bool,
    pub is_final: bool,
    pub blocks: Vec<TimelineBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TimelineTurn {
    pub turn_id: String,
    /// 会话内的**全局回合序号**（0-based，从最旧回合数起）——分页游标用它。
    ///
    /// **为什么不能用 `turn_id` 当游标**：`turn_id` 由 worker 的计数器生成，会复用
    /// （`TimelineAppender::open_turn` 明确容忍并原地 reopen，注释里记着实测的
    /// `t14` 重启重号）；归档投影侧的 id 又只是「已加载消息池内的下标」
    /// （`projection::build_turns`）。两者都不是稳定游标。
    ///
    /// `Option` 而非裸 `u64`：**缺省**与「真的排第 0」必须可区分——实时路径追加的
    /// 回合不带序号（它不参与历史分页），消费侧据此判断能否拿它当游标。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(as = "Option<u32>"))]
    pub turn_index: Option<u64>,
    /// seq of the TurnOpened entry that created this turn — the authoritative
    /// time order across snapshots. `0` means unknown (legacy persisted data);
    /// consumers fall back to the turn_id numeric suffix in that case.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(as = "u32"))]
    pub created_seq: u64,
    pub user_text: String,
    pub sealed: bool,
    /// True when `rounds` contains only the bounded preview shell and the
    /// complete turn is stored in the timeline offload sidecar.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub offloaded: bool,
    pub state: TimelineTurnState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<TimelineFailure>,
    pub rounds: Vec<TimelineRound>,
}

/// Authoritative recovery state, not an event array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TimelineSnapshot {
    /// The largest timeline sequence included in `turns`.
    #[cfg_attr(feature = "ts", ts(as = "u32"))]
    pub watermark: u64,
    pub turns: Vec<TimelineTurn>,
}

/// One mutation of the ordered transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum TimelineEvent {
    TurnOpened {
        user_text: String,
    },
    BlockOpened {
        block: TimelineBlock,
    },
    /// `fragment_seq` is monotonic within a text/reasoning block.
    TextDelta {
        block_id: String,
        #[cfg_attr(feature = "ts", ts(as = "u32"))]
        fragment_seq: u64,
        delta: String,
    },
    /// Periodic text-block synchronization, carried as an **increment**.
    ///
    /// `arg` is the text appended to the block since the previous checkpoint
    /// (empty when nothing changed); `text` is the full overwrite and is only
    /// used when the increment cannot be derived from delivered events — a
    /// client that lost `TextDelta` frames, or a block whose text moved
    /// sideways instead of growing. Consumers append `arg`, or replace with
    /// `text` when present, so a stream of checkpoints alone rebuilds the
    /// block. Writers re-baseline after an overwrite, so the next checkpoint
    /// returns to the incremental form.
    BlockCheckpoint {
        block_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arg: Option<String>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        text: String,
    },
    ToolUpdated {
        block_id: String,
        tool: TimelineTool,
    },
    /// A tool-output chunk, appended to the current progress buffer by the
    /// single transcript reducer.
    ToolProgress {
        block_id: String,
        chunk: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
        /// 进度流标识（"stdout" | "stderr"）；None = 未知/历史数据。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stream: Option<String>,
        /// 累计观测字节（emitted + dropped），0 = 未接线。
        #[serde(default, skip_serializing_if = "is_zero_u64")]
        bytes_total: u64,
    },
    BlockSealed {
        block_id: String,
    },
    RoundSealed {
        is_final: bool,
    },
    TurnSealed {
        state: TimelineTurnState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<TimelineFailure>,
    },
}

/// A globally ordered record for one session seed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TimelineEntry {
    /// Strictly monotonic for one `(server epoch, seed)` across all display kinds.
    #[cfg_attr(feature = "ts", ts(as = "u32"))]
    pub timeline_seq: u64,
    pub turn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_num: Option<u32>,
    pub event: TimelineEvent,
}

/// Producer-to-writer command for the native transcript. Producers never
/// allocate `timeline_seq` or a text fragment sequence: those are assigned by
/// the single writer after intents from model and tool workers have been
/// serialized onto one queue.
///
/// This is deliberately not an `Agent2Ui` or Ringing-event wrapper. It has no
/// channel, delivery, SSE, or legacy message fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum TimelineIntent {
    TurnOpened {
        turn_id: String,
        user_text: String,
    },
    BlockOpened {
        turn_id: String,
        round_num: u32,
        block_id: String,
        kind: TimelineBlockKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<TimelineTool>,
    },
    TextDelta {
        turn_id: String,
        round_num: u32,
        block_id: String,
        delta: String,
    },
    /// Replaceable full value for one reasoning/text block. The writer
    /// overwrites `block.text`; fragment accounting is left untouched so
    /// later `TextDelta`s keep validating against the monotonic counter.
    BlockCheckpoint {
        turn_id: String,
        round_num: u32,
        block_id: String,
        text: String,
    },
    ToolUpdated {
        turn_id: String,
        round_num: u32,
        block_id: String,
        tool: TimelineTool,
    },
    /// Append execution output without replacing the tool's identity or
    /// arguments. The transcript writer applies this patch to the block.
    ToolProgress {
        turn_id: String,
        round_num: u32,
        block_id: String,
        chunk: String,
        /// 进度流标识（"stdout" | "stderr" | "mixed"）；None = 未知。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stream: Option<String>,
        /// 累计观测字节（emitted + dropped），0 = 未接线。
        #[serde(default, skip_serializing_if = "is_zero_u64")]
        bytes_total: u64,
    },
    BlockSealed {
        turn_id: String,
        round_num: u32,
        block_id: String,
    },
    RoundSealed {
        turn_id: String,
        round_num: u32,
        is_final: bool,
    },
    TurnSealed {
        turn_id: String,
        state: TimelineTurnState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<TimelineFailure>,
    },
}

// ═══════════════════════════════════════════════════════════
// Aggregate projections（PR-3-4 迁入：resume 路径的回合聚合树）
// ═══════════════════════════════════════════════════════════
//
// TurnData/RoundData/RoundBlock/ToolCallDef/ToolResultDef 是 resume /
// 归档推导活跃视图使用的**聚合投影**（回合聚合树 ≠ domain 事件流）。
// 原 proto 同名类型原样迁入；刻意不加 ts-rs 导出（维持零前端曝光现状）。
// JSON/磁盘形状（含字段顺序与 skip_serializing_if）保持逐字节不变。

/// Tool call definition used in turn projections.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
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
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
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
}

/// File metadata snapshot for rich rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
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
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
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
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
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
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RoundBlock {
    /// Model reasoning/thinking block (collapsible in UI).
    Reasoning { content: String },
    /// Plain text answer block.
    Text { content: String },
    /// A tool call the model wants to invoke.
    Tool { card: ToolCallDef },
    /// A server-side web search performed by the model's built-in tool
    /// (Responses API). Shown as a record line; the search itself ran on the
    /// provider, so there is no local tool card or result round-trip.
    WebSearch { action: String },
}

#[cfg(test)]
mod display_contract_tests {
    use super::*;

    #[test]
    fn legacy_timeline_tool_json_without_display_still_parses() {
        let raw = r#"{"tool_call_id":"c1","name":"read","state":"succeeded","summary":"read src/lib.rs","progress":"","progress_truncated":false}"#;
        let tool: TimelineTool = serde_json::from_str(raw).expect("legacy json must parse");
        assert!(tool.display.is_none());
        assert_eq!(tool.summary.as_deref(), Some("read src/lib.rs"));
    }

    #[test]
    fn unknown_display_variants_and_path_ops_fall_back_to_unknown() {
        let body: TimelineToolBody =
            serde_json::from_str(r#"{"kind":"hologram","x":1}"#).expect("unknown body tolerated");
        assert!(matches!(body, TimelineToolBody::Unknown));

        let header: TimelineToolHeader =
            serde_json::from_str(r#"{"kind":"wormhole"}"#).expect("unknown header tolerated");
        assert!(matches!(header, TimelineToolHeader::Unknown));

        let op: TimelinePathOp =
            serde_json::from_str("\"frobnicate\"").expect("unknown op tolerated");
        assert!(matches!(op, TimelinePathOp::Unknown));
    }

    #[test]
    fn streams_body_roundtrips_with_separate_output() {
        let body = TimelineToolBody::Streams {
            stdout: "out\n".into(),
            stderr: "err\n".into(),
            exit_code: Some(1),
            truncated: false,
            interleaved: false,
        };
        let json = serde_json::to_value(&body).expect("serialize");
        assert_eq!(json["kind"], "streams");
        assert_eq!(json["stdout"], "out\n");
        assert_eq!(json["stderr"], "err\n");
        let restored: TimelineToolBody = serde_json::from_value(json).expect("deserialize");
        assert_eq!(restored, body);
    }

    #[test]
    fn shell_body_serializes_exit_code_key_even_when_none() {
        let body = TimelineToolBody::Shell {
            output: String::new(),
            exit_code: None,
            truncated: false,
        };
        let json = serde_json::to_value(&body).expect("serialize");
        assert!(json.get("exit_code").is_some(), "H2: key must exist");
        assert!(json["exit_code"].is_null());
    }
}
