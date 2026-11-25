//! 共享失败诊断：最近似片段定位、纯空白差异识别与 unified diff 渲染。
//!
//! 仅供 `edit` 与 `apply_patch_engine` 在精确/规则匹配**失败后**生成诊断信息，
//! 绝不参与匹配采纳。

use similar::TextDiff;

use crate::file_shared::LineIndex;

/// 最低相似度门槛：低于此值说明文件中不存在有意义的相似片段，
/// 宁可不输出 diff，也不要吐出风马牛不相及的噪音误导模型。
pub(crate) const MIN_DIAGNOSTIC_SIMILARITY: f32 = 0.30;

/// 超过该行数时先按特征行粗筛候选窗口，避免大文件上全表 O(N) 字符级 diff 卡顿。
const LARGE_FILE_LINE_THRESHOLD: usize = 400;
const LARGE_FILE_TOP_CANDIDATES: usize = 40;

/// 最接近目标文本的文件区域（仅用于失败诊断）。
#[derive(Debug, Clone)]
pub(crate) struct NearestMatch {
    /// 1-based 起始行号。
    pub(crate) start_line: usize,
    /// 1-based 结束行号（含）。
    pub(crate) end_line: usize,
    pub(crate) text: String,
    pub(crate) score: f32,
}

/// 在已切分的文件行视图中寻找最接近 `pat_lines` 的连续行区间。
pub(crate) fn nearest_match_in_lines(
    file_lines: &[&str],
    pat_lines: &[&str],
    min_score: f32,
) -> Option<NearestMatch> {
    if file_lines.is_empty() {
        return None;
    }
    let mut pat: Vec<&str> = pat_lines.to_vec();
    while pat.last() == Some(&"") {
        pat.pop();
    }
    if pat.is_empty() {
        return None;
    }
    let old_text = pat.join("\n");
    let p = pat.len();
    let min_win = p.saturating_sub(2).max(1);
    let max_win = (p + 2).min(file_lines.len());
    if min_win > max_win {
        return None;
    }

    let mut best: Option<(f32, usize, usize)> = None;

    if file_lines.len() > LARGE_FILE_LINE_THRESHOLD {
        // 选 pattern 中最长的非空行作为锚行做行级粗筛，定位 Top-K 候选中心。
        let (anchor_offset, anchor_line) = pat
            .iter()
            .enumerate()
            .max_by_key(|(_, l)| l.trim().len())
            .map(|(i, l)| (i, *l))
            .unwrap_or((0, pat[0]));
        let mut line_scores: Vec<(f32, usize)> = file_lines
            .iter()
            .enumerate()
            .map(|(i, line)| (quick_line_score(anchor_line, line), i))
            .collect();
        line_scores.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
        line_scores.truncate(LARGE_FILE_TOP_CANDIDATES);

        let mut seen = std::collections::HashSet::new();
        for &(_, hit_line) in &line_scores {
            let center_start = hit_line.saturating_sub(anchor_offset);
            let lo = center_start.saturating_sub(2);
            let hi = (center_start + 2).min(file_lines.len().saturating_sub(1));
            for win in min_win..=max_win {
                let max_s = file_lines.len().saturating_sub(win);
                for s in lo..=hi.min(max_s) {
                    if !seen.insert((s, win)) {
                        continue;
                    }
                    let text = file_lines[s..s + win].join("\n");
                    let score = char_ratio(&old_text, &text);
                    if best.is_none_or(|(bs, _, _)| score > bs) {
                        best = Some((score, s, win));
                    }
                }
            }
        }
    } else {
        for win in min_win..=max_win {
            for s in 0..=file_lines.len() - win {
                let text = file_lines[s..s + win].join("\n");
                let score = char_ratio(&old_text, &text);
                if best.is_none_or(|(bs, _, _)| score > bs) {
                    best = Some((score, s, win));
                }
            }
        }
        if p == 1 {
            for (i, line) in file_lines.iter().enumerate() {
                let score = char_ratio(&old_text, line);
                if best.is_none_or(|(bs, _, _)| score > bs) {
                    best = Some((score, i, 1));
                }
            }
        }
    }

    best.filter(|(score, _, _)| *score >= min_score)
        .map(|(score, s, win)| NearestMatch {
            start_line: s + 1,
            end_line: s + win,
            text: file_lines[s..s + win].join("\n"),
            score,
        })
}

/// 在完整 LF 规范文本中寻找最接近 `old` 的行区间。
pub(crate) fn nearest_match_in_text(
    content: &str,
    old: &str,
    min_score: f32,
) -> Option<NearestMatch> {
    let index = LineIndex::new(content);
    let file_lines = index.lines();
    let pat_lines: Vec<&str> = old.split('\n').collect();
    nearest_match_in_lines(file_lines, &pat_lines, min_score)
}

/// 若 `expected` 与 `actual` 仅在空白/缩进/行尾空格不同，返回一句简短的事实说明。
pub(crate) fn whitespace_only_mismatch_note(expected: &str, actual: &str) -> Option<&'static str> {
    let exp = expected.trim_end_matches('\n');
    let act = actual.trim_end_matches('\n');
    if exp == act {
        return None;
    }
    let exp_lines: Vec<&str> = exp.lines().collect();
    let act_lines: Vec<&str> = act.lines().collect();
    if exp_lines.len() == act_lines.len() {
        if exp_lines
            .iter()
            .zip(&act_lines)
            .all(|(a, b)| a.trim_end() == b.trim_end())
        {
            return Some("differs only in trailing whitespace");
        }
        if exp_lines
            .iter()
            .zip(&act_lines)
            .all(|(a, b)| a.trim() == b.trim())
        {
            let has_tab_diff = exp_lines.iter().zip(&act_lines).any(|(a, b)| {
                let la = leading_ws(a);
                let lb = leading_ws(b);
                la.contains('\t') != lb.contains('\t')
            });
            if has_tab_diff {
                return Some("differs only in indentation (tabs vs spaces)");
            }
            return Some("differs only in indentation/whitespace");
        }
    }
    if exp.split_whitespace().eq(act.split_whitespace()) {
        return Some("differs only in whitespace/indentation (spaces, tabs, or line breaks)");
    }
    None
}

/// 渲染 `closest match` + 可选空白差异注记 + unified diff 块。
pub(crate) fn render_diff_block(
    expected_label: &str,
    expected: &str,
    nearest: &NearestMatch,
    indent: &str,
) -> String {
    let mut out = String::new();
    let line_span = if nearest.start_line == nearest.end_line {
        format!("L{}", nearest.start_line)
    } else {
        format!("L{}-L{}", nearest.start_line, nearest.end_line)
    };
    out.push_str(&format!(
        "{indent}closest match: {line_span} (similarity {:.2})\n",
        nearest.score
    ));
    if let Some(note) = whitespace_only_mismatch_note(expected, &nearest.text) {
        out.push_str(&format!("{indent}note: {note}\n"));
    }
    out.push_str(&format!(
        "{indent}diff (your {expected_label} → actual file content):\n"
    ));
    let before = truncate_for_diff(expected);
    let after = truncate_for_diff(&nearest.text);
    let diff = TextDiff::from_lines(before.as_ref(), after.as_ref())
        .unified_diff()
        .context_radius(3)
        .header(expected_label, &format!("file:{line_span}"))
        .to_string();
    for line in diff.lines() {
        out.push_str(indent);
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn leading_ws(s: &str) -> &str {
    let trimmed = s.trim_start();
    // trim_start 只切空白字节，偏移必落在字符边界；split_at 与 string_slice
    // 等价但可过 `clippy::string-slice` 门（workspace lints deny，582d4d1c 遗留）。
    s.split_at(s.len() - trimmed.len()).0
}

fn char_ratio(a: &str, b: &str) -> f32 {
    TextDiff::from_chars(a, b).ratio()
}

/// 大文件粗筛评分：长度比 + 共同非空白字符 bigram 重合度，线性开销。
fn quick_line_score(a: &str, b: &str) -> f32 {
    let ta = a.trim();
    let tb = b.trim();
    if ta.is_empty() && tb.is_empty() {
        return 1.0;
    }
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    if ta == tb {
        return 1.0;
    }
    let ab: Vec<char> = ta.chars().collect();
    let bb: Vec<char> = tb.chars().collect();
    if ab.len() < 2 || bb.len() < 2 {
        return char_ratio(ta, tb);
    }
    let mut bigrams = std::collections::HashSet::with_capacity(ab.len());
    for pair in ab.windows(2) {
        bigrams.insert((pair[0], pair[1]));
    }
    let hits = bb
        .windows(2)
        .filter(|pair| bigrams.contains(&(pair[0], pair[1])))
        .count();
    (2.0 * hits as f32) / ((ab.len() - 1 + bb.len() - 1) as f32)
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
