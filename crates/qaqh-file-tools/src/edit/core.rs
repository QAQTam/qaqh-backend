//! core — 精确子串定位、唯一性判定与失败诊断。
//!
//! 定位是纯字节级精确匹配（`str::find` 语义，含重叠命中计数）；
//! 相似度只用于**失败诊断**（找最接近的区域生成 diff），绝不参与采纳。

use crate::file_shared::LineIndex;
pub(crate) use crate::match_diag::{
    MIN_DIAGNOSTIC_SIMILARITY, NearestMatch, nearest_match_in_text, render_diff_block,
};

/// 单次替换的产物。
#[derive(Debug)]
pub(crate) struct ReplaceOutcome {
    pub(crate) edited: String,
    /// 实际替换的处数。
    pub(crate) replaced: usize,
    /// 每处替换的起始行（1-based，基于编辑前快照）。
    pub(crate) lines: Vec<usize>,
    /// 账本行号偏移：(编辑前起始行, 行号增量)，与 `lines` 一一对应。
    pub(crate) shifts: Vec<(usize, i64)>,
    /// 每处命中的 LF 视图字节区间 [start, end)，供原始内容拼接映射。
    pub(crate) matches: Vec<(usize, usize)>,
}

#[derive(Debug)]
pub(crate) enum ReplaceError {
    NotFound {
        nearest: Option<NearestMatch>,
    },
    Ambiguous {
        total: usize,
        occurrences: Vec<Occurrence>,
    },
}

/// 一处命中：1-based 行号 + 带行号的上下文片段。
#[derive(Debug)]
pub(crate) struct Occurrence {
    pub(crate) line: usize,
    pub(crate) context: String,
}

/// 找出 `needle` 在 `haystack` 中的所有起始字节位置（含重叠命中）。
pub(crate) fn find_occurrences(haystack: &str, needle: &str) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut from = 0usize;
    while from <= haystack.len() {
        let Some(rel) = haystack.get(from..).and_then(|rest| rest.find(needle)) else {
            break;
        };
        let pos = from + rel;
        out.push(pos);
        let Some(ch) = haystack.get(pos..).and_then(|rest| rest.chars().next()) else {
            break;
        };
        from = pos + ch.len_utf8();
    }
    out
}

/// 核心替换：
/// - 0 处命中 → `NOT_FOUND`（附最近似 diff）；
/// - 多处命中且 `replace_all=false` → `AMBIGUOUS_MATCH`；
/// - 多处命中且 `replace_all=true` → 全部替换（非重叠，`str::replace` 语义）；
/// - 恰好 1 处 → 替换。
pub(crate) fn run_replace(
    content: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<ReplaceOutcome, ReplaceError> {
    let positions = find_occurrences(content, old);
    if positions.is_empty() {
        return Err(ReplaceError::NotFound {
            nearest: nearest_match(content, old),
        });
    }
    if positions.len() > 1 && !replace_all {
        return Err(ReplaceError::Ambiguous {
            total: positions.len(),
            occurrences: positions
                .iter()
                .take(3)
                .map(|&pos| occurrence(content, pos, old))
                .collect(),
        });
    }
    // 删除语义（Claude Code hW2）：new 为空且 old 不以 \n 结尾时，
    // 自动尝试 old+"\n"——连行尾一起删，不留空行；old 以 \n 结尾或
    // 文件中无 old+"\n" 形态时退回裸匹配。
    let (old, new) =
        if new.is_empty() && !old.ends_with('\n') && content.contains(&format!("{old}\n")) {
            (&*format!("{old}\n"), new)
        } else {
            (old, new)
        };
    let targets = if replace_all {
        non_overlapping_occurrences(content, old)
    } else {
        vec![positions[0]]
    };
    Ok(apply_all(content, old, new, &targets))
}

/// 非重叠命中（`str::replace` 语义），供 `replace_all` 使用。
fn non_overlapping_occurrences(haystack: &str, needle: &str) -> Vec<usize> {
    haystack.match_indices(needle).map(|(i, _)| i).collect()
}

fn apply_all(content: &str, old: &str, new: &str, positions: &[usize]) -> ReplaceOutcome {
    let index = LineIndex::new(content);
    let extra = new.len().saturating_sub(old.len());
    let mut edited = String::with_capacity(content.len() + extra * positions.len());
    let mut cursor = 0usize;
    let mut lines = Vec::with_capacity(positions.len());
    let mut shifts = Vec::with_capacity(positions.len());
    let mut matches = Vec::with_capacity(positions.len());
    let removed_lf = count_lf(old) as i64;
    let added_lf = count_lf(new) as i64;
    for &start in positions {
        let end = start + old.len();
        edited.push_str(content.get(cursor..start).unwrap_or_default());
        edited.push_str(new);
        cursor = end;
        matches.push((start, end));
        let line = index.line_of_byte(start);
        lines.push(line);
        shifts.push((line, added_lf - removed_lf));
    }
    edited.push_str(content.get(cursor..).unwrap_or_default());
    ReplaceOutcome {
        edited,
        replaced: positions.len(),
        lines,
        shifts,
        matches,
    }
}

pub(crate) fn count_lf(text: &str) -> usize {
    text.bytes().filter(|&b| b == b'\n').count()
}

/// 一处命中的上下文（前后各 2 行，带 1-based 行号）。
fn occurrence(content: &str, pos: usize, old: &str) -> Occurrence {
    let index = LineIndex::new(content);
    let line = index.line_of_byte(pos);
    let lines = index.lines();
    let matched_lines = count_lf(old) + 1;
    let start_idx = (line - 1).saturating_sub(2);
    let end_idx = ((line - 1) + matched_lines + 2).min(lines.len());
    let context = lines[start_idx..end_idx]
        .iter()
        .enumerate()
        .map(|(i, l)| format!("L{}: {l}", start_idx + i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    Occurrence { line, context }
}

/// 最近似区域：委托共享诊断模块（带最低相似度门槛与大文件粗筛）。
pub(crate) fn nearest_match(content: &str, old: &str) -> Option<NearestMatch> {
    nearest_match_in_text(content, old, MIN_DIAGNOSTIC_SIMILARITY)
}

/// `NOT_FOUND` 的模型可见正文：错误说明 + 最近似位置 + 可选空白差异注记 + old_string→实际内容的 unified diff。
pub(crate) fn render_not_found(path: &str, old: &str, nearest: Option<&NearestMatch>) -> String {
    let mut out =
        format!("[ERROR] edit {path}\n  not_found: old_string was not found in the file.\n");
    if let Some(n) = nearest {
        out.push_str(&render_diff_block("old_string", old, n, "  "));
    } else {
        out.push_str("  no similar region found; re-read the file with read.\n");
    }
    out
}

/// `AMBIGUOUS_MATCH` 的模型可见正文：命中总数 + 前 3 处的位置与上下文。
pub(crate) fn render_ambiguous(path: &str, total: usize, occurrences: &[Occurrence]) -> String {
    let mut out = format!(
        "[ERROR] edit {path}\n  AMBIGUOUS_MATCH: old_string matches {total} locations. To replace only one occurrence, extend 'old_string' with surrounding lines so it matches exactly one; to replace all, set replace_all=true.\n"
    );
    for (i, occ) in occurrences.iter().enumerate() {
        out.push_str(&format!("  occurrence #{} at L{}:\n", i + 1, occ.line));
        for line in occ.context.lines() {
            out.push_str("    ");
            out.push_str(line);
            out.push('\n');
        }
    }
    if total > occurrences.len() {
        out.push_str(&format!(
            "  ... and {} more occurrence(s)\n",
            total - occurrences.len()
        ));
    }
    out
}

/// 成功回复的编辑点内容锚（Claude Code「snippet of the edited file」模式，
/// 皮用本项目 `L<n>: ` 约定与 read 同源）：new_string 落点前后各
/// [`SNIPPET_RADIUS`] 行。行号基于编辑后内容；`start_line` 是替换起始行
/// （插入语义下新旧内容同一起点）。输入 `edited` 是 LF 规范视图。
pub(crate) const SNIPPET_RADIUS: usize = 3;

pub(crate) fn render_success_snippet(edited: &str, start_line: usize, new_lf: &str) -> String {
    let index = LineIndex::new(edited);
    let lines = index.lines();
    if lines.is_empty() {
        return String::new();
    }
    let start0 = start_line.saturating_sub(1).min(lines.len() - 1);
    let inserted = count_lf(new_lf) + 1;
    let lo = start0.saturating_sub(SNIPPET_RADIUS);
    let hi = (start0 + inserted + SNIPPET_RADIUS)
        .min(lines.len())
        .max(lo + 1);
    (lo..hi)
        .map(|i| format!("L{}: {}", i + 1, lines[i]))
        .collect::<Vec<_>>()
        .join("\n")
}
