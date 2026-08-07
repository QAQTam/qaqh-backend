//! Transcript 状态机：协议事件 → 渲染命令。
//!
//! 这是"针对后端写前端"的核心：**渲染是事件驱动的增量，不是全量投影**。
//! Web 端 `sessionPresentation.ts` 每帧全量重建 turns（SolidJS 响应式
//! diff 的产物）——XAML 侧不迁移该模式，改为按 `(turn, round, kind)`
//! 键寻址的局部更新。
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
//! 1. **`RoundCompleted` 前的 round**：只有其"活尾"参与更新（`UpdateLiveTail`）；
//! 2. **`RoundCompleted` 后的 round**：冻结，不再产生任何命令（内容永不重建）；
//! 3. **前序 turn/round 永不被触碰**（append-only）——万级会话下追加成本
//!    与历史规模无关，虚拟化的"高度估算/锚定"难题因此消失（round 高度在
//!    final 时固定一次）。
//!
//! 命令序列是纯数据（`RenderCommand`），UI 层（XAML 控件树）按命令执行；
//! 测试断言命令序列即验证渲染路径，无需窗口实例。

use std::collections::HashMap;

use markdown_core::ast::{Block, Inline};
use markdown_core::live::parse_live;
use markdown_core::live_table::{LiveTableTracker, TableSnapshot};
use markdown_core::parse_final;

use crate::protocol::{ConversationEvent, RoundDeltaKind};
use crate::{RichTextOutput, TableData, render_final};

/// 渲染命令（UI 层按序执行；测试断言命令序列）。
#[derive(Clone, Debug, PartialEq)]
pub enum RenderCommand {
    /// 挂载新 turn（append-only：只 push 尾部）。
    MountTurn {
        index: usize,
        user_text: String,
    },
    /// turn 状态变更（running → completed / failed）。
    UpdateTurnStatus {
        index: usize,
        status: TurnStatus,
    },
    /// 答案活尾预览替换（字面/表格交错序列；未闭合语法字面输出）。
    ///
    /// 协议表格（```table）在流式中渐进长出：表格行从字面剥离进 `segments`
    /// 的 Table 段（网格渲染），残行实时显示在网格末行（逐字生长）；
    /// 字面保留在 Text 段。多表格/闭合表格均按隐藏区间切分，内容不重复。
    UpdateLiveTail {
        turn: usize,
        round: usize,
        inlines: Vec<Inline>,
        /// 可见字面全文（全部 Text 段拼接；诊断/CLI 用）。
        raw: String,
        /// 字面/表格交错序列（UI 按序渲染）。
        segments: Vec<LiveSegment>,
    },
    /// 权威终态：全量重建该 round（RichTextBlock 段落 + 代码块通道）。
    /// `thinking` 为权威思考文本（有则折叠区也一并重建）。
    RebuildRound {
        turn: usize,
        round: usize,
        rich: RichTextOutput,
        thinking: Option<String>,
    },
    /// 思考块摘要增量（Expander 头部）。
    UpdateThinking {
        turn: usize,
        round: usize,
        text: String,
    },
    /// 工具卡创建/更新（upsert by tool_call_id）。
    UpsertToolCard {
        turn: usize,
        round: usize,
        card: ToolCardView,
    },
    /// 正文大时外置（output_ref）：UI 显示占位，应用层按 ref 拉取后
    /// 调用 [`Transcript::resolve_output`]。
    LoadOutput {
        turn: usize,
        round: usize,
        output_ref: String,
    },
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
    /// 参数 raw（原型简化：直接展示累积文本）。
    pub args_display: String,
    /// true = 工具卡完成（后续 delta 不再更新）。
    pub done: bool,
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
    /// 正在累积的工具调用 raw（未完成的 ToolCalling 流）。
    tool_raw: String,
    /// 最近一次 live 的 raw（防抖：相同 raw 不重复发命令）。
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
    Final { blocks: Vec<Block>, rich: RichTextOutput },
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
    /// 解析结果同步写回 `AnswerView::Streaming`（状态机自持，与命令一致）。
    fn answer_delta(&mut self, delta: &str) -> Option<LiveTailView> {
        let AnswerView::Streaming {
            raw,
            inlines,
            segments,
        } = &mut self.answer
        else {
            return None; // 终态后忽略 delta（协议保证不会发生）
        };
        raw.push_str(delta);
        // 表格跟踪：增量行扫描（O(新增行)）
        self.table_tracker.feed(raw);
        // 可见字面 = raw 减去表格隐藏区间；segments = 字面/表格交错
        let (visible, segs) = split_segments(raw, &self.table_tracker);
        let parsed = parse_live(&visible);
        let changed = self.last_live_raw != *raw;
        if changed {
            *inlines = parsed.clone();
            *segments = segs.clone();
            self.last_live_raw.clone_from(raw);
        }
        changed.then_some(LiveTailView {
            inlines: parsed,
            raw: visible,
            segments: segs,
        })
    }

    /// BlockCheckpoint 覆盖（自愈）：整段替换，重解析（表格跟踪器重置）。
    fn answer_checkpoint(&mut self, text: &str) -> Option<LiveTailView> {
        let AnswerView::Streaming {
            raw,
            inlines,
            segments,
        } = &mut self.answer
        else {
            return None;
        };
        self.table_tracker.reset();
        raw.clear();
        raw.push_str(text);
        self.table_tracker.feed(raw);
        let (visible, segs) = split_segments(raw, &self.table_tracker);
        let parsed = parse_live(&visible);
        let changed = self.last_live_raw != *raw;
        if changed {
            *inlines = parsed.clone();
            *segments = segs.clone();
            self.last_live_raw.clone_from(raw);
        }
        changed.then_some(LiveTailView {
            inlines: parsed,
            raw: visible,
            segments: segs,
        })
    }

    /// RoundCompleted：以权威 answer 全量重建（忽略流式累积差异）。
    fn finalize(&mut self, thinking: Option<&str>, answer: Option<&str>) {
        if let Some(t) = thinking {
            self.thinking = Some(t.to_string());
        }
        self.table_tracker.reset();
        if let Some(a) = answer {
            let blocks = parse_final(a);
            self.answer = AnswerView::Final {
                rich: render_final(&blocks),
                blocks,
            };
            self.last_live_raw.clear();
        }
    }

    /// ToolCalling 增量：累积并尝试提取工具名（upsert by id）。
    fn tool_delta(&mut self, delta: &str) -> Option<ToolCardView> {
        if self.tool_calls.last().is_some_and(|c| c.done) {
            return None; // 上一张卡已完成
        }
        self.tool_raw.push_str(delta);
        self.upsert_current_card()
    }

    fn tool_checkpoint(&mut self, text: &str) -> Option<ToolCardView> {
        self.tool_raw.clear();
        self.tool_raw.push_str(text);
        self.upsert_current_card()
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
        })
    }

    /// 工具调用完成（RoundCompleted 时收尾所有卡）。
    fn finish_tool_cards(&mut self) {
        for card in &mut self.tool_calls {
            card.done = true;
        }
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

/// 活尾视图：live 答案的渲染产物（可见字面 + 字面/表格交错序列）。
#[derive(Clone, Debug, PartialEq)]
pub struct LiveTailView {
    pub inlines: Vec<Inline>,
    /// 可见字面全文（全部 Text 段拼接；诊断/CLI 用）。
    pub raw: String,
    pub segments: Vec<LiveSegment>,
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
}

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

    /// 快照恢复：用权威 turns（timeline 快照解析产物）整体替换当前状态。
    /// 历史回合直接落 Final（不再流式）；此后增量事件照常 append。
    ///
    /// 幂等语义：快照是权威全量（daemon timeline 快照），增量事件在其后
    /// 到达；调用方在 seed 切换时应先重置（`Transcript::new`）再 restore。
    pub fn restore(&mut self, turns: Vec<RestoredTurn>) {
        self.turns = turns
            .into_iter()
            .map(|t| TurnView {
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
            })
            .collect();
        self.turn_index = self
            .turns
            .iter()
            .enumerate()
            .map(|(i, t)| (t.turn_id.clone(), i))
            .collect();
    }

    /// 应用一个协议事件，产出渲染命令（可能为空 = 无需触碰 UI）。
    pub fn apply(&mut self, ev: &ConversationEvent) -> Vec<RenderCommand> {
        match ev {
            ConversationEvent::TurnStarted {
                turn_id,
                user_text,
            } => {
                let index = self.turns.len();
                self.turns.push(TurnView {
                    turn_id: turn_id.clone(),
                    user_text: user_text.clone(),
                    status: TurnStatus::Running,
                    rounds: Vec::new(),
                });
                self.turn_index.insert(turn_id.clone(), index);
                vec![RenderCommand::MountTurn {
                    index,
                    user_text: user_text.clone(),
                }]
            }
            ConversationEvent::TurnCompleted { turn_id } => {
                let Some(&index) = self.turn_index.get(turn_id) else {
                    return Vec::new();
                };
                self.turns[index].status = TurnStatus::Completed;
                vec![RenderCommand::UpdateTurnStatus {
                    index,
                    status: TurnStatus::Completed,
                }]
            }
            ConversationEvent::TurnFailed { turn_id, .. } => {
                let Some(&index) = self.turn_index.get(turn_id) else {
                    return Vec::new();
                };
                self.turns[index].status = TurnStatus::Failed;
                vec![RenderCommand::UpdateTurnStatus {
                    index,
                    status: TurnStatus::Failed,
                }]
            }
            // 渲染不关心的领域事件（provider_retrying / usage_updated /
            // compact_* / conversation_cancelled 等）：零命令。
            ConversationEvent::Unknown => Vec::new(),
            ConversationEvent::RoundDelta {
                turn_id,
                round_num,
                kind,
                delta,
            } => {
                let Some(&turn) = self.turn_index.get(turn_id) else {
                    return Vec::new();
                };
                let (round_idx, round) = self.round_mut(turn, *round_num);
                match kind {
                    RoundDeltaKind::Answering => round
                        .answer_delta(delta)
                        .map(|view| {
                            vec![RenderCommand::UpdateLiveTail {
                                turn,
                                round: round_idx,
                                inlines: view.inlines,
                                raw: view.raw,
                                segments: view.segments,
                            }]
                        })
                        .unwrap_or_default(),
                    RoundDeltaKind::Thinking => {
                        let t = round.thinking.get_or_insert_with(String::new);
                        t.push_str(delta);
                        vec![RenderCommand::UpdateThinking {
                            turn,
                            round: round_idx,
                            text: t.clone(),
                        }]
                    }
                    RoundDeltaKind::ToolCalling => round
                        .tool_delta(delta)
                        .map(|card| {
                            vec![RenderCommand::UpsertToolCard {
                                turn,
                                round: round_idx,
                                card,
                            }]
                        })
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
                    return Vec::new();
                };
                let (round_idx, round) = self.round_mut(turn, *round_num);
                match kind {
                    RoundDeltaKind::Answering => round
                        .answer_checkpoint(text)
                        .map(|view| {
                            vec![RenderCommand::UpdateLiveTail {
                                turn,
                                round: round_idx,
                                inlines: view.inlines,
                                raw: view.raw,
                                segments: view.segments,
                            }]
                        })
                        .unwrap_or_default(),
                    RoundDeltaKind::Thinking => {
                        round.thinking = Some(text.clone());
                        vec![RenderCommand::UpdateThinking {
                            turn,
                            round: round_idx,
                            text: text.clone(),
                        }]
                    }
                    RoundDeltaKind::ToolCalling => round
                        .tool_checkpoint(text)
                        .map(|card| {
                            vec![RenderCommand::UpsertToolCard {
                                turn,
                                round: round_idx,
                                card,
                            }]
                        })
                        .unwrap_or_default(),
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
                    return Vec::new();
                };
                // 外置正文：占位，等应用层拉取后 resolve_output
                if let Some(ref_uri) = output_ref
                    && answer.is_none()
                {
                    let (round_idx, _) = self.round_mut(turn, *round_num);
                    return vec![RenderCommand::LoadOutput {
                        turn,
                        round: round_idx,
                        output_ref: ref_uri
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| ref_uri.to_string()),
                    }];
                }
                let (round_idx, round) = self.round_mut(turn, *round_num);
                round.finalize(thinking.as_deref(), answer.as_deref());
                round.finish_tool_cards();
                let rich = match &round.answer {
                    AnswerView::Final { rich, .. } => rich.clone(),
                    _ => RichTextOutput::default(),
                };
                vec![RenderCommand::RebuildRound {
                    turn,
                    round: round_idx,
                    rich,
                    thinking: thinking.clone(),
                }]
            }
        }
    }

    /// 外置正文拉取完成：以权威文本重建（对应 `output_ref` 加载路径）。
    pub fn resolve_output(
        &mut self,
        turn_id: &str,
        round_num: u32,
        text: &str,
    ) -> Vec<RenderCommand> {
        let Some(&turn) = self.turn_index.get(turn_id) else {
            return Vec::new();
        };
        let (round_idx, round) = self.round_mut(turn, round_num);
        round.finalize(None, Some(text));
        let rich = match &round.answer {
            AnswerView::Final { rich, .. } => rich.clone(),
            _ => RichTextOutput::default(),
        };
        vec![RenderCommand::RebuildRound {
            turn,
            round: round_idx,
            rich,
            thinking: round.thinking.clone(),
        }]
    }

    fn round_mut(&mut self, turn: usize, round_num: u32) -> (usize, &mut RoundView) {
        let turn_view = &mut self.turns[turn];
        if let Some(r) = turn_view.rounds.iter().position(|r| r.round_num == round_num) {
            (r, &mut turn_view.rounds[r])
        } else {
            turn_view.rounds.push(RoundView::new(round_num));
            let idx = turn_view.rounds.len() - 1;
            (idx, &mut turn_view.rounds[idx])
        }
    }
}
