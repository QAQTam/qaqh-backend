//! Transcript 状态机：协议事件 → 声明式视图状态。
//!
//! 协议事件只负责增量更新 [`Transcript`]；UI 从 Transcript 声明
//! `Element` 树，稳定 key 与 `windows-reactor` 的 reconciler 负责把状态
//! 变化提交到 XAML 控件树。这里不维护第二套命令式控件 patch 协议。
//!
//! XAML 渲染模型对应：
//! ```text
//! ConversationTranscript (ScrollViewer, 跟随尾部 + 锚点补偿)
//! └─ StackPanel（append-only：新 turn 只 push 尾部）
//!    └─ TurnView
//!       ├─ 用户气泡（TextBlock）
//!       └─ RoundView × N
//!          ├─ Thinking  → Expander（摘要随流更新）
//!          ├─ Answer    → Streaming: 轻量 TextBlock（每帧替换 Inlines）
//!          │              Final:     RichTextBlock（parse_final 一次构建）
//!          └─ ToolCall  → ToolCard（upsert by tool_call_id）
//! ```
//!
//! 核心不变量（协议局域化，见设计讨论）：
//! 1. `RoundCompleted` 前只更新对应 round 的活尾；
//! 2. `RoundCompleted` 后答案冻结，迟到 delta 被忽略；
//! 3. 事件按 `(turn_id, round_num)` O(1) 定位，渲染窗口与完整历史分离；
//! 4. 返回值只描述本帧最低失效等级，不复制 RichText/工具卡等渲染载荷。

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use markdown_core::ast::{Block, Inline};
use markdown_core::live::parse_live;
use markdown_core::gfm_live_table::GfmTableTracker;
use markdown_core::live_table::{LiveTableTracker, TableHiddenSpan, TableSnapshot};
use markdown_core::parse_final;

use crate::protocol::{ConversationEvent, ProviderToolState, RoundDeltaKind};
use crate::{
    ChangeStats, RichTextOutput, TableData, ToolBody, change_stats_from_result, render_final,
    tool_body_from_result,
};

/// The smallest declarative render invalidation needed after a model update.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum TranscriptInvalidation {
    #[default]
    None,
    Live,
    Structural,
}

/// Compact result of applying one event or one presentation-frame batch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TranscriptChange {
    pub invalidation: TranscriptInvalidation,
    /// Content mounted or changed height, so a near-tail viewport may need a
    /// post-layout follow request.
    pub extent_changed: bool,
}

impl TranscriptChange {
    pub(crate) const fn live(extent_changed: bool) -> Self {
        Self {
            invalidation: TranscriptInvalidation::Live,
            extent_changed,
        }
    }

    pub(crate) const fn structural(extent_changed: bool) -> Self {
        Self {
            invalidation: TranscriptInvalidation::Structural,
            extent_changed,
        }
    }

    pub fn merge(&mut self, other: Self) {
        self.invalidation = self.invalidation.max(other.invalidation);
        self.extent_changed |= other.extent_changed;
    }

    pub fn changed(&self) -> bool {
        self.invalidation != TranscriptInvalidation::None
    }

    pub fn is_structural(&self) -> bool {
        self.invalidation == TranscriptInvalidation::Structural
    }
}

/// An external final answer requested by `RoundCompleted.output_ref`.
///
/// This is model-owned work, not a UI patch command. The application drains
/// these requests, resolves them through the Ringing content endpoint, then
/// calls [`Transcript::resolve_output`] or [`Transcript::fail_output`].
#[derive(Clone, Debug, PartialEq)]
pub struct PendingOutput {
    pub turn_id: String,
    pub round_num: u32,
    pub reference: serde_json::Value,
}

/// TurnFailed 的 DomainError（`{error_id, code, message, retryable, ..}`）
/// 提取为 UI 可显示文本；未知形状时给出兜底文案。
fn extract_failed_error(error: &serde_json::Value) -> String {
    let code = error
        .get("code")
        .and_then(|v| v.as_str())
        .unwrap_or("turn_failed");
    let message = error
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("Model request failed");
    let retryable = error
        .get("retryable")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if retryable {
        format!("{code}: {message}（可重试）")
    } else {
        format!("{code}: {message}")
    }
}

/// turn 生命周期（渲染用；对齐协议 `TurnStarted/TurnCompleted/TurnFailed`）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TurnStatus {
    #[default]
    Running,
    Completed,
    Failed,
}

/// 工具卡视图（流式累积，id 稳定）。
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCardView {
    pub id: String,
    /// 从 ToolCalling 流中提取的工具名（未解析出时为 None）。
    pub name: Option<String>,
    /// 参数 raw（原型简化：直接展示累积文本）；provider 卡为状态文案。
    pub args_display: String,
    /// Original structured arguments, retained so patch/read renderers can
    /// derive a useful body even when the terminal receipt is compact.
    pub args_json: Option<String>,
    /// Typed native presentation instead of one undifferentiated text blob.
    pub body: ToolBody,
    /// Optional file-change totals delivered by the code-change channel.
    pub changes: Option<ChangeStats>,
    /// true = 工具卡完成（后续 delta 不再更新）。
    pub done: bool,
    /// Failed 终态标记（渲染 ✕ 红色 + failure 摘要；头部按最差状态显示）。
    pub failed: bool,
    /// 失败摘要（"code: message"，TimelineTool.failure 透传）。
    pub failure: Option<String>,
    /// provider 内建工具卡（web_search 等，`provider_tool_status` 事件）：
    /// 无参数流，展开区显示执行状态（args_display 承载）。
    pub provider: bool,
    /// 工具是否已真正开始执行（ToolStarted / state=Running）。
    /// Prepared（LLM 刚吐出 tool_call 的 replaceable 预览）为 false——
    /// 前端在 started 前不渲染，避免「一收到调用就闪烁」。
    pub started: bool,
}

/// 恢复的回合（timeline 快照解析产物；`Transcript::restore` 消费）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RestoredRound {
    pub round_num: u32,
    pub thinking: Option<String>,
    /// 答案 markdown 原文（kind=text 块按 block_order 拼接）。
    pub answer: Option<String>,
    pub tool_calls: Vec<ToolCardView>,
}

/// 恢复的 turn（timeline 快照解析产物；`Transcript::restore` 消费）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RestoredTurn {
    pub turn_id: String,
    /// 快照里的权威创建序（TimelineTurn.created_seq）；0 = 未知（旧数据），
    /// 排序时退化为 turn_id 数值兜底。
    pub created_seq: u64,
    pub user_text: String,
    pub status: TurnStatus,
    pub rounds: Vec<RestoredRound>,
}

/// 一个 turn 的视图状态（append-only 累积）。
#[derive(Clone, Debug, Default)]
pub struct TurnView {
    pub turn_id: String,
    pub user_text: String,
    pub status: TurnStatus,
    /// TurnFailed 的错误信息（`code: message`，含 retryable 提示），UI 显示用。
    pub failed_error: Option<String>,
    /// Round payloads use copy-on-write identity. Cloning a changed turn for the
    /// declarative tree therefore stays shallow; completed rounds keep the same
    /// `Rc` and are eligible for round-level memo skips.
    pub rounds: Vec<Rc<RoundView>>,
    /// 仅本 turn 可见内容变化时递增。ChatView 的窗口快照用它复用
    /// 未变化 turn 的 `Rc<TurnView>`，避免每个 token 都深拷贝整个窗口。
    pub mutation_rev: u64,
}

/// 一个 round 的视图状态。
#[derive(Clone, Debug, Default)]
pub struct RoundView {
    pub round_num: u32,
    pub thinking: Option<String>,
    pub answer: AnswerView,
    pub tool_calls: Vec<ToolCardView>,
    /// An external final answer is being resolved through the content service.
    pub output_loading: bool,
    /// Content resolution failed; the live preview remains visible and the
    /// error is rendered alongside it instead of silently producing a blank.
    pub output_error: Option<String>,
    /// Last external reference, retained to make replayed completion events
    /// idempotent after the content has already been resolved.
    output_ref: Option<serde_json::Value>,
    /// Authoritative final markdown used to make replayed completions cheap.
    final_raw: Option<String>,
    /// 正在累积的工具调用 raw（未完成的 ToolCalling 流）。
    tool_raw: String,
    /// 仅本 round 的可见内容变化时递增。声明树用它在同一 turn 内跳过
    /// 已完成的 rounds，只重建当前活尾或状态发生变化的工具卡。
    pub mutation_rev: u64,
}

/// 流式答案的可见内容片段（字面文本与表格交错；UI 按序渲染）。
#[derive(Clone, Debug, PartialEq)]
pub enum LiveSegment {
    /// 字面文本片段（表格外内容；未闭合语法照旧字面输出）。
    Text(String),
    /// 流式表格（网格渲染；未闭合表格含残行 partial 作为末行）。
    Table(TableData),
}

/// 答案视图状态机：流式预览 → 权威终态。
///
/// 表格跟踪器自持在 `Streaming` 内（协议表格 + GFM 表格），使状态机可被
/// round 模型（[`RoundView`]）与 block 模型（[`crate::block_transcript`]）
/// 共用——两个 reducer 对答案流的处理是同一份实现。
#[derive(Clone, Debug, PartialEq)]
pub enum AnswerView {
    /// 流式中：字面/表格交错序列（仅已闭合语法；未闭合字面输出；
    /// 协议表格渐进长出，残行逐字生长在网格末行）。
    Streaming {
        raw: String,
        inlines: Vec<Inline>,
        segments: Vec<LiveSegment>,
        table_tracker: LiveTableTracker,
        gfm_table_tracker: GfmTableTracker,
    },
    /// 权威终态：全量块（冻结，不再变化）。
    Final {
        blocks: Vec<Block>,
        rich: RichTextOutput,
    },
}

impl Default for AnswerView {
    fn default() -> Self {
        Self::Streaming {
            raw: String::new(),
            inlines: Vec::new(),
            segments: Vec::new(),
            table_tracker: LiveTableTracker::default(),
            gfm_table_tracker: GfmTableTracker::default(),
        }
    }
}

impl AnswerView {
    /// 当前是否处于流式且有可见内容（失败冻结兜底判定用）。
    pub fn streaming_nonempty(&self) -> bool {
        matches!(self, Self::Streaming { raw, .. } if !raw.trim().is_empty())
    }

    /// 追加增量：累积 raw → 表格跟踪 → 行内预览。
    ///
    /// 未闭合语法跨 delta 边界（`**bo` + `ld**`），必须对**整段 raw**
    /// 重解析（O(段长)；UI 层以 DispatcherQueue 节流合并，同 Web rAF）。
    /// 协议表格（```table）行级确认：表格行从字面剥离进 `segments`，
    /// 残行实时显示在网格末行（逐字生长），字面保留在 Text 段。
    /// 解析结果同步写回 `Self::Streaming`（状态机自持）。
    pub fn live_delta(&mut self, delta: &str) -> bool {
        if delta.is_empty() {
            return false;
        }
        let Self::Streaming {
            raw,
            inlines,
            segments,
            table_tracker,
            gfm_table_tracker,
        } = self
        else {
            return false; // 终态后忽略 delta（协议保证不会发生）
        };
        raw.push_str(delta);
        // 表格跟踪：增量行扫描（O(新增行)）——协议表格 + GFM 表格。
        table_tracker.feed(raw);
        gfm_table_tracker.feed(raw);
        // 可见字面 = raw 减去表格隐藏区间；segments = 字面/表格交错
        let (visible, segs) = split_segments(raw, table_tracker, gfm_table_tracker);
        *inlines = parse_live(&visible);
        *segments = segs;
        true
    }

    /// BlockCheckpoint 覆盖（自愈）：整段替换，重解析（表格跟踪器重置）。
    /// 相同值幂等（防抖：不产生模型失效）。
    pub fn live_checkpoint(&mut self, text: &str) -> bool {
        let Self::Streaming {
            raw,
            inlines,
            segments,
            table_tracker,
            gfm_table_tracker,
        } = self
        else {
            return false;
        };
        if *raw == text {
            return false;
        }
        table_tracker.reset();
        gfm_table_tracker.reset();
        raw.clear();
        raw.push_str(text);
        table_tracker.feed(raw);
        gfm_table_tracker.feed(raw);
        let (visible, segs) = split_segments(raw, table_tracker, gfm_table_tracker);
        *inlines = parse_live(&visible);
        *segments = segs;
        true
    }

    /// 权威终态：以完整文本全量重建为 Final（冻结，忽略流式累积差异）。
    /// 幂等：已是 Final 时返回 false。
    pub fn finalize_text(&mut self, text: &str) -> bool {
        let Self::Streaming { .. } = self else {
            return false;
        };
        let blocks = parse_final(text);
        let rich = render_final(&blocks);
        *self = Self::Final { rich, blocks };
        true
    }
}

impl RoundView {
    fn new(round_num: u32) -> Self {
        Self {
            round_num,
            ..Self::default()
        }
    }

    /// 追加 Answering 增量（委托 [`AnswerView::live_delta`]，同一份答案状态机）。
    fn answer_delta(&mut self, delta: &str) -> bool {
        self.answer.live_delta(delta)
    }

    /// BlockCheckpoint 覆盖（委托 [`AnswerView::live_checkpoint`]）。
    fn answer_checkpoint(&mut self, text: &str) -> bool {
        self.answer.live_checkpoint(text)
    }

    /// RoundCompleted：以权威 answer 全量重建（忽略流式累积差异）。
    fn finalize(&mut self, thinking: Option<&str>, answer: Option<&str>) -> bool {
        let mut changed = false;
        if let Some(t) = thinking {
            if self.thinking.as_deref() != Some(t) {
                self.thinking = Some(t.to_string());
                changed = true;
            }
        }
        if let Some(a) = answer {
            if self.final_raw.as_deref() != Some(a) {
                // final_raw 不同 ⇒ 文本不同或仍流式 ⇒ finalize_text 必返回 true
                self.answer.finalize_text(a);
                self.final_raw = Some(a.to_string());
                changed = true;
            }
            if self.output_loading || self.output_error.is_some() {
                self.output_loading = false;
                self.output_error = None;
                changed = true;
            }
        }
        changed
    }

    /// ToolCalling 增量：累积并尝试提取工具名（upsert by id）。
    fn tool_delta(&mut self, delta: &str) -> bool {
        if self.tool_calls.last().is_some_and(|c| c.done) {
            return false; // 上一张卡已完成
        }
        if delta.is_empty() {
            return false;
        }
        self.tool_raw.push_str(delta);
        self.upsert_current_card().is_some()
    }

    fn tool_checkpoint(&mut self, text: &str) -> bool {
        if self.tool_raw == text {
            return false;
        }
        self.tool_raw.clear();
        self.tool_raw.push_str(text);
        self.upsert_current_card().is_some()
    }

    /// 把当前累积的卡写入 tool_calls（同 id 更新，否则新建）。
    fn upsert_current_card(&mut self) -> Option<ToolCardView> {
        let card = self.current_tool_card()?;
        if let Some(existing) = self
            .tool_calls
            .iter_mut()
            .find(|c| !c.id.is_empty() && c.id == card.id)
        {
            existing.name = card.name.clone();
            existing.args_display.clone_from(&card.args_display);
            existing.args_json.clone_from(&card.args_json);
            existing.body.clone_from(&card.body);
            existing.changes.clone_from(&card.changes);
        } else {
            self.tool_calls.push(card.clone());
        }
        Some(card)
    }

    /// 从累积 raw 提取工具卡（原型简化解析：`"name":"..."` 与 `"id":"..."`）。
    fn current_tool_card(&self) -> Option<ToolCardView> {
        if self.tool_raw.trim().is_empty() {
            return None;
        }
        let id = extract_json_str(&self.tool_raw, "id").unwrap_or_default();
        let name = extract_json_str(&self.tool_raw, "name");
        Some(ToolCardView {
            id,
            name,
            args_display: self.tool_raw.clone(),
            args_json: Some(self.tool_raw.clone()),
            body: ToolBody::Text(self.tool_raw.clone()),
            changes: None,
            done: false,
            failed: false,
            failure: None,
            provider: false,
            started: true,
        })
    }

    /// 工具调用完成（RoundCompleted 时收尾所有卡）。
    fn finish_tool_cards(&mut self) -> bool {
        let mut changed = false;
        for card in &mut self.tool_calls {
            changed |= !card.done;
            card.done = true;
        }
        changed
    }
}

/// 极简 JSON 字符串提取（原型用；正式实现由应用层工具卡解析器承担）。
fn extract_json_str(raw: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let idx = raw.find(&needle)?;
    let after = &raw[idx + needle.len()..];
    let colon = after.find(':')?;
    let value = &after[colon + 1..].trim_start();
    let value = value.strip_prefix('"')?;
    let end = value.find('"')?;
    Some(value[..end].to_string())
}

/// raw → (可见字面拼接, 字面/表格交错序列)。
///
/// 按表格隐藏区间切分：区间外字面进 Text 段，区间内进 Table 段
/// （sealed 表格为完整快照；打开表格含残行 partial 作为网格末行）。
/// 残行不重复出现在 Text 段（`open_tail_start` 截断）。
/// 字面/表格分段：协议表格（```table）与 GFM 表格（|...|）各自隐藏
/// 区间合并（GFM tracker 围栏感知 → 两者 span 不重叠），按起点排序后
/// 统一产出可见字面 + 表格段。尾部字面到两个 tracker 中更早的残行起点。
fn split_segments(
    raw: &str,
    protocol_tracker: &LiveTableTracker,
    gfm_tracker: &GfmTableTracker,
) -> (String, Vec<LiveSegment>) {
    let mut spans: Vec<(TableHiddenSpan, TableSnapshot)> = protocol_tracker.tables_with_spans();
    spans.extend(gfm_tracker.tables_with_spans());
    spans.sort_by_key(|(s, _)| s.start);
    let mut segments: Vec<LiveSegment> = Vec::new();
    let mut visible = String::new();
    let mut prev = 0usize;
    for (span, snap) in spans {
        if span.start > prev {
            let text = raw[prev..span.start].to_string();
            visible.push_str(&text);
            segments.push(LiveSegment::Text(text));
        }
        segments.push(LiveSegment::Table(table_snapshot_to_data(snap)));
        prev = span.end;
    }
    // 尾部字面：从 prev 到残行起点（残行已在网格末行，不重复显示）
    let tail_end = [protocol_tracker.open_tail_start(), gfm_tracker.open_tail_start()]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(raw.len());
    if prev < tail_end {
        let text = raw[prev..tail_end].to_string();
        visible.push_str(&text);
        segments.push(LiveSegment::Text(text));
    }
    (visible, segments)
}

/// 协议表格快照 → 渲染用 TableData（单元格包一层纯文本 Inline）。
fn table_snapshot_to_data(s: TableSnapshot) -> TableData {
    TableData {
        headers: s
            .headers
            .iter()
            .map(|h| vec![Inline::Text(h.clone())])
            .collect(),
        rows: s
            .rows
            .iter()
            .map(|r| r.iter().map(|c| vec![Inline::Text(c.clone())]).collect())
            .collect(),
    }
}

/// 会话级渲染状态机。
#[derive(Clone, Debug, Default)]
pub struct Transcript {
    turns: Vec<TurnView>,
    /// turn_id → index（长会话 O(1) 寻址，不做全量扫描）。
    turn_index: HashMap<String, usize>,
    /// 渲染窗口起点（turns 下标）：`[window_start, turns.len())` 是实际
    /// 传给 list_view 的回合。restore 后只渲染最近 `WINDOW_DEFAULT_LEN`
    /// 个回合；向上滚动接近顶部时 `expand_window` 前移起点（预加载更早
    /// 回合）。窗口是**渲染投影**：`turns()`/`turn_count()` 仍返回全量，
    /// 增量事件与 key 稳定性不受影响。
    window_start: usize,
    /// 窗口是否处于「跟随尾部」模式：restore 后 true；用户上滚扩展
    /// （`expand_window`）置 false（窗口保持，避免浏览内容跳动）；
    /// `slide_window_tail` 恢复 true。
    tail_following: bool,
    /// External final answers waiting for the application transport to load.
    pending_outputs: Vec<PendingOutput>,
    /// 渲染投影版本号：仅当窗口结构或某个 turn 的可见内容真实变化时递增。
    /// ChatView 据此复用窗口快照；每个 TurnView 另有局部 mutation_rev，
    /// 使变化帧也能结构共享未变化行。协议 no-op 不再使缓存失效。
    rev: u64,
}

/// 默认渲染窗口大小（回合数）：restore 后只渲染最近 N 个回合，与总回合
/// 数解耦，restore/每帧 diff 成本恒定。经实机手感调优。
pub const WINDOW_DEFAULT_LEN: usize = 30;

/// restore 窗口的 round 预算：尾部累计 rounds 超过此值即收缩窗口
/// （超大回合会话：30 turns 可能含 600+ rounds / 1800+ blocks，一次
/// mount 数千 XAML 元素 → 切换标签秒级卡顿）。200 rounds ≈ 500 blocks，
/// debug 构建单次 mount 可接受；可经实机手感调优。
pub const RESTORE_ROUND_BUDGET: usize = 200;

/// 渲染窗口之前最多保留的 turn 数量（上滚预加载缓冲）。
/// 超出此限制的最旧 turn 在 `slide_window_tail` / `restore` 时被驱逐，
/// 以限制长对话的内存占用。用户上滚超出缓冲时由分页拉取兜底。
pub const MAX_RETAINED_BEFORE_WINDOW: usize = 100;

/// restore 时最多保留的 turn 数量（窗口 + 上滚缓冲）。
///
/// `chat_adapter::restored_turns` 在**解析前**只取尾部这么多 turn，
/// 窗口外的历史由 daemon 分页（`spawn_fetch_earlier`）按需回放——
/// 对齐 Reasonix history-slice 的窗口化思路：内存只驻留可立即浏览的
/// 部分，更早内容按页从后端申请。9MB 快照 + 数百 turn 的会话不再
/// 全量解析驻留（restore 秒级 → 毫秒级，内存降低一个量级）。
pub const RESTORE_KEEP_TURNS: usize = WINDOW_DEFAULT_LEN + MAX_RETAINED_BEFORE_WINDOW;

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    /// 已挂载 turn 数（规模观测）。
    pub fn turn_count(&self) -> usize {
        self.turns.len()
    }

    pub fn turns(&self) -> &[TurnView] {
        &self.turns
    }

    /// 当前渲染窗口（尾部连续区间切片）——list_view 的实际数据源。
    /// 窗口化后每帧 clone 量 ≤ `WINDOW_DEFAULT_LEN`。
    pub fn window_turns(&self) -> &[TurnView] {
        &self.turns[self.window_start..]
    }

    /// 窗口内回合数（= list_view 行数）。
    pub fn window_len(&self) -> usize {
        self.turns.len() - self.window_start
    }

    /// 当前变更版本号（见 [`Self::rev`] 注释）。
    pub fn mutation_rev(&self) -> u64 {
        self.rev
    }

    /// 向前扩展窗口（预加载更早回合）：起点前移 `by`，钳制到 0。
    /// 返回实际前移量；0 = 已全量放行（调用方短路，避免无谓渲染）。
    /// 扩展后窗口脱离「跟随尾部」模式（用户上滚浏览中）。
    pub fn expand_window(&mut self, by: usize) -> usize {
        let moved = self.window_start.min(by);
        if moved > 0 {
            self.window_start -= moved;
            self.tail_following = false;
            self.bump_rev();
        }
        moved
    }

    /// 是否已全量放行（窗口覆盖全部 turns）。
    pub fn window_full(&self) -> bool {
        self.window_start == 0
    }

    /// 窗口是否处于「跟随尾部」模式（调用方据此决定新回合到达时是否
    /// 调用 [`Self::slide_window_tail`]）。
    pub fn tail_following(&self) -> bool {
        self.tail_following
    }

    /// 窗口滑向末尾（跟随尾部语义）：起点右移，保持窗口大小为
    /// `WINDOW_DEFAULT_LEN`，并恢复「跟随尾部」模式。由调用方在「新
    /// turn 到达且本帧跟随尾部」时显式调用——用户上滚浏览时**不要**
    /// 调用（窗口保持，避免视口跳动）。
    ///
    /// 同时驱逐最早期的 turn（超出 [`MAX_RETAINED_BEFORE_WINDOW`] 缓冲），
    /// 以限制长对话的内存占用。
    pub fn slide_window_tail(&mut self) {
        let old_start = self.window_start;
        let old_len = self.turns.len();
        let old_following = self.tail_following;
        let keep = WINDOW_DEFAULT_LEN;
        if self.turns.len() > keep {
            self.window_start = self.window_start.max(self.turns.len() - keep);
        }
        self.tail_following = true;
        self.evict_old_turns();
        if self.window_start != old_start
            || self.turns.len() != old_len
            || self.tail_following != old_following
        {
            self.bump_rev();
        }
    }

    /// 驱逐渲染窗口前超出缓冲的最旧 turn，释放其内存。
    ///
    /// 保留 `MAX_RETAINED_BEFORE_WINDOW` 个 turn 在窗口前作为上滚预加载缓冲；
    /// 超出部分从 `turns` 头部移除，同时重建 `turn_index` 并校正 `window_start`。
    /// 用户上滚超出缓冲时由 `spawn_fetch_earlier`（服务端分页）兜底。
    fn evict_old_turns(&mut self) {
        let keep_before = MAX_RETAINED_BEFORE_WINDOW.min(self.window_start);
        let evict = self.window_start - keep_before;
        if evict == 0 {
            return;
        }
        self.turns.drain(..evict);
        self.window_start -= evict;
        self.turn_index = self
            .turns
            .iter()
            .enumerate()
            .map(|(i, t)| (t.turn_id.clone(), i))
            .collect();
    }

    /// 裁剪到仅保留渲染窗口内的 turn（会话缓存用，降低 clone 体积）。
    /// 返回后 `turns` 只含 `window_turns()` 的内容，`window_start` 归零。
    /// 被裁剪的旧 turn 在切回时由 daemon 权威快照恢复。
    pub fn trim_to_window(&mut self) {
        if self.window_start == 0 {
            return;
        }
        self.turns.drain(..self.window_start);
        self.window_start = 0;
        self.turn_index = self
            .turns
            .iter()
            .enumerate()
            .map(|(i, t)| (t.turn_id.clone(), i))
            .collect();
        self.bump_rev();
    }

    /// 前插一页更早的回合（分页加载：resume 只取尾部页，上滚翻页把更早
    /// 的页插到最前）。已存在的 turn_id 跳过（页码边界可能重叠）；窗口
    /// 起点右移 `n` 保持渲染窗口位置（新回合在窗口**前面**，chat_view
    /// 以 `n` 做锚定补偿，视口不跳）。返回实际前插数；0 = 无新回合。
    pub fn prepend_turns(&mut self, turns: Vec<RestoredTurn>) -> usize {
        let known: HashSet<&str> = self.turns.iter().map(|t| t.turn_id.as_str()).collect();
        let fresh: Vec<RestoredTurn> = turns
            .into_iter()
            .filter(|t| !known.contains(t.turn_id.as_str()))
            .collect();
        if fresh.is_empty() {
            return 0;
        }
        let n = fresh.len();
        let mut new_turns: Vec<TurnView> = fresh.into_iter().map(to_turn_view).collect();
        new_turns.append(&mut self.turns);
        self.turns = new_turns;
        self.turn_index = self
            .turns
            .iter()
            .enumerate()
            .map(|(i, t)| (t.turn_id.clone(), i))
            .collect();
        self.window_start += n;
        self.bump_rev();
        n
    }

    /// 快照恢复：用权威 turns（timeline 快照解析产物）整体替换当前状态。
    /// 历史回合直接落 Final（不再流式）；此后增量事件照常 append。
    ///
    /// 幂等语义：快照是权威全量（daemon timeline 快照），增量事件在其后
    /// 到达；调用方在 seed 切换时应先重置（`Transcript::new`）再 restore。
    pub fn restore(&mut self, turns: Vec<RestoredTurn>) {
        let previous: HashMap<String, TurnView> = std::mem::take(&mut self.turns)
            .into_iter()
            .map(|turn| (turn.turn_id.clone(), turn))
            .collect();
        let mut restored = Vec::with_capacity(turns.len());
        for turn in turns {
            let turn_id = turn.turn_id.clone();
            let mut view = to_turn_view(turn);
            if let Some(old) = previous.get(&turn_id) {
                for round in &mut view.rounds {
                    if let Some(old_round) = old
                        .rounds
                        .iter()
                        .find(|old_round| old_round.round_num == round.round_num)
                    {
                        if same_rendered_round(old_round, round) {
                            *round = old_round.clone();
                        } else {
                            Rc::make_mut(round).mutation_rev =
                                old_round.mutation_rev.wrapping_add(1);
                        }
                    }
                }
                view.mutation_rev = if same_rendered_turn(old, &view) {
                    old.mutation_rev
                } else {
                    old.mutation_rev.wrapping_add(1)
                };
            }
            restored.push(view);
        }
        self.turns = restored;
        self.turn_index = self
            .turns
            .iter()
            .enumerate()
            .map(|(i, t)| (t.turn_id.clone(), i))
            .collect();
        // 窗口化：只渲染最近 N 个回合（长会话 restore 成本与总回合数解耦）。
        // 再叠加 **round 预算**：固定 30 turns 窗口在超大回合下仍会一次
        // mount 数千元素（实测单 turn 可达 100+ rounds、40 turns 共 680
        // rounds / 1783 blocks → 切换标签秒级卡顿）。预算从尾部累计
        // rounds，超限即收缩窗口（保留最新回合，裁剪最旧）。
        let mut budget = RESTORE_ROUND_BUDGET;
        let mut start_budget = 0usize;
        for (i, t) in self.turns.iter().enumerate().rev() {
            if budget < t.rounds.len().max(1) {
                // 当前 turn 超预算：保留它（含 i），但不再向前扩展。
                start_budget = i;
                break;
            }
            budget -= t.rounds.len().max(1);
            start_budget = i;
        }
        self.window_start = start_budget.max(self.turns.len().saturating_sub(WINDOW_DEFAULT_LEN));
        self.tail_following = true;
        // 立即驱逐窗口前超出缓冲的最旧 turn：restore 是全量权威快照，
        // 但内存只保留可立即浏览的部分（窗口 + 上滚缓冲），更早历史
        // 由 daemon 分页按需回放（on_top_reached → spawn_fetch_earlier）。
        // 释放 9MB 级长会话的解析产物驻留，避免切换标签内存台阶。
        self.evict_old_turns();
        self.bump_rev();
    }

    /// Apply one protocol event to the canonical presentation model.
    pub fn apply(&mut self, ev: &ConversationEvent) -> TranscriptChange {
        let target_turn_id = (!ev.turn_id().is_empty()).then(|| ev.turn_id().to_owned());
        let target_round_num = ev.round_num();
        let change = match ev {
            ConversationEvent::TurnStarted { turn_id, user_text } => {
                if let Some(&index) = self.turn_index.get(turn_id) {
                    let turn = &mut self.turns[index];
                    if turn.user_text == *user_text {
                        return TranscriptChange::default();
                    }
                    turn.user_text.clone_from(user_text);
                    self.bump_turn(index);
                    return TranscriptChange::structural(true);
                }
                let index = self.turns.len();
                self.turns.push(TurnView {
                    turn_id: turn_id.clone(),
                    user_text: user_text.clone(),
                    status: TurnStatus::Running,
                    failed_error: None,
                    rounds: Vec::new(),
                    mutation_rev: 0,
                });
                self.turn_index.insert(turn_id.clone(), index);
                TranscriptChange::structural(true)
            }
            ConversationEvent::TurnCompleted { turn_id } => {
                let Some(&index) = self.turn_index.get(turn_id) else {
                    return TranscriptChange::default();
                };
                if self.turns[index].status == TurnStatus::Completed {
                    return TranscriptChange::default();
                }
                self.turns[index].status = TurnStatus::Completed;
                TranscriptChange::structural(false)
            }
            ConversationEvent::TurnFailed { turn_id, error } => {
                let Some(&index) = self.turn_index.get(turn_id) else {
                    return TranscriptChange::default();
                };
                if self.turns[index].status == TurnStatus::Failed {
                    return TranscriptChange::default();
                }
                self.turns[index].status = TurnStatus::Failed;
                self.turns[index].failed_error = Some(extract_failed_error(&error));
                // Best-effort：若流式内容已完整输出（如 [DONE] 后断连误判），
                // 把 Live 答案冻结为 Final 富文本，避免"永远 Live"状态残留。
                let mut changed = false;
                for round in &mut self.turns[index].rounds {
                    let round = Rc::make_mut(round);
                    if let AnswerView::Streaming { raw, .. } = &round.answer
                        && !raw.is_empty()
                    {
                        let raw_text = raw.clone();
                        if round.finalize(None, Some(&raw_text)) {
                            round.mutation_rev = round.mutation_rev.wrapping_add(1);
                            changed = true;
                        }
                    }
                }
                TranscriptChange::structural(changed)
            }
            ConversationEvent::Unknown => TranscriptChange::default(),
            ConversationEvent::RoundDelta {
                turn_id,
                round_num,
                kind,
                delta,
            } => {
                let Some(&turn) = self.turn_index.get(turn_id) else {
                    return TranscriptChange::default();
                };
                let (_, round) = self.round_mut(turn, *round_num);
                match kind {
                    RoundDeltaKind::Answering => round
                        .answer_delta(delta)
                        .then(|| TranscriptChange::live(true))
                        .unwrap_or_default(),
                    RoundDeltaKind::Thinking => {
                        if delta.is_empty() {
                            return TranscriptChange::default();
                        }
                        let t = round.thinking.get_or_insert_with(String::new);
                        t.push_str(delta);
                        TranscriptChange::live(true)
                    }
                    RoundDeltaKind::ToolCalling => round
                        .tool_delta(delta)
                        .then(|| TranscriptChange::structural(true))
                        .unwrap_or_default(),
                }
            }
            ConversationEvent::BlockCheckpoint {
                turn_id,
                round_num,
                kind,
                text,
            } => {
                let Some(&turn) = self.turn_index.get(turn_id) else {
                    return TranscriptChange::default();
                };
                let (_, round) = self.round_mut(turn, *round_num);
                match kind {
                    RoundDeltaKind::Answering => round
                        .answer_checkpoint(text)
                        .then(|| TranscriptChange::live(true))
                        .unwrap_or_default(),
                    RoundDeltaKind::Thinking => {
                        if round.thinking.as_deref() == Some(text) {
                            return TranscriptChange::default();
                        }
                        round.thinking = Some(text.clone());
                        TranscriptChange::live(true)
                    }
                    RoundDeltaKind::ToolCalling => round
                        .tool_checkpoint(text)
                        .then(|| TranscriptChange::structural(true))
                        .unwrap_or_default(),
                }
            }
            ConversationEvent::ProviderToolStatus {
                turn_id,
                round_num,
                call_id,
                tool_kind,
                state,
            } => {
                let Some(&turn) = self.turn_index.get(turn_id) else {
                    return TranscriptChange::default();
                };
                let (_, round) = self.round_mut(turn, *round_num);
                // provider 内建工具卡：无参数流，展开区显示执行状态。
                let label = match state {
                    ProviderToolState::InProgress => "进行中…".to_string(),
                    ProviderToolState::Searching => "搜索中…".to_string(),
                    ProviderToolState::Completed => String::new(),
                };
                let card = ToolCardView {
                    id: call_id.clone(),
                    name: Some(tool_kind.clone()),
                    args_display: label,
                    args_json: None,
                    body: ToolBody::Empty,
                    changes: None,
                    done: *state == ProviderToolState::Completed,
                    failed: false,
                    failure: None,
                    provider: true,
                    started: true,
                };
                // upsert by call_id（replaceable 语义：同 id 覆盖状态）。
                upsert_tool_card(round, card)
                    .then(|| TranscriptChange::structural(true))
                    .unwrap_or_default()
            }
            ConversationEvent::ToolCallPrepared {
                tool_call_id,
                turn_id,
                round_num,
                name,
                args_so_far,
            } => {
                let turn = self.ensure_turn(turn_id);
                let (_, round) = self.round_mut(turn, *round_num);
                let card = ToolCardView {
                    id: tool_call_id.clone(),
                    name: Some(name.clone()),
                    args_display: args_so_far.clone(),
                    args_json: Some(args_so_far.clone()),
                    body: if args_so_far.trim().is_empty() {
                        ToolBody::Empty
                    } else {
                        ToolBody::Text(args_so_far.clone())
                    },
                    changes: None,
                    done: false,
                    failed: false,
                    failure: None,
                    provider: false,
                    // Prepared 预览：尚未真正执行，前端不渲染。
                    started: false,
                };
                upsert_tool_card(round, card)
                    .then(|| TranscriptChange::structural(true))
                    .unwrap_or_default()
            }
            ConversationEvent::ToolStarted {
                tool_call_id,
                turn_id,
                round_num,
                name,
            } => {
                let turn = self.ensure_turn(turn_id);
                let (_, round) = self.round_mut(turn, *round_num);
                let existing = round
                    .tool_calls
                    .iter()
                    .find(|card| card.id == *tool_call_id)
                    .cloned();
                let card = ToolCardView {
                    id: tool_call_id.clone(),
                    name: Some(name.clone()),
                    args_display: existing
                        .as_ref()
                        .map(|card| card.args_display.clone())
                        .unwrap_or_default(),
                    args_json: existing.as_ref().and_then(|card| card.args_json.clone()),
                    body: existing
                        .as_ref()
                        .map(|card| card.body.clone())
                        .unwrap_or_default(),
                    changes: existing.and_then(|card| card.changes),
                    done: false,
                    failed: false,
                    failure: None,
                    provider: false,
                    started: true,
                };
                upsert_tool_card(round, card)
                    .then(|| TranscriptChange::structural(true))
                    .unwrap_or_default()
            }
            ConversationEvent::ToolFinished {
                tool_call_id,
                turn_id,
                round_num,
                result,
            } => {
                let turn = self.ensure_turn(turn_id);
                let (_, round) = self.round_mut(turn, *round_num);
                // 结果摘要（对齐 timeline 块 summary）；失败保留 error 摘要。
                let summary = result
                    .get("summary")
                    .and_then(|s| s.as_str())
                    .map(str::to_string)
                    .or_else(|| {
                        result.get("error").and_then(|e| {
                            e.get("message")
                                .and_then(|m| m.as_str())
                                .map(str::to_string)
                        })
                    })
                    .unwrap_or_default();
                let existing = round
                    .tool_calls
                    .iter()
                    .find(|card| card.id == *tool_call_id)
                    .cloned();
                let name = existing.as_ref().and_then(|card| card.name.clone());
                let args_json = existing.as_ref().and_then(|card| card.args_json.clone());
                let candidate_body = name
                    .as_deref()
                    .map(|name| tool_body_from_result(name, args_json.as_deref(), result))
                    .unwrap_or_default();
                let body = match (candidate_body, existing.as_ref().map(|card| &card.body)) {
                    (ToolBody::Empty, Some(existing_body)) => existing_body.clone(),
                    (
                        ToolBody::Text(_),
                        Some(existing_body @ (ToolBody::Code(_) | ToolBody::Diff(_))),
                    ) => existing_body.clone(),
                    (candidate_body, _) => candidate_body,
                };
                let changes = change_stats_from_result(result, &body)
                    .or_else(|| existing.as_ref().and_then(|card| card.changes.clone()));
                let card = ToolCardView {
                    id: tool_call_id.clone(),
                    name,
                    args_display: summary,
                    args_json,
                    body,
                    changes,
                    done: true,
                    failed: false,
                    failure: None,
                    provider: false,
                    started: true,
                };
                upsert_tool_card(round, card)
                    .then(|| TranscriptChange::structural(true))
                    .unwrap_or_default()
            }
            ConversationEvent::CodeChanged {
                tool_call_id,
                turn_id,
                round_num,
                lines_added,
                lines_removed,
                files_created,
                files_deleted,
                file,
            } => {
                let turn = self.ensure_turn(turn_id);
                let (_, round) = self.round_mut(turn, *round_num);
                let Some(card) = round
                    .tool_calls
                    .iter_mut()
                    .find(|card| card.id == *tool_call_id)
                else {
                    return TranscriptChange::default();
                };
                let changes = ChangeStats {
                    lines_added: *lines_added,
                    lines_removed: *lines_removed,
                    files_created: *files_created,
                    files_deleted: *files_deleted,
                    file: file.clone(),
                };
                if card.changes.as_ref() == Some(&changes) {
                    TranscriptChange::default()
                } else {
                    card.changes = Some(changes);
                    TranscriptChange::structural(true)
                }
            }
            ConversationEvent::RoundCompleted {
                turn_id,
                round_num,
                thinking,
                answer,
                output_ref,
                is_final: _,
            } => {
                let Some(&turn) = self.turn_index.get(turn_id) else {
                    return TranscriptChange::default();
                };
                let (_, round) = self.round_mut(turn, *round_num);
                let mut changed = false;
                if let Some(t) = thinking {
                    if round.thinking.as_ref() != Some(t) {
                        round.thinking = Some(t.clone());
                        changed = true;
                    }
                }
                changed |= round.finish_tool_cards();

                // External content remains a model state. The app drains the
                // request and resolves it asynchronously through deepx-client.
                if let Some(ref_uri) = output_ref
                    && answer.is_none()
                {
                    if round.output_ref.as_ref() == Some(ref_uri)
                        && (round.output_loading
                            || matches!(round.answer, AnswerView::Final { .. }))
                    {
                        let change = changed
                            .then(|| TranscriptChange::structural(true))
                            .unwrap_or_default();
                        if change.changed() {
                            self.bump_round(turn, *round_num);
                        }
                        return change;
                    }
                    round.output_ref = Some(ref_uri.clone());
                    round.output_loading = true;
                    round.output_error = None;
                    self.pending_outputs.push(PendingOutput {
                        turn_id: turn_id.clone(),
                        round_num: *round_num,
                        reference: ref_uri.clone(),
                    });
                    self.bump_round(turn, *round_num);
                    return TranscriptChange::structural(true);
                }
                changed |= round.finalize(None, answer.as_deref());
                changed
                    .then(|| TranscriptChange::structural(true))
                    .unwrap_or_default()
            }
        };
        if change.changed() {
            if let Some(turn_id) = target_turn_id
                && let Some(&index) = self.turn_index.get(&turn_id)
            {
                if let Some(round_num) = target_round_num {
                    self.bump_round(index, round_num);
                } else {
                    self.bump_turn(index);
                }
            } else {
                self.bump_rev();
            }
        }
        change
    }

    /// Apply a presentation-frame batch.
    ///
    /// Adjacent deltas for the same `(turn, round, kind)` are concatenated
    /// before parsing. A burst of token events therefore updates the live
    /// RichText/TextBlock tail once per dispatcher frame instead of repeatedly
    /// reparsing the growing answer on the UI thread.
    pub fn apply_frame(
        &mut self,
        events: impl IntoIterator<Item = ConversationEvent>,
    ) -> TranscriptChange {
        let mut update = TranscriptChange::default();
        for event in coalesce_adjacent_deltas(events) {
            update.merge(self.apply(&event));
        }
        update
    }

    /// Apply one already-coalesced presentation event.
    ///
    /// The app frame pump uses this when enforcing a wall-clock reducer budget
    /// across a queued batch. It intentionally skips the temporary Vec and
    /// coalescing pass because the app-side deferred queue has already grouped it.
    pub fn apply_coalesced(&mut self, event: ConversationEvent) -> TranscriptChange {
        self.apply(&event)
    }

    /// Drain external content requests created while applying completion events.
    pub fn take_pending_outputs(&mut self) -> Vec<PendingOutput> {
        std::mem::take(&mut self.pending_outputs)
    }

    /// 外置正文拉取完成：以权威文本重建（对应 `output_ref` 加载路径）。
    pub fn resolve_output(
        &mut self,
        turn_id: &str,
        round_num: u32,
        text: &str,
    ) -> TranscriptChange {
        let Some(&turn) = self.turn_index.get(turn_id) else {
            return TranscriptChange::default();
        };
        let (_, round) = self.round_mut(turn, round_num);
        let change = round
            .finalize(None, Some(text))
            .then(|| TranscriptChange::structural(true))
            .unwrap_or_default();
        if change.changed() {
            self.bump_round(turn, round_num);
        }
        change
    }

    /// Mark external content resolution as failed while preserving the live
    /// preview. The UI can surface the failure instead of rendering blank text.
    pub fn fail_output(
        &mut self,
        turn_id: &str,
        round_num: u32,
        message: impl Into<String>,
    ) -> TranscriptChange {
        let Some(&turn) = self.turn_index.get(turn_id) else {
            return TranscriptChange::default();
        };
        let message = message.into();
        let (_, round) = self.round_mut(turn, round_num);
        if !round.output_loading && round.output_error.as_deref() == Some(message.as_str()) {
            return TranscriptChange::default();
        }
        round.output_loading = false;
        round.output_error = Some(message);
        self.bump_round(turn, round_num);
        TranscriptChange::structural(true)
    }

    fn bump_rev(&mut self) {
        self.rev = self.rev.wrapping_add(1);
    }

    fn bump_turn(&mut self, turn: usize) {
        self.turns[turn].mutation_rev = self.turns[turn].mutation_rev.wrapping_add(1);
        self.bump_rev();
    }

    fn bump_round(&mut self, turn: usize, round_num: u32) {
        if let Some(round) = self.turns[turn]
            .rounds
            .iter_mut()
            .find(|round| round.round_num == round_num)
        {
            let round = Rc::make_mut(round);
            round.mutation_rev = round.mutation_rev.wrapping_add(1);
        }
        self.bump_turn(turn);
    }

    fn round_mut(&mut self, turn: usize, round_num: u32) -> (usize, &mut RoundView) {
        let turn_view = &mut self.turns[turn];
        if let Some(r) = turn_view
            .rounds
            .iter()
            .position(|r| r.round_num == round_num)
        {
            (r, Rc::make_mut(&mut turn_view.rounds[r]))
        } else {
            turn_view.rounds.push(Rc::new(RoundView::new(round_num)));
            let idx = turn_view.rounds.len() - 1;
            (idx, Rc::make_mut(&mut turn_view.rounds[idx]))
        }
    }

    /// 定位 turn；Tool 频道事件可能先于 Conversation 频道的 TurnStarted 到达
    /// （双 SSE 频道无顺序保证），此时自动创建空 turn 兜底，避免工具卡丢失。
    fn ensure_turn(&mut self, turn_id: &str) -> usize {
        if let Some(&index) = self.turn_index.get(turn_id) {
            return index;
        }
        let index = self.turns.len();
        self.turns.push(TurnView {
            turn_id: turn_id.to_string(),
            user_text: String::new(),
            status: TurnStatus::Running,
            failed_error: None,
            rounds: Vec::new(),
            mutation_rev: 0,
        });
        self.turn_index.insert(turn_id.to_string(), index);
        index
    }
}

fn coalesce_adjacent_deltas(
    events: impl IntoIterator<Item = ConversationEvent>,
) -> Vec<ConversationEvent> {
    let mut coalesced: Vec<ConversationEvent> = Vec::new();
    for event in events {
        match event {
            ConversationEvent::RoundDelta {
                turn_id,
                round_num,
                kind,
                delta,
            } => {
                if let Some(ConversationEvent::RoundDelta {
                    turn_id: previous_turn,
                    round_num: previous_round,
                    kind: previous_kind,
                    delta: previous_delta,
                }) = coalesced.last_mut()
                    && *previous_turn == turn_id
                    && *previous_round == round_num
                    && *previous_kind == kind
                {
                    previous_delta.push_str(&delta);
                    continue;
                }
                coalesced.push(ConversationEvent::RoundDelta {
                    turn_id,
                    round_num,
                    kind,
                    delta,
                });
            }
            checkpoint @ ConversationEvent::BlockCheckpoint { .. } => {
                // A checkpoint is the complete current block value. If it
                // immediately follows same-target deltas/checkpoint in this
                // presentation frame, parsing the superseded value is wasted.
                if coalesced
                    .last()
                    .is_some_and(|previous| checkpoint_replaces(previous, &checkpoint))
                {
                    coalesced.pop();
                }
                coalesced.push(checkpoint);
            }
            status @ ConversationEvent::ProviderToolStatus { .. } => {
                // Provider status is replaceable by call_id; only its latest
                // value in an adjacent frame run needs to touch the tool card.
                if coalesced
                    .last()
                    .is_some_and(|previous| provider_status_replaces(previous, &status))
                {
                    coalesced.pop();
                }
                coalesced.push(status);
            }
            other => coalesced.push(other),
        }
    }
    coalesced
}

fn checkpoint_replaces(previous: &ConversationEvent, next: &ConversationEvent) -> bool {
    let ConversationEvent::BlockCheckpoint {
        turn_id,
        round_num,
        kind,
        ..
    } = next
    else {
        return false;
    };
    match previous {
        ConversationEvent::RoundDelta {
            turn_id: previous_turn,
            round_num: previous_round,
            kind: previous_kind,
            ..
        }
        | ConversationEvent::BlockCheckpoint {
            turn_id: previous_turn,
            round_num: previous_round,
            kind: previous_kind,
            ..
        } => previous_turn == turn_id && previous_round == round_num && previous_kind == kind,
        _ => false,
    }
}

fn provider_status_replaces(previous: &ConversationEvent, next: &ConversationEvent) -> bool {
    let ConversationEvent::ProviderToolStatus {
        turn_id,
        round_num,
        call_id,
        ..
    } = next
    else {
        return false;
    };
    matches!(
        previous,
        ConversationEvent::ProviderToolStatus {
            turn_id: previous_turn,
            round_num: previous_round,
            call_id: previous_call,
            ..
        } if previous_turn == turn_id
            && previous_round == round_num
            && previous_call == call_id
    )
}

/// 按 tool_call_id upsert 工具卡（同 id 覆盖状态，保持卡位置稳定）。
fn upsert_tool_card(round: &mut RoundView, card: ToolCardView) -> bool {
    if let Some(existing) = round.tool_calls.iter_mut().find(|c| c.id == card.id) {
        if *existing == card {
            return false;
        }
        *existing = card;
    } else {
        round.tool_calls.push(card);
    }
    true
}

fn same_rendered_turn(old: &TurnView, new: &TurnView) -> bool {
    old.turn_id == new.turn_id
        && old.user_text == new.user_text
        && old.status == new.status
        && old.failed_error == new.failed_error
        && old.rounds.len() == new.rounds.len()
        && old
            .rounds
            .iter()
            .zip(&new.rounds)
            .all(|(old, new)| same_rendered_round(old, new))
}

fn same_rendered_round(old: &RoundView, new: &RoundView) -> bool {
    old.round_num == new.round_num
        && old.thinking == new.thinking
        && old.answer == new.answer
        && old.tool_calls == new.tool_calls
        && old.output_loading == new.output_loading
        && old.output_error == new.output_error
}

/// RestoredTurn → TurnView（历史回合直接落 Final；restore 与分页前插共用）。
fn to_turn_view(t: RestoredTurn) -> TurnView {
    TurnView {
        turn_id: t.turn_id.clone(),
        user_text: t.user_text,
        status: t.status,
        failed_error: None,
        rounds: t
            .rounds
            .into_iter()
            .map(|r| {
                let mut round = RoundView::new(r.round_num);
                round.thinking = r.thinking;
                round.tool_calls = r.tool_calls;
                round.answer = match r.answer {
                    Some(a) => {
                        let blocks = parse_final(&a);
                        round.final_raw = Some(a);
                        AnswerView::Final {
                            rich: render_final(&blocks),
                            blocks,
                        }
                    }
                    None => AnswerView::Final {
                        rich: RichTextOutput::default(),
                        blocks: Vec::new(),
                    },
                };
                Rc::new(round)
            })
            .collect(),
        mutation_rev: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ConversationEvent, ProviderToolState, RoundDeltaKind};

    fn start_turn(ts: &mut Transcript, turn_id: &str) {
        ts.apply(&ConversationEvent::TurnStarted {
            turn_id: turn_id.into(),
            user_text: "hi".into(),
        });
    }

    fn restored_turns(n: usize) -> Vec<RestoredTurn> {
        (0..n)
            .map(|i| RestoredTurn {
                turn_id: format!("t{i}"),
                created_seq: i as u64,
                user_text: format!("q{i}"),
                status: TurnStatus::Completed,
                rounds: Vec::new(),
            })
            .collect()
    }

    /// 生成 t{start}..t{start+count} 区间的 turns（分页页面前插测试用）。
    fn restored_turns_range(start: usize, count: usize) -> Vec<RestoredTurn> {
        (start..start + count)
            .map(|i| RestoredTurn {
                turn_id: format!("t{i}"),
                created_seq: i as u64,
                user_text: format!("q{i}"),
                status: TurnStatus::Completed,
                rounds: Vec::new(),
            })
            .collect()
    }

    /// restore 后只渲染最近 `WINDOW_DEFAULT_LEN` 个回合；短会话（未超过
    /// `RESTORE_KEEP_TURNS`）全量保留。
    #[test]
    fn window_after_restore_is_tail_only() {
        let mut ts = Transcript::new();
        ts.restore(restored_turns(40));
        assert_eq!(ts.turn_count(), 40, "未超裁剪阈值 → 全量保留");
        assert_eq!(ts.window_len(), WINDOW_DEFAULT_LEN);
        assert_eq!(ts.window_turns()[0].turn_id, "t10", "窗口 = 最近 30 个");
        assert_eq!(ts.window_turns().last().unwrap().turn_id, "t39");
        assert!(!ts.window_full());
    }

    /// 大快照（超过窗口 + 上滚缓冲）restore 时：窗口外历史立即驱逐，
    /// 内存只驻留 `RESTORE_KEEP_TURNS` 个 turn；更早历史由 daemon 分页
    /// 按需回放（expand_window 耗尽缓冲后返回 0，调用方转分页拉取）。
    #[test]
    fn restore_evicts_history_beyond_buffer() {
        let mut ts = Transcript::new();
        ts.restore(restored_turns(300));
        // 窗口前 270 个 → 保留 100 上滚缓冲 → 共 30 + 100 = 130。
        assert_eq!(ts.turn_count(), RESTORE_KEEP_TURNS, "窗口外历史已驱逐");
        assert_eq!(ts.window_len(), WINDOW_DEFAULT_LEN);
        assert_eq!(ts.window_turns()[0].turn_id, "t270", "窗口 = 最新 30 个");
        assert_eq!(ts.window_turns().last().unwrap().turn_id, "t299");
        assert_eq!(ts.turns()[0].turn_id, "t170", "缓冲起点 = 窗口前 100");
        // 上滚缓冲可本地扩展：expand 100 后窗口起点到达 t170。
        assert_eq!(ts.expand_window(100), 100);
        assert_eq!(ts.window_turns()[0].turn_id, "t170");
        assert!(ts.window_full());
        // 缓冲耗尽后再扩展返回 0（调用方转 daemon 分页）。
        assert_eq!(ts.expand_window(10), 0);
    }

    /// 短会话（少于窗口大小）：窗口 = 全量，window_full 立即为 true。
    #[test]
    fn window_is_full_for_short_sessions() {
        let mut ts = Transcript::new();
        ts.restore(restored_turns(5));
        assert_eq!(ts.window_len(), 5);
        assert!(ts.window_full());
        assert_eq!(ts.expand_window(10), 0, "无更早回合可扩展");
    }

    /// GFM 表格流式分段：增量喂表头/分隔/数据行 → 产出 Table 段，
    /// 表头行从可见字面剥离（骨架先行、逐格填充的输入侧）。
    #[test]
    fn gfm_table_streams_to_table_segment() {
        let mut rv = RoundView::new(1);
        rv.answer_delta("| A | B |\n");
        rv.answer_delta("| --- | --- |\n");
        rv.answer_delta("| 1 | 2 |\n");
        let AnswerView::Streaming {
            raw,
            segments,
            table_tracker,
            gfm_table_tracker,
            ..
        } = &rv.answer
        else {
            panic!("expected streaming")
        };
        assert!(
            segments.iter().any(|s| matches!(s, LiveSegment::Table(_))),
            "GFM 表格应产出 Table 段"
        );
        let (visible, _) = split_segments(raw, table_tracker, gfm_table_tracker);
        assert!(!visible.contains("| A | B |"), "表头行应从可见字面剥离");
    }

    /// GFM 表格在 ``` 代码围栏内不激活（FenceState 围栏感知），保持字面。
    #[test]
    fn gfm_table_inside_code_fence_stays_literal() {
        let mut rv = RoundView::new(1);
        rv.answer_delta("```\n");
        rv.answer_delta("| A | B |\n");
        rv.answer_delta("| --- | --- |\n");
        rv.answer_delta("```\n");
        let AnswerView::Streaming { segments, .. } = &rv.answer else {
            panic!("expected streaming")
        };
        assert!(
            !segments.iter().any(|s| matches!(s, LiveSegment::Table(_))),
            "代码围栏内表格不激活"
        );
    }

    /// expand_window 前移起点；到 0 后短路（返回 0，避免无谓渲染）。
    #[test]
    fn expand_window_moves_start_and_short_circuits() {
        let mut ts = Transcript::new();
        ts.restore(restored_turns(40));
        assert!(ts.tail_following());
        assert_eq!(ts.expand_window(10), 10);
        assert_eq!(ts.window_len(), 40);
        assert!(ts.window_full());
        assert!(!ts.tail_following(), "用户上滚扩展后脱离跟随尾部");
        assert_eq!(ts.expand_window(10), 0, "已全量放行，短路");
        assert_eq!(ts.window_turns()[0].turn_id, "t0");
        ts.slide_window_tail();
        assert!(ts.tail_following(), "滑动恢复跟随尾部");
    }

    /// 分页前插：更早一页插到最前，窗口起点右移，turn 顺序正确。
    #[test]
    fn prepend_turns_puts_earlier_page_in_front() {
        let mut ts = Transcript::new();
        // resume 只拿到尾部页 t10..t39（30 个）。
        ts.restore(restored_turns_range(10, 30));
        assert_eq!(ts.turn_count(), 30);
        assert_eq!(ts.window_len(), 30, "30 个全在窗口内");
        // 上滚翻页：t0..t9 前插。
        let n = ts.prepend_turns(restored_turns_range(0, 10));
        assert_eq!(n, 10);
        assert_eq!(ts.turn_count(), 40);
        assert_eq!(ts.turns().first().unwrap().turn_id, "t0");
        assert_eq!(ts.turns().last().unwrap().turn_id, "t39");
        // 窗口起点右移 10：渲染视图仍是尾部 30 个（t10..t39）。
        assert_eq!(ts.window_len(), 30);
        assert_eq!(ts.window_turns().first().unwrap().turn_id, "t10");
        assert_eq!(ts.expand_window(10), 10, "可继续向前扩展 t0..t9");
    }

    /// 页码边界重叠去重：重复 turn 跳过，不重复计数。
    #[test]
    fn prepend_turns_skips_overlapping_turn_ids() {
        let mut ts = Transcript::new();
        ts.restore(restored_turns_range(20, 20));
        // 服务端翻页可能返回 t15..t25（重叠 t20..t24 已加载）。
        let n = ts.prepend_turns(restored_turns_range(15, 10));
        assert_eq!(n, 5, "t20..t24 已存在跳过");
        assert_eq!(ts.turn_count(), 25);
        assert_eq!(ts.turns().first().unwrap().turn_id, "t15");
        // 空页 / 全重叠页 → 0。
        assert_eq!(ts.prepend_turns(Vec::new()), 0);
        assert_eq!(ts.prepend_turns(restored_turns_range(15, 10)), 0);
    }

    /// slide_window_tail：跟随尾部时窗口保持大小为 WINDOW_DEFAULT_LEN；
    /// 用户上滚扩展后调用则回到最近 N 个（由调用方决定何时调用）。
    #[test]
    fn slide_window_tail_keeps_window_size() {
        let mut ts = Transcript::new();
        ts.restore(restored_turns(40));
        // 已是最新 30：滑动无变化。
        ts.slide_window_tail();
        assert_eq!(ts.window_len(), WINDOW_DEFAULT_LEN);
        assert_eq!(ts.window_turns()[0].turn_id, "t10");
        // 用户扩展窗口（上滚预加载）后，跟随尾部时滑回最近 N 个。
        ts.expand_window(10);
        assert_eq!(ts.window_len(), 40);
        ts.slide_window_tail();
        assert_eq!(ts.window_len(), WINDOW_DEFAULT_LEN);
        assert_eq!(ts.window_turns()[0].turn_id, "t10");
    }

    /// 新 turn 追加（增量事件）不影响窗口起点；窗口是渲染投影，滑动由
    /// 调用方在「跟随尾部」时显式 `slide_window_tail`。
    #[test]
    fn apply_growth_keeps_window_consistent() {
        let mut ts = Transcript::new();
        ts.restore(restored_turns(40));
        start_turn(&mut ts, "t40");
        assert_eq!(ts.turn_count(), 41);
        assert_eq!(ts.window_len(), 31, "起点不动，窗口随尾部增长");
        assert_eq!(ts.window_turns()[0].turn_id, "t10", "起点未变");
        assert_eq!(ts.window_turns().last().unwrap().turn_id, "t40");
        // 跟随尾部：显式滑动，窗口回到最近 N 个。
        ts.slide_window_tail();
        assert_eq!(ts.window_len(), WINDOW_DEFAULT_LEN);
        assert_eq!(ts.window_turns()[0].turn_id, "t11");
    }

    /// `provider_tool_status` 按 call_id upsert：状态流 进行中→搜索中→完成，
    /// 同 id 覆盖不重复加卡；done 随 completed 置位。
    #[test]
    fn provider_tool_status_upserts_card() {
        let mut ts = Transcript::new();
        start_turn(&mut ts, "t1");
        ts.apply(&ConversationEvent::ProviderToolStatus {
            turn_id: "t1".into(),
            round_num: 0,
            call_id: "call-1".into(),
            tool_kind: "web_search".into(),
            state: ProviderToolState::InProgress,
        });
        assert_eq!(ts.turns()[0].rounds[0].tool_calls.len(), 1);
        let card = &ts.turns()[0].rounds[0].tool_calls[0];
        assert_eq!(card.id, "call-1");
        assert!(card.provider);
        assert!(!card.done);
        assert_eq!(card.args_display, "进行中…");

        // 状态流转：同 id 覆盖。
        ts.apply(&ConversationEvent::ProviderToolStatus {
            turn_id: "t1".into(),
            round_num: 0,
            call_id: "call-1".into(),
            tool_kind: "web_search".into(),
            state: ProviderToolState::Searching,
        });
        ts.apply(&ConversationEvent::ProviderToolStatus {
            turn_id: "t1".into(),
            round_num: 0,
            call_id: "call-1".into(),
            tool_kind: "web_search".into(),
            state: ProviderToolState::Completed,
        });
        let rounds = &ts.turns()[0].rounds;
        assert_eq!(rounds.len(), 1);
        assert_eq!(rounds[0].tool_calls.len(), 1, "同 call_id 覆盖不新增卡");
        assert!(rounds[0].tool_calls[0].done);
        assert_eq!(rounds[0].tool_calls[0].args_display, "");
    }

    /// Tool 频道事件（ToolCallPrepared → ToolStarted → ToolFinished）按
    /// tool_call_id upsert；流式时工具卡从「预览」→「执行中」→「完成」。
    #[test]
    fn tool_channel_events_upsert_card() {
        let mut ts = Transcript::new();
        start_turn(&mut ts, "t1");

        // Prepared：预览卡（带 args）。
        ts.apply(&ConversationEvent::ToolCallPrepared {
            tool_call_id: "call-1".into(),
            turn_id: "t1".into(),
            round_num: 0,
            name: "exec".into(),
            args_so_far: "{\"cmd\":\"ls\"}".into(),
        });
        let card = &ts.turns()[0].rounds[0].tool_calls[0];
        assert_eq!(card.id, "call-1");
        assert_eq!(card.name.as_deref(), Some("exec"));
        assert!(!card.done);
        assert!(!card.provider);

        // Started：同 id 覆盖（清 args 展示）。
        ts.apply(&ConversationEvent::ToolStarted {
            tool_call_id: "call-1".into(),
            turn_id: "t1".into(),
            round_num: 0,
            name: "exec".into(),
        });
        let rounds = &ts.turns()[0].rounds;
        assert_eq!(rounds[0].tool_calls.len(), 1, "同 id 不新增卡");
        assert!(!rounds[0].tool_calls[0].done);

        // Finished：done 置位 + 结果摘要。
        ts.apply(&ConversationEvent::ToolFinished {
            tool_call_id: "call-1".into(),
            turn_id: "t1".into(),
            round_num: 0,
            result: serde_json::json!({ "summary": "8 files listed" }),
        });
        let rounds = &ts.turns()[0].rounds;
        assert_eq!(rounds[0].tool_calls.len(), 1);
        assert!(rounds[0].tool_calls[0].done);
        assert_eq!(rounds[0].tool_calls[0].args_display, "8 files listed");
    }

    #[test]
    fn read_tool_keeps_args_and_finishes_as_native_code() {
        let mut ts = Transcript::new();
        start_turn(&mut ts, "t1");
        ts.apply(&ConversationEvent::ToolCallPrepared {
            tool_call_id: "read-1".into(),
            turn_id: "t1".into(),
            round_num: 0,
            name: "read_file".into(),
            args_so_far: r#"{"path":"src/lib.rs","start_line":4}"#.into(),
        });
        ts.apply(&ConversationEvent::ToolStarted {
            tool_call_id: "read-1".into(),
            turn_id: "t1".into(),
            round_num: 0,
            name: "read_file".into(),
        });
        ts.apply(&ConversationEvent::ToolFinished {
            tool_call_id: "read-1".into(),
            turn_id: "t1".into(),
            round_num: 0,
            result: serde_json::json!({
                "summary": "read src/lib.rs",
                "data": {"files": [{"path": "src/lib.rs", "start_line": 4, "total_lines": 8}]},
                "model": {"text": "L4: pub fn answer() -> u32 {\nL5:     42\nL6: }"}
            }),
        });

        let card = &ts.turns()[0].rounds[0].tool_calls[0];
        assert_eq!(
            card.args_json.as_deref(),
            Some(r#"{"path":"src/lib.rs","start_line":4}"#)
        );
        let ToolBody::Code(documents) = &card.body else {
            panic!("read_file should render as native code");
        };
        assert_eq!(documents[0].path.as_deref(), Some("src/lib.rs"));
        assert_eq!(documents[0].start_line, 4);
        assert_eq!(
            documents[0].text.lines().next(),
            Some("pub fn answer() -> u32 {")
        );
    }

    #[test]
    fn code_changed_targets_one_card_and_is_idempotent() {
        let mut ts = Transcript::new();
        start_turn(&mut ts, "t1");
        for call_id in ["edit-1", "edit-2"] {
            ts.apply(&ConversationEvent::ToolCallPrepared {
                tool_call_id: call_id.into(),
                turn_id: "t1".into(),
                round_num: 0,
                name: "edit_file".into(),
                args_so_far: "{}".into(),
            });
        }
        let changed = ConversationEvent::CodeChanged {
            tool_call_id: "edit-2".into(),
            turn_id: "t1".into(),
            round_num: 0,
            lines_added: 7,
            lines_removed: 3,
            files_created: 1,
            files_deleted: 0,
            file: Some("src/lib.rs".into()),
        };
        assert!(ts.apply(&changed).changed());
        assert!(
            !ts.apply(&changed).changed(),
            "same stats should not invalidate twice"
        );

        let cards = &ts.turns()[0].rounds[0].tool_calls;
        assert!(cards[0].changes.is_none());
        assert_eq!(cards[1].changes.as_ref().unwrap().label(), "+7  −3  新建 1");
    }

    #[test]
    fn compact_duplicate_finish_does_not_replace_exact_diff() {
        let mut ts = Transcript::new();
        start_turn(&mut ts, "t1");
        ts.apply(&ConversationEvent::ToolCallPrepared {
            tool_call_id: "edit-1".into(),
            turn_id: "t1".into(),
            round_num: 0,
            name: "edit_file".into(),
            args_so_far: r#"{"path":"src/lib.rs"}"#.into(),
        });
        ts.apply(&ConversationEvent::ToolFinished {
            tool_call_id: "edit-1".into(),
            turn_id: "t1".into(),
            round_num: 0,
            result: serde_json::json!({
                "summary": "edited",
                "data": {"files": [{"ops": [{"diff": "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n"}]}]},
                "model": {"text": "+1 -1"}
            }),
        });
        ts.apply(&ConversationEvent::ToolFinished {
            tool_call_id: "edit-1".into(),
            turn_id: "t1".into(),
            round_num: 0,
            result: serde_json::json!({"summary": "+1 -1", "model": {"text": "+1 -1"}}),
        });
        assert!(matches!(
            ts.turns()[0].rounds[0].tool_calls[0].body,
            ToolBody::Diff(_)
        ));
    }

    /// Tool 频道与 Conversation 频道无顺序保证：工具事件先于 TurnStarted
    /// 到达时自动建 turn，不丢卡。
    #[test]
    fn tool_event_before_turn_started_creates_turn() {
        let mut ts = Transcript::new();
        ts.apply(&ConversationEvent::ToolCallPrepared {
            tool_call_id: "call-1".into(),
            turn_id: "t1".into(),
            round_num: 0,
            name: "file".into(),
            args_so_far: "{}".into(),
        });
        assert_eq!(ts.turns().len(), 1, "自动建 turn");
        assert_eq!(ts.turns()[0].turn_id, "t1");
        assert_eq!(ts.turns()[0].rounds[0].tool_calls.len(), 1);
    }

    /// 未知 turn 的 provider 状态：忽略（防跨回合错灌）。
    #[test]
    fn provider_tool_status_unknown_turn_ignored() {
        let mut ts = Transcript::new();
        start_turn(&mut ts, "t1");
        let change = ts.apply(&ConversationEvent::ProviderToolStatus {
            turn_id: "ghost".into(),
            round_num: 0,
            call_id: "call-1".into(),
            tool_kind: "web_search".into(),
            state: ProviderToolState::Completed,
        });
        assert!(!change.changed());
        assert!(ts.turns()[0].rounds.is_empty());
    }

    /// 与 DeepX 工具调用卡（ToolCalling 流）共存：不同 id 各自成卡。
    #[test]
    fn provider_card_coexists_with_deepx_tool_card() {
        let mut ts = Transcript::new();
        start_turn(&mut ts, "t1");
        ts.apply(&ConversationEvent::RoundDelta {
            turn_id: "t1".into(),
            round_num: 0,
            kind: RoundDeltaKind::ToolCalling,
            delta: "{\"id\":\"c1\",\"name\":\"web_search\"".into(),
        });
        ts.apply(&ConversationEvent::ProviderToolStatus {
            turn_id: "t1".into(),
            round_num: 0,
            call_id: "call-1".into(),
            tool_kind: "web_search".into(),
            state: ProviderToolState::Searching,
        });
        let cards = &ts.turns()[0].rounds[0].tool_calls;
        assert_eq!(cards.len(), 2);
        assert!(cards.iter().any(|c| c.id == "c1" && !c.provider));
        assert!(cards.iter().any(|c| c.id == "call-1" && c.provider));
    }

    #[test]
    fn round_copy_on_write_preserves_completed_sibling_identity() {
        let mut ts = Transcript::new();
        start_turn(&mut ts, "t1");
        for round_num in [0, 1] {
            ts.apply(&ConversationEvent::RoundDelta {
                turn_id: "t1".into(),
                round_num,
                kind: RoundDeltaKind::Answering,
                delta: format!("round-{round_num}"),
            });
        }

        let completed = ts.turns()[0].rounds[0].clone();
        let active_before = ts.turns()[0].rounds[1].clone();
        let active_rev = active_before.mutation_rev;
        ts.apply(&ConversationEvent::RoundDelta {
            turn_id: "t1".into(),
            round_num: 1,
            kind: RoundDeltaKind::Answering,
            delta: "-next".into(),
        });

        let rounds = &ts.turns()[0].rounds;
        assert!(Rc::ptr_eq(&completed, &rounds[0]));
        assert!(!Rc::ptr_eq(&active_before, &rounds[1]));
        assert_eq!(rounds[1].mutation_rev, active_rev.wrapping_add(1));
    }

    #[test]
    fn restore_reuses_unchanged_round_identity() {
        let snapshot = RestoredTurn {
            turn_id: "t1".into(),
            created_seq: 1,
            user_text: "question".into(),
            status: TurnStatus::Completed,
            rounds: vec![RestoredRound {
                round_num: 0,
                thinking: None,
                answer: Some("answer".into()),
                tool_calls: Vec::new(),
            }],
        };
        let mut ts = Transcript::new();
        ts.restore(vec![snapshot.clone()]);
        let first = ts.turns()[0].rounds[0].clone();
        ts.restore(vec![snapshot]);
        assert!(Rc::ptr_eq(&first, &ts.turns()[0].rounds[0]));
    }
}
