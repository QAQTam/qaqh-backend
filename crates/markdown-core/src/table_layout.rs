//! 表格列布局：分类 + 优先级压缩（Codex TUI `markdown_render` 移植）。
//!
//! 纯函数、无 UI 依赖：输入表格单元格（行内 AST），输出列统计与压缩后
//! 宽度（字符单元）。UI 层把宽度向量映射为 Grid Star 权重。
//!
//! 语义（对齐 Codex）：
//! - **列分类**：`TokenHeavy`（≥20 宽 token 占一半以上，路径/URL/哈希）
//!   最先让宽；`Narrative`（平均 ≥4 词/格 或 ≥28 宽）次让；`Compact`
//!   （短值，计数/状态）最后让——保持可扫描；
//! - **软地板**：Narrative/TokenHeavy 16 字符；Compact = max(表头最长
//!   token, min(正文最长 token, 16))；
//! - **均衡压缩**：同类型列按超出地板最多的先让（二分 cap 均摊），
//!   避免同形列宽差悬殊；连每列 3 字符都放不下 → `None`（触发降级）。

use crate::ast::{Inline, concat_inlines};

/// 列类型（宽度分配优先级，从先让到后让）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnKind {
    /// 长 token 主导（路径/URL/哈希）：最先让宽（可 wrap）。
    TokenHeavy,
    /// 叙述性长文本：次先让宽。
    Narrative,
    /// 短值（计数/状态）：最后让宽（保持可扫描）。
    Compact,
}

/// 单列统计（宽度分配输入；单遍收集）。
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnMetrics {
    /// 全表最宽单元格（显示宽，字符单元）。
    pub max_width: usize,
    /// 表头最长 token 宽。
    pub header_token_width: usize,
    /// 正文最长 token 宽。
    pub body_token_width: usize,
    /// 列类型。
    pub kind: ColumnKind,
}

/// 字符显示宽（字符单元）：CJK/全角 = 2，其余 = 1。
pub fn char_width(c: char) -> usize {
    let cp = c as u32;
    if (0x1100..=0x115F).contains(&cp)
        || (0x2E80..=0xA4CF).contains(&cp)
        || (0xAC00..=0xD7A3).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0xFE30..=0xFE4F).contains(&cp)
        || (0xFF00..=0xFF60).contains(&cp)
        || (0xFFE0..=0xFFE6).contains(&cp)
    {
        2
    } else {
        1
    }
}

/// 字符串显示宽（字符单元）。
pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

fn longest_token_width(s: &str) -> usize {
    s.split_whitespace().map(display_width).max().unwrap_or(0)
}

/// 单遍收集列统计（对齐 Codex `collect_table_column_metrics`）。
pub fn collect_column_metrics(
    headers: &[Vec<Inline>],
    rows: &[Vec<Vec<Inline>>],
    column_count: usize,
) -> Vec<ColumnMetrics> {
    let mut metrics = Vec::with_capacity(column_count);
    for column in 0..column_count {
        let header_plain = concat_inlines(&headers[column]);
        let header_token_width = longest_token_width(&header_plain);
        let mut max_width = display_width(&header_plain);
        let mut body_token_width = 0usize;
        let mut body_token_count = 0usize;
        let mut long_body_token_count = 0usize;
        let mut total_words = 0usize;
        let mut total_cells = 0usize;
        let mut total_cell_width = 0usize;

        for row in rows {
            let plain = concat_inlines(&row[column]);
            max_width = max_width.max(display_width(&plain));
            let mut word_count = 0usize;
            for token in plain.split_whitespace() {
                let token_width = display_width(token);
                body_token_width = body_token_width.max(token_width);
                long_body_token_count += usize::from(token_width >= 20);
                word_count += 1;
            }
            if word_count > 0 {
                body_token_count += word_count;
                total_words += word_count;
                total_cells += 1;
                total_cell_width += display_width(&plain);
            }
        }

        let avg_words_per_cell = if total_cells == 0 {
            header_plain.split_whitespace().count() as f64
        } else {
            total_words as f64 / total_cells as f64
        };
        let avg_cell_width = if total_cells == 0 {
            display_width(&header_plain) as f64
        } else {
            total_cell_width as f64 / total_cells as f64
        };
        let kind = if long_body_token_count > 0
            && long_body_token_count >= body_token_count.saturating_sub(long_body_token_count)
        {
            ColumnKind::TokenHeavy
        } else if avg_words_per_cell >= 4.0 || avg_cell_width >= 28.0 {
            ColumnKind::Narrative
        } else {
            ColumnKind::Compact
        };

        metrics.push(ColumnMetrics {
            max_width,
            header_token_width,
            body_token_width,
            kind,
        });
    }
    metrics
}

/// 压缩后列宽（字符单元）。`available_width = None` 时返回内容自然宽。
///
/// 返回 `None` 表示连每列 3 字符的最小宽度都放不下（调用方降级渲染）。
pub fn compute_column_widths(
    metrics: &[ColumnMetrics],
    available_width: Option<usize>,
) -> Option<Vec<usize>> {
    let min_column_width = 3usize;
    let mut widths: Vec<usize> = metrics
        .iter()
        .map(|col| col.max_width.max(min_column_width))
        .collect();

    let Some(max_width) = available_width else {
        return Some(widths);
    };
    let minimum_total = metrics.len() * min_column_width;
    if max_width < minimum_total {
        return None;
    }

    // 软地板：先压地板（若地板总和超限），再压内容宽。
    let mut floors: Vec<usize> = metrics
        .iter()
        .map(|col| preferred_column_floor(col, min_column_width))
        .collect();
    let floor_total: usize = floors.iter().sum();
    if floor_total > max_width {
        let minimums = vec![min_column_width; floors.len()];
        shrink_columns(&mut floors, &minimums, metrics, floor_total - max_width);
    }

    let total_width: usize = widths.iter().sum();
    if total_width > max_width {
        let remaining = shrink_columns(&mut widths, &floors, metrics, total_width - max_width);
        if remaining > 0 {
            return None;
        }
    }

    Some(widths)
}

/// 列软地板：Narrative/TokenHeavy 保留 16 字符可读下限；Compact 保留
/// token 宽（正文 token 上限 16），保证短值不被 wrap 成碎片。
fn preferred_column_floor(metrics: &ColumnMetrics, min_column_width: usize) -> usize {
    let token_target = match metrics.kind {
        ColumnKind::Narrative | ColumnKind::TokenHeavy => 16,
        ColumnKind::Compact => metrics
            .header_token_width
            .max(metrics.body_token_width.min(16)),
    };
    token_target.max(min_column_width).min(metrics.max_width)
}

/// 按优先级均衡压缩（对齐 Codex `shrink_columns`）：
/// TokenHeavy → Narrative → Compact；同类型内用二分 cap 均摊超地板宽度，
/// 保证同形列收缩一致；返回未能压缩的剩余量。
fn shrink_columns(
    widths: &mut [usize],
    floors: &[usize],
    metrics: &[ColumnMetrics],
    mut amount: usize,
) -> usize {
    for kind in [
        ColumnKind::TokenHeavy,
        ColumnKind::Narrative,
        ColumnKind::Compact,
    ] {
        let slack_total = widths
            .iter()
            .enumerate()
            .filter(|(idx, _)| metrics[*idx].kind == kind)
            .map(|(idx, width)| width.saturating_sub(floors[idx]))
            .sum::<usize>();
        let to_remove = amount.min(slack_total);
        if to_remove == 0 {
            continue;
        }

        // 二分 cap：找到让"该类型列总减量 ≤ to_remove"的最小 cap。
        let mut low = 0usize;
        let mut high = widths
            .iter()
            .enumerate()
            .filter(|(idx, _)| metrics[*idx].kind == kind)
            .map(|(idx, width)| width.saturating_sub(floors[idx]))
            .max()
            .unwrap_or(0);
        while low < high {
            let cap = low + (high - low) / 2;
            let removed = widths
                .iter()
                .enumerate()
                .filter(|(idx, _)| metrics[*idx].kind == kind)
                .map(|(idx, width)| width.saturating_sub(floors[idx]).saturating_sub(cap))
                .sum::<usize>();
            if removed > to_remove {
                low = cap + 1;
            } else {
                high = cap;
            }
        }

        let cap = low;
        let mut removed = 0usize;
        for (idx, width) in widths.iter_mut().enumerate() {
            if metrics[idx].kind != kind {
                continue;
            }
            let reduction = width.saturating_sub(floors[idx]).saturating_sub(cap);
            *width -= reduction;
            removed += reduction;
        }

        // 余量：恰好在地板上的列逐个让 1（保持均衡）。
        let mut remainder = to_remove - removed;
        for (idx, width) in widths.iter_mut().enumerate() {
            if remainder == 0 {
                break;
            }
            if metrics[idx].kind == kind && width.saturating_sub(floors[idx]) == cap {
                *width -= 1;
                remainder -= 1;
            }
        }

        amount -= to_remove;
        if amount == 0 {
            break;
        }
    }
    amount
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(s: &str) -> Vec<Inline> {
        vec![Inline::Text(s.to_string())]
    }

    fn table(
        headers: &[&str],
        rows: &[&[&str]],
    ) -> (Vec<Vec<Inline>>, Vec<Vec<Vec<Inline>>>) {
        let h = headers.iter().map(|h| cell(h)).collect();
        let r = rows
            .iter()
            .map(|row| row.iter().map(|c| cell(c)).collect())
            .collect();
        (h, r)
    }

    /// 列分类：路径列 → TokenHeavy；长文本列 → Narrative；短值 → Compact
    #[test]
    fn column_kinds_classified() {
        let (h, r) = table(
            &["文件", "状态"],
            &[
                &["crates/markdown-winui/src/lib.rs", "完成"],
                &["crates/deepx-winui/src/chat_view.rs", "进行中"],
            ],
        );
        let m = collect_column_metrics(&h, &r, 2);
        assert_eq!(m[0].kind, ColumnKind::TokenHeavy, "路径列");
        assert_eq!(m[1].kind, ColumnKind::Compact, "短状态值");
    }

    /// Narrative：平均 ≥4 词/格（用带空格的文本，避免被当成长 token）
    #[test]
    fn narrative_detected_by_word_count() {
        let (h, r) = table(
            &["说明"],
            &[&["this is a fairly long narrative description text"]],
        );
        let m = collect_column_metrics(&h, &r, 1);
        assert_eq!(m[0].kind, ColumnKind::Narrative);
    }

    /// 无可用宽：返回内容自然宽（每列至少 3 字符保底）
    #[test]
    fn natural_width_without_constraint() {
        let (h, r) = table(&["A", "BB"], &[&["CCC", "D"]]);
        let m = collect_column_metrics(&h, &r, 2);
        let w = compute_column_widths(&m, None).expect("widths");
        assert_eq!(w, vec![3, 3]);
    }

    /// 压缩优先级：TokenHeavy 先让，Compact 保持地板
    #[test]
    fn compact_column_preserved_under_pressure() {
        let (h, r) = table(
            &["路径", "状态"],
            &[
                &["crates/markdown-winui/src/lib.rs", "完成"],
                &["crates/deepx-winui/src/chat_view.rs", "进行中"],
            ],
        );
        let m = collect_column_metrics(&h, &r, 2);
        let w = compute_column_widths(&m, Some(30)).expect("widths");
        assert!(w[0] < m[0].max_width, "TokenHeavy 让宽: {} < {}", w[0], m[0].max_width);
        // Compact 列保持地板（token 宽 "进行中" = 6 字符）
        assert_eq!(w[1], 6, "Compact 列不被压缩");
        assert!(w.iter().sum::<usize>() <= 30);
    }

    /// 空间不足（每列 3 字符都放不下）→ None（触发降级）
    #[test]
    fn too_narrow_returns_none() {
        let (h, r) = table(&["A", "B"], &[&["1", "2"]]);
        let m = collect_column_metrics(&h, &r, 2);
        assert!(compute_column_widths(&m, Some(5)).is_none(), "2 列需 ≥6 字符");
    }

    /// 软地板：Narrative/TokenHeavy 地板 16
    #[test]
    fn soft_floors() {
        let (h, r) = table(&["a"], &[&["very long narrative text here"]]);
        let m = collect_column_metrics(&h, &r, 1);
        assert_eq!(preferred_column_floor(&m[0], 3), 16);
    }

    /// 均衡压缩：同类型（都 TokenHeavy）列收缩一致
    #[test]
    fn balanced_shrink_within_kind() {
        let (h, r) = table(
            &["x", "y"],
            &[&["crates/markdown-winui/src/lib.rs", "crates/deepx-winui/src/main.rs"]],
        );
        let m = collect_column_metrics(&h, &r, 2);
        let w = compute_column_widths(&m, Some(30)).expect("widths");
        assert_eq!(w[0], w[1], "同形 TokenHeavy 列均衡收缩");
    }

    /// 显示宽：CJK = 2，ASCII = 1
    #[test]
    fn display_width_counts() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width("a中b"), 4);
    }
}
