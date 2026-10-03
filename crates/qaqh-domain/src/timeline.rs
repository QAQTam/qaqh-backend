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

/// 失败槽 message 的单行上界：状态行/导出承载的是「为什么失败」的一句话，
/// 不是正文证据——证据由 `output` 独自承载，两者**永不互相复制**。
pub const FAILURE_MESSAGE_MAX_CHARS: usize = 200;

/// 工具终态 → timeline 失败槽的**单一事实源**投影。
///
/// - `code` 取 `error.code`（机器可读的真实错误码）；无 error 时回退
///   `tool_execution_failed`（历史字面量，旧 journal 无 error 字段）。
/// - `message` 只从 `error.message` 取首个非空行并压平、有界——**不读
///   `output`**。此前三处发射点都把整段模型向文本塞进 message，与 output
///   逐字重复，TUI 状态行与正文因此双重显示同一错误。
///
/// 非 failure 状态返回 `None`。
pub fn tool_failure_of(result: &qaqh_types::ToolResult) -> Option<TimelineFailure> {
    if !result.status.is_failure() {
        return None;
    }
    let error = result.error.as_ref();
    let code = error
        .map(|error| error.code.trim())
        .filter(|code| !code.is_empty())
        .unwrap_or("tool_execution_failed")
        .to_owned();
    let message = error
        .map(|error| one_line_bounded(&error.message, FAILURE_MESSAGE_MAX_CHARS))
        .unwrap_or_default();
    Some(TimelineFailure { code, message })
}

/// 首个非空行压平为单行并按字符数有界（UTF-8 边界安全）。
///
/// 失败槽的唯一文本规整入口：live 发射（[`tool_failure_of`]）与 journal
/// rebuild 共用，保证两条路径产出同形的 message。
pub fn one_line_bounded(text: &str, max_chars: usize) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .chars()
        .map(|ch| if ch == '\r' || ch == '\t' { ' ' } else { ch })
        .collect::<String>();
    let trimmed = line.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_owned();
    }
    let mut bounded: String = trimmed.chars().take(max_chars).collect();
    bounded.push('…');
    bounded
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
    /// 稳定档名标签(read-only / workspace-write / skip-permissions)。
    /// 展示端用档名而非裸数字——数字语义已在三档制中整体平移,单看数字
    /// 会误导。旧记录经 serde default 反序列化为空串,展示端兜底显数字。
    #[serde(default)]
    pub level_name: String,
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
        #[serde(rename = "session_id")]
        session_id: String,
    },
    #[serde(other)]
    Unknown,
}

impl TimelineToolBody {
    /// body 携带的退出码（契约切片 2026-10-03：runtime 据此填充
    /// `TimelineTool.exit_code` 顶层槽，client 无需拆 body）。
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Shell { exit_code, .. } | Self::Streams { exit_code, .. } => *exit_code,
            _ => None,
        }
    }
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
    /// 工具退出码（提升为顶层槽，契约切片 2026-10-03）。由 runtime 从
    /// display body 提取——client 无需拆 body/legacy JSON 即可拿到；非
    /// exec 家族恒为 `None`。归档 rebuild 从同源 display 提取。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// 调用完成时间（epoch ms，runtime 在终态发射时盖戳，不信工具自报）。
    /// 终态前 / 旧归档为 `None`——client 对缺席**不画**，不用本地时钟兜底。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
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
    /// 结构化错误（rebuild 侧失败槽的单一事实源）。历史归档无此字段
    /// （serde default 兼容）；缺失时 rebuild 只能从 output 首行降级。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<qaqh_types::ToolError>,
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
mod failure_slot_tests {
    use super::*;
    use qaqh_types::ToolResult;

    fn failed_result(error: Option<qaqh_types::ToolError>, model_text: &str) -> ToolResult {
        let mut result = ToolResult::text(qaqh_types::ToolStatus::Error, model_text.to_owned());
        result.error = error;
        result
    }

    /// 失败槽单一事实源：code/message 只从 error 取，**永不抄 output**——
    /// 此前三处发射点把整段模型向文本塞进 message，TUI 状态行与正文双重
    /// 显示同一错误（2026-10-03 实锤）。
    #[test]
    fn failure_slot_takes_code_and_one_line_message_from_error_not_output() {
        let result = failed_result(
            Some(qaqh_types::ToolError {
                code: "not_found".into(),
                message: "file not found: /tmp/missing.txt".into(),
                retryable: false,
                hint: Some("re-read".into()),
            }),
            "file not found: /tmp/missing.txt
Hint: re-read",
        );
        let failure = tool_failure_of(&result).expect("failure slot");
        assert_eq!(failure.code, "not_found", "code 用真实错误码");
        assert_eq!(failure.message, "file not found: /tmp/missing.txt");
        assert!(
            !failure.message.contains("Hint"),
            "message 是单行理由，不是整段 output 的复制"
        );
    }

    #[test]
    fn failure_slot_flattens_to_first_nonempty_line_and_bounds() {
        let result = failed_result(
            Some(qaqh_types::ToolError {
                code: "execution".into(),
                message: "

  second line is the real reason  
more"
                    .into(),
                retryable: false,
                hint: None,
            }),
            "whatever",
        );
        let failure = tool_failure_of(&result).expect("failure slot");
        assert_eq!(failure.message, "second line is the real reason");

        let long = "x".repeat(FAILURE_MESSAGE_MAX_CHARS + 50);
        let result = failed_result(
            Some(qaqh_types::ToolError {
                code: "execution".into(),
                message: long.clone(),
                retryable: false,
                hint: None,
            }),
            "whatever",
        );
        let failure = tool_failure_of(&result).expect("failure slot");
        assert_eq!(
            failure.message.chars().count(),
            FAILURE_MESSAGE_MAX_CHARS + 1
        );
        assert!(failure.message.ends_with('…'));
    }

    #[test]
    fn failure_slot_falls_back_when_error_missing_or_blank() {
        let result = failed_result(None, "some output");
        let failure = tool_failure_of(&result).expect("failure slot");
        assert_eq!(failure.code, "tool_execution_failed");
        assert_eq!(failure.message, "");

        let result = failed_result(
            Some(qaqh_types::ToolError {
                code: "  ".into(),
                message: "   ".into(),
                retryable: false,
                hint: None,
            }),
            "some output",
        );
        let failure = tool_failure_of(&result).expect("failure slot");
        assert_eq!(failure.code, "tool_execution_failed");
        assert_eq!(failure.message, "");
    }

    #[test]
    fn success_and_cancelled_free_statuses_have_no_failure_slot() {
        let mut result = ToolResult::text(qaqh_types::ToolStatus::Ok, "fine".into());
        assert!(tool_failure_of(&result).is_none());
        result.status = qaqh_types::ToolStatus::Backgrounded;
        assert!(tool_failure_of(&result).is_none());
    }

    /// 契约切片（2026-10-03）：TimelineTool 顶层 exit_code/completed_at_ms
    /// 可选，旧 wire JSON（无这两个键）可解析且为 None。
    #[test]
    fn timeline_tool_parses_without_terminal_slots() {
        let raw = r#"{"tool_call_id":"c1","name":"read","state":"succeeded","progress":""}"#;
        let tool: TimelineTool = serde_json::from_str(raw).expect("旧 wire 必须可解析");
        assert_eq!(tool.exit_code, None);
        assert_eq!(tool.completed_at_ms, None);
    }

    /// body.exit_code() 顶层提取：Shell/Streams 携带，其余变体 None。
    #[test]
    fn body_exit_code_accessor_reads_shell_and_streams() {
        let shell = TimelineToolBody::Shell {
            output: String::new(),
            exit_code: Some(101),
            truncated: false,
        };
        assert_eq!(shell.exit_code(), Some(101));
        let streams = TimelineToolBody::Streams {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            truncated: false,
            interleaved: false,
        };
        assert_eq!(streams.exit_code(), None);
        assert_eq!(TimelineToolBody::None.exit_code(), None);
        assert_eq!(TimelineToolBody::Unknown.exit_code(), None);
    }

    /// 归档 wire 兼容：ToolResultDef.error 可选，旧 journal（无该键）可解析。
    #[test]
    fn tool_result_def_parses_without_error_field() {
        let raw = r#"{"tool_call_id":"c1","output":"legacy","success":false}"#;
        let def: ToolResultDef = serde_json::from_str(raw).expect("旧 journal 必须可解析");
        assert!(def.error.is_none());
        assert!(def.status.is_none());
    }
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
