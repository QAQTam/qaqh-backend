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

use markdown_core::ast::{Block, Inline};
use markdown_core::live::parse_live;
use markdown_core::live_table::{LiveTableTracker, TableSnapshot};
use markdown_core::parse_final;

use crate::protocol::{ConversationEvent, ProviderToolState, RoundDeltaKind};
use crate::{RichTextOutput, TableData, render_final};

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
    const fn live(extent_changed: bool) -> Self {
        Self {
            invalidation: TranscriptInvalidation::Live,
            extent_changed,
        }
    }

    const fn structural(extent_changed: bool) -> Self {
        Self {
            invalidation: TranscriptInvalidation::Structural,
            extent_changed,
        }
    }

    fn merge(&mut self, other: Self) {
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
    /// true = 工具卡完成（后续 delta 不再更新）。
    pub done: bool,
    /// provider 内建工具卡（web_search 等，`provider_tool_status` 事件）：
    /// 无参数流，展开区显示执行状态（args_display 承载）。
    pub provider: bool,
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
    pub rounds: Vec<RoundView>,
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
    /// 最近一次 live 的 raw（防抖：相同 raw 不产生模型失效）。
    last_live_raw: String,
    /// 协议表格流式跟踪（```table 围栏；增量行扫描）。
    table_tracker: LiveTableTracker,
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
#[derive(Clone, Debug, PartialEq)]
pub enum AnswerView {
    /// 流式中：字面/表格交错序列（仅已闭合语法；未闭合字面输出；
    /// 协议表格渐进长出，残行逐字生长在网格末行）。
    Streaming {
        raw: String,
        inlines: Vec<Inline>,
        segments: Vec<LiveSegment>,
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
        }
    }
}

impl RoundView {
    fn new(round_num: u32) -> Self {
        Self {
            round_num,
            ..Self::default()
        }
    }

    /// 追加 Answering 增量：累积 raw → 表格跟踪 → 行内预览。
    /// 未闭合语法跨 delta 边界（`**bo` + `ld**`），必须对**整段 raw**
    /// 重解析（O(段长)；UI 层以 DispatcherQueue 节流合并，同 Web rAF）。
    /// 协议表格（```table）行级确认：表格行从字面剥离进 `segments`，
    /// 残行实时显示在网格末行（逐字生长），字面保留在 Text 段。
    /// 解析结果同步写回 `AnswerView::Streaming`（状态机自持）。
    fn answer_delta(&mut self, delta: &str) -> bool {
        if delta.is_empty() {
            return false;
        }
        let AnswerView::Streaming {
            raw,
            inlines,
            segments,
        } = &mut self.answer
        else {
            return false; // 终态后忽略 delta（协议保证不会发生）
        };
        raw.push_str(delta);
        // 表格跟踪：增量行扫描（O(新增行)）
        self.table_tracker.feed(raw);
        // 可见字面 = raw 减去表格隐藏区间；segments = 字面/表格交错
        let (visible, segs) = split_segments(raw, &self.table_tracker);
        let parsed = parse_live(&visible);
        let changed = self.last_live_raw != *raw;
        if changed {
            *inlines = parsed;
            *segments = segs;
            self.last_live_raw.clone_from(raw);
        }
        changed
    }

    /// BlockCheckpoint 覆盖（自愈）：整段替换，重解析（表格跟踪器重置）。
    fn answer_checkpoint(&mut self, text: &str) -> bool {
        let AnswerView::Streaming {
            raw,
            inlines,
            segments,
        } = &mut self.answer
        else {
            return false;
        };
        self.table_tracker.reset();
        raw.clear();
        raw.push_str(text);
        self.table_tracker.feed(raw);
        let (visible, segs) = split_segments(raw, &self.table_tracker);
        let parsed = parse_live(&visible);
        let changed = self.last_live_raw != *raw;
        if changed {
            *inlines = parsed;
            *segments = segs;
            self.last_live_raw.clone_from(raw);
        }
        changed
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
        self.table_tracker.reset();
        if let Some(a) = answer {
            if self.final_raw.as_deref() != Some(a) {
                let blocks = parse_final(a);
                self.answer = AnswerView::Final {
                    rich: render_final(&blocks),
                    blocks,
                };
                self.final_raw = Some(a.to_string());
                self.last_live_raw.clear();
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
            done: false,
            provider: false,
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
fn split_segments(raw: &str, tracker: &LiveTableTracker) -> (String, Vec<LiveSegment>) {
    let mut segments: Vec<LiveSegment> = Vec::new();
    let mut visible = String::new();
    let mut prev = 0usize;
    for (span, snap) in tracker.tables_with_spans() {
        if span.start > prev {
            let text = raw[prev..span.start].to_string();
            visible.push_str(&text);
            segments.push(LiveSegment::Text(text));
        }
        segments.push(LiveSegment::Table(table_snapshot_to_data(snap)));
        prev = span.end;
    }
    // 尾部字面：从 prev 到残行起点（残行已在网格末行，不重复显示）
    let tail_end = tracker.open_tail_start().unwrap_or(raw.len());
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
}

/// 默认渲染窗口大小（回合数）：restore 后只渲染最近 N 个回合，与总回合
/// 数解耦，restore/每帧 diff 成本恒定。经实机手感调优。
pub const WINDOW_DEFAULT_LEN: usize = 30;

/// restore 窗口的 round 预算：尾部累计 rounds 超过此值即收缩窗口
/// （超大回合会话：30 turns 可能含 600+ rounds / 1800+ blocks，一次
/// mount 数千 XAML 元素 → 切换标签秒级卡顿）。200 rounds ≈ 500 blocks，
/// debug 构建单次 mount 可接受；可经实机手感调优。
pub const RESTORE_ROUND_BUDGET: usize = 200;

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

    /// 向前扩展窗口（预加载更早回合）：起点前移 `by`，钳制到 0。
    /// 返回实际前移量；0 = 已全量放行（调用方短路，避免无谓渲染）。
    /// 扩展后窗口脱离「跟随尾部」模式（用户上滚浏览中）。
    pub fn expand_window(&mut self, by: usize) -> usize {
        let moved = self.window_start.min(by);
        if moved > 0 {
            self.window_start -= moved;
            self.tail_following = false;
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
    pub fn slide_window_tail(&mut self) {
        let keep = WINDOW_DEFAULT_LEN;
        if self.turns.len() > keep {
            self.window_start = self.window_start.max(self.turns.len() - keep);
        }
        self.tail_following = true;
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
        n
    }

    /// 快照恢复：用权威 turns（timeline 快照解析产物）整体替换当前状态。
    /// 历史回合直接落 Final（不再流式）；此后增量事件照常 append。
    ///
    /// 幂等语义：快照是权威全量（daemon timeline 快照），增量事件在其后
    /// 到达；调用方在 seed 切换时应先重置（`Transcript::new`）再 restore。
    pub fn restore(&mut self, turns: Vec<RestoredTurn>) {
        self.turns = turns.into_iter().map(to_turn_view).collect();
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
    }

    /// Apply one protocol event to the canonical presentation model.
    pub fn apply(&mut self, ev: &ConversationEvent) -> TranscriptChange {
        match ev {
            ConversationEvent::TurnStarted { turn_id, user_text } => {
                if let Some(&index) = self.turn_index.get(turn_id) {
                    let turn = &mut self.turns[index];
                    if turn.user_text == *user_text {
                        return TranscriptChange::default();
                    }
                    turn.user_text.clone_from(user_text);
                    return TranscriptChange::structural(true);
                }
                let index = self.turns.len();
                self.turns.push(TurnView {
                    turn_id: turn_id.clone(),
                    user_text: user_text.clone(),
                    status: TurnStatus::Running,
                    rounds: Vec::new(),
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
            ConversationEvent::TurnFailed { turn_id, .. } => {
                let Some(&index) = self.turn_index.get(turn_id) else {
                    return TranscriptChange::default();
                };
                if self.turns[index].status == TurnStatus::Failed {
                    return TranscriptChange::default();
                }
                self.turns[index].status = TurnStatus::Failed;
                TranscriptChange::structural(false)
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
                    done: *state == ProviderToolState::Completed,
                    provider: true,
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
                    done: false,
                    provider: false,
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
                let card = ToolCardView {
                    id: tool_call_id.clone(),
                    name: Some(name.clone()),
                    args_display: String::new(),
                    done: false,
                    provider: false,
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
                let card = ToolCardView {
                    id: tool_call_id.clone(),
                    name: round
                        .tool_calls
                        .iter()
                        .find(|c| c.id == *tool_call_id)
                        .and_then(|c| c.name.clone()),
                    args_display: summary,
                    done: true,
                    provider: false,
                };
                upsert_tool_card(round, card)
                    .then(|| TranscriptChange::structural(true))
                    .unwrap_or_default()
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
                        return changed
                            .then(|| TranscriptChange::structural(true))
                            .unwrap_or_default();
                    }
                    round.output_ref = Some(ref_uri.clone());
                    round.output_loading = true;
                    round.output_error = None;
                    self.pending_outputs.push(PendingOutput {
                        turn_id: turn_id.clone(),
                        round_num: *round_num,
                        reference: ref_uri.clone(),
                    });
                    return TranscriptChange::structural(true);
                }
                changed |= round.finalize(None, answer.as_deref());
                changed
                    .then(|| TranscriptChange::structural(true))
                    .unwrap_or_default()
            }
        }
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
        let events = coalesce_adjacent_deltas(events);
        let mut update = TranscriptChange::default();
        for event in events {
            update.merge(self.apply(&event));
        }
        update
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
        round
            .finalize(None, Some(text))
            .then(|| TranscriptChange::structural(true))
            .unwrap_or_default()
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
        let (_, round) = self.round_mut(turn, round_num);
        round.output_loading = false;
        round.output_error = Some(message.into());
        TranscriptChange::structural(true)
    }

    fn round_mut(&mut self, turn: usize, round_num: u32) -> (usize, &mut RoundView) {
        let turn_view = &mut self.turns[turn];
        if let Some(r) = turn_view
            .rounds
            .iter()
            .position(|r| r.round_num == round_num)
        {
            (r, &mut turn_view.rounds[r])
        } else {
            turn_view.rounds.push(RoundView::new(round_num));
            let idx = turn_view.rounds.len() - 1;
            (idx, &mut turn_view.rounds[idx])
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
            rounds: Vec::new(),
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

/// RestoredTurn → TurnView（历史回合直接落 Final；restore 与分页前插共用）。
fn to_turn_view(t: RestoredTurn) -> TurnView {
    TurnView {
        turn_id: t.turn_id.clone(),
        user_text: t.user_text,
        status: t.status,
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
                round
            })
            .collect(),
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

    /// restore 后只渲染最近 `WINDOW_DEFAULT_LEN` 个回合，全量 turns 保留。
    #[test]
    fn window_after_restore_is_tail_only() {
        let mut ts = Transcript::new();
        ts.restore(restored_turns(40));
        assert_eq!(ts.turn_count(), 40, "全量保留");
        assert_eq!(ts.window_len(), WINDOW_DEFAULT_LEN);
        assert_eq!(ts.window_turns()[0].turn_id, "t10", "窗口 = 最近 30 个");
        assert_eq!(ts.window_turns().last().unwrap().turn_id, "t39");
        assert!(!ts.window_full());
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
}
