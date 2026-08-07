//! 流式协议表格跟踪器（```table 围栏的渐进渲染，P0）。
//!
//! 背景：GFM 表格在流式中无法渐进（分隔行到达前无法确认块身份），而协议
//! 表格（```table + TSV/管道分隔）有天然红利——**围栏即知表格、首行即表头、
//! 列数即定**。本模块消费这个红利：
//!
//! ```text
//! ```table           ← 围栏行（从字面隐藏）
//! 语言\t类型\t内存    ← 表头行确认 → 表格激活（表头出网格）
//! Rust\t静态\t手动    ← 每完成一行 → 数据行追加
//! &mut T\t独占\t是     ← 残行（未换行）→ 实时显示在网格最后一行（逐字生长）
//! ```                ← 闭合 → 整表封存（内容继续隐藏，不回到字面）
//! ```
//!
//! 语义（对齐 REFERENCE §3）：
//! - **行级确认**：仅已换行的行参与表格；残行（未完成行）实时暴露为
//!   `partial`——UI 把它渲染为网格最后一行，**打字机效果延续进表格**；
//! - **坏格式回退**：表头行无分隔符（含 JSON 单行）→ 围栏行恢复字面，
//!   内容零丢失（与 parse_final 的 `parse_table_protocol` 拒绝语义一致）；
//! - **闭合后继续隐藏**：已封存的表格（sealed）保留隐藏区间，字面不会
//!   重复出现表格内容（与网格并存）；
//! - **增量 O(新增行)**：`feed` 只扫描上次位置之后的新行（`pos` 游标），
//!   与 `StreamingMarkdown::append` 的 O(1) 追加契约同一量级。
//!
//! 注意：JSON 协议形态（单行大块）无法渐进，保持等 final（围栏行恢复
//! 字面，最终由 `parse_final` 权威渲染）——P0 只做 TSV/管道分隔符。

/// 表格快照（流式预览）：headers + rows（单元格为纯文本 String）。
///
/// UI 端转 `TableData`（包一层 `Inline::Text`）后走 `table_view` 网格通道。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TableSnapshot {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// 字面隐藏区间 `[start, end)`（字节偏移，随 raw 追加保持有效）：
/// 围栏行 + 已确认表格行 + 闭合行。残行（partial）不隐藏（它在网格里）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableHiddenSpan {
    pub start: usize,
    pub end: usize,
}

/// 解析状态。
#[derive(Clone, Debug, Default, PartialEq)]
enum State {
    /// 围栏外（普通文本流）。
    #[default]
    Outside,
    /// 已见 ```table 围栏，等表头行确认。表格未激活：仅围栏行隐藏，
    /// 候选表头行仍以字面显示（坏格式回退时零代价恢复）。
    FencePending {
        fence_start: usize,
        fence_end: usize,
    },
    /// 表头确认，表格激活：数据行逐行累积，已确认行从字面隐藏。
    Active {
        fence_start: usize,
        sep: char,
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
        /// 最后确认行的结束偏移（= hidden.end）。
        confirmed_to: usize,
    },
}

/// 流式协议表格跟踪器（无状态机侵入的独立组件）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LiveTableTracker {
    state: State,
    /// 已闭合表格（完整快照 + 隐藏区间；追加不变更）。
    sealed: Vec<(TableHiddenSpan, TableSnapshot)>,
    /// 当前残行（Active 状态下未换行的内容；逐字生长显示在网格末行）。
    partial: String,
    /// 增量扫描游标（行首字节偏移；raw 只追加，游标单调前进）。
    pos: usize,
}

impl LiveTableTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入完整 raw（调用方累积；内部只扫 `pos` 之后的新行）。
    ///
    /// O(新增行数)；每行 O(列数)。可重复调用（幂等：不重复处理旧行）。
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
    /// 返回 `(span, snapshot)`：
    /// - sealed 表格：完整快照，span 覆盖围栏行 → 闭合行；
    /// - 打开的表格：span 覆盖围栏行 → 最后确认行；快照 **rows 尾部附残行**
    ///   （partial，逐字生长行）——UI 无需额外逻辑即可渲染打字机效果。
    pub fn tables_with_spans(&self) -> Vec<(TableHiddenSpan, TableSnapshot)> {
        let mut out: Vec<(TableHiddenSpan, TableSnapshot)> = self.sealed.clone();
        if let State::Active {
            fence_start,
            sep,
            headers,
            rows,
            confirmed_to,
        } = &self.state
        {
            let mut snap = TableSnapshot {
                headers: headers.clone(),
                rows: rows.clone(),
            };
            if !self.partial.is_empty() {
                let mut cells = split_cells(&self.partial, *sep);
                // 防御：残行列数超过表头 → 截断（避免 Grid 越界）
                cells.truncate(headers.len());
                snap.rows.push(cells);
            }
            out.push((
                TableHiddenSpan {
                    start: *fence_start,
                    end: *confirmed_to,
                },
                snap,
            ));
        }
        out
    }

    /// 当前打开表格的残行起点（= 最后确认行尾；残行显示在网格末行，
    /// 调用方据此截断尾部字面，避免残行重复显示）。无打开表格时 None。
    pub fn open_tail_start(&self) -> Option<usize> {
        match &self.state {
            State::Active { confirmed_to, .. } => Some(*confirmed_to),
            _ => None,
        }
    }

    /// 重置（BlockCheckpoint 整段覆盖时调用；丢弃全部累积）。
    pub fn reset(&mut self) {
        self.state = State::Outside;
        self.sealed.clear();
        self.partial.clear();
        self.pos = 0;
    }

    fn handle_line(&mut self, line: &str, line_start: usize) {
        let trimmed = line.trim();
        // 取出状态处理再写回（消除 self.state 借用冲突）
        let state = std::mem::take(&mut self.state);
        self.state = match state {
            State::Outside => {
                if trimmed == "```table" {
                    let fence_end = line_start + line.len() + 1; // 含换行
                    State::FencePending {
                        fence_start: line_start,
                        fence_end,
                    }
                } else {
                    State::Outside
                }
            }
            State::FencePending {
                fence_start,
                fence_end: _,
            } => {
                if trimmed == "```" {
                    // 空表格：回退（围栏行恢复字面，零内容丢失）
                    State::Outside
                } else if let Some(sep) = detect_sep(trimmed) {
                    let headers = split_cells(trimmed, sep);
                    if headers.is_empty() {
                        State::Outside // 防御：表头为空
                    } else {
                        let confirmed_to = line_start + line.len() + 1;
                        State::Active {
                            fence_start,
                            sep,
                            headers,
                            rows: Vec::new(),
                            confirmed_to,
                        }
                    }
                } else {
                    // 无分隔符（普通文本 / JSON 单行）：不是表格 → 回退字面
                    State::Outside
                }
            }
            State::Active {
                fence_start,
                sep,
                mut headers,
                mut rows,
                confirmed_to,
            } => {
                if trimmed == "```" {
                    // 闭合：整表封存（span 覆盖到闭合行尾，内容不再回到字面）
                    let _ = confirmed_to;
                    let span = TableHiddenSpan {
                        start: fence_start,
                        end: line_start + line.len() + 1,
                    };
                    self.sealed.push((
                        span,
                        TableSnapshot {
                            headers: std::mem::take(&mut headers),
                            rows: std::mem::take(&mut rows),
                        },
                    ));
                    State::Outside
                } else if trimmed.is_empty() {
                    // 空行：确认但不入行（与 parse_table_tsv 的过滤一致）
                    State::Active {
                        fence_start,
                        sep,
                        headers,
                        rows,
                        confirmed_to: line_start + line.len() + 1,
                    }
                } else {
                    // 列数不一致：P0 容忍（UI 网格按表头列数铺，错位可见但
                    // 不抖动；final 权威渲染仍走 parse_final 严格校验）
                    rows.push(split_cells(trimmed, sep));
                    State::Active {
                        fence_start,
                        sep,
                        headers,
                        rows,
                        confirmed_to: line_start + line.len() + 1,
                    }
                }
            }
        };
    }
}

/// 分隔符检测：`\t` 优先，无 `\t` 时用 `|`（对齐 parse_table_tsv）。
fn detect_sep(line: &str) -> Option<char> {
    if line.contains('\t') {
        Some('\t')
    } else if line.contains('|') {
        Some('|')
    } else {
        None
    }
}

/// 按分隔符切单元格（trim 每格）。
fn split_cells(line: &str, sep: char) -> Vec<String> {
    line.split(sep).map(str::trim).map(String::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 围栏 + 表头激活：表格出现，隐藏区间覆盖围栏行 + 表头行
    #[test]
    fn fence_and_header_activates_table() {
        let mut t = LiveTableTracker::new();
        t.feed("```table\n语言\t类型\n");
        let tables = t.tables();
        assert_eq!(tables.len(), 1, "表头确认即出表格");
        assert_eq!(tables[0].headers, vec!["语言", "类型"]);
        assert!(tables[0].rows.is_empty());
        // 隐藏区间：围栏行起点 → 表头行尾（含换行）
        let (h, _) = t.tables_with_spans()[0].clone();
        assert_eq!(h.start, 0);
        assert_eq!(h.end, "```table\n语言\t类型\n".len());
    }

    /// 数据行逐行确认（O(1) 追加）；feed 每次传完整 raw（内部游标增量）
    #[test]
    fn data_rows_append_line_by_line() {
        let mut t = LiveTableTracker::new();
        let mut raw = String::from("```table\n语言\t类型\nRust\t静态\n");
        t.feed(&raw);
        let mut tables = t.tables();
        assert_eq!(tables[0].rows.len(), 1);
        // 残行：不确认，但作为网格末行实时暴露（partial）
        raw.push_str("Go\t静");
        t.feed(&raw);
        let (_, snap) = &t.tables_with_spans()[0];
        assert_eq!(snap.rows.len(), 2, "残行实时显示在网格末行");
        assert_eq!(snap.rows[1], vec!["Go", "静"]);
        // 残行完成：确认（partial 转移为正式行）
        raw.push_str("态\n");
        t.feed(&raw);
        tables = t.tables();
        assert_eq!(tables[0].rows.len(), 2);
        assert_eq!(tables[0].rows[1], vec!["Go", "静态"]);
        // 隐藏区间随确认前进（残行不在隐藏内）
        let (h, _) = &t.tables_with_spans()[0];
        assert_eq!(h.end, "```table\n语言\t类型\nRust\t静态\nGo\t静态\n".len());
    }

    /// 残行逐字生长：partial 随每次 feed 更新（网格末行打字机效果）
    #[test]
    fn partial_grows_char_by_char() {
        let mut t = LiveTableTracker::new();
        let mut raw = String::from("```table\nA\tB\n");
        t.feed(&raw);
        for (i, ch) in "&mut".chars().enumerate() {
            raw.push(ch);
            t.feed(&raw);
            let (_, snap) = &t.tables_with_spans()[0];
            let last = snap.rows.last().expect("残行在网格末行");
            let expect: String = "&mut".chars().take(i + 1).collect();
            assert_eq!(last[0], expect, "残行逐字生长（第 {} 字符）", i + 1);
            assert_eq!(last.len(), 1, "残行尚无 \\t → 仅一列（其余列由网格留空）");
        }
    }

    /// 围栏闭合：整表封存 sealed，隐藏区间保留到闭合行尾（字面不重复）
    #[test]
    fn fence_close_seals_table() {
        let mut t = LiveTableTracker::new();
        t.feed("```table\nA\tB\n1\t2\n```\n");
        let tables = t.tables();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].headers, vec!["A", "B"]);
        assert_eq!(tables[0].rows, vec![vec!["1", "2"]]);
        // 闭合后：表格仍隐藏（sealed span），字面不重复出现表格内容
        let (h, snap) = &t.tables_with_spans()[0];
        assert_eq!(h.start, 0);
        assert_eq!(h.end, "```table\nA\tB\n1\t2\n```\n".len(), "闭合行也隐藏");
        assert_eq!(snap.rows.len(), 1, "sealed 快照不含残行");
    }

    /// 无分隔符（普通段落 / JSON 单行）：回退字面，内容不丢
    #[test]
    fn no_separator_falls_back() {
        let mut t = LiveTableTracker::new();
        t.feed("```table\n{\"headers\":[\"a\"],\"rows\":[]}\n```\n");
        assert!(t.tables().is_empty());
        assert!(t.tables_with_spans().is_empty(), "回退后无隐藏区间");
    }

    /// 空表格（```table 直接闭合）：回退
    #[test]
    fn empty_fence_falls_back() {
        let mut t = LiveTableTracker::new();
        t.feed("```table\n```\n");
        assert!(t.tables().is_empty());
        assert!(t.tables_with_spans().is_empty());
    }

    /// 管道分隔（无 \t 时）：`|` 分隔 + 单元格 trim
    #[test]
    fn pipe_separator_works() {
        let mut t = LiveTableTracker::new();
        t.feed("```table\n名称 | 用途\nbutton | 按钮\n```\n");
        let tables = t.tables();
        assert_eq!(tables[0].headers, vec!["名称", "用途"]);
        assert_eq!(tables[0].rows[0], vec!["button", "按钮"]);
    }

    /// 多表格连续：sealed 累积，顺序保持（含隐藏区间）
    #[test]
    fn multiple_tables_accumulate() {
        let mut t = LiveTableTracker::new();
        let raw = "```table\nA\tB\n1\t2\n```\n\n中间文本\n\n```table\nC\tD\n3\t4\n```\n";
        t.feed(raw);
        let spans = t.tables_with_spans();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].1.headers, vec!["A", "B"]);
        assert_eq!(spans[1].1.headers, vec!["C", "D"]);
        // 区间互不重叠，且覆盖各自围栏
        assert!(spans[0].0.end <= spans[1].0.start);
        assert_eq!(spans[0].0.start, 0);
        assert_eq!(spans[1].0.start, "```table\nA\tB\n1\t2\n```\n\n中间文本\n\n".len());
    }

    /// 增量幂等：重复 feed 相同 raw 不重复处理
    #[test]
    fn feed_is_idempotent() {
        let mut t = LiveTableTracker::new();
        let raw = "```table\nA\tB\n1\t2\n";
        t.feed(raw);
        t.feed(raw);
        let tables = t.tables();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].rows.len(), 1);
    }

    /// reset 清空（BlockCheckpoint 覆盖语义）
    #[test]
    fn reset_clears_all() {
        let mut t = LiveTableTracker::new();
        t.feed("```table\nA\tB\n1\t2\n```\n");
        t.reset();
        assert!(t.tables().is_empty());
        assert!(t.tables_with_spans().is_empty());
        // 重置后新表格正常
        t.feed("```table\nX\tY\n```\n");
        assert_eq!(t.tables().len(), 1);
    }

    /// 空行在表格内：确认但不入行（hidden 前进）
    #[test]
    fn blank_line_inside_table_confirms() {
        let mut t = LiveTableTracker::new();
        t.feed("```table\nA\tB\n\n1\t2\n");
        let tables = t.tables();
        assert_eq!(tables[0].rows.len(), 1, "空行不入行");
    }

    /// 围栏行带尾随空格/缩进：trim 后识别
    #[test]
    fn fence_with_whitespace_recognized() {
        let mut t = LiveTableTracker::new();
        t.feed("  ```table  \nA\tB\n");
        assert_eq!(t.tables()[0].headers, vec!["A", "B"]);
    }
}
