//! Single-writer native transcript timeline.
//!
//! The appender owns sequence allocation and materializes snapshots from the
//! same records it returns to transport. It intentionally does not depend on
//! `Agent2Ui` or the legacy Ringing conversation/tool projections.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;

use qaqh_domain::{
    TimelineBlock, TimelineBlockKind, TimelineBlockState, TimelineEntry, TimelineEvent,
    TimelineIntent, TimelinePathOp, TimelineRound, TimelineSnapshot, TimelineTool,
    TimelineToolBody, TimelineToolDisplay, TimelineToolHeader, TimelineToolMetrics,
    TimelineToolState, TimelineTurn, TimelineTurnState,
};

/// A live Ringing V1 timeline delivery record. `entry.timeline_seq` is the sole SSE cursor for
/// this seed; no per-channel sequence is exposed to a transcript consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineLiveEntry {
    pub seed: String,
    pub entry: TimelineEntry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineError {
    DuplicateTurn(String),
    MissingTurn(String),
    MissingRound {
        turn_id: String,
        round_num: u32,
    },
    DuplicateBlock(String),
    MissingBlock(String),
    InvalidBlockShape(String),
    InvalidBlockKind(String),
    InvalidToolIdentity(String),
    SealedBlock(String),
    SealedRound {
        turn_id: String,
        round_num: u32,
    },
    SealedTurn(String),
    RoundOutOfOrder {
        turn_id: String,
        expected: u32,
        received: u32,
    },
    FragmentOutOfOrder {
        block_id: String,
        expected: u64,
        received: u64,
    },
    RoundNotReady {
        turn_id: String,
        round_num: u32,
    },
    TurnNotReady(String),
}

impl fmt::Display for TimelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateTurn(turn_id) => write!(f, "timeline turn already exists: {turn_id}"),
            Self::MissingTurn(turn_id) => write!(f, "timeline turn does not exist: {turn_id}"),
            Self::MissingRound { turn_id, round_num } => {
                write!(f, "timeline round does not exist: {turn_id}/{round_num}")
            }
            Self::DuplicateBlock(block_id) => {
                write!(f, "timeline block already exists: {block_id}")
            }
            Self::MissingBlock(block_id) => write!(f, "timeline block does not exist: {block_id}"),
            Self::InvalidBlockShape(block_id) => {
                write!(f, "timeline block kind and payload disagree: {block_id}")
            }
            Self::InvalidBlockKind(block_id) => {
                write!(f, "timeline block cannot receive text: {block_id}")
            }
            Self::InvalidToolIdentity(block_id) => {
                write!(f, "timeline tool identity changed: {block_id}")
            }
            Self::SealedBlock(block_id) => write!(f, "timeline block is sealed: {block_id}"),
            Self::SealedRound { turn_id, round_num } => {
                write!(f, "timeline round is sealed: {turn_id}/{round_num}")
            }
            Self::SealedTurn(turn_id) => write!(f, "timeline turn is sealed: {turn_id}"),
            Self::RoundOutOfOrder {
                turn_id,
                expected,
                received,
            } => write!(
                f,
                "timeline round out of order for {turn_id}: expected {expected}, got {received}"
            ),
            Self::FragmentOutOfOrder {
                block_id,
                expected,
                received,
            } => write!(
                f,
                "timeline fragment out of order for {block_id}: expected {expected}, got {received}"
            ),
            Self::RoundNotReady { turn_id, round_num } => {
                write!(
                    f,
                    "timeline round has unsealed blocks: {turn_id}/{round_num}"
                )
            }
            Self::TurnNotReady(turn_id) => {
                write!(f, "timeline turn has unsealed rounds: {turn_id}")
            }
        }
    }
}

impl std::error::Error for TimelineError {}

/// Canonical tool summary projection (09-18 展示契约 §7.1)。
///
/// 优先级：
/// 1. `display_summary`（Phase B 起的工具声明摘要，当前调用点传 `None`）；
/// 2. legacy 输出的首个**非 JSON** 行；
/// 3. `"{name} · {state}"` 兜底。
///
/// H1：任一路径产出的摘要命中 [`is_json_like_summary`] 都必须丢弃并降级，
/// 保证 `TimelineTool.summary` 永远不是被截断的 JSON。
pub(crate) fn project_tool_summary(
    effective_name: &str,
    state: TimelineToolState,
    display_summary: Option<&str>,
    legacy_output: Option<&str>,
) -> String {
    if let Some(summary) = display_summary.and_then(bounded_summary)
        && !is_json_like_summary(&summary)
    {
        return summary;
    }
    if let Some(summary) = legacy_output.map(first_line_summary)
        && !summary.is_empty()
        && !is_json_like_summary(&summary)
    {
        return summary;
    }
    bounded_summary(&format!("{effective_name} · {}", state_label(state)))
        .unwrap_or_else(|| effective_name.to_string())
}

/// H1 判定：摘要不得是 JSON object/array，也不得是它们的截断前缀。
///
/// 解析式判定覆盖完整 JSON；前缀判定覆盖被 `TOOL_SUMMARY_MAX_CHARS` 截断的
/// `{...` / `[{...` / `["...`。`[2/5] Building` 这类人类文本不误伤。
pub(crate) fn is_json_like_summary(summary: &str) -> bool {
    let value = summary.trim();
    if value.is_empty() {
        return false;
    }
    if value.starts_with('{') {
        return true;
    }
    if matches!(
        serde_json::from_str::<serde_json::Value>(value),
        Ok(parsed) if parsed.is_object() || parsed.is_array()
    ) {
        return true;
    }
    if let Some(rest) = value.strip_prefix('[') {
        let rest = rest.trim_start();
        if rest.starts_with('{') || rest.starts_with('[') || rest.starts_with('"') {
            return true;
        }
    }
    false
}

fn first_line_summary(output: &str) -> String {
    bounded_summary(output.lines().next().unwrap_or("")).unwrap_or_default()
}

fn bounded_summary(value: &str) -> Option<String> {
    let one_line: String = value
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let trimmed = one_line.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(
        trimmed
            .chars()
            .take(qaqh_types::TOOL_SUMMARY_MAX_CHARS)
            .collect(),
    )
}

fn state_label(state: TimelineToolState) -> &'static str {
    match state {
        TimelineToolState::Prepared => "prepared",
        TimelineToolState::Running => "running",
        TimelineToolState::Succeeded => "succeeded",
        TimelineToolState::Failed => "failed",
        TimelineToolState::Cancelled => "cancelled",
        TimelineToolState::Backgrounded => "backgrounded",
    }
}

/// 把 canonical `ToolResult.metrics` 填入展示投影（H4：框架填充，工具不写）。
pub(crate) fn apply_result_metrics(
    display: &mut qaqh_workspace::tool_api::ToolDisplay,
    metrics: &qaqh_types::ToolResultMetrics,
) {
    display.metrics = qaqh_workspace::tool_api::ToolMetrics {
        elapsed_ms: metrics.elapsed_ms,
        output_bytes: metrics.output_bytes,
        retry_count: metrics.retry_count,
        effective_tool_name: metrics.effective_tool_name.clone(),
        user_initiated: metrics.user_initiated,
    };
    if let Some(outcome) = display.outcome.as_mut() {
        if let Some(elapsed_ms) = metrics.elapsed_ms {
            outcome.duration_ms = Some(elapsed_ms);
        }
        if outcome.output_bytes.is_none() || metrics.output_bytes != 0 {
            outcome.output_bytes = Some(metrics.output_bytes);
        }
    }
}

/// SDK 内部展示投影 → wire 类型（09-18 契约 §3.4 的唯一映射点）。
///
/// `metrics` 仅在 `elapsed_ms` 已接线时出现；本期未接时 wire 为 `None`，
/// client 回退旧字段（H16）。
pub(crate) fn wire_display(display: &qaqh_workspace::tool_api::ToolDisplay) -> TimelineToolDisplay {
    use qaqh_workspace::tool_api as sdk;

    let header = match &display.header {
        sdk::ToolHeader::None => None,
        sdk::ToolHeader::Path { path, op } => Some(TimelineToolHeader::Path {
            path: path.clone(),
            op: match op {
                sdk::PathOp::Read => TimelinePathOp::Read,
                sdk::PathOp::Write => TimelinePathOp::Write,
                sdk::PathOp::Edit => TimelinePathOp::Edit,
                sdk::PathOp::List => TimelinePathOp::List,
                sdk::PathOp::Patch => TimelinePathOp::Patch,
                sdk::PathOp::Delete => TimelinePathOp::Delete,
            },
        }),
        sdk::ToolHeader::Shell { command } => Some(TimelineToolHeader::Shell {
            command: command.clone(),
        }),
        sdk::ToolHeader::Query { query, scope } => Some(TimelineToolHeader::Query {
            query: query.clone(),
            scope: scope.clone(),
        }),
        sdk::ToolHeader::Other { label } => Some(TimelineToolHeader::Other {
            label: label.clone(),
        }),
    };

    let body = match &display.body {
        sdk::ToolBody::None => Some(TimelineToolBody::None),
        sdk::ToolBody::Text { text, truncated } => Some(TimelineToolBody::Text {
            text: text.clone(),
            truncated: *truncated,
        }),
        sdk::ToolBody::Diff { unified, files } => Some(TimelineToolBody::Diff {
            unified: unified.clone(),
            files: files.clone(),
        }),
        sdk::ToolBody::Shell {
            output,
            exit_code,
            truncated,
        } => Some(TimelineToolBody::Shell {
            output: output.clone(),
            exit_code: *exit_code,
            truncated: *truncated,
        }),
        sdk::ToolBody::Streams {
            stdout,
            stderr,
            exit_code,
            truncated,
            interleaved,
        } => Some(TimelineToolBody::Streams {
            stdout: stdout.clone(),
            stderr: stderr.clone(),
            exit_code: *exit_code,
            truncated: *truncated,
            interleaved: *interleaved,
        }),
        sdk::ToolBody::Subagent { name, seed } => Some(TimelineToolBody::Subagent {
            name: name.clone(),
            seed: seed.clone(),
        }),
    };

    let metrics = display
        .metrics
        .elapsed_ms
        .map(|elapsed_ms| TimelineToolMetrics {
            elapsed_ms,
            output_bytes: display.metrics.output_bytes,
            retry_count: display.metrics.retry_count,
            effective_tool_name: display.metrics.effective_tool_name.clone(),
            user_initiated: display.metrics.user_initiated,
        });
    let outcome = display
        .outcome
        .as_ref()
        .map(|outcome| qaqh_types::ToolResultDisplayOutcome {
            state: match outcome.state {
                sdk::ToolTerminalState::Succeeded => {
                    qaqh_types::ToolResultDisplayOutcomeState::Succeeded
                }
                sdk::ToolTerminalState::Failed => qaqh_types::ToolResultDisplayOutcomeState::Failed,
                sdk::ToolTerminalState::Cancelled => {
                    qaqh_types::ToolResultDisplayOutcomeState::Cancelled
                }
                sdk::ToolTerminalState::TimedOut => {
                    qaqh_types::ToolResultDisplayOutcomeState::TimedOut
                }
                sdk::ToolTerminalState::Backgrounded => {
                    qaqh_types::ToolResultDisplayOutcomeState::Backgrounded
                }
                sdk::ToolTerminalState::Unknown => {
                    qaqh_types::ToolResultDisplayOutcomeState::Unknown
                }
            },
            exit_code: outcome.exit_code,
            duration_ms: outcome.duration_ms,
            output_bytes: outcome.output_bytes,
            truncated: outcome.truncated,
        });

    TimelineToolDisplay {
        summary: display.summary.clone(),
        diff: display.diff.clone(),
        header,
        body,
        metrics,
        outcome,
    }
}

/// Tool progress is a display tail, not a second copy of the complete tool
/// output. Keep enough context for the live card while bounding the snapshot.
pub(crate) const TOOL_PROGRESS_MAX_BYTES: usize = 16 * 1024;

/// A single progress event is bounded independently from the retained tail.
/// This keeps one SSE frame (and its replay/journal copy) small even when the
/// upstream process emits a multi-megabyte line in one read.
pub(crate) const TOOL_PROGRESS_CHUNK_MAX_BYTES: usize = 8 * 1024;

fn retain_utf8_tail(value: &mut String, max_bytes: usize) -> bool {
    if value.len() <= max_bytes {
        return false;
    }
    let mut start = value.len() - max_bytes;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    value.drain(..start);
    true
}

#[derive(Default)]
struct SeedTimeline {
    next_seq: u64,
    turns: BTreeMap<String, TimelineTurn>,
    /// 回放尾：只在尾部追加、只在头部驱逐，故用 `VecDeque` 使两端均摊 O(1)
    /// （BUG-2026-09-12-13：曾用 `Vec::remove(0)`，越过 8192 条上限后每条新
    /// 事件 memmove 整个 journal）。
    journal: VecDeque<TimelineEntry>,
    next_fragment: HashMap<(String, u32, String), u64>,
    /// journal 内滞留的 payload 字节数（text_delta/checkpoint/进度 chunk）。
    /// 驱逐时从头部扣减，O(1) 维护。
    journal_bytes: u64,
    /// True = 已 seal turn 的 blocks 文本在写入侧车成功后被卸载出内存。
    offload_enabled: bool,
}

/// The only component allowed to allocate timeline sequences.
///
/// A future transport actor owns this mutably; keeping its API on `&mut self`
/// makes accidental concurrent producers impossible without an explicit queue.
#[derive(Default)]
pub struct TimelineAppender {
    seeds: HashMap<String, SeedTimeline>,
}

impl TimelineAppender {
    pub fn new() -> Self {
        Self::default()
    }

    /// 该 seed 的 timeline 是否已在内存（懒加载索引检查用）。
    pub fn contains(&self, seed: &str) -> bool {
        self.seeds.contains_key(seed)
    }

    pub fn open_turn(
        &mut self,
        seed: &str,
        turn_id: impl Into<String>,
        user_text: impl Into<String>,
    ) -> Result<TimelineEntry, TimelineError> {
        let turn_id = turn_id.into();
        let user_text = user_text.into();
        let timeline = self.seeds.entry(seed.to_string()).or_default();
        if timeline.turns.contains_key(&turn_id) {
            // Reopen allowance: the message store is the authoritative history
            // and counts every turn, while meta.turn_count only persists on
            // completion — so a daemon restart can leave the worker's restored
            // counter behind the timeline's recorded turns. The next input then
            // legitimately reuses an id the timeline already sealed, either
            // orphan-Cancelled by the sealer or Completed when the count lagged
            // further (observed: t14 reused as Completed after restart).
            // Rejecting the intent (DuplicateTurn) starves the frontend
            // transcript for that turn forever: every later block/text intent
            // fails against the sealed turn and the resumed stream stays blank.
            // Any sealed turn is terminal history; a fresh TurnOpened for the
            // same id means the worker reuses it for a new input, so reset it
            // in place. Only an unsealed (still running) duplicate is a genuine
            // error.
            let reopenable = {
                let turn = timeline.turns.get(&turn_id).expect("checked above");
                turn.sealed
            };
            if reopenable {
                let reopened_user_text = {
                    let turn = timeline.turns.get_mut(&turn_id).expect("checked above");
                    turn.user_text = user_text;
                    turn.sealed = false;
                    turn.offloaded = false;
                    turn.state = TimelineTurnState::Running;
                    turn.failure = None;
                    turn.rounds.clear();
                    turn.created_seq = timeline.next_seq.saturating_add(1);
                    turn.user_text.clone()
                };
                // Fragment counters are keyed by (turn, round, block) and the
                // reopened turn may reuse the same round/block ids, so reset
                // them — otherwise the resumed stream's first TextDelta is
                // rejected with FragmentOutOfOrder and the transcript stalls
                // again.
                timeline
                    .next_fragment
                    .retain(|(turn, _, _), _| turn != &turn_id);
                return Ok(next_entry(
                    timeline,
                    turn_id,
                    None,
                    TimelineEvent::TurnOpened {
                        user_text: reopened_user_text,
                    },
                ));
            }
            return Err(TimelineError::DuplicateTurn(turn_id));
        }
        // 预分配：紧接其后的 TurnOpened entry 将占用 next_seq+1，作为该
        // turn 的权威创建序（快照排序依据，不依赖 turn_id 命名格式）。
        let created_seq = timeline.next_seq.saturating_add(1);
        timeline.turns.insert(
            turn_id.clone(),
            TimelineTurn {
                turn_id: turn_id.clone(),
                // 实时路径不产生全局序号：分页游标只服务历史（常驻窗口与归档页），
                // 而实时追加的回合总是最新的那条，没有「更旧的一页」需要它。
                // 消费侧据此判断能否拿它当游标（见 `TimelineTurn::turn_index`）。
                turn_index: None,
                created_seq,
                user_text: user_text.clone(),
                sealed: false,
                offloaded: false,
                state: TimelineTurnState::Running,
                failure: None,
                rounds: vec![],
            },
        );
        Ok(next_entry(
            timeline,
            turn_id,
            None,
            TimelineEvent::TurnOpened { user_text },
        ))
    }

    pub fn open_block(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        block_id: impl Into<String>,
        kind: TimelineBlockKind,
        tool: Option<TimelineTool>,
    ) -> Result<TimelineEntry, TimelineError> {
        let block_id = block_id.into();
        if (kind == TimelineBlockKind::Tool) != tool.is_some() {
            return Err(TimelineError::InvalidBlockShape(block_id));
        }
        let timeline = self.timeline_mut(seed)?;
        let round = ensure_round_mut(timeline, turn_id, round_num)?;
        if round.sealed {
            return Err(TimelineError::SealedRound {
                turn_id: turn_id.to_string(),
                round_num,
            });
        }
        if round.blocks.iter().any(|block| block.block_id == block_id) {
            return Err(TimelineError::DuplicateBlock(block_id));
        }
        let block = TimelineBlock {
            block_id: block_id.clone(),
            block_order: u32::try_from(round.blocks.len()).unwrap_or(u32::MAX),
            kind,
            state: TimelineBlockState::Open,
            text: String::new(),
            tool,
        };
        round.blocks.push(block.clone());
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::BlockOpened { block },
        ))
    }

    pub fn append_text(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        block_id: &str,
        fragment_seq: u64,
        delta: impl Into<String>,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let key = (turn_id.to_string(), round_num, block_id.to_string());
        let expected = *timeline.next_fragment.get(&key).unwrap_or(&0);
        if fragment_seq != expected {
            return Err(TimelineError::FragmentOutOfOrder {
                block_id: block_id.to_string(),
                expected,
                received: fragment_seq,
            });
        }
        let delta = delta.into();
        let round = existing_round_mut(timeline, turn_id, round_num)?;
        let block = block_mut(round, block_id)?;
        if block.state == TimelineBlockState::Sealed {
            return Err(TimelineError::SealedBlock(block_id.to_string()));
        }
        if !matches!(
            block.kind,
            TimelineBlockKind::Reasoning | TimelineBlockKind::Text
        ) {
            return Err(TimelineError::InvalidBlockKind(block_id.to_string()));
        }
        block.text.push_str(&delta);
        timeline
            .next_fragment
            .insert(key, expected.saturating_add(1));
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::TextDelta {
                block_id: block_id.to_string(),
                fragment_seq,
                delta,
            },
        ))
    }

    /// Applies a text-block synchronization for one reasoning/text block.
    ///
    /// The event carries the **increment** since the previous checkpoint
    /// (`arg`) whenever the delivered stream can account for it, so the
    /// payload — and therefore the SSE frame, the journal entry and the
    /// snapshot write amplification — stays proportional to what the block
    /// actually grew by, not to the total block length. When the block text
    /// cannot be derived from what the transport delivered (lost/reordered
    /// `TextDelta`s, or a non-appending rewrite) the event falls back to a
    /// full-value overwrite (`text`), which is self-healing; the next
    /// checkpoint returns to the incremental form.
    ///
    /// The in-memory projection always materializes the full text, so the
    /// snapshot/recovery semantics are unchanged. `next_fragment` accounting
    /// is intentionally left untouched so subsequent `TextDelta`s still
    /// validate against the monotonic counter.
    pub fn checkpoint_block(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        block_id: &str,
        text: impl Into<String>,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let round = existing_round_mut(timeline, turn_id, round_num)?;
        let block = block_mut(round, block_id)?;
        if block.state == TimelineBlockState::Sealed {
            return Err(TimelineError::SealedBlock(block_id.to_string()));
        }
        if !matches!(
            block.kind,
            TimelineBlockKind::Reasoning | TimelineBlockKind::Text
        ) {
            return Err(TimelineError::InvalidBlockKind(block_id.to_string()));
        }
        let text = text.into();
        // 增量推导：`block.text` 就是"上一个已交付事件之后客户端持有的文本"
        // （每个 delta / checkpoint 都同时更新它与传输层），因此差值天然是
        // 本帧需要补的余量。文本不是 `block.text` 的追加式延伸时（丢/乱序
        // delta 后的整流、非追加改写），降级为一次性全量覆盖（`text`），
        // 消费侧 replace 语义自愈。
        let (arg, overwrite) = match text.strip_prefix(block.text.as_str()) {
            Some(arg) => (Some(arg.to_string()), String::new()),
            None => (None, text.clone()),
        };
        block.text = text;
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::BlockCheckpoint {
                block_id: block_id.to_string(),
                arg,
                text: overwrite,
            },
        ))
    }

    pub fn update_tool(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        block_id: &str,
        state: TimelineToolState,
        summary: Option<String>,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let round = existing_round_mut(timeline, turn_id, round_num)?;
        let block = block_mut(round, block_id)?;
        if block.state == TimelineBlockState::Sealed {
            return Err(TimelineError::SealedBlock(block_id.to_string()));
        }
        let Some(tool) = block.tool.as_mut() else {
            return Err(TimelineError::InvalidBlockKind(block_id.to_string()));
        };
        tool.state = state;
        if tool.display.is_none() {
            tool.summary = summary.or_else(|| tool.summary.clone());
        }
        let tool = tool.clone();
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::ToolUpdated {
                block_id: block_id.to_string(),
                tool,
            },
        ))
    }

    /// Updates mutable presentation fields while preserving the identity and
    /// any durable detail omitted by a lifecycle producer (notably retained
    /// execution progress and a pending permission record).
    pub fn replace_tool(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        block_id: &str,
        mut next_tool: TimelineTool,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let round = existing_round_mut(timeline, turn_id, round_num)?;
        let block = block_mut(round, block_id)?;
        if block.state == TimelineBlockState::Sealed {
            return Err(TimelineError::SealedBlock(block_id.to_string()));
        }
        let Some(tool) = block.tool.as_mut() else {
            return Err(TimelineError::InvalidBlockKind(block_id.to_string()));
        };
        if tool.tool_call_id != next_tool.tool_call_id || tool.name != next_tool.name {
            return Err(TimelineError::InvalidToolIdentity(block_id.to_string()));
        }
        if next_tool.progress.is_empty() {
            next_tool.progress = tool.progress.clone();
            next_tool.progress_truncated = tool.progress_truncated;
        } else if retain_utf8_tail(&mut next_tool.progress, TOOL_PROGRESS_MAX_BYTES) {
            next_tool.progress_truncated = true;
        } else {
            next_tool.progress_truncated |= tool.progress_truncated;
        }
        // 进度元数据是单调量：终态更新不携带时保留运行中累积的值。
        if next_tool.progress_stream.is_none() {
            next_tool.progress_stream = tool.progress_stream.clone();
        }
        next_tool.progress_bytes_total = next_tool
            .progress_bytes_total
            .max(tool.progress_bytes_total);
        if next_tool.permission.is_none() {
            next_tool.permission = tool.permission.clone();
        }
        *tool = next_tool.clone();
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::ToolUpdated {
                block_id: block_id.to_string(),
                tool: next_tool,
            },
        ))
    }

    /// Applies an append-only execution-output patch to an existing tool
    /// block. Identity, arguments, terminal output, and permission state stay
    /// untouched until their explicit lifecycle update arrives.
    #[allow(clippy::too_many_arguments)] // stream/bytes_total 为 09-18 契约新增；参数面塑形另立项（PLAN D-5）
    pub fn append_tool_progress(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        block_id: &str,
        mut chunk: String,
        stream: Option<String>,
        bytes_total: u64,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let round = existing_round_mut(timeline, turn_id, round_num)?;
        let block = block_mut(round, block_id)?;
        if block.state == TimelineBlockState::Sealed {
            return Err(TimelineError::SealedBlock(block_id.to_string()));
        }
        let Some(tool) = block.tool.as_mut() else {
            return Err(TimelineError::InvalidBlockKind(block_id.to_string()));
        };
        let chunk_truncated = retain_utf8_tail(&mut chunk, TOOL_PROGRESS_CHUNK_MAX_BYTES);
        tool.progress.push_str(&chunk);
        let buffer_truncated = retain_utf8_tail(&mut tool.progress, TOOL_PROGRESS_MAX_BYTES);
        let truncated = chunk_truncated || buffer_truncated;
        tool.progress_truncated |= truncated;
        if let Some(stream) = stream {
            tool.progress_stream = Some(stream);
        }
        tool.progress_bytes_total = tool.progress_bytes_total.max(bytes_total);
        let progress_stream = tool.progress_stream.clone();
        let progress_bytes_total = tool.progress_bytes_total;
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::ToolProgress {
                block_id: block_id.to_string(),
                chunk,
                truncated,
                stream: progress_stream,
                bytes_total: progress_bytes_total,
            },
        ))
    }

    /// Applies one producer intent. The method is the only place that turns a
    /// producer's ordered intent into a numbered transcript record.
    pub fn apply_intent(
        &mut self,
        seed: &str,
        intent: TimelineIntent,
    ) -> Result<TimelineEntry, TimelineError> {
        match intent {
            TimelineIntent::TurnOpened { turn_id, user_text } => {
                self.open_turn(seed, turn_id, user_text)
            }
            TimelineIntent::BlockOpened {
                turn_id,
                round_num,
                block_id,
                kind,
                tool,
            } => self.open_block(seed, &turn_id, round_num, block_id, kind, tool),
            TimelineIntent::TextDelta {
                turn_id,
                round_num,
                block_id,
                delta,
            } => {
                let fragment_seq = self
                    .seeds
                    .get(seed)
                    .and_then(|timeline| {
                        timeline
                            .next_fragment
                            .get(&(turn_id.clone(), round_num, block_id.clone()))
                    })
                    .copied()
                    .unwrap_or(0);
                self.append_text(seed, &turn_id, round_num, &block_id, fragment_seq, delta)
            }
            TimelineIntent::BlockCheckpoint {
                turn_id,
                round_num,
                block_id,
                text,
            } => self.checkpoint_block(seed, &turn_id, round_num, &block_id, text),
            TimelineIntent::ToolUpdated {
                turn_id,
                round_num,
                block_id,
                tool,
            } => self.replace_tool(seed, &turn_id, round_num, &block_id, tool),
            TimelineIntent::ToolProgress {
                turn_id,
                round_num,
                block_id,
                chunk,
                stream,
                bytes_total,
            } => self.append_tool_progress(
                seed,
                &turn_id,
                round_num,
                &block_id,
                chunk,
                stream,
                bytes_total,
            ),
            TimelineIntent::BlockSealed {
                turn_id,
                round_num,
                block_id,
            } => self.seal_block(seed, &turn_id, round_num, &block_id),
            TimelineIntent::RoundSealed {
                turn_id,
                round_num,
                is_final,
            } => self.seal_round(seed, &turn_id, round_num, is_final),
            TimelineIntent::TurnSealed {
                turn_id,
                state,
                failure,
            } => self.seal_turn_with_state(seed, &turn_id, state, failure),
        }
    }

    pub fn seal_block(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        block_id: &str,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let round = existing_round_mut(timeline, turn_id, round_num)?;
        let block = block_mut(round, block_id)?;
        block.state = TimelineBlockState::Sealed;
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::BlockSealed {
                block_id: block_id.to_string(),
            },
        ))
    }

    pub fn seal_round(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        is_final: bool,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let round = existing_round_mut(timeline, turn_id, round_num)?;
        if round
            .blocks
            .iter()
            .any(|block| block.state != TimelineBlockState::Sealed)
        {
            return Err(TimelineError::RoundNotReady {
                turn_id: turn_id.to_string(),
                round_num,
            });
        }
        round.sealed = true;
        round.is_final = is_final;
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::RoundSealed { is_final },
        ))
    }

    pub fn seal_turn(&mut self, seed: &str, turn_id: &str) -> Result<TimelineEntry, TimelineError> {
        self.seal_turn_with_state(seed, turn_id, TimelineTurnState::Completed, None)
    }

    pub fn seal_turn_with_state(
        &mut self,
        seed: &str,
        turn_id: &str,
        state: TimelineTurnState,
        failure: Option<qaqh_domain::TimelineFailure>,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let turn = timeline
            .turns
            .get_mut(turn_id)
            .ok_or_else(|| TimelineError::MissingTurn(turn_id.into()))?;
        if turn.rounds.iter().any(|round| !round.sealed) {
            return Err(TimelineError::TurnNotReady(turn_id.to_string()));
        }
        turn.sealed = true;
        turn.state = state;
        turn.failure = failure.clone();
        let entry = next_entry(
            timeline,
            turn_id.to_string(),
            None,
            TimelineEvent::TurnSealed { state, failure },
        );
        // **不再在 seal 时无条件裁剪该 turn 的 journal 条目**（#42 gap 后续）。
        //
        // 原实现「seal 即时裁剪」的论据是「sealed 内容已在快照内物化，回放不再
        // 需要」。但这条在**重连窗口**里不成立：客户端 gap 恢复时取的快照可能
        // 是**回合中途**的（watermark 落在该 turn 内），此后它要靠
        // `seq > watermark` 的条目把这个回合补完。若这些条目在 seal 时被裁掉、
        // 而重连又晚于它们的 live 投递，客户端就**永远收不到 TurnSealed**——
        // 回合停在未封口态，回复不渲染（实测：`MODE=gap` 注入丢帧后
        // `[✓] 用户消息仍在 / [✗] 回复可见`）。
        //
        // 内存上界本来就有两条硬约束（`enforce_journal_budget`：条数
        // `MAX_TIMELINE_JOURNAL_ENTRIES` + 字节 `journal_byte_limit`），
        // 每次 `next_entry` 都会执行；seal 裁剪是**冗余**的第二道，却以
        // 「最近一个回合不可重放」为代价。这里去掉它，内存仍由预算钉死。
        Ok(entry)
    }

    /// 开启 turn-seal 卸载。实际写盘由 hub 在 timeline/store 两把锁之外完成，
    /// 成功后才调用 `mark_offloaded_if_current` 把内存对象壳化。
    pub fn enable_offload(&mut self, seed: &str) {
        self.seeds
            .entry(seed.to_string())
            .or_default()
            .offload_enabled = true;
    }

    /// 已 seal、尚未卸载的 turn id 快照，用于加载后渐进回填侧车。
    pub fn sealed_turn_ids(&self, seed: &str) -> Vec<String> {
        self.seeds.get(seed).map_or_else(Vec::new, |timeline| {
            timeline
                .turns
                .values()
                .filter(|turn| turn.sealed && !turn.offloaded)
                .map(|turn| turn.turn_id.clone())
                .collect()
        })
    }

    /// 返回可写入侧车的完整 turn 快照。调用方必须在释放 timeline 锁后写盘，
    /// 再以 `created_seq` 做代际校验，避免 reopen 后旧写入误壳化新 turn。
    pub fn offload_candidate(&self, seed: &str, turn_id: &str) -> Option<TimelineTurn> {
        let timeline = self.seeds.get(seed)?;
        if !timeline.offload_enabled {
            return None;
        }
        let turn = timeline.turns.get(turn_id)?;
        (turn.sealed && !turn.offloaded).then(|| turn.clone())
    }

    /// 侧车写入成功后壳化。若 turn 已 reopen 或已被新一代 seal 替换，
    /// `created_seq` 不匹配，旧写入不得影响当前内存状态。
    pub fn mark_offloaded_if_current(
        &mut self,
        seed: &str,
        turn_id: &str,
        created_seq: u64,
    ) -> bool {
        let Some(timeline) = self.seeds.get_mut(seed) else {
            return false;
        };
        let Some(turn) = timeline.turns.get_mut(turn_id) else {
            return false;
        };
        if !turn.sealed || turn.offloaded || turn.created_seq != created_seq {
            return false;
        }
        offload_turn_blocks(turn);
        turn.offloaded = true;
        true
    }

    pub fn replay_since(&self, seed: &str, watermark: u64) -> Vec<TimelineEntry> {
        self.seeds.get(seed).map_or_else(Vec::new, |timeline| {
            timeline
                .journal
                .iter()
                .filter(|entry| entry.timeline_seq > watermark)
                .cloned()
                .collect()
        })
    }

    pub fn snapshot(&self, seed: &str) -> Option<TimelineSnapshot> {
        self.seeds.get(seed).map(|timeline| {
            let mut turns: Vec<TimelineTurn> = timeline.turns.values().cloned().collect();
            // turns 存于 HashMap（无序）——按 created_seq 排序（TurnOpened
            // entry 的 seq，权威时间序）；旧磁盘数据 created_seq=0 时退化为
            // turn_id 数值序（t1..tN 递增）。两者混合时旧 turn 在前、新 turn
            // 在后，时间序依然正确。快照数组序必须=时间序：前端恢复按"尾部
            // 窗口"取最新回合，顺序错乱会恢复出错误的回合集合（实测：40
            // turns 会话恢复窗口落在旧回合，最新消息缺失）。
            turns.sort_by_key(|t| (t.created_seq, turn_num(&t.turn_id)));
            TimelineSnapshot {
                watermark: timeline.next_seq,
                turns,
            }
        })
    }

    /// Restores a journal that was previously produced by this appender. The
    /// persisted materialized snapshot is authoritative; the journal is kept
    /// solely for replay after a reconnect watermark.
    pub fn restore(
        &mut self,
        seed: String,
        snapshot: TimelineSnapshot,
        journal: Vec<TimelineEntry>,
    ) {
        let journal: VecDeque<TimelineEntry> = journal.into_iter().collect();
        let mut next_fragment = HashMap::new();
        let mut journal_bytes = 0u64;
        for entry in &journal {
            if let TimelineEvent::TextDelta {
                block_id,
                fragment_seq,
                ..
            } = &entry.event
                && let Some(round_num) = entry.round_num
            {
                next_fragment.insert(
                    (entry.turn_id.clone(), round_num, block_id.clone()),
                    fragment_seq.saturating_add(1),
                );
            }
            journal_bytes += journal_entry_payload_bytes(&entry.event);
        }
        self.seeds.insert(
            seed,
            SeedTimeline {
                next_seq: snapshot.watermark,
                turns: snapshot
                    .turns
                    .into_iter()
                    .map(|turn| (turn.turn_id.clone(), turn))
                    .collect(),
                journal,
                next_fragment,
                journal_bytes,
                offload_enabled: false,
            },
        );
    }

    fn timeline_mut(&mut self, seed: &str) -> Result<&mut SeedTimeline, TimelineError> {
        self.seeds
            .get_mut(seed)
            .ok_or_else(|| TimelineError::MissingTurn(format!("seed:{seed}")))
    }
}

/// turn_id → 数值序（t1/t10 → 1/10）；无数字后缀按 0（保持原序兜底）。
fn turn_num(id: &str) -> u64 {
    id.trim_start_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .unwrap_or(0)
}

fn next_entry(
    timeline: &mut SeedTimeline,
    turn_id: String,
    round_num: Option<u32>,
    event: TimelineEvent,
) -> TimelineEntry {
    timeline.next_seq = timeline.next_seq.saturating_add(1);
    let entry = TimelineEntry {
        timeline_seq: timeline.next_seq,
        turn_id,
        round_num,
        event,
    };
    timeline.journal_bytes += journal_entry_payload_bytes(&entry.event);
    timeline.journal.push_back(entry.clone());
    enforce_journal_budget(timeline);
    entry
}

/// journal 条目的 payload 字节数（内存预算估算；结构事件为 0）。
fn journal_entry_payload_bytes(event: &TimelineEvent) -> u64 {
    match event {
        TimelineEvent::TextDelta { delta, .. } => delta.len() as u64,
        TimelineEvent::BlockCheckpoint { text, .. } => text.len() as u64,
        TimelineEvent::ToolUpdated { tool, .. } => {
            let optional_bytes = |value: &Option<String>| value.as_ref().map_or(0, String::len);
            (optional_bytes(&tool.summary)
                + optional_bytes(&tool.output)
                + optional_bytes(&tool.diff)
                + tool.progress.len()) as u64
        }
        TimelineEvent::ToolProgress { chunk, .. } => chunk.len() as u64,
        _ => 0,
    }
}

/// 双限（条数 + 字节）头部驱逐：保序移除最老条目直至两项都回到界内。
///
/// 两端均摊 O(1)（`pop_front`），与 journal 的 FIFO 语义一致——`Vec::remove(0)`
/// 会让每条越过上限的事件 memmove 整个窗口，实测 52–83× 阶跃且全程持锁。
fn enforce_journal_budget(timeline: &mut SeedTimeline) {
    let entry_limit = crate::ringing::persistence_policy::MAX_TIMELINE_JOURNAL_ENTRIES;
    let byte_limit = crate::ringing::persistence_policy::journal_byte_limit();
    while timeline.journal.len() > entry_limit || timeline.journal_bytes > byte_limit {
        let Some(oldest) = timeline.journal.pop_front() else {
            break;
        };
        timeline.journal_bytes = timeline
            .journal_bytes
            .saturating_sub(journal_entry_payload_bytes(&oldest.event));
    }
}

/// 把 turn 卸载成壳：清空各 block 文本/进度，保留身份与元数据。
/// 首块保留 512 字符预览，前端列表仍可显示摘要；全文从侧车恢复。
fn offload_turn_blocks(turn: &mut TimelineTurn) {
    for round in &mut turn.rounds {
        for block in &mut round.blocks {
            if block.text.chars().count() > 512 {
                let preview: String = block.text.chars().take(512).collect();
                block.text = preview;
            }
            if let Some(tool) = &mut block.tool {
                if tool.progress.chars().count() > 512 {
                    let preview: String = tool.progress.chars().take(512).collect();
                    tool.progress = preview;
                    tool.progress_truncated = true;
                }
                tool.output = None;
                tool.diff = None;
            }
        }
    }
}

fn existing_round_mut<'a>(
    timeline: &'a mut SeedTimeline,
    turn_id: &str,
    round_num: u32,
) -> Result<&'a mut TimelineRound, TimelineError> {
    let turn = timeline
        .turns
        .get_mut(turn_id)
        .ok_or_else(|| TimelineError::MissingTurn(turn_id.into()))?;
    let index = turn
        .rounds
        .iter()
        .position(|round| round.round_num == round_num)
        .ok_or_else(|| TimelineError::MissingRound {
            turn_id: turn_id.to_string(),
            round_num,
        })?;
    Ok(&mut turn.rounds[index])
}

fn ensure_round_mut<'a>(
    timeline: &'a mut SeedTimeline,
    turn_id: &str,
    round_num: u32,
) -> Result<&'a mut TimelineRound, TimelineError> {
    let turn = timeline
        .turns
        .get_mut(turn_id)
        .ok_or_else(|| TimelineError::MissingTurn(turn_id.into()))?;
    if turn.sealed {
        return Err(TimelineError::SealedTurn(turn_id.to_string()));
    }
    let index = match turn
        .rounds
        .iter()
        .position(|round| round.round_num == round_num)
    {
        Some(index) => index,
        None => {
            let expected = turn
                .rounds
                .last()
                .map_or(0, |round| round.round_num.saturating_add(1));
            if round_num != expected {
                return Err(TimelineError::RoundOutOfOrder {
                    turn_id: turn_id.to_string(),
                    expected,
                    received: round_num,
                });
            }
            turn.rounds.push(TimelineRound {
                round_num,
                sealed: false,
                is_final: false,
                blocks: vec![],
            });
            turn.rounds.len() - 1
        }
    };
    Ok(&mut turn.rounds[index])
}

fn block_mut<'a>(
    round: &'a mut TimelineRound,
    block_id: &str,
) -> Result<&'a mut TimelineBlock, TimelineError> {
    round
        .blocks
        .iter_mut()
        .find(|block| block.block_id == block_id)
        .ok_or_else(|| TimelineError::MissingBlock(block_id.to_string()))
}

// `materialize_timeline_from_journal` / `apply_journal_entry` / `block_mut_replay`
// 已随 timeline-journal 一并移除（2026-09-10）。重建投影现由
// `ringing::timeline_rebuild` 从 messages.jsonl 归档完成。

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::TimelineFailure;

    fn tool() -> TimelineTool {
        TimelineTool {
            tool_call_id: "call-1".into(),
            name: "read".into(),
            state: TimelineToolState::Prepared,
            summary: None,
            args_json: None,
            output: None,
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            progress_stream: None,
            progress_bytes_total: 0,
            display: None,
            failure: None,
            permission: None,
        }
    }

    #[test]
    fn orphan_cancelled_turn_is_reopened_by_the_next_input() {
        // Regression: after a daemon restart the orphan-sealer marks the
        // interrupted turn Cancelled, but the message store still counts it,
        // so the worker's next input reuses the same turn_id. open_turn must
        // reset that placeholder instead of rejecting with DuplicateTurn —
        // otherwise every timeline intent for the turn is dropped and the
        // frontend transcript stays blank.
        let mut appender = TimelineAppender::new();
        appender
            .open_turn("s", "t1", "interrupted question")
            .unwrap();
        appender
            .open_block("s", "t1", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        appender
            .append_text("s", "t1", 0, "answer", 0, "partial")
            .unwrap();
        appender.seal_block("s", "t1", 0, "answer").unwrap();
        appender.seal_round("s", "t1", 0, false).unwrap();
        // Simulate daemon restart orphan seal.
        appender
            .seal_turn_with_state(
                "s",
                "t1",
                TimelineTurnState::Cancelled,
                Some(TimelineFailure {
                    code: "daemon_restart_interrupted".into(),
                    message: "interrupted".into(),
                }),
            )
            .unwrap();

        // The worker reuses t1 for the resumed conversation.
        let reopened = appender
            .open_turn("s", "t1", "next question")
            .expect("orphan cancelled turn must be reopenable");
        assert!(matches!(reopened.event, TimelineEvent::TurnOpened { .. }));

        let snapshot = appender.snapshot("s").unwrap();
        let turn = &snapshot.turns[0];
        assert_eq!(turn.user_text, "next question");
        assert!(!turn.sealed);
        assert_eq!(turn.state, TimelineTurnState::Running);
        assert!(turn.rounds.is_empty(), "stale rounds are cleared");
        assert!(turn.failure.is_none(), "failure marker is cleared");

        // Subsequent intents for the reopened turn flow normally.
        appender
            .open_block("s", "t1", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        appender
            .append_text("s", "t1", 0, "answer", 0, "full reply")
            .unwrap();
        let final_snapshot = appender.snapshot("s").unwrap();
        assert_eq!(
            final_snapshot.turns[0].rounds[0].blocks[0].text,
            "full reply"
        );
    }

    #[test]
    fn completed_turn_is_reopened_when_id_is_reused() {
        // Regression: after a daemon restart the worker's restored turn
        // counter (message store) can lag behind the timeline's recorded
        // turns, so the next input reuses an id the timeline already sealed
        // as Completed (observed: t14 reused after restart). The timeline must
        // reset that terminal turn in place instead of rejecting with
        // DuplicateTurn — otherwise every intent for the resumed turn is
        // dropped and the frontend transcript stays blank.
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t1", "question").unwrap();
        appender.seal_turn("s", "t1").unwrap();
        let reopened = appender
            .open_turn("s", "t1", "reused question")
            .expect("sealed turns must be reopenable on id reuse");
        assert!(matches!(reopened.event, TimelineEvent::TurnOpened { .. }));
        let snapshot = appender.snapshot("s").unwrap();
        let turn = &snapshot.turns[0];
        assert_eq!(turn.user_text, "reused question");
        assert!(!turn.sealed);
        assert_eq!(turn.state, TimelineTurnState::Running);
        assert!(turn.failure.is_none());
        assert!(turn.rounds.is_empty(), "stale rounds are cleared");
    }

    #[test]
    fn running_turn_is_not_reopened() {
        // An unsealed (still running) turn is a live producer; a second
        // TurnOpened for the same id is a genuine duplicate and must fail.
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t1", "question").unwrap();
        let err = appender
            .open_turn("s", "t1", "duplicate")
            .expect_err("running turns must not be reopened");
        assert!(matches!(err, TimelineError::DuplicateTurn(_)));
    }

    #[test]
    fn appender_assigns_a_single_order_across_text_and_tools() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "reasoning", TimelineBlockKind::Reasoning, None)
            .unwrap();
        appender
            .append_text("s", "t", 0, "reasoning", 0, "inspect")
            .unwrap();
        appender.seal_block("s", "t", 0, "reasoning").unwrap();
        appender
            .open_block("s", "t", 0, "tool", TimelineBlockKind::Tool, Some(tool()))
            .unwrap();
        appender
            .update_tool(
                "s",
                "t",
                0,
                "tool",
                TimelineToolState::Succeeded,
                Some("read file".into()),
            )
            .unwrap();
        appender.seal_block("s", "t", 0, "tool").unwrap();
        appender
            .open_block("s", "t", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        appender
            .append_text("s", "t", 0, "answer", 0, "done")
            .unwrap();
        appender.seal_block("s", "t", 0, "answer").unwrap();
        appender.seal_round("s", "t", 0, true).unwrap();
        // seal 裁剪语义：TurnSealed 会清空该 turn 的回放尾，seq 连续性
        // 必须在 seal 前断言；watermark 含 TurnSealed 占用的下一序号。
        let last_seq_before_seal = appender
            .replay_since("s", 0)
            .last()
            .expect("journal holds entries while the turn is active")
            .timeline_seq;
        appender.seal_turn("s", "t").unwrap();

        let snapshot = appender.snapshot("s").unwrap();
        let blocks = &snapshot.turns[0].rounds[0].blocks;
        assert_eq!(
            blocks
                .iter()
                .map(|block| block.block_id.as_str())
                .collect::<Vec<_>>(),
            ["reasoning", "tool", "answer"]
        );
        assert!(
            blocks
                .iter()
                .all(|block| block.state == TimelineBlockState::Sealed)
        );
        assert_eq!(blocks[2].text, "done");
        assert_eq!(
            blocks[1].tool.as_ref().unwrap().state,
            TimelineToolState::Succeeded
        );
        assert_eq!(snapshot.watermark, last_seq_before_seal + 1);
    }

    #[test]
    fn appender_rejects_missing_or_reordered_text_fragments() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        assert!(matches!(
            appender.append_text("s", "t", 0, "answer", 1, "late"),
            Err(TimelineError::FragmentOutOfOrder {
                expected: 0,
                received: 1,
                ..
            })
        ));
        appender
            .append_text("s", "t", 0, "answer", 0, "first")
            .unwrap();
        appender.seal_block("s", "t", 0, "answer").unwrap();
        assert!(matches!(
            appender.append_text("s", "t", 0, "answer", 1, "after seal"),
            Err(TimelineError::SealedBlock(_))
        ));
    }

    #[test]
    fn tool_progress_carries_stream_and_cumulative_bytes_through_terminal_update() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "tool", TimelineBlockKind::Tool, Some(tool()))
            .unwrap();
        appender
            .apply_intent(
                "s",
                TimelineIntent::ToolProgress {
                    turn_id: "t".into(),
                    round_num: 0,
                    block_id: "tool".into(),
                    chunk: "downloading…".into(),
                    stream: Some("stdout".into()),
                    bytes_total: 12_600,
                },
            )
            .unwrap();

        // 终态更新不携带进度元数据：必须保留运行中累积的值（单调）。
        let mut final_tool = tool();
        final_tool.state = TimelineToolState::Succeeded;
        final_tool.output = Some("done".into());
        appender
            .replace_tool("s", "t", 0, "tool", final_tool)
            .unwrap();

        let snapshot = appender.snapshot("s").unwrap();
        let tool = snapshot.turns[0].rounds[0].blocks[0].tool.as_ref().unwrap();
        assert_eq!(tool.progress_stream.as_deref(), Some("stdout"));
        assert_eq!(tool.progress_bytes_total, 12_600);
    }

    #[test]
    fn tool_progress_survives_a_terminal_lifecycle_update() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "tool", TimelineBlockKind::Tool, Some(tool()))
            .unwrap();
        let progress = appender
            .apply_intent(
                "s",
                TimelineIntent::ToolProgress {
                    turn_id: "t".into(),
                    round_num: 0,
                    block_id: "tool".into(),
                    chunk: "executing\\n".into(),
                    stream: None,
                    bytes_total: 0,
                },
            )
            .unwrap();
        let mut final_tool = tool();
        final_tool.state = TimelineToolState::Succeeded;
        final_tool.output = Some("done".into());
        appender
            .replace_tool("s", "t", 0, "tool", final_tool)
            .unwrap();

        assert!(matches!(progress.event, TimelineEvent::ToolProgress { .. }));
        let snapshot = appender.snapshot("s").unwrap();
        let tool = snapshot.turns[0].rounds[0].blocks[0].tool.as_ref().unwrap();
        assert_eq!(tool.progress, "executing\\n");
        assert_eq!(tool.output.as_deref(), Some("done"));
        assert_eq!(tool.state, TimelineToolState::Succeeded);
    }

    #[test]
    fn tool_progress_is_bounded_per_chunk_and_in_the_snapshot() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "tool", TimelineBlockKind::Tool, Some(tool()))
            .unwrap();

        let chunk = format!("{}{}", "a".repeat(TOOL_PROGRESS_CHUNK_MAX_BYTES), "tail");
        let event = appender
            .append_tool_progress("s", "t", 0, "tool", chunk, None, 0)
            .unwrap();
        assert!(matches!(
            event.event,
            TimelineEvent::ToolProgress {
                ref chunk,
                truncated: true,
                ..
            } if chunk.len() == TOOL_PROGRESS_CHUNK_MAX_BYTES
                && chunk.ends_with("tail")
        ));

        for _ in 0..3 {
            appender
                .append_tool_progress(
                    "s",
                    "t",
                    0,
                    "tool",
                    "x".repeat(TOOL_PROGRESS_CHUNK_MAX_BYTES),
                    None,
                    0,
                )
                .unwrap();
        }
        let snapshot = appender.snapshot("s").unwrap();
        let tool = &snapshot.turns[0].rounds[0].blocks[0].tool.as_ref().unwrap();
        assert_eq!(tool.progress.len(), TOOL_PROGRESS_MAX_BYTES);
        assert!(tool.progress_truncated);
    }

    #[test]
    fn tool_progress_truncation_survives_terminal_tool_replacement() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "tool", TimelineBlockKind::Tool, Some(tool()))
            .unwrap();
        appender
            .append_tool_progress(
                "s",
                "t",
                0,
                "tool",
                "x".repeat(TOOL_PROGRESS_CHUNK_MAX_BYTES + 1),
                None,
                0,
            )
            .unwrap();

        let mut final_tool = tool();
        final_tool.state = TimelineToolState::Succeeded;
        final_tool.output = Some("done".into());
        appender
            .replace_tool("s", "t", 0, "tool", final_tool)
            .unwrap();

        let snapshot = appender.snapshot("s").unwrap();
        let tool = &snapshot.turns[0].rounds[0].blocks[0].tool.as_ref().unwrap();
        assert!(tool.progress.ends_with('x'));
        assert_eq!(tool.progress.len(), TOOL_PROGRESS_CHUNK_MAX_BYTES);
        assert!(tool.progress_truncated);
    }

    #[test]
    fn stale_offload_generation_does_not_shell_a_reopened_turn() {
        let mut appender = TimelineAppender::new();
        appender.enable_offload("s");
        appender.open_turn("s", "t", "first").unwrap();
        appender.seal_turn("s", "t").unwrap();
        let old = appender.offload_candidate("s", "t").unwrap();

        appender.open_turn("s", "t", "second").unwrap();
        assert!(!appender.mark_offloaded_if_current("s", "t", old.created_seq));
        let turn = &appender.snapshot("s").unwrap().turns[0];
        assert!(!turn.offloaded);
        assert!(!turn.sealed);
        assert_eq!(turn.user_text, "second");
    }

    #[test]
    fn tool_summary_keeps_non_json_text_and_prefers_display_summary() {
        assert_eq!(
            project_tool_summary(
                "read",
                TimelineToolState::Succeeded,
                None,
                Some("src/lib.rs\n---\nbody"),
            ),
            "src/lib.rs"
        );
        assert_eq!(
            project_tool_summary(
                "exec",
                TimelineToolState::Succeeded,
                Some("exit 0 · cargo check"),
                Some(r#"{"status":"completed"}"#),
            ),
            "exit 0 · cargo check"
        );
    }

    #[test]
    fn json_like_summaries_fall_back_to_name_and_state() {
        let exec_json =
            r#"{"status":"completed","command":"bash -lc ls","exit_code":0,"output":"a\nb"}"#;
        assert_eq!(
            project_tool_summary("exec", TimelineToolState::Succeeded, None, Some(exec_json)),
            "exec · succeeded"
        );

        // 被 512 字符截断的 JSON 前缀同样必须降级。
        let truncated = r#"{"status":"completed","command":"bash -lc ls","output":"aaa"#;
        assert_eq!(
            project_tool_summary("exec", TimelineToolState::Failed, None, Some(truncated)),
            "exec · failed"
        );

        // display summary 是 JSON 时也不采用，退回 legacy 文本。
        assert_eq!(
            project_tool_summary(
                "todo_write",
                TimelineToolState::Succeeded,
                Some(r#"{"status":"ok","total":3}"#),
                Some("Plan updated: 3 items"),
            ),
            "Plan updated: 3 items"
        );
    }

    #[test]
    fn json_like_detection_does_not_reject_human_brackets() {
        assert!(!is_json_like_summary("[2/5] Building"));
        assert!(!is_json_like_summary("read src/lib.rs"));
        assert!(!is_json_like_summary(""));
        assert!(is_json_like_summary(r#"[{"id":"T1"}]"#));
        assert!(is_json_like_summary(r#"[{"id":"T1"#));
        assert!(is_json_like_summary("[1, 2, 3]"));
        assert!(is_json_like_summary("{not json but brace-prefixed}"));
    }

    #[test]
    fn fallback_summary_is_bounded_and_never_empty() {
        let long = "界".repeat(qaqh_types::TOOL_SUMMARY_MAX_CHARS + 10);
        let summary = project_tool_summary("read", TimelineToolState::Succeeded, None, Some(&long));
        assert_eq!(summary.chars().count(), qaqh_types::TOOL_SUMMARY_MAX_CHARS);

        assert_eq!(
            project_tool_summary("exec", TimelineToolState::Running, None, Some("")),
            "exec · running"
        );
        assert_eq!(
            project_tool_summary("exec", TimelineToolState::Backgrounded, None, None),
            "exec · backgrounded"
        );
    }

    #[test]
    fn appender_rejects_ambiguous_block_shapes_and_late_rounds() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        assert!(matches!(
            appender.open_block("s", "t", 0, "tool", TimelineBlockKind::Tool, None),
            Err(TimelineError::InvalidBlockShape(_))
        ));
        assert!(matches!(
            appender.open_block("s", "t", 2, "text", TimelineBlockKind::Text, None),
            Err(TimelineError::RoundOutOfOrder {
                expected: 0,
                received: 2,
                ..
            })
        ));
    }

    #[test]
    fn snapshot_and_replay_form_a_lossless_recovery_boundary() {
        let mut appender = TimelineAppender::new();
        let opened = appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        let first = appender
            .append_text("s", "t", 0, "answer", 0, "hel")
            .unwrap();
        let snapshot = appender.snapshot("s").unwrap();
        let second = appender
            .append_text("s", "t", 0, "answer", 1, "lo")
            .unwrap();

        assert_eq!(opened.timeline_seq, 1);
        assert!(first.timeline_seq <= snapshot.watermark);
        let tail = appender.replay_since("s", snapshot.watermark);
        assert_eq!(tail, vec![second]);
    }

    #[test]
    fn intents_allocate_fragment_and_timeline_sequences_at_the_single_writer() {
        let mut appender = TimelineAppender::new();
        let intents = [
            TimelineIntent::TurnOpened {
                turn_id: "t".into(),
                user_text: "question".into(),
            },
            TimelineIntent::BlockOpened {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "answer".into(),
                kind: TimelineBlockKind::Text,
                tool: None,
            },
            TimelineIntent::TextDelta {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "answer".into(),
                delta: "hel".into(),
            },
            TimelineIntent::TextDelta {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "answer".into(),
                delta: "lo".into(),
            },
        ];
        let entries: Vec<_> = intents
            .into_iter()
            .map(|intent| appender.apply_intent("s", intent).unwrap())
            .collect();

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.timeline_seq)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert!(matches!(
            &entries[2].event,
            TimelineEvent::TextDelta {
                fragment_seq: 0,
                ..
            }
        ));
        assert!(matches!(
            &entries[3].event,
            TimelineEvent::TextDelta {
                fragment_seq: 1,
                ..
            }
        ));
        assert_eq!(
            appender.snapshot("s").unwrap().turns[0].rounds[0].blocks[0].text,
            "hello"
        );
    }

    #[test]
    fn checkpoint_overwrites_text_and_later_deltas_keep_appending() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        appender
            .append_text("s", "t", 0, "answer", 0, "hel")
            .unwrap();
        // 追加式整流：只发余量（`arg`），不发全量文本。
        let checkpoint = appender
            .checkpoint_block("s", "t", 0, "answer", "hello wor")
            .unwrap();
        assert!(matches!(
            checkpoint.event,
            TimelineEvent::BlockCheckpoint {
                ref block_id,
                ref arg,
                ref text,
            } if block_id == "answer"
                && arg.as_deref() == Some("lo wor")
                && text.is_empty()
        ));
        // fragment accounting is untouched: next delta validates against 1.
        appender
            .append_text("s", "t", 0, "answer", 1, "ld")
            .unwrap();
        let snapshot = appender.snapshot("s").unwrap();
        assert_eq!(snapshot.turns[0].rounds[0].blocks[0].text, "hello world");
    }

    #[test]
    fn checkpoint_via_intent_roundtrips_as_event() {
        let mut appender = TimelineAppender::new();
        let intents = [
            TimelineIntent::TurnOpened {
                turn_id: "t".into(),
                user_text: "question".into(),
            },
            TimelineIntent::BlockOpened {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "reasoning".into(),
                kind: TimelineBlockKind::Reasoning,
                tool: None,
            },
            TimelineIntent::BlockCheckpoint {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "reasoning".into(),
                text: "full thinking".into(),
            },
        ];
        let entries: Vec<_> = intents
            .into_iter()
            .map(|intent| appender.apply_intent("s", intent).unwrap())
            .collect();
        // 空块上的首次整流 = 从空串追加，`arg` 即全文（自愈语义不变）。
        assert!(matches!(
            &entries[2].event,
            TimelineEvent::BlockCheckpoint { block_id, arg, text }
                if block_id == "reasoning"
                    && arg.as_deref() == Some("full thinking")
                    && text.is_empty()
        ));
        assert_eq!(
            appender.snapshot("s").unwrap().turns[0].rounds[0].blocks[0].text,
            "full thinking"
        );
    }

    #[test]
    fn checkpoint_rejected_on_sealed_block() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        appender.seal_block("s", "t", 0, "answer").unwrap();
        assert!(matches!(
            appender.checkpoint_block("s", "t", 0, "answer", "late"),
            Err(TimelineError::SealedBlock(_))
        ));
    }

    #[test]
    fn checkpoint_rejected_on_tool_or_missing_block() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "tool:1", TimelineBlockKind::Tool, Some(tool()))
            .unwrap();
        assert!(matches!(
            appender.checkpoint_block("s", "t", 0, "tool:1", "nope"),
            Err(TimelineError::InvalidBlockKind(_))
        ));
        assert!(matches!(
            appender.checkpoint_block("s", "t", 0, "ghost", "nope"),
            Err(TimelineError::MissingBlock(_))
        ));
    }

    #[test]
    fn replay_tail_covers_every_entry_above_the_watermark() {
        // 断线重连回放尾的契约（进程内 journal）：`replay_since(watermark)` 必须
        // 逐条覆盖 watermark 之后的全部条目，且 seq 严格单调、无缺无重。
        // timeline-journal 移除后，replay 只依赖内存 journal，本测试守住该语义。
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t1", "q1").unwrap();
        appender
            .open_block(
                "s",
                "t1",
                0,
                "reasoning",
                TimelineBlockKind::Reasoning,
                None,
            )
            .unwrap();
        appender
            .append_text("s", "t1", 0, "reasoning", 0, "think")
            .unwrap();
        appender.seal_block("s", "t1", 0, "reasoning").unwrap();
        appender.seal_round("s", "t1", 0, true).unwrap();

        let all = appender.replay_since("s", 0);
        assert!(!all.is_empty(), "journal must hold the written entries");
        let watermark = appender.snapshot("s").unwrap().watermark;
        assert_eq!(
            watermark,
            all.last().unwrap().timeline_seq,
            "snapshot watermark equals the newest entry"
        );

        // 从任意水位切片：尾部必须恰好是 cut 之后的严格后缀。
        let mid = all.len() / 2;
        let cut = all[mid].timeline_seq;
        let tail = appender.replay_since("s", cut);
        assert_eq!(tail, all[mid + 1..], "tail is exactly the suffix above cut");
        assert!(
            tail.windows(2)
                .all(|w| w[0].timeline_seq < w[1].timeline_seq),
            "replay tail must be strictly monotonic"
        );
        // 水位到底 = 无条目可回放（客户端已对齐，不产生 gap）。
        assert!(appender.replay_since("s", watermark).is_empty());

        // seal **不再**清空回放尾（#314 契约变更）：sealed turn 的条目必须留到
        // 双限驱逐为止，否则「回合中途重基线」的客户端永远拿不到补齐所需的
        // `TurnSealed`（快照落在回合内 + seal 裁剪 = 回复永不渲染）。
        appender.seal_turn("s", "t1").unwrap();
        let after_seal = appender.replay_since("s", cut);
        // 下界断言（而非切片相等）：失败时给出可读信息，而不是切片越界 panic。
        // 期望 `after_seal` ⊇ `all[mid+1..]`，另加 seal 自身产出的 TurnSealed。
        assert!(
            after_seal.len() > all.len() - mid - 1,
            "seal 不得拿走回合中途水位之后的条目（after_seal={}, 期望 > {}）",
            after_seal.len(),
            all.len() - mid - 1
        );
        assert!(
            matches!(
                after_seal.last().map(|entry| &entry.event),
                Some(TimelineEvent::TurnSealed { .. })
            ),
            "补齐区间必须收在 TurnSealed 上"
        );
    }

    #[test]
    fn sealed_turn_journal_survives_until_budget_eviction() {
        // #314（2026-09-23 契约变更）：turn seal **不再**即时裁剪该 turn 的条目。
        //
        // 原契约「sealed 内容已在快照内物化、回放不再需要」在**回合中途重基线**时
        // 不成立：客户端 gap 恢复取到的快照可能落在该 turn 内，此后要靠
        // `seq > watermark` 的条目补完；裁掉就永远收不到 `TurnSealed`，回复不渲染。
        // 内存上界改由双限驱逐兜底——`journal_enforcement_bounds_entries_and_bytes`
        // 钉的就是那条。
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t1", "q1").unwrap();
        appender
            .open_block("s", "t1", 0, "r", TimelineBlockKind::Reasoning, None)
            .unwrap();
        appender
            .append_text("s", "t1", 0, "r", 0, "long reasoning text")
            .unwrap();
        appender.seal_block("s", "t1", 0, "r").unwrap();
        appender.seal_round("s", "t1", 0, false).unwrap();
        let before_seal = appender.replay_since("s", 0);
        assert!(!before_seal.is_empty());

        appender.seal_turn("s", "t1").unwrap();
        let after_seal = appender.replay_since("s", 0);
        // 下界断言（而非切片相等）：失败时给出可读信息，而不是切片越界 panic。
        assert!(
            after_seal.len() >= before_seal.len(),
            "seal 不得裁剪该 turn 的既有条目（after_seal={}, before_seal={}）",
            after_seal.len(),
            before_seal.len()
        );
        assert!(
            matches!(
                after_seal.last().map(|entry| &entry.event),
                Some(TimelineEvent::TurnSealed { .. })
            ),
            "TurnSealed 必须留在回放尾里"
        );
        let snapshot = appender.snapshot("s").unwrap();
        assert_eq!(
            snapshot.turns.len(),
            1,
            "snapshot keeps the materialized turn"
        );
        assert!(snapshot.turns[0].sealed);
        assert_eq!(
            snapshot.turns[0].rounds[0].blocks[0].text,
            "long reasoning text"
        );

        appender.open_turn("s", "t2", "q2").unwrap();
        let final_tail = appender.replay_since("s", 0);
        // 绝对下界（不只是相对增量）：t1 的条目在 t2 进来后仍必须全部留存。
        assert!(
            before_seal.iter().all(|old| final_tail
                .iter()
                .any(|new| new.timeline_seq == old.timeline_seq)),
            "后续 turn 进来后，t1 的条目仍必须全部在回放尾里"
        );
        assert_eq!(
            final_tail.len(),
            after_seal.len() + 1,
            "后续 turn 的条目照常进入回放窗口"
        );
    }

    #[test]
    fn journal_enforcement_bounds_entries_and_bytes() {
        // 双限驱逐：字节越界后最老 delta 被驱逐，且被驱逐区间是连续前缀。
        // 测试覆写为 512 KB（生产 256 MB 无法在单测内写满）。
        crate::ringing::persistence_policy::set_journal_byte_limit_for_test(512 * 1024);
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t1", "long stream").unwrap();
        appender
            .open_block("s", "t1", 0, "r", TimelineBlockKind::Reasoning, None)
            .unwrap();
        // 600 条 x 1 KB = 600 KB payload > 512 KB 测试字节界。
        let chunk = "x".repeat(1024);
        for seq in 0..600u64 {
            appender
                .append_text("s", "t1", 0, "r", seq, chunk.clone())
                .unwrap();
        }
        let tail = appender.replay_since("s", 0);
        assert!(
            tail.len() < 600,
            "byte budget must evict old deltas, got {} entries",
            tail.len()
        );
        let fragment_seqs: Vec<u64> = tail
            .iter()
            .filter_map(|e| match &e.event {
                TimelineEvent::TextDelta { fragment_seq, .. } => Some(*fragment_seq),
                _ => None,
            })
            .collect();
        // 窗口必须覆盖最新 delta，且保留区连续。
        assert!(
            fragment_seqs.contains(&599),
            "newest delta must survive eviction, got tail len {}",
            tail.len()
        );
        let min_kept = *fragment_seqs.iter().min().unwrap();
        let expected: Vec<u64> = (min_kept..600).collect();
        assert_eq!(
            fragment_seqs, expected,
            "retained window must be contiguous"
        );

        // 条数上限分支：微小 delta 推过 8192 条后同样头部驱逐。
        let mut many = TimelineAppender::new();
        many.open_turn("s2", "t1", "many deltas").unwrap();
        many.open_block("s2", "t1", 0, "r", TimelineBlockKind::Reasoning, None)
            .unwrap();
        for seq in 0..8500u64 {
            many.append_text("s2", "t1", 0, "r", seq, "x".to_string())
                .unwrap();
        }
        let bounded = many.replay_since("s2", 0);
        assert!(
            bounded.len() <= 8192,
            "entry budget must bound the replay tail, got {}",
            bounded.len()
        );
        let last_seq = bounded.last().unwrap().timeline_seq;
        // 最新条目仍在窗口内 → watermark 与其 seq 一致；
        // watermark 与窗口头部的差值 = 被驱逐的前缀长度。
        assert_eq!(
            many.snapshot("s2").unwrap().watermark,
            last_seq,
            "newest allocated entry must remain in the bounded tail"
        );
    }

    #[test]
    fn restore_rebuilds_journal_byte_budget() {
        // restore 后字节预算必须与 journal 实际 payload 一致。
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t1", "q").unwrap();
        appender
            .open_block("s", "t1", 0, "r", TimelineBlockKind::Reasoning, None)
            .unwrap();
        appender
            .append_text("s", "t1", 0, "r", 0, "payload-123")
            .unwrap();
        appender
            .open_block("s", "t1", 0, "tool", TimelineBlockKind::Tool, Some(tool()))
            .unwrap();
        appender
            .append_tool_progress("s", "t1", 0, "tool", "progress".into(), None, 0)
            .unwrap();
        let mut final_tool = tool();
        final_tool.summary = Some("summary".into());
        final_tool.output = Some("output".into());
        final_tool.diff = Some("diff".into());
        appender
            .replace_tool("s", "t1", 0, "tool", final_tool)
            .unwrap();
        let journal = appender.replay_since("s", 0);
        let snapshot = appender.snapshot("s").unwrap();

        let mut restored = TimelineAppender::new();
        restored.restore("s".into(), snapshot, journal);
        let expected = "payload-123".len()
            + "progress".len()
            + "summary".len()
            + "output".len()
            + "diff".len()
            + "progress".len();
        assert!(expected > 0);
        let seed = restored.seeds.get("s").unwrap();
        assert_eq!(seed.journal_bytes, expected as u64);
    }

    /// 8192 条上限之上的摊还成本必须与上限之内同阶（O(1)），且驱逐只丢最老前缀。
    ///
    /// BUG-2026-09-12-13：`enforce_journal_budget` 用 `Vec::remove(0)` 头部驱逐，
    /// 每条新事件 memmove 整个 journal；报告实测 2.21 µs → 115.88 µs（16000 条）
    /// /182.83 µs（32768 条），52–83× 阶跃，且全程持全局 timeline 锁。修法是
    /// `VecDeque::pop_front()`（摊还 O(1)）。
    ///
    /// 阈值是**同机比值**而非绝对时长（无绝对耗时阈值，机器无关）：越界窗口的
    /// 平均每事件耗时不得超过上限内窗口的 2.5×——O(1) 驱逐下该比值恒 ≈1，O(n)
    /// memmove 下随 CAP 线性放大。修复前本机实测 ratio **7.1**（debug，38.2 vs
    /// 1.3 µs/event）；修复后 **0.24–0.96**（示例 1.30 vs 1.36 µs/event，越界侧
    /// 甚至更快，因 int 编码后少一条移除）。2.5× 两侧仍各有 ≥2× 余量。
    #[test]
    fn journal_eviction_is_amortized_constant_time_past_the_entry_cap() {
        const CAP: u64 = 8192;
        const WINDOW: u64 = 2048;
        const BUDGET_RATIO: f64 = 2.5;

        // 单条 payload 64 B：~0.5 MB 总量，远离 256 MB 字节上限，
        // 保证越界驱逐**只**由条数上限触发（否则测的不是这条路径）。
        let chunk = "x".repeat(64);
        let mut appender = TimelineAppender::new();
        appender
            .open_turn("perf", "t1", "high frequency deltas")
            .unwrap();
        appender
            .open_block("perf", "t1", 0, "r", TimelineBlockKind::Reasoning, None)
            .unwrap();

        // 窗口 A：上限之内写 CAP 条（无驱逐）。
        let inside_start = std::time::Instant::now();
        for seq in 0..CAP {
            appender
                .append_text("perf", "t1", 0, "r", seq, chunk.clone())
                .unwrap();
        }
        let inside = inside_start.elapsed();

        // 窗口 B：越过上限，每条驱逐一条最老条目。
        let over_start = std::time::Instant::now();
        for seq in CAP..(CAP + WINDOW) {
            appender
                .append_text("perf", "t1", 0, "r", seq, chunk.clone())
                .unwrap();
        }
        let over = over_start.elapsed();

        let tail = appender.replay_since("perf", 0);
        assert_eq!(
            tail.len() as u64,
            CAP,
            "replay tail must stay pinned at the entry cap while evicting"
        );
        assert_eq!(
            tail.last().map(|entry| entry.timeline_seq),
            Some(CAP + WINDOW + 2),
            "newest delta must survive eviction"
        );
        let inside = inside.as_secs_f64().max(1e-9);
        assert!(
            over.as_secs_f64() <= inside * BUDGET_RATIO,
            "eviction past the cap must stay O(1): {:.3} µs/event over the whole \
             {CAP}-entry run vs {:.3} µs/event while evicting (ratio {:.2} > {BUDGET_RATIO})",
            inside * 1e6 / CAP as f64,
            over.as_secs_f64() * 1e6 / WINDOW as f64,
            over.as_secs_f64() / inside,
        );
    }
}

#[cfg(test)]
mod display_mapping_tests {
    use super::*;

    #[test]
    fn wire_display_maps_sdk_types_and_defers_metrics() {
        use qaqh_workspace::tool_api as sdk;

        let display = sdk::ToolDisplay::new(
            sdk::ToolHeader::Shell {
                command: "bash ls".into(),
            },
            sdk::ToolBody::Shell {
                output: "ok".into(),
                exit_code: Some(0),
                truncated: false,
            },
        )
        .with_summary("exit 0 · bash ls")
        .with_outcome(sdk::ToolDisplayOutcome {
            state: sdk::ToolTerminalState::Succeeded,
            exit_code: Some(0),
            duration_ms: Some(12),
            output_bytes: Some(2),
            truncated: Some(false),
        });
        let wire = wire_display(&display);

        assert_eq!(wire.summary.as_deref(), Some("exit 0 · bash ls"));
        assert!(
            matches!(wire.header, Some(TimelineToolHeader::Shell { ref command }) if command == "bash ls")
        );
        assert!(matches!(
            wire.body,
            Some(TimelineToolBody::Shell {
                exit_code: Some(0),
                ..
            })
        ));
        assert!(wire.metrics.is_none(), "metrics 未接线时不得伪造");
        let outcome = wire.outcome.expect("structured outcome");
        assert_eq!(
            outcome.state,
            qaqh_types::ToolResultDisplayOutcomeState::Succeeded
        );
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.duration_ms, Some(12));
    }

    #[test]
    fn wire_display_maps_separated_streams() {
        use qaqh_workspace::tool_api as sdk;

        let display = sdk::ToolDisplay::new(
            sdk::ToolHeader::Shell {
                command: "sh -c 'echo out; echo err >&2'".into(),
            },
            sdk::ToolBody::Streams {
                stdout: "out\n".into(),
                stderr: "err\n".into(),
                exit_code: Some(1),
                truncated: false,
                interleaved: false,
            },
        );
        let wire = wire_display(&display);
        assert!(matches!(
            wire.body,
            Some(TimelineToolBody::Streams {
                stdout,
                stderr,
                exit_code: Some(1),
                interleaved: false,
                ..
            }) if stdout == "out\n" && stderr == "err\n"
        ));
    }

    #[test]
    fn wire_display_emits_framework_metrics_once_elapsed_is_known() {
        use qaqh_workspace::tool_api as sdk;

        let mut display = sdk::ToolDisplay::new(
            sdk::ToolHeader::Shell {
                command: "bash ls".into(),
            },
            sdk::ToolBody::Shell {
                output: "ok".into(),
                exit_code: Some(0),
                truncated: false,
            },
        )
        .with_summary("exit 0 · bash ls")
        .with_outcome(sdk::ToolDisplayOutcome {
            state: sdk::ToolTerminalState::Succeeded,
            exit_code: Some(0),
            duration_ms: None,
            output_bytes: None,
            truncated: Some(false),
        });
        apply_result_metrics(
            &mut display,
            &qaqh_types::ToolResultMetrics {
                elapsed_ms: Some(1234),
                output_bytes: 2,
                retry_count: 0,
                effective_tool_name: None,
                user_initiated: true,
            },
        );
        let wire = wire_display(&display);
        let metrics = wire.metrics.expect("elapsed 已知时必须产出 metrics");
        assert_eq!(metrics.elapsed_ms, 1234);
        assert_eq!(metrics.output_bytes, 2);
        assert!(metrics.user_initiated);
        assert!(metrics.effective_tool_name.is_none());
        let outcome = wire.outcome.expect("structured outcome");
        assert_eq!(outcome.duration_ms, Some(1234));
        assert_eq!(outcome.output_bytes, Some(2));
    }
}
