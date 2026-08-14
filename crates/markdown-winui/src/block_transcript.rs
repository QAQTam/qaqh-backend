//! Block 级 Transcript 状态机：timeline 事件 → 声明式视图状态。
//!
//! 与 [`crate::round_renderer`] 的关系：本模块是 **timeline 驱动的权威
//! transcript 渲染模型**（Phase 2 主体），round 模型是 legacy 投影（供
//! 双通道对照，Phase 3 退役）。
//!
//! 核心差异：blocks 按**到达序**存储（= 真实块序，跨 round 保序）——
//! "思考-工具-回复"交错场景不再被压平为固定三段式布局（缺陷 D1）。
//! `block_order` 仅作 round 内防御性校验。
//!
//! 渲染模型对应：
//! ```text
//! ConversationTranscript (ScrollViewer, 跟随尾部 + 锚点补偿)
//! └─ StackPanel（append-only：新 turn 只 push 尾部）
//!    └─ BlockTurnView
//!       ├─ 用户气泡（TextBlock）
//!       └─ BlockView × N（按到达序 = block_order 展平）
//!          ├─ Reasoning → Expander（摘要随流更新）
//!          ├─ Text     → Streaming: 轻量 TextBlock（每帧替换 Inlines）
//!          │              Sealed:    RichTextBlock（final markdown 一次构建）
//!          ├─ Tool     → ToolCard（upsert by block_id）
//!          └─ Notice   → 通知文本块
//! ```
//!
//! 核心不变量（对齐设计 Invariant 1-4，`timeline-protocol-design.md`）：
//! 1. `BlockSealed` 前只更新对应块的活尾（纯文本/流式）；
//! 2. `BlockSealed` 后冻结为 final markdown，迟到 delta 被忽略；
//! 3. 事件按 turn_id → block_id O(1) 定位（HashMap 索引）；
//! 4. 返回值只描述本帧最低失效等级（`TranscriptChange`），不复制渲染载荷。

use std::collections::HashMap;
use std::rc::Rc;

use crate::round_renderer::{
    AnswerView, ToolCardView, TranscriptChange,
};
use crate::timeline_protocol::{
    TimelineBlock, TimelineBlockKind, TimelineBlockState, TimelineEntry, TimelineEvent,
    TimelineRound, TimelineSnapshot, TimelineToolState, TimelineTurn, TimelineTurnState,
};

/// 恢复的 turn（timeline 快照解析产物；[`BlockTranscript::restore`] 消费）。
/// blocks 已按 (round_num, block_order) 展平为全局到达序。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BlockRestoredTurn {
    pub turn_id: String,
    /// 快照里的权威创建序（TimelineTurn.created_seq）；0 = 未知（旧数据）。
    pub created_seq: u64,
    pub user_text: String,
    /// 展示态（cancelled 与 completed 均展示为完成态；失败单独标记）。
    pub failed: bool,
    pub failure: Option<String>,
    pub blocks: Vec<BlockView>,
}

/// 一个 turn 的视图状态（append-only 累积；blocks 按到达序）。
#[derive(Clone, Debug, Default)]
pub struct BlockTurnView {
    pub turn_id: String,
    pub created_seq: u64,
    pub user_text: String,
    /// turn 是否已终态（sealed）。
    pub sealed: bool,
    /// 失败态（TurnSealed state=Failed；UI 显示失败信息）。
    pub failed: bool,
    /// TurnSealed 携带的失败信息（code: message）。
    pub failure: Option<String>,
    /// 按到达序的块列表（= 真实块序；round 间交错保序）。
    pub blocks: Vec<Rc<BlockView>>,
    /// 仅本 turn 可见内容变化时递增（窗口快照复用判据）。
    pub mutation_rev: u64,
}

/// 一个块的视图状态。
#[derive(Clone, Debug, PartialEq)]
pub struct BlockView {
    pub block_id: String,
    /// round 内稳定序（更新永不改变；渲染顺序以到达序为准）。
    pub block_order: u32,
    /// 所属模型轮次（turn 内分步展示的分组键；restore 自快照层级）。
    pub round_num: u32,
    pub kind: TimelineBlockKind,
    /// 封存标记：sealed 后冻结为 final markdown。
    pub sealed: bool,
    /// reasoning/text 累积文本（live 期间随 delta 增长；checkpoint 覆盖）。
    pub text: String,
    /// 答案流式/终态状态机（text 块专用；reasoning 复用其 live 解析）。
    pub answer: AnswerView,
    /// 工具块数据（ToolUpdated upsert；ToolProgress 追加 output）。
    pub tool: Option<ToolCardView>,
    /// 工具输出尾部（ToolProgress 累积；渲染时与 tool 卡合并）。
    pub tool_output: String,
    /// 块首次出现（BlockOpened）的墙钟；restore 的历史块为 None（无耗时）。
    pub opened_at: Option<std::time::Instant>,
    /// BlockSealed 时计算的耗时毫秒（opened_at 存在时；restore 块为 None）。
    pub duration_ms: Option<u64>,
    /// 仅本块的可见内容变化时递增（turn 内行级 memo 判据）。
    pub mutation_rev: u64,
}

impl BlockView {
    fn new(block: &TimelineBlock, restored: bool, round_num: u32) -> Self {
        let mut view = Self {
            block_id: block.block_id.clone(),
            block_order: block.block_order,
            round_num,
            kind: block.kind,
            sealed: block.state == TimelineBlockState::Sealed,
            text: block.text.clone(),
            answer: AnswerView::default(),
            tool: None,
            tool_output: String::new(),
            opened_at: (!restored).then(std::time::Instant::now),
            duration_ms: None,
            mutation_rev: 0,
        };
        if let Some(tool) = &block.tool {
            view.tool = Some(parse_tool(tool));
        }
        if view.kind == TimelineBlockKind::Tool {
            if let Some(tool) = &block.tool {
                view.tool_output = tool
                    .output
                    .clone()
                    .unwrap_or_default()
                    .trim_end()
                    .to_string();
            }
        }
        // 快照中的 text 块直接 final（历史不再流式）。
        if view.kind == TimelineBlockKind::Text
            && view.sealed
            && !view.text.is_empty()
        {
            view.answer.finalize_text(&view.text);
        }
        view
    }

    /// TextDelta 追加（委托答案状态机；已 sealed 忽略）。
    ///
    /// 文本累积适用于 `Reasoning` 与 `Text` 两种块（producer 对两者都发
    /// `TextDelta`/`BlockCheckpoint`）；`Tool`/`Notice` 块不走文本流。
    fn text_delta(&mut self, delta: &str) -> bool {
        if self.sealed
            || matches!(self.kind, TimelineBlockKind::Tool | TimelineBlockKind::Notice)
            || delta.is_empty()
        {
            return false;
        }
        let changed = self.answer.live_delta(delta);
        if changed {
            self.text.push_str(delta);
            self.mutation_rev = self.mutation_rev.wrapping_add(1);
        }
        changed
    }

    /// BlockCheckpoint 覆盖（自愈：整段替换；幂等防抖）。
    fn text_checkpoint(&mut self, text: &str) -> bool {
        if self.sealed
            || matches!(self.kind, TimelineBlockKind::Tool | TimelineBlockKind::Notice)
            || self.text == text
        {
            return false;
        }
        let changed = self.answer.live_checkpoint(text);
        if changed {
            self.text.clear();
            self.text.push_str(text);
            self.mutation_rev = self.mutation_rev.wrapping_add(1);
        }
        changed
    }

    /// 封存：冻结为 final markdown（幂等）。
    fn seal(&mut self) -> bool {
        if self.sealed {
            return false;
        }
        self.sealed = true;
        // 耗时：opened_at → sealed 的墙钟差（restore 块无起始时间 → None）。
        if let Some(opened) = self.opened_at {
            self.duration_ms = Some(opened.elapsed().as_millis() as u64);
        }
        self.mutation_rev = self.mutation_rev.wrapping_add(1);
        // text 块：流式累积 → final 富文本（设计 Invariant 2/4）。
        if self.kind == TimelineBlockKind::Text {
            self.answer.finalize_text(&self.text);
        }
        true
    }
}

/// 会话级 block transcript 状态机（timeline 单源）。
#[derive(Clone, Debug, Default)]
pub struct BlockTranscript {
    turns: Vec<BlockTurnView>,
    /// turn_id → index（长会话 O(1) 寻址）。
    turn_index: HashMap<String, usize>,
    /// 渲染窗口起点（turns 下标）：`[window_start, turns.len())` 是实际
    /// 传给 list_view 的回合（镜像 round_renderer 的窗口化语义）。
    window_start: usize,
    /// 窗口是否处于「跟随尾部」模式（语义同 round 版）。
    tail_following: bool,
    /// 渲染投影版本号（同 round 版语义）。
    rev: u64,
}

/// 默认渲染窗口大小（回合数）；与 round 版一致（XAML 行数同量级）。
pub const BLOCK_WINDOW_DEFAULT_LEN: usize = 30;

/// restore 窗口的 block 预算：尾部累计 blocks 超过此值即收缩窗口
/// （超大回合会话一次 mount 数千元素 → 切换标签卡顿）。
pub const RESTORE_BLOCK_BUDGET: usize = 500;

/// 渲染窗口之前最多保留的 turn 数量（上滚预加载缓冲）。
pub const MAX_RETAINED_BEFORE_WINDOW: usize = 100;

/// restore 时最多保留的 turn 数量（窗口 + 上滚缓冲）。
pub const BLOCK_RESTORE_KEEP_TURNS: usize = BLOCK_WINDOW_DEFAULT_LEN + MAX_RETAINED_BEFORE_WINDOW;

impl BlockTranscript {
    pub fn new() -> Self {
        Self::default()
    }

    /// 已挂载 turn 数（规模观测）。
    pub fn turn_count(&self) -> usize {
        self.turns.len()
    }

    pub fn turns(&self) -> &[BlockTurnView] {
        &self.turns
    }

    /// 当前渲染窗口（尾部连续区间切片）——list_view 的实际数据源。
    pub fn window_turns(&self) -> &[BlockTurnView] {
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

    /// 窗口是否处于「跟随尾部」模式。
    pub fn tail_following(&self) -> bool {
        self.tail_following
    }

    /// 窗口滑向末尾（跟随尾部语义）：起点右移保持窗口大小，驱逐窗口前
    /// 超出缓冲的最旧 turn。
    pub fn slide_window_tail(&mut self) {
        let old_start = self.window_start;
        let old_len = self.turns.len();
        let old_following = self.tail_following;
        if self.turns.len() > BLOCK_WINDOW_DEFAULT_LEN {
            self.window_start = self
                .window_start
                .max(self.turns.len() - BLOCK_WINDOW_DEFAULT_LEN);
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

    /// 裁剪到仅保留渲染窗口内的 turn（会话缓存用，降低 clone 体积）。
    pub fn trim_to_window(&mut self) {
        if self.window_start == 0 {
            return;
        }
        self.turns.drain(..self.window_start);
        self.window_start = 0;
        self.rebuild_index();
        self.bump_rev();
    }

    /// 驱逐渲染窗口前超出缓冲的最旧 turn，释放其内存。
    fn evict_old_turns(&mut self) {
        let keep_before = MAX_RETAINED_BEFORE_WINDOW.min(self.window_start);
        let evict = self.window_start - keep_before;
        if evict == 0 {
            return;
        }
        self.turns.drain(..evict);
        self.window_start -= evict;
        self.rebuild_index();
    }

    fn rebuild_index(&mut self) {
        self.turn_index = self
            .turns
            .iter()
            .enumerate()
            .map(|(i, t)| (t.turn_id.clone(), i))
            .collect();
    }

    fn bump_rev(&mut self) {
        self.rev = self.rev.wrapping_add(1);
    }

    /// 前插一页更早的回合（分页加载）。已存在的 turn_id 跳过；窗口起点
    /// 右移 `n` 保持渲染窗口位置。返回实际前插数。
    pub fn prepend_turns(&mut self, turns: Vec<BlockRestoredTurn>) -> usize {
        let known: std::collections::HashSet<&str> =
            self.turns.iter().map(|t| t.turn_id.as_str()).collect();
        let fresh: Vec<BlockRestoredTurn> = turns
            .into_iter()
            .filter(|t| !known.contains(t.turn_id.as_str()))
            .collect();
        if fresh.is_empty() {
            return 0;
        }
        let n = fresh.len();
        let mut new_turns: Vec<BlockTurnView> =
            fresh.into_iter().map(to_turn_view).collect();
        new_turns.append(&mut self.turns);
        self.turns = new_turns;
        self.rebuild_index();
        self.window_start += n;
        self.bump_rev();
        n
    }

    /// 分页前插（快照页入口）：内部展平 turns 后复用 [`Self::prepend_turns`]。
    pub fn prepend_snapshot(&mut self, snapshot: &TimelineSnapshot) -> usize {
        let turns: Vec<BlockRestoredTurn> =
            snapshot.turns.iter().map(flatten_turn).collect();
        self.prepend_turns(turns)
    }

    /// 快照恢复：权威全量替换当前状态（幂等语义同 round 版）。窗口化：
    /// 只渲染最近 N 个 turn + block 预算收缩（超大回合防 mount 爆炸）。
    pub fn restore(&mut self, snapshot: &TimelineSnapshot) {
        let previous: HashMap<String, BlockTurnView> = std::mem::take(&mut self.turns)
            .into_iter()
            .map(|turn| (turn.turn_id.clone(), turn))
            .collect();
        let mut restored = Vec::with_capacity(snapshot.turns.len());
        for turn in &snapshot.turns {
            let mut view = to_turn_view(flatten_turn(turn));
            // 幂等合并：与旧视图同 id 时保留 mutation_rev 连续性（仅当
            // 可见内容等价——快照是权威，旧视图仅供渲染缓存复用）。
            if let Some(old) = previous.get(&view.turn_id) {
                view.mutation_rev = if same_rendered_turn(old, &view) {
                    old.mutation_rev
                } else {
                    old.mutation_rev.wrapping_add(1)
                };
            }
            restored.push(view);
        }
        self.turns = restored;
        self.rebuild_index();
        // 窗口化：按 block 预算从尾部累计收缩（保留最新回合）。
        let mut budget = RESTORE_BLOCK_BUDGET;
        let mut start_budget = 0usize;
        for (i, t) in self.turns.iter().enumerate().rev() {
            let blocks = t.blocks.len().max(1);
            if budget < blocks {
                start_budget = i;
                break;
            }
            budget -= blocks;
            start_budget = i;
        }
        self.window_start = start_budget
            .max(self.turns.len().saturating_sub(BLOCK_WINDOW_DEFAULT_LEN));
        self.tail_following = true;
        self.evict_old_turns();
        self.bump_rev();
    }

    /// Apply one timeline entry（live SSE 单条）。
    pub fn apply_entry(&mut self, entry: &TimelineEntry) -> TranscriptChange {
        let turn_id = entry.turn_id.clone();
        match &entry.event {
            TimelineEvent::TurnOpened { user_text } => {
                if let Some(&index) = self.turn_index.get(&turn_id) {
                    let turn = &mut self.turns[index];
                    if turn.user_text == *user_text {
                        return TranscriptChange::default();
                    }
                    turn.user_text.clone_from(user_text);
                    self.bump_turn(index);
                    return TranscriptChange::structural(true);
                }
                let index = self.turns.len();
                self.turns.push(BlockTurnView {
                    turn_id: turn_id.clone(),
                    created_seq: entry.timeline_seq,
                    user_text: user_text.clone(),
                    sealed: false,
                    failed: false,
                    failure: None,
                    blocks: Vec::new(),
                    mutation_rev: 0,
                });
                self.turn_index.insert(turn_id, index);
                self.bump_rev();
                TranscriptChange::structural(true)
            }
            TimelineEvent::BlockOpened { block } => {
                let index = self.ensure_turn(&turn_id);
                let turn = &mut self.turns[index];
                // 防御：同 id 已存在则覆盖（writer 单写保证不会发生）。
                if let Some(existing) = turn.blocks.iter_mut().find(|b| b.block_id == block.block_id)
                {
                    let mut view = BlockView::new(
                        block,
                        false,
                        entry.round_num.unwrap_or(0),
                    );
                    view.mutation_rev = existing.mutation_rev.wrapping_add(1);
                    *existing = Rc::new(view);
                } else {
                    turn.blocks
                        .push(Rc::new(BlockView::new(block, false, entry.round_num.unwrap_or(0))));
                }
                self.bump_turn(index);
                TranscriptChange::structural(true)
            }
            TimelineEvent::TextDelta {
                block_id,
                fragment_seq: _,
                delta,
            } => {
                let Some(block) = self.block_mut(&turn_id, block_id) else {
                    return TranscriptChange::default();
                };
                let changed = block.text_delta(delta);
                if changed {
                    // live 内容变化必须反映到渲染投影（窗口快照缓存键
                    // 含 transcript.rev；漏 bump 会导致整段封存才一次性刷出）。
                    self.bump_turn_for_block(&turn_id, block_id);
                }
                changed
                    .then(|| TranscriptChange::live(true))
                    .unwrap_or_default()
            }
            TimelineEvent::BlockCheckpoint { block_id, text } => {
                let Some(block) = self.block_mut(&turn_id, block_id) else {
                    return TranscriptChange::default();
                };
                let changed = block.text_checkpoint(text);
                if changed {
                    self.bump_turn_for_block(&turn_id, block_id);
                }
                changed
                    .then(|| TranscriptChange::live(true))
                    .unwrap_or_default()
            }
            TimelineEvent::ToolUpdated { block_id, tool } => {
                let Some(block) = self.block_mut(&turn_id, block_id) else {
                    return TranscriptChange::default();
                };
                let card = parse_tool(tool);
                let changed = match &mut block.tool {
                    Some(existing) => {
                        let different = *existing != card;
                        if different {
                            *existing = card;
                        }
                        different
                    }
                    None => {
                        block.tool = Some(card);
                        true
                    }
                };
                if changed {
                    block.mutation_rev = block.mutation_rev.wrapping_add(1);
                    self.bump_turn_for_block(&turn_id, block_id);
                    TranscriptChange::structural(true)
                } else {
                    TranscriptChange::default()
                }
            }
            TimelineEvent::ToolProgress { block_id, chunk } => {
                let Some(block) = self.block_mut(&turn_id, block_id) else {
                    return TranscriptChange::default();
                };
                if chunk.is_empty() {
                    return TranscriptChange::default();
                }
                block.tool_output.push_str(chunk);
                block.mutation_rev = block.mutation_rev.wrapping_add(1);
                self.bump_turn_for_block(&turn_id, block_id);
                TranscriptChange::live(true)
            }
            TimelineEvent::BlockSealed { block_id } => {
                let Some(block) = self.block_mut(&turn_id, block_id) else {
                    return TranscriptChange::default();
                };
                let changed = block.seal();
                if changed {
                    block.mutation_rev = block.mutation_rev.wrapping_add(1);
                    self.bump_turn_for_block(&turn_id, block_id);
                    TranscriptChange::structural(true)
                } else {
                    TranscriptChange::default()
                }
            }
            TimelineEvent::RoundSealed { .. } => {
                // round 边界仅影响 header（保留 conversation turn 事件）；
                // transcript 的完成判定以 BlockSealed/TurnSealed 为准。
                TranscriptChange::default()
            }
            TimelineEvent::TurnSealed { state, failure } => {
                let Some(&index) = self.turn_index.get(&turn_id) else {
                    return TranscriptChange::default();
                };
                let turn = &mut self.turns[index];
                let failed = *state == TimelineTurnState::Failed;
                if turn.sealed && turn.failed == failed {
                    return TranscriptChange::default();
                }
                turn.sealed = true;
                turn.failed = failed;
                turn.failure = failure
                    .as_ref()
                    .map(|f| format!("{}: {}", f.code, f.message));
                self.bump_turn(index);
                TranscriptChange::structural(false)
            }
        }
    }

    /// Apply 一帧内多条 entry（live SSE 批量；合并失效等级）。
    pub fn apply_frame(&mut self, entries: &[TimelineEntry]) -> TranscriptChange {
        let mut change = TranscriptChange::default();
        for entry in entries {
            change.merge(self.apply_entry(entry));
        }
        change
    }

    /// turn 定位（不存在则自动建——防御：live 流乱序/断点恢复时
    /// BlockOpened 先于 TurnOpened 到达）。
    fn ensure_turn(&mut self, turn_id: &str) -> usize {
        if let Some(&index) = self.turn_index.get(turn_id) {
            return index;
        }
        let index = self.turns.len();
        self.turns.push(BlockTurnView {
            turn_id: turn_id.to_string(),
            created_seq: 0,
            user_text: String::new(),
            sealed: false,
            failed: false,
            failure: None,
            blocks: Vec::new(),
            mutation_rev: 0,
        });
        self.turn_index.insert(turn_id.to_string(), index);
        index
    }

    /// 定位 (turn, block)；block 不存在返回 None（writer 单写保证已打开）。
    fn block_mut(&mut self, turn_id: &str, block_id: &str) -> Option<&mut BlockView> {
        let index = *self.turn_index.get(turn_id)?;
        let turn = &mut self.turns[index];
        turn.blocks
            .iter_mut()
            .find(|b| b.block_id == block_id)
            .map(Rc::make_mut)
    }

    /// 递增 turn 可见内容版本（block 定位后调用；避免重复查找）。
    fn bump_turn_for_block(&mut self, turn_id: &str, _block_id: &str) {
        if let Some(&index) = self.turn_index.get(turn_id) {
            self.bump_turn(index);
        }
    }

    fn bump_turn(&mut self, index: usize) {
        self.turns[index].mutation_rev = self.turns[index].mutation_rev.wrapping_add(1);
        self.bump_rev();
    }
}

/// TimelineTool → 工具卡视图（与 round 版 parse_tool 同构）。
fn parse_tool(tool: &crate::timeline_protocol::TimelineTool) -> ToolCardView {
    let args_display = tool
        .summary
        .as_deref()
        .or(tool.args_json.as_deref())
        .unwrap_or("")
        .to_string();
    let done = matches!(
        tool.state,
        TimelineToolState::Succeeded | TimelineToolState::Failed
    );
    let failed = tool.state == TimelineToolState::Failed;
    // Prepared = LLM 刚吐出 tool_call 的 replaceable 预览（未真正执行）——
    // 前端在 started 前不渲染，ToolStarted 后才显示转圈。
    let started = tool.state != TimelineToolState::Prepared;
    let failure = tool
        .failure
        .as_ref()
        .map(|f| format!("{}: {}", f.code, f.message));
    let body = crate::tool_body_from_timeline(
        &tool.name,
        tool.args_json.as_deref(),
        tool.output.as_deref(),
        tool.diff.as_deref(),
    );
    let changes = crate::change_stats_from_timeline(
        &body,
        tool.summary.as_deref().or(tool.output.as_deref()),
    );
    ToolCardView {
        id: tool.tool_call_id.clone(),
        name: Some(tool.name.clone()),
        args_display,
        args_json: tool.args_json.clone(),
        body,
        changes,
        done,
        failed,
        failure,
        provider: false,
        started,
    }
}

/// TimelineSnapshot turn → 展平（rounds 按 round_num 排序、blocks 按
/// block_order 排序后拼接为全局到达序）。
fn flatten_turn(turn: &TimelineTurn) -> BlockRestoredTurn {
    let mut rounds: Vec<&TimelineRound> = turn.rounds.iter().collect();
    rounds.sort_by_key(|r| r.round_num);
    let mut blocks = Vec::new();
    for round in rounds {
        let mut round_blocks: Vec<&TimelineBlock> = round.blocks.iter().collect();
        round_blocks.sort_by_key(|b| b.block_order);
        for block in round_blocks {
            blocks.push(BlockView::new(block, true, round.round_num));
        }
    }
    BlockRestoredTurn {
        turn_id: turn.turn_id.clone(),
        created_seq: turn.created_seq,
        user_text: turn.user_text.clone(),
        failed: turn.state == TimelineTurnState::Failed,
        failure: turn
            .failure
            .as_ref()
            .map(|f| format!("{}: {}", f.code, f.message)),
        blocks,
    }
}

fn to_turn_view(t: BlockRestoredTurn) -> BlockTurnView {
    let mutation_rev = 0;
    BlockTurnView {
        turn_id: t.turn_id,
        created_seq: t.created_seq,
        user_text: t.user_text,
        sealed: true,
        failed: t.failed,
        failure: t.failure,
        blocks: t.blocks.into_iter().map(Rc::new).collect(),
        mutation_rev,
    }
}

/// 幂等合并判据：快照视图与旧视图的可见内容是否等价（仅渲染缓存复用，
/// 不参与语义）。
fn same_rendered_turn(old: &BlockTurnView, new: &BlockTurnView) -> bool {
    old.user_text == new.user_text
        && old.failed == new.failed
        && old.failure == new.failure
        && old.blocks.len() == new.blocks.len()
        && old
            .blocks
            .iter()
            .zip(new.blocks.iter())
            .all(|(a, b)| a.block_id == b.block_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline_protocol::{
        TimelineBlock, TimelineBlockState, TimelineEvent, TimelineFailure, TimelineRound,
        TimelineTool, TimelineToolState,
    };

    fn entry(seq: u64, turn_id: &str, round: Option<u32>, event: TimelineEvent) -> TimelineEntry {
        TimelineEntry {
            timeline_seq: seq,
            turn_id: turn_id.into(),
            round_num: round,
            event,
        }
    }

    fn open_block(block_id: &str, order: u32, kind: TimelineBlockKind) -> TimelineEntry {
        entry(
            2,
            "t1",
            Some(0),
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: block_id.into(),
                    block_order: order,
                    kind,
                    state: TimelineBlockState::Open,
                    text: String::new(),
                    tool: None,
                },
            },
        )
    }

    fn open_turn(seq: u64, user_text: &str) -> TimelineEntry {
        entry(seq, "t1", None, TimelineEvent::TurnOpened {
            user_text: user_text.into(),
        })
    }

    /// 工具块携带展示平面 diff → 卡片 body 解析为 Diff（turn 末尾「查看详情」
    /// 按钮 + diff 抽屉的数据源；edit_file/write 常规执行的链路终结点）。
    #[test]
    fn tool_diff_from_timeline_becomes_diff_body() {
        let mut ts = BlockTranscript::new();
        ts.apply_entry(&open_turn(1, "hi"));
        ts.apply_entry(&open_block("tool:c1", 0, TimelineBlockKind::Tool));
        ts.apply_entry(&entry(
            3,
            "t1",
            Some(0),
            TimelineEvent::ToolUpdated {
                block_id: "tool:c1".into(),
                tool: TimelineTool {
                    tool_call_id: "c1".into(),
                    name: "edit_file".into(),
                    state: TimelineToolState::Succeeded,
                    summary: Some("[OK] edit_file\n  src/a.rs: 1/1 op(s) applied at L2 (+1 -1)".into()),
                    args_json: Some(r#"{"old_string":"old","new_string":"new"}"#.into()),
                    output: Some("[OK] edit_file\n  src/a.rs: 1/1 op(s) applied at L2 (+1 -1)".into()),
                    diff: Some("--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-old\n+new\n".into()),
                    progress: String::new(),
                    failure: None,
                    permission: None,
                },
            },
        ));
        ts.apply_entry(&entry(
            4,
            "t1",
            Some(0),
            TimelineEvent::BlockSealed {
                block_id: "tool:c1".into(),
            },
        ));
        let card = ts.turns()[0].blocks[0].tool.as_ref().unwrap();
        assert!(matches!(card.body, crate::ToolBody::Diff(_)));
        // 变更统计来自 Diff 文档（精确计数），而非摘要行猜测。
        let changes = card.changes.as_ref().unwrap();
        assert_eq!((changes.lines_added, changes.lines_removed), (1, 1));
        assert_eq!(changes.file.as_deref(), Some("src/a.rs"));
    }

    /// live 事件序列：TurnOpened → BlockOpened(text) → TextDelta × 2 →
    /// BlockCheckpoint（自愈）→ BlockSealed → TurnSealed。
    #[test]
    fn live_sequence_reaches_final_state() {
        let mut ts = BlockTranscript::new();
        ts.apply_entry(&open_turn(1, "hi"));
        ts.apply_entry(&open_block("text:b1", 0, TimelineBlockKind::Text));
        ts.apply_entry(&entry(
            3,
            "t1",
            Some(0),
            TimelineEvent::TextDelta {
                block_id: "text:b1".into(),
                fragment_seq: 0,
                delta: "hel".into(),
            },
        ));
        ts.apply_entry(&entry(
            4,
            "t1",
            Some(0),
            TimelineEvent::TextDelta {
                block_id: "text:b1".into(),
                fragment_seq: 1,
                delta: "lo".into(),
            },
        ));
        // 乱序/丢失自愈：checkpoint 覆盖
        ts.apply_entry(&entry(
            5,
            "t1",
            Some(0),
            TimelineEvent::BlockCheckpoint {
                block_id: "text:b1".into(),
                text: "hello world".into(),
            },
        ));
        ts.apply_entry(&entry(
            6,
            "t1",
            Some(0),
            TimelineEvent::BlockSealed {
                block_id: "text:b1".into(),
            },
        ));
        ts.apply_entry(&entry(
            7,
            "t1",
            None,
            TimelineEvent::TurnSealed {
                state: TimelineTurnState::Completed,
                failure: None,
            },
        ));
        let turn = &ts.turns()[0];
        assert_eq!(turn.user_text, "hi");
        assert!(turn.sealed);
        assert!(!turn.failed);
        let block = &turn.blocks[0];
        assert_eq!(block.text, "hello world", "checkpoint 覆盖自愈");
        assert!(block.sealed);
        assert!(
            matches!(block.answer, AnswerView::Final { .. }),
            "sealed 后 final markdown"
        );
    }

    /// 交错块序：思考-工具-回复（工具块在中间）——blocks 保持到达序，
    /// 不被压平为三段式。
    #[test]
    fn interleaved_blocks_keep_arrival_order() {
        let mut ts = BlockTranscript::new();
        ts.apply_entry(&open_turn(1, "hi"));
        ts.apply_entry(&open_block("reasoning:r1", 0, TimelineBlockKind::Reasoning));
        ts.apply_entry(&open_block("tool:c1", 1, TimelineBlockKind::Tool));
        ts.apply_entry(&open_block("text:a1", 2, TimelineBlockKind::Text));
        ts.apply_entry(&entry(
            3,
            "t1",
            Some(0),
            TimelineEvent::ToolUpdated {
                block_id: "tool:c1".into(),
                tool: TimelineTool {
                    tool_call_id: "c1".into(),
                    name: "exec".into(),
                    state: TimelineToolState::Running,
                    summary: None,
                    args_json: Some("{\"cmd\":\"ls\"}".into()),
                    output: None,
                    diff: None,
                    progress: String::new(),
                    failure: None,
                    permission: None,
                },
            },
        ));
        ts.apply_entry(&entry(
            4,
            "t1",
            Some(0),
            TimelineEvent::TextDelta {
                block_id: "text:a1".into(),
                fragment_seq: 0,
                delta: "answer".into(),
            },
        ));
        let kinds: Vec<TimelineBlockKind> = ts.turns()[0]
            .blocks
            .iter()
            .map(|b| b.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                TimelineBlockKind::Reasoning,
                TimelineBlockKind::Tool,
                TimelineBlockKind::Text
            ],
            "块序 = 到达序（思考-工具-回复交错保序）"
        );
        let tool_block = &ts.turns()[0].blocks[1];
        assert_eq!(tool_block.tool.as_ref().unwrap().name.as_deref(), Some("exec"));
    }

    /// 工具失败态透传：ToolUpdated state=Failed + failure → ToolCardView
    /// failed=true + failure 摘要（V4 行级 ✕ 与区段头部 ⚠ 的数据源）。
    #[test]
    fn tool_failure_is_propagated_to_card() {
        let mut ts = BlockTranscript::new();
        ts.apply_entry(&open_turn(1, "hi"));
        ts.apply_entry(&open_block("tool:c1", 0, TimelineBlockKind::Tool));
        ts.apply_entry(&entry(
            3,
            "t1",
            Some(0),
            TimelineEvent::ToolUpdated {
                block_id: "tool:c1".into(),
                tool: TimelineTool {
                    tool_call_id: "c1".into(),
                    name: "read_file".into(),
                    state: TimelineToolState::Failed,
                    summary: None,
                    args_json: Some("{\"path\":\"src/x.rs\"}".into()),
                    output: None,
                    diff: None,
                    progress: String::new(),
                    failure: Some(TimelineFailure {
                        code: "not_found".into(),
                        message: "找不到符号 restored_turns".into(),
                    }),
                    permission: None,
                },
            },
        ));
        let card = ts.turns()[0].blocks[0].tool.as_ref().unwrap();
        assert!(card.done, "Failed 视为终态");
        assert!(card.failed, "失败标记透传");
        assert_eq!(
            card.failure.as_deref(),
            Some("not_found: 找不到符号 restored_turns")
        );
    }

    /// 耗时：live 块 seal 后 duration_ms 有值；restore 块为 None。
    #[test]
    fn duration_is_measured_for_live_blocks_only() {
        let mut ts = BlockTranscript::new();
        ts.apply_entry(&open_turn(1, "hi"));
        ts.apply_entry(&open_block("tool:c1", 0, TimelineBlockKind::Tool));
        let before = ts.turns()[0].blocks[0].duration_ms;
        assert_eq!(before, None);
        ts.apply_entry(&entry(
            3,
            "t1",
            Some(0),
            TimelineEvent::BlockSealed {
                block_id: "tool:c1".into(),
            },
        ));
        let after = ts.turns()[0].blocks[0].duration_ms;
        assert!(after.is_some(), "live 块 seal 后应有时耗");
        // restore 路径：opened_at=None → duration 恒 None
        let snapshot = TimelineSnapshot {
            watermark: 10,
            turns: vec![TimelineTurn {
                turn_id: "t2".into(),
                created_seq: 2,
                user_text: "hi".into(),
                sealed: true,
                state: TimelineTurnState::Completed,
                failure: None,
                rounds: vec![TimelineRound {
                    round_num: 0,
                    sealed: true,
                    is_final: true,
                    blocks: vec![TimelineBlock {
                        block_id: "tool:c2".into(),
                        block_order: 0,
                        kind: TimelineBlockKind::Tool,
                        state: TimelineBlockState::Sealed,
                        text: String::new(),
                        tool: Some(TimelineTool {
                            tool_call_id: "c2".into(),
                            name: "exec".into(),
                            state: TimelineToolState::Succeeded,
                            summary: None,
                            args_json: None,
                            output: None,
                            diff: None,
                            progress: String::new(),
                            failure: None,
                            permission: None,
                        }),
                    }],
                }],
            }],
        };
        ts.restore(&snapshot);
        assert_eq!(
            ts.turns()[0].blocks[0].duration_ms,
            None,
            "restore 历史块无耗时"
        );
    }

    /// 快照 restore：rounds/blocks 展平保序（防御性排序）。
    #[test]
    fn restore_flattens_rounds_preserving_block_order() {
        let snapshot = TimelineSnapshot {
            watermark: 10,
            turns: vec![TimelineTurn {
                turn_id: "t1".into(),
                created_seq: 1,
                user_text: "hi".into(),
                sealed: true,
                state: TimelineTurnState::Completed,
                failure: None,
                rounds: vec![
                    TimelineRound {
                        round_num: 1,
                        sealed: true,
                        is_final: true,
                        blocks: vec![
                            TimelineBlock {
                                block_id: "text:b2".into(),
                                block_order: 1,
                                kind: TimelineBlockKind::Text,
                                state: TimelineBlockState::Sealed,
                                text: "answer".into(),
                                tool: None,
                            },
                            TimelineBlock {
                                block_id: "reasoning:r1".into(),
                                block_order: 0,
                                kind: TimelineBlockKind::Reasoning,
                                state: TimelineBlockState::Sealed,
                                text: "think".into(),
                                tool: None,
                            },
                        ],
                    },
                    TimelineRound {
                        round_num: 0,
                        sealed: true,
                        is_final: false,
                        blocks: vec![TimelineBlock {
                            block_id: "tool:c1".into(),
                            block_order: 0,
                            kind: TimelineBlockKind::Tool,
                            state: TimelineBlockState::Sealed,
                            text: String::new(),
                            tool: Some(TimelineTool {
                                tool_call_id: "c1".into(),
                                name: "exec".into(),
                                state: TimelineToolState::Succeeded,
                                summary: Some("ok".into()),
                                args_json: None,
                                output: None,
                                diff: None,
                                progress: String::new(),
                                failure: None,
                                permission: None,
                            }),
                        }],
                    },
                ],
            }],
        };
        let mut ts = BlockTranscript::new();
        ts.restore(&snapshot);
        let turn = &ts.turns()[0];
        assert_eq!(turn.turn_id, "t1");
        assert!(turn.sealed);
        // round0 在前，round1 在后；round1 内部 block_order 0 在前
        let ids: Vec<&str> = turn.blocks.iter().map(|b| b.block_id.as_str()).collect();
        assert_eq!(ids, vec!["tool:c1", "reasoning:r1", "text:b2"]);
        // sealed text 块 final 渲染
        assert!(matches!(turn.blocks[2].answer, AnswerView::Final { .. }));
        // 工具卡
        assert_eq!(
            turn.blocks[0].tool.as_ref().unwrap().name.as_deref(),
            Some("exec")
        );
    }

    /// 失败 turn：TurnSealed state=Failed 携带 failure 文案。
    #[test]
    fn failed_turn_carries_failure() {
        let mut ts = BlockTranscript::new();
        ts.apply_entry(&open_turn(1, "hi"));
        ts.apply_entry(&entry(
            2,
            "t1",
            None,
            TimelineEvent::TurnSealed {
                state: TimelineTurnState::Failed,
                failure: Some(TimelineFailure {
                    code: "E_TIMEOUT".into(),
                    message: "provider timeout".into(),
                }),
            },
        ));
        let turn = &ts.turns()[0];
        assert!(turn.sealed);
        assert!(turn.failed);
        assert_eq!(turn.failure.as_deref(), Some("E_TIMEOUT: provider timeout"));
    }

    /// sealed 后迟到 delta 被忽略（设计 Invariant 2）。
    #[test]
    fn late_delta_after_seal_is_ignored() {
        let mut ts = BlockTranscript::new();
        ts.apply_entry(&open_turn(1, "hi"));
        ts.apply_entry(&open_block("text:b1", 0, TimelineBlockKind::Text));
        ts.apply_entry(&entry(
            3,
            "t1",
            Some(0),
            TimelineEvent::BlockSealed {
                block_id: "text:b1".into(),
            },
        ));
        let before = ts.turns()[0].blocks[0].text.clone();
        ts.apply_entry(&entry(
            4,
            "t1",
            Some(0),
            TimelineEvent::TextDelta {
                block_id: "text:b1".into(),
                fragment_seq: 0,
                delta: "late".into(),
            },
        ));
        assert_eq!(ts.turns()[0].blocks[0].text, before);
        assert_eq!(ts.mutation_rev(), 3, "迟到 delta 不产生失效");
    }

    /// 回归：reasoning 块的 TextDelta 必须累积（producer 对 reasoning 与
    /// text 都发 TextDelta；此前 kind 限制导致思考链永远为空）。
    #[test]
    fn reasoning_block_accumulates_deltas() {
        let mut ts = BlockTranscript::new();
        ts.apply_entry(&open_turn(1, "hi"));
        ts.apply_entry(&open_block("reasoning:r1", 0, TimelineBlockKind::Reasoning));
        let rev_before = ts.mutation_rev();
        ts.apply_entry(&entry(
            3,
            "t1",
            Some(0),
            TimelineEvent::TextDelta {
                block_id: "reasoning:r1".into(),
                fragment_seq: 0,
                delta: "思考".into(),
            },
        ));
        ts.apply_entry(&entry(
            4,
            "t1",
            Some(0),
            TimelineEvent::TextDelta {
                block_id: "reasoning:r1".into(),
                fragment_seq: 1,
                delta: "过程".into(),
            },
        ));
        ts.apply_entry(&entry(
            5,
            "t1",
            Some(0),
            TimelineEvent::BlockCheckpoint {
                block_id: "reasoning:r1".into(),
                text: "思考过程完整值".into(),
            },
        ));
        let block = &ts.turns()[0].blocks[0];
        assert_eq!(block.text, "思考过程完整值", "checkpoint 覆盖自愈");
        assert!(
            ts.mutation_rev() > rev_before,
            "live delta 必须推进渲染投影 rev（防整段封存才刷出）"
        );
    }

    /// 回归：text 块 live delta 后 transcript rev 递增（窗口快照缓存键
    /// 含 rev；漏 bump 会导致流式内容整段打印）。
    #[test]
    fn text_live_delta_bumps_render_rev() {
        let mut ts = BlockTranscript::new();
        ts.apply_entry(&open_turn(1, "hi"));
        ts.apply_entry(&open_block("text:b1", 0, TimelineBlockKind::Text));
        let rev_before = ts.mutation_rev();
        for (i, chunk) in ["你", "好", "世", "界"].iter().enumerate() {
            ts.apply_entry(&entry(
                3 + i as u64,
                "t1",
                Some(0),
                TimelineEvent::TextDelta {
                    block_id: "text:b1".into(),
                    fragment_seq: i as u64,
                    delta: chunk.to_string(),
                },
            ));
        }
        assert_eq!(ts.turns()[0].blocks[0].text, "你好世界");
        assert_eq!(
            ts.mutation_rev(),
            rev_before + 4,
            "每个 live delta 都推进 rev（逐 token 流式刷新）"
        );
    }

    /// 窗口化：restore 后只渲染最近 N 个回合 + block 预算收缩。
    #[test]
    fn restore_windows_by_turn_and_block_budget() {
        let turns: Vec<TimelineTurn> = (0..40)
            .map(|i| TimelineTurn {
                turn_id: format!("t{i}"),
                created_seq: i as u64,
                user_text: format!("q{i}"),
                sealed: true,
                state: TimelineTurnState::Completed,
                failure: None,
                rounds: vec![TimelineRound {
                    round_num: 0,
                    sealed: true,
                    is_final: true,
                    blocks: vec![TimelineBlock {
                        block_id: format!("text:b{i}"),
                        block_order: 0,
                        kind: TimelineBlockKind::Text,
                        state: TimelineBlockState::Sealed,
                        text: "x".into(),
                        tool: None,
                    }],
                }],
            })
            .collect();
        let mut ts = BlockTranscript::new();
        ts.restore(&TimelineSnapshot {
            watermark: 40,
            turns,
        });
        assert_eq!(ts.turn_count(), 40);
        assert_eq!(ts.window_len(), BLOCK_WINDOW_DEFAULT_LEN);
        assert_eq!(ts.window_turns()[0].turn_id, "t10");
    }
}
