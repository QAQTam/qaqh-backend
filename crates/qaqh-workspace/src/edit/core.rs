//! core — 精确子串定位、唯一性判定与失败诊断。
//!
//! 定位是纯字节级精确匹配（`str::find` 语义，含重叠命中计数）；
//! 相似度只用于**失败诊断**（找最接近的区域生成 diff），绝不参与采纳。

use crate::file_shared::LineIndex;
use similar::TextDiff;

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

/// 最接近 `old_str` 的文件区域（仅用于失败诊断）。
#[derive(Debug)]
pub(crate) struct NearestMatch {
    pub(crate) start_line: usize,
    pub(crate) end_line: usize,
    pub(crate) text: String,
    pub(crate) score: f32,
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

/// 最近似区域：多行 old 用行窗口滑动；单行 old 额外与每一行整体比较（片段友好）。
pub(crate) fn nearest_match(content: &str, old: &str) -> Option<NearestMatch> {
    let index = LineIndex::new(content);
    let file_lines = index.lines();
    let mut pat: Vec<&str> = old.split('\n').collect();
    while pat.last() == Some(&"") {
        pat.pop();
    }
    if pat.is_empty() {
        return None;
    }
    let old_text = pat.join("\n");
    let p = pat.len();
    let mut best: Option<(f32, usize, usize)> = None;
    let min_win = p.saturating_sub(2).max(1);
    let max_win = (p + 2).min(file_lines.len());
    if min_win <= max_win {
        for win in min_win..=max_win {
            for s in 0..=file_lines.len() - win {
                let text = file_lines[s..s + win].join("\n");
                let score = ratio(&old_text, &text);
                if best.is_none_or(|(bs, _, _)| score > bs) {
                    best = Some((score, s, win));
                }
            }
        }
    }
    if p == 1 {
        for (i, line) in file_lines.iter().enumerate() {
            let score = ratio(&old_text, line);
            if best.is_none_or(|(bs, _, _)| score > bs) {
                best = Some((score, i, 1));
            }
        }
    }
    best.map(|(score, s, win)| NearestMatch {
        start_line: s + 1,
        end_line: s + win,
        text: file_lines[s..s + win].join("\n"),
        score,
    })
}

fn ratio(a: &str, b: &str) -> f32 {
    TextDiff::from_chars(a, b).ratio()
}

/// `NOT_FOUND` 的模型可见正文：错误说明 + 最近似位置 + old_str→实际内容的 unified diff。
pub(crate) fn render_not_found(path: &str, old: &str, nearest: Option<&NearestMatch>) -> String {
    let mut out = format!("[ERROR] edit {path}\n  NOT_FOUND: old_str was not found in the file.\n");
    if let Some(n) = nearest {
        out.push_str(&format!(
            "  closest match: L{}-L{} (similarity {:.2})\n",
            n.start_line, n.end_line, n.score
        ));
        out.push_str("  diff (your old_str → actual file content):\n");
        let before = truncate_for_diff(old);
        let after = truncate_for_diff(&n.text);
        let diff = TextDiff::from_lines(before.as_ref(), after.as_ref())
            .unified_diff()
            .context_radius(3)
            .header(
                "old_str",
                &format!("file:L{}-L{}", n.start_line, n.end_line),
            )
            .to_string();
        for line in diff.lines() {
            out.push_str("    ");
            out.push_str(line);
            out.push('\n');
        }
    } else {
        out.push_str("  no similar region found; re-read the file with read.\n");
    }
    out
}

/// `AMBIGUOUS_MATCH` 的模型可见正文：命中总数 + 前 3 处的位置与上下文。
pub(crate) fn render_ambiguous(path: &str, total: usize, occurrences: &[Occurrence]) -> String {
    let mut out = format!(
        "[ERROR] edit {path}\n  AMBIGUOUS_MATCH: old_str matches {total} locations; extend it with surrounding lines so it matches exactly one.\n"
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

fn truncate_for_diff(text: &str) -> std::borrow::Cow<'_, str> {
    const MAX: usize = 2000;
    if text.len() <= MAX {
        std::borrow::Cow::Borrowed(text)
    } else {
        let cut = text.floor_char_boundary(MAX);
        std::borrow::Cow::Owned(format!(
            "{}…[truncated]",
            text.get(..cut).unwrap_or_default()
        ))
    }
}
