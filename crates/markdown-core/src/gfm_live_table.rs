//! GFM 表格流式跟踪器（普通 markdown 表格的渐进渲染）。
//!
//! 背景：```table 协议表格有围栏红利（围栏即知表格、首行即表头），而
//! GFM 表格在分隔行（`|---|---|`）到达前**无法确认块身份**。本模块用
//! 「延迟确认」消费这个约束：
//!
//! - **候选期**（`|` 开头的行，分隔行未到）：区间**不隐藏**，按普通文本
//!   渲染——若最终不是表格，零回退成本（字面一直正确）；
//! - **分隔行确认**：表头行 + 分隔行从字面隐藏 → 表格激活（网格出现）；
//! - **行级确认**：数据行逐行追加；残行（未换行）逐字生长在网格末行；
//! - **结束封存**：空行 / 非表格行 → 表格封存（隐藏区间到末行尾，不包含
//!   结束行）；后续内容恢复正常流式。
//!
//! 语义对齐 `parse_final`（pulldown-cmark GFM）+ Codex TUI 的 table_detect：
//! - 表头行必须紧邻分隔行（候选只保留最近一行 `|` 开头行）；
//! - 分隔行：去掉首尾 `|` 后每格匹配 `:?-+:?`（**至少 3 个 `-`**）；
//! - 数据行：trim 后以 `|` 开头；列数按表头定，不足补空 / 超出截断
//!   （P0 容忍错位，final 权威渲染仍走 pulldown-cmark 严格校验）；
//! - **转义管道**：`\|` 是字面量不算分隔符（结构检测保留反斜杠）；
//! - **围栏感知**（Codex 移植）：`` ```md ``/`` ```markdown `` 围栏内的
//!   管道行可作表格（LLM 常把表格包进 markdown 围栏）；其他围栏
//!   （sh/rust/无 info）内的 `|` 是代码，不参与表格；围栏行（开/闭）
//!   结束当前表格；
//! - 与协议表格的边界：```table 围栏被识别为 Other 围栏，其内容行
//!   天然不参与 GFM 表格。
//!
//! 输出与 [`super::live_table`] 同构（`TableHiddenSpan` + `TableSnapshot`），
//! UI 端共用 `table_view` 网格通道。

use super::live_table::{TableHiddenSpan, TableSnapshot};
use super::table_detect::{FenceContext, FenceState, is_table_delimiter_line, parse_table_segments};

/// 解析状态。
#[derive(Clone, Debug, Default, PartialEq)]
enum State {
    /// 普通文本流。
    #[default]
    Outside,
    /// 候选：最近一行以 `|` 开头，等分隔行确认。区间未隐藏（按文本渲染）。
    Candidate {
        /// 候选行起点（= 潜在表头行起点；确认时作为 block_start）。
        line_start: usize,
        /// 候选表头单元格。
        header: Vec<String>,
    },
    /// 确认：表格激活，数据行逐行累积；已确认行从字面隐藏。
    Active {
        /// 表格块起点（= 表头行起点）。
        block_start: usize,
        header: Vec<String>,
        rows: Vec<Vec<String>>,
        /// 最后确认行的结束偏移（= 隐藏区间 end；残行不隐藏）。
        confirmed_to: usize,
    },
}

/// 流式 GFM 表格跟踪器（无状态机侵入的独立组件，同 `LiveTableTracker`）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GfmTableTracker {
    state: State,
    /// 围栏上下文（```md 围栏内管道行可作表格）。
    fence: Option<FenceState>,
    /// 已封存表格（完整快照 + 隐藏区间；追加不变更）。
    sealed: Vec<(TableHiddenSpan, TableSnapshot)>,
    /// 当前残行（Active 下未换行内容；逐字生长在网格末行）。
    partial: String,
    /// 增量扫描游标（行首字节偏移；raw 只追加，游标单调前进）。
    pos: usize,
}

impl GfmTableTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入完整 raw（调用方累积；内部只扫 `pos` 之后的新行）。
    ///
    /// O(新增行数)；每行 O(列数)。幂等：重复 feed 相同 raw 不重复处理。
    pub fn feed(&mut self, raw: &str) {
        let mut i = self.pos.min(raw.len());
        while let Some(rel) = raw[i..].find('\n') {
            let line_start = i;
            let line = &raw[i..i + rel];
            self.handle_line(line, line_start);
            i += rel + 1;
        }
        self.pos = i;
        // 残行：仅 Active 状态下有意义（表格激活后，未换行内容逐字生长）
        self.partial = match &self.state {
            State::Active { .. } => raw[i..].to_string(),
            _ => String::new(),
        };
    }

    /// 全部表格快照（closed + 当前打开的表格；不含残行）。
    pub fn tables(&self) -> Vec<TableSnapshot> {
        self.tables_with_spans()
            .into_iter()
            .map(|(_, snap)| snap)
            .collect()
    }

    /// 隐藏区间与表格快照的有序列表（渲染用：字面按区间切分后插入表格）。
    ///
    /// 候选期不产生区间（按文本渲染）；确认后区间覆盖表头行 → 最后确认行；
    /// 打开的表格快照 rows 尾部附残行（partial，逐字生长行）。
    pub fn tables_with_spans(&self) -> Vec<(TableHiddenSpan, TableSnapshot)> {
        let mut out: Vec<(TableHiddenSpan, TableSnapshot)> = self.sealed.clone();
        if let State::Active {
            block_start,
            header,
            rows,
            confirmed_to,
        } = &self.state
        {
            let mut snap = TableSnapshot {
                headers: header.clone(),
                rows: rows.clone(),
            };
            if !self.partial.is_empty() {
                let mut cells = split_cells(&self.partial);
                cells.truncate(header.len());
                // 补空到表头列数：网格骨架先行，单元格逐字填充——
                // 否则新列在第一个字符到达前"不存在"，产生列跳变。
                while cells.len() < header.len() {
                    cells.push(String::new());
                }
                snap.rows.push(cells);
            }
            out.push((
                TableHiddenSpan {
                    start: *block_start,
                    end: *confirmed_to,
                },
                snap,
            ));
        }
        out
    }

    /// 当前打开表格的残行起点（= 最后确认行尾）；无打开表格时 None。
    pub fn open_tail_start(&self) -> Option<usize> {
        match &self.state {
            State::Active { confirmed_to, .. } => Some(*confirmed_to),
            _ => None,
        }
    }

    /// 重置（BlockCheckpoint 整段覆盖时调用；丢弃全部累积）。
    pub fn reset(&mut self) {
        self.state = State::Outside;
        self.fence = None;
        self.sealed.clear();
        self.partial.clear();
        self.pos = 0;
    }

    /// 表格收尾：Active 封存 / Candidate 丢弃（候选期按文本渲染，无损失）。
    /// 围栏上下文变化（进入/离开围栏、进入非 markdown 围栏）时调用。
    fn drop_table(&mut self) {
        let state = std::mem::take(&mut self.state);
        if let State::Active {
            block_start,
            header,
            rows,
            confirmed_to,
        } = state
        {
            self.seal(block_start, confirmed_to, header, rows);
        }
    }

    fn handle_line(&mut self, line: &str, cur_start: usize) {
        // 1. 围栏行（开/闭）：表格收尾，上下文切换。
        if FenceState::advance(&mut self.fence, line) {
            self.drop_table();
            return;
        }
        // 2. 非 markdown 围栏内：管道是代码，不参与表格。
        if self
            .fence
            .as_ref()
            .is_some_and(|f| f.ctx == FenceContext::Other)
        {
            self.drop_table();
            return;
        }

        let trimmed = line.trim();
        // 取出状态处理再写回（消除 self.state 借用冲突）
        let state = std::mem::take(&mut self.state);
        self.state = match state {
            State::Outside => {
                if is_candidate_line(trimmed) {
                    State::Candidate {
                        line_start: cur_start,
                        header: split_cells(trimmed),
                    }
                } else {
                    State::Outside
                }
            }
            State::Candidate { line_start, header } => {
                if is_table_delimiter_line(trimmed) {
                    // 确认：表头 + 分隔行隐藏（block_start = 候选行起点）
                    State::Active {
                        block_start: line_start,
                        header,
                        rows: Vec::new(),
                        confirmed_to: cur_start + line.len() + 1,
                    }
                } else if is_candidate_line(trimmed) {
                    // 连续候选行：GFM 表头必须紧邻分隔行 → 只保留最近一行
                    State::Candidate {
                        line_start: cur_start,
                        header: split_cells(trimmed),
                    }
                } else {
                    // 非表格结构：回退（候选行已按文本渲染，零损失）
                    State::Outside
                }
            }
            State::Active {
                block_start,
                header,
                rows,
                confirmed_to,
            } => {
                if trimmed.is_empty() {
                    // 空行：表格结束封存（隐藏区间不含空行）
                    self.seal(block_start, confirmed_to, header, rows);
                    State::Outside
                } else if is_candidate_line(trimmed) {
                    // 数据行：列数按表头定（不足补空 / 超出截断）
                    let mut cells = split_cells(trimmed);
                    cells.truncate(header.len());
                    while cells.len() < header.len() {
                        cells.push(String::new());
                    }
                    let mut rows = rows;
                    rows.push(cells);
                    State::Active {
                        block_start,
                        header,
                        rows,
                        confirmed_to: cur_start + line.len() + 1,
                    }
                } else {
                    // 非表格行：表格结束封存；该行按 Outside 语义重新处理
                    self.seal(block_start, confirmed_to, header, rows);
                    if is_candidate_line(trimmed) {
                        State::Candidate {
                            line_start: cur_start,
                            header: split_cells(trimmed),
                        }
                    } else {
                        State::Outside
                    }
                }
            }
        };
    }

    fn seal(
        &mut self,
        block_start: usize,
        end: usize,
        header: Vec<String>,
        rows: Vec<Vec<String>>,
    ) {
        self.sealed.push((
            TableHiddenSpan {
                start: block_start,
                end,
            },
            TableSnapshot { headers: header, rows },
        ));
    }
}

/// 候选行：trim 后以 `|` 开头且非单字符（GFM 表头/数据行惯例；
/// 转义 `\|` 开头的行不是表格行——首字符是反斜杠）。
fn is_candidate_line(line: &str) -> bool {
    let t = line.trim();
    t.len() > 1 && t.starts_with('|')
}

/// 按 GFM 管道分隔切单元格（结构解析 + trim），并还原转义管道
/// （`\|` → `|`）：结构检测保留反斜杠是为了正确切分，渲染层消费时
/// 需要还原，与 final（pulldown-cmark）行为一致。
fn split_cells(line: &str) -> Vec<String> {
    parse_table_segments(line)
        .unwrap_or_default()
        .into_iter()
        .map(unescape_pipe)
        .collect()
}

/// `\|` → `|`（其余反斜杠序列保留；`\\|` → `\|`，字面反斜杠 + 管道）。
fn unescape_pipe(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'|') {
            chars.next();
            out.push('|');
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 表头 + 分隔行确认：表格出现，隐藏区间覆盖两行
    #[test]
    fn header_and_sep_activates_table() {
        let mut t = GfmTableTracker::new();
        t.feed("| A | B |\n|---|---|\n");
        let tables = t.tables();
        assert_eq!(tables.len(), 1, "分隔行确认即出表格");
        assert_eq!(tables[0].headers, vec!["A", "B"]);
        assert!(tables[0].rows.is_empty());
        let (h, _) = t.tables_with_spans()[0].clone();
        assert_eq!(h.start, 0);
        assert_eq!(h.end, "| A | B |\n|---|---|\n".len(), "表头 + 分隔行隐藏");
    }

    /// 候选期（分隔行未到）：不产生区间（按文本渲染，零回退成本）
    #[test]
    fn candidate_is_not_hidden() {
        let mut t = GfmTableTracker::new();
        t.feed("| A | B |\n");
        assert!(t.tables().is_empty());
        assert!(t.tables_with_spans().is_empty(), "候选期不得隐藏");
    }

    /// 回退：| 行后跟普通文本 → 无表格，字面零丢失
    #[test]
    fn falls_back_when_not_table() {
        let mut t = GfmTableTracker::new();
        t.feed("| 这不是表格 |\n只是普通文本\n");
        assert!(t.tables().is_empty());
        assert!(t.tables_with_spans().is_empty());
    }

    /// 数据行逐行追加 + 残行逐字生长在网格末行
    #[test]
    fn data_rows_append_with_partial() {
        let mut t = GfmTableTracker::new();
        let mut raw = String::from("| A | B |\n|---|---|\n| 1 | 2 |\n");
        t.feed(&raw);
        let mut tables = t.tables();
        assert_eq!(tables[0].rows.len(), 1);
        assert_eq!(tables[0].rows[0], vec!["1", "2"]);
        // 残行：不确认，但作为网格末行实时暴露（partial，骨架补空）
        raw.push_str("| 3 | ");
        t.feed(&raw);
        let (_, snap) = &t.tables_with_spans()[0];
        assert_eq!(snap.rows.len(), 2, "残行实时显示在网格末行");
        assert_eq!(
            snap.rows[1],
            vec!["3", ""],
            "残行按表头列数补空：骨架先行、单元格逐字填充"
        );
        // 残行完成：确认（partial 转移为正式行）
        raw.push_str("4\n");
        t.feed(&raw);
        tables = t.tables();
        assert_eq!(tables[0].rows.len(), 2);
        assert_eq!(tables[0].rows[1], vec!["3", "4"]);
        let (h, _) = &t.tables_with_spans()[0];
        assert_eq!(
            h.end,
            "| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4\n".len(),
            "隐藏区间到确认行尾（残行补全后无尾部 |）"
        );
    }

    /// 空行结束表格：封存，隐藏区间到末行尾（不含空行）
    #[test]
    fn blank_line_seals_table() {
        let mut t = GfmTableTracker::new();
        t.feed("| A |\n|---|\n| 1 |\n\n后文\n");
        let tables = t.tables();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].rows.len(), 1);
        let (h, snap) = &t.tables_with_spans()[0];
        assert_eq!(h.end, "| A |\n|---|\n| 1 |\n".len(), "空行不隐藏");
        assert_eq!(snap.rows.len(), 1, "封存快照不含残行");
    }

    /// 非表格行结束表格：封存后该行按 Outside 语义重新处理
    #[test]
    fn non_table_line_seals_and_rescans() {
        let mut t = GfmTableTracker::new();
        t.feed("| A |\n|---|\n| 1 |\nnext text\n");
        let spans = t.tables_with_spans();
        assert_eq!(spans.len(), 1, "表格封存");
        assert_eq!(
            spans[0].0.end,
            "| A |\n|---|\n| 1 |\n".len(),
            "next text 不隐藏"
        );
    }

    /// 连续候选行：GFM 表头必须紧邻分隔行 → 只保留最近一行
    #[test]
    fn candidate_replaced_by_later_line() {
        let mut t = GfmTableTracker::new();
        t.feed("| A |\n| B |\n|---|---|\n");
        let tables = t.tables();
        assert_eq!(tables[0].headers, vec!["B"], "表头 = 最近候选行");
        // 隐藏区间从 B 行开始（A 行按文本可见）
        let (h, _) = &t.tables_with_spans()[0];
        assert_eq!(h.start, "| A |\n".len());
    }

    /// 列数容忍：数据行超出截断、不足补空
    #[test]
    fn column_count_tolerance() {
        let mut t = GfmTableTracker::new();
        t.feed("| A | B | C |\n|---|---|---|\n| 1 | 2 | 3 | 4 |\n| 5 |\n");
        let tables = t.tables();
        assert_eq!(tables[0].rows[0], vec!["1", "2", "3"], "超出截断");
        assert_eq!(tables[0].rows[1], vec!["5", "", ""], "不足补空");
    }

    /// 分隔行变体：`:---:`、无首尾 `|` 的 `---`、带空白的 `| --- | --- |`
    #[test]
    fn sep_line_variants() {
        assert!(is_table_delimiter_line("|---|---|"));
        assert!(is_table_delimiter_line("|:---:|:---:|"));
        assert!(is_table_delimiter_line("| --- | --- |"));
        assert!(!is_table_delimiter_line("| A | B |"));
        assert!(!is_table_delimiter_line("|||"));
        assert!(!is_table_delimiter_line("plain"));
    }

    /// 多表格连续：sealed 累积，顺序与区间保持
    #[test]
    fn multiple_tables_accumulate() {
        let mut t = GfmTableTracker::new();
        let raw = "| A |\n|---|\n| 1 |\n\n中间文本\n\n| C |\n|---|\n| 3 |\n";
        t.feed(raw);
        let spans = t.tables_with_spans();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].1.headers, vec!["A"]);
        assert_eq!(spans[1].1.headers, vec!["C"]);
        assert!(spans[0].0.end <= spans[1].0.start, "区间不重叠");
    }

    /// 增量幂等：重复 feed 相同 raw 不重复处理
    #[test]
    fn feed_is_idempotent() {
        let mut t = GfmTableTracker::new();
        let raw = "| A | B |\n|---|---|\n| 1 | 2 |\n";
        t.feed(raw);
        t.feed(raw);
        let tables = t.tables();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].rows.len(), 1);
    }

    /// reset 清空（BlockCheckpoint 覆盖语义）
    #[test]
    fn reset_clears_all() {
        let mut t = GfmTableTracker::new();
        t.feed("| A |\n|---|\n| 1 |\n");
        t.reset();
        assert!(t.tables().is_empty());
        t.feed("| X |\n|---|\n");
        assert_eq!(t.tables().len(), 1);
    }

    /// 文档尾未闭合表格：保持打开（残行持续生长；final 权威接管）
    #[test]
    fn trailing_open_table_keeps_partial() {
        let mut t = GfmTableTracker::new();
        let mut raw = String::from("| A | B |\n|---|---|\n| 1 | 2 |\n| 3");
        t.feed(&raw);
        let (_, snap) = &t.tables_with_spans()[0];
        assert_eq!(snap.rows.len(), 2, "残行在网格末行");
        raw.push_str(" | 4");
        t.feed(&raw);
        let (_, snap) = &t.tables_with_spans()[0];
        assert_eq!(snap.rows[1], vec!["3", "4"]);
    }

    /// ```markdown 围栏内：管道行可作表格（LLM 包表格习惯，Codex 同款）
    #[test]
    fn markdown_fence_enables_tables() {
        let mut t = GfmTableTracker::new();
        t.feed("```markdown\n| A | B |\n|---|---|\n| 1 | 2 |\n```\n");
        let tables = t.tables();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].headers, vec!["A", "B"]);
        assert_eq!(tables[0].rows, vec![vec!["1", "2"]]);
        // 隐藏区间覆盖表头 → 末数据行（围栏行不隐藏）
        let (h, _) = &t.tables_with_spans()[0];
        assert_eq!(h.start, "```markdown\n".len());
        assert_eq!(h.end, "```markdown\n| A | B |\n|---|---|\n| 1 | 2 |\n".len());
    }

    /// 其他围栏（rust/sh/无 info）内：管道是代码，不参与表格
    #[test]
    fn other_fence_ignores_pipes() {
        let mut t = GfmTableTracker::new();
        t.feed("```rust\n| a | b |\n|---|---|\n| 1 | 2 |\n```\n");
        assert!(t.tables().is_empty(), "rust 围栏内不得识别表格");
        let mut t2 = GfmTableTracker::new();
        t2.feed("```\n| a | b |\n|---|---|\n```\n");
        assert!(t2.tables().is_empty(), "无 info 围栏内不得识别表格");
    }

    /// 围栏行（开/闭）结束当前表格（封存/丢弃）
    #[test]
    fn fence_line_seals_active_table() {
        // 普通表格激活中遇到其他围栏开行 → 封存
        let mut t = GfmTableTracker::new();
        t.feed("| A |\n|---|\n| 1 |\n```sh\n");
        let spans = t.tables_with_spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].0.end, "| A |\n|---|\n| 1 |\n".len(), "围栏行不隐藏");
        // markdown 围栏闭合 → 封存
        let mut t2 = GfmTableTracker::new();
        t2.feed("```markdown\n| A |\n|---|\n| 1 |\n```\n");
        let spans = t2.tables_with_spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].1.rows, vec![vec!["1"]]);
    }

    /// 转义管道：`\|` 是字面量不算分隔符，且渲染时还原为 `|`
    #[test]
    fn escaped_pipe_is_literal() {
        let mut t = GfmTableTracker::new();
        t.feed("| a \\| b | c |\n|---|---|\n| 1 | 2 |\n");
        let tables = t.tables();
        assert_eq!(tables[0].headers, vec!["a | b", "c"], "转义还原为字面管道");
        assert_eq!(tables[0].rows[0], vec!["1", "2"]);
    }

    /// markdown 围栏未闭合（流式中）：表格保持 active，残行生长
    #[test]
    fn markdown_fence_open_keeps_partial() {
        let mut t = GfmTableTracker::new();
        let mut raw = String::from("```markdown\n| A | B |\n|---|---|\n| 1 | 2 |\n| 3");
        t.feed(&raw);
        let (_, snap) = &t.tables_with_spans()[0];
        assert_eq!(snap.rows.len(), 2, "残行在网格末行");
        raw.push_str(" | 4");
        t.feed(&raw);
        let (_, snap) = &t.tables_with_spans()[0];
        assert_eq!(snap.rows[1], vec!["3", "4"]);
    }
}
