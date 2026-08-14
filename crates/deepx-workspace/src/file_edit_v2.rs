//! edit_file_v2 — 差分实现的第二代编辑工具。
//!
//! 定位（见 docs/nextdev/PLAN-EDIT-FILE-V2.md）：**只实现 v1 没有的部分**，
//! v1（edit_file）保留行号定位、replace_all、regex、每 op 独立事务与诊断体系。
//!
//! v2 的能力面（对齐 edit-tool-design-spec.md）：
//!
//! - 结构化 hunk 协议：`replace` / `insert_after` / `insert_before` /
//!   `prepend_file` / `append_file`，字段名（old/new/anchor）与 v1 刻意区分。
//! - 四层匹配流水线：Tier1 精确（context 全等消歧）→ Tier2 缩进形状 →
//!   Tier3 相似度评分（0.6/0.2/0.2 加权 + 阈值 0.85 + margin 0.10 自动采纳）→
//!   Tier4 拒绝并返回 Top3 候选。
//! - 单快照两阶段全事务：全部 hunk 在同一份未修改快照上定位，任一失败整体拒绝；
//!   区间重叠 → OVERLAPPING_HUNKS；应用按位置倒序走 ropey remove/insert。
//! - 显式 `expected_hash` 乐观锁；失配返回 current_hash + current_content（截断）。
//! - 成功返回 new_hash（sha256，与 read_file 协议一致），可续接下一次调用。
//!
//! 复用（基础设施白名单，非 v1 能力）：`file_shared::{content_hash,
//! normalize_newlines, atomic_write, unified_diff}`。CRLF 契约与 v1 相同：
//! LF 规范视图上匹配与算 hash，写回按 was_crlf 还原。

use std::ops::Range;

use ropey::Rope;
use serde_json::{json, Value};

use crate::file_shared::{atomic_write, content_hash, normalize_newlines, unified_diff};
use crate::{ToolCallCtx, ToolHandler, ToolManager, ToolPlacement, ToolResult, ToolRisk};

// ─────────────────────────────────────────────────────────────
// 配置常量
// ─────────────────────────────────────────────────────────────

/// Tier3 相似度采纳阈值。保守起步值 0.90（spec 建议 0.85）；
/// 灰度期收集真实误改案例后再回调。
const T3_THRESHOLD: f32 = 0.90;
/// Tier3 胜出边际：最高分必须显著超过次高分才自动采纳。
/// 保守起步值 0.15（spec 建议 0.10）。
const T3_MARGIN: f32 = 0.15;
/// 单次调用 hunk 数上限。
const MAX_HUNKS: usize = 64;
/// HashMismatch 时回传内容的截断字节数。
const CONTENT_CAP: usize = 64 * 1024;
/// 失败候选列表条数上限。
const CANDIDATE_MAX: usize = 3;
/// 候选 snippet 展示长度（字符）。
const SNIPPET_MAX: usize = 120;

// ─────────────────────────────────────────────────────────────
// 请求模型
// ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Hunk {
    /// 替换；old 为空字符串 = 纯插入（context_before/after 至少一侧非空）。
    Replace {
        old: String,
        new: String,
        context_before: String,
        context_after: String,
    },
    InsertAfter { anchor: String, new: String },
    InsertBefore { anchor: String, new: String },
    PrependFile { new: String },
    AppendFile { new: String },
}

impl Hunk {
    fn kind_name(&self) -> &'static str {
        match self {
            Hunk::Replace { .. } => "replace",
            Hunk::InsertAfter { .. } => "insert_after",
            Hunk::InsertBefore { .. } => "insert_before",
            Hunk::PrependFile { .. } => "prepend_file",
            Hunk::AppendFile { .. } => "append_file",
        }
    }

    /// 解析单个 hunk。文本字段统一做 CRLF → LF 归一化（LF 规范视图契约），
    /// 检测到 CR 时向 `notes` 追加说明。
    fn parse(v: &Value, notes: &mut Vec<String>) -> Result<Hunk, String> {
        let norm = |s: &str, notes: &mut Vec<String>| -> String {
            if s.contains('\r') {
                notes.push("CRLF in request normalized to LF".to_string());
                normalize_newlines(s).0
            } else {
                s.to_string()
            }
        };
        let kind = v
            .get("kind")
            .and_then(|x| x.as_str())
            .ok_or_else(|| "missing 'kind'".to_string())?;
        match kind {
            "replace" => {
                let old = v
                    .get("old")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| "replace hunk requires 'old'".to_string())?;
                let new = v
                    .get("new")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| "replace hunk requires 'new'".to_string())?;
                let context_before = v
                    .get("context_before")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let context_after = v
                    .get("context_after")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                Ok(Hunk::Replace {
                    old: norm(old, notes),
                    new: norm(new, notes),
                    context_before: norm(context_before, notes),
                    context_after: norm(context_after, notes),
                })
            }
            "insert_after" | "insert_before" => {
                let anchor = v
                    .get("anchor")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| format!("{kind} hunk requires non-empty 'anchor'"))?;
                let new = v
                    .get("new")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| format!("{kind} hunk requires 'new'"))?;
                let (anchor, new) = (norm(anchor, notes), norm(new, notes));
                if kind == "insert_after" {
                    Ok(Hunk::InsertAfter { anchor, new })
                } else {
                    Ok(Hunk::InsertBefore { anchor, new })
                }
            }
            "prepend_file" | "append_file" => {
                let new = v
                    .get("new")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| format!("{kind} hunk requires 'new'"))?;
                let new = norm(new, notes);
                if kind == "prepend_file" {
                    Ok(Hunk::PrependFile { new })
                } else {
                    Ok(Hunk::AppendFile { new })
                }
            }
            other => Err(format!(
                "unknown hunk kind '{other}' (expected replace / insert_after / insert_before / prepend_file / append_file)"
            )),
        }
    }
}

// ─────────────────────────────────────────────────────────────
// 文件视图（LF 规范视图）
// ─────────────────────────────────────────────────────────────

struct FileView<'a> {
    content: &'a str,
    /// 每行内容（不含 '\n'）。文件以 '\n' 结尾时含一个尾空行。
    lines: Vec<&'a str>,
    /// 每行行首 char index；len = lines.len() + 1，末项 = content.len()。
    line_starts: Vec<usize>,
}

impl<'a> FileView<'a> {
    fn new(content: &'a str) -> Self {
        let mut lines = Vec::new();
        let mut line_starts = Vec::new();
        let mut start = 0usize;
        for (i, ch) in content.char_indices() {
            if ch == '\n' {
                lines.push(&content[start..i]);
                line_starts.push(start);
                start = i + 1;
            }
        }
        if start <= content.len() {
            lines.push(&content[start..]);
            line_starts.push(start);
        }
        line_starts.push(content.len());
        FileView {
            content,
            lines,
            line_starts,
        }
    }

    /// 行窗口 [s, s+win) 的 char 区间。
    fn char_range(&self, s: usize, win: usize) -> Range<usize> {
        self.line_starts[s]..self.line_starts[s + win]
    }
}

// ─────────────────────────────────────────────────────────────
// 匹配流水线（Tier1 → Tier2 → Tier3 → 拒绝）
// ─────────────────────────────────────────────────────────────

/// 把文本拆成行数组：只去掉末尾因结尾换行产生的空元素，保留中间空行。
/// 尾随换行不参与匹配（old = "foo\nbar" 与 "foo\nbar\n" 等价）。
fn pattern_lines(s: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = s.split('\n').collect();
    while lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

#[derive(Debug, Clone)]
struct Candidate {
    line_range: (usize, usize),
    snippet: String,
    score: f32,
    tier: u8,
    /// pattern（模型给的）与文件窗口的 -/+ 行对照：模型一眼看出差异。
    diff: String,
}

#[derive(Debug)]
struct Located {
    start_char: usize,
    end_char: usize,
    /// 窗口首行（0-based）；win_lines = 0 表示插入点。
    start_line: usize,
    win_lines: usize,
    tier: u8,
    score: f32,
    note: String,
}

#[derive(Debug)]
enum LocateError {
    NoMatch {
        candidates: Vec<Candidate>,
        /// 为什么没命中：差阈值 / 差 margin / 完全不像（见 no_match_detail）。
        detail: String,
    },
    Ambiguous {
        candidates: Vec<Candidate>,
        detail: String,
    },
    Underspecified,
}

/// Tier1：逐行全等匹配。返回全部命中位置。
fn tier1(view: &FileView, pat: &[&str]) -> Vec<usize> {
    let fl = &view.lines;
    if pat.is_empty() || pat.len() > fl.len() {
        return Vec::new();
    }
    (0..=fl.len() - pat.len())
        .filter(|&s| (0..pat.len()).all(|k| fl[s + k] == pat[k]))
        .collect()
}

/// 剥离公共最小缩进（spec Tier2 的"形状"）。空行/纯空白行 → 空串。
fn strip_indent(lines: &[&str]) -> Vec<String> {
    let min = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                // 前导空白（空格/tab）是单字节 UTF-8，min 必落在 char boundary。
                l[min.min(l.len())..].to_string()
            }
        })
        .collect()
}

/// Tier2：缩进形状匹配。返回全部命中位置。
fn tier2(view: &FileView, pat: &[&str]) -> Vec<usize> {
    let fl = &view.lines;
    let shape = strip_indent(pat);
    if shape.is_empty() || shape.len() > fl.len() {
        return Vec::new();
    }
    (0..=fl.len() - shape.len())
        .filter(|&s| strip_indent(&fl[s..s + shape.len()]) == shape)
        .collect()
}

/// 字符级 SequenceMatcher ratio（difflib 语义：1.0=相同，0.0=完全不同）。
///
/// ⚠ 不用 `TextDiff::from_lines().ratio()`：它是**行粒度**比较（每行是原子
/// 单元），单行 typo 场景（"let fo0" vs "let foo"）会直接判 0——spec 原文
/// 假设其等价 difflib，实测不符，以 crate 实际行为为准改 from_chars。
fn ratio(a: &str, b: &str) -> f32 {
    similar::TextDiff::from_chars(a, b).ratio()
}

/// context 全等消歧：候选窗口中，紧贴窗口前/后的行与 context 逐行全等。
fn context_filter(
    view: &FileView,
    cands: &[usize],
    win: usize,
    before: &[&str],
    after: &[&str],
) -> Vec<usize> {
    let fl = &view.lines;
    cands.iter()
        .copied()
        .filter(|&s| {
            let b_ok = before.is_empty()
                || (s >= before.len()
                    && (0..before.len()).all(|k| fl[s - before.len() + k] == before[k]));
            let a_ok = after.is_empty()
                || (s + win + after.len() <= fl.len()
                    && (0..after.len()).all(|k| fl[s + win + k] == after[k]));
            b_ok && a_ok
        })
        .collect()
}

/// 窗口 snippet（展示用，截断）。
fn snippet_of(view: &FileView, s: usize, win: usize) -> String {
    let text = view.lines[s..s + win].join("\n");
    let cut = text.floor_char_boundary(text.len().min(SNIPPET_MAX));
    let mut out = text[..cut].to_string();
    if cut < text.len() {
        out.push('…');
    }
    out
}

/// pattern（模型给的）与文件窗口的 -/+ 行对照：模型一眼看出差异在哪。
fn pattern_vs_candidate(pat: &[&str], cand: &[&str]) -> String {
    let mut out = String::new();
    let n = pat.len().min(cand.len());
    for i in 0..n {
        if pat[i] == cand[i] {
            out.push_str(&format!("    {}\n", cand[i]));
        } else {
            out.push_str(&format!("  - {}\n  + {}\n", pat[i], cand[i]));
        }
    }
    for l in &cand[n..] {
        out.push_str(&format!("  + {}\n", l));
    }
    for l in &pat[n..] {
        out.push_str(&format!("  - {}\n", l));
    }
    out.trim_end().to_string()
}

fn make_candidates(
    view: &FileView,
    positions: &[usize],
    win: usize,
    tier: u8,
    score: f32,
    pat: &[&str],
) -> Vec<Candidate> {
    positions
        .iter()
        .take(CANDIDATE_MAX)
        .map(|&s| Candidate {
            line_range: (s + 1, s + win),
            snippet: snippet_of(view, s, win),
            score,
            tier,
            diff: pattern_vs_candidate(pat, &view.lines[s..s + win]),
        })
        .collect()
}

/// Tier3 探测结果：hit 达标（满足阈值 + margin），best/second 供失败诊断。
struct Tier3Probe {
    hit: Option<(usize, usize, f32)>,
    best: f32,
    second: f32,
}

/// 失败诊断：告诉模型**为什么**没采纳（差阈值 / 差 margin / 完全不像），
/// 而不是只给一句 NO_MATCH。
fn no_match_detail(probe: &Tier3Probe) -> String {
    if probe.best >= T3_THRESHOLD {
        format!(
            "best score {:.2} but margin to second-best ({:.2}) is below {:.2} — two locations look equally plausible; add context_before/context_after or extend 'old'",
            probe.best, probe.second, T3_MARGIN
        )
    } else if probe.best > 0.01 {
        format!(
            "best score {:.2} is below threshold {:.2} — closest location is probably wrong; re-check 'old' against the file (see candidates below)",
            probe.best, T3_THRESHOLD
        )
    } else {
        "no window had any similarity — 'old' does not resemble any part of the file".to_string()
    }
}

/// Tier3：相似度评分探测（不采纳）。达标返回 hit。
///
/// 权重：old 0.6，context_before 0.2，context_after 0.2；缺失的 context
/// 不占权重（总权重归一化，score 恒在 0~1）。
/// 窗口行数容差：pattern 行数 −2..=+2（≥1）。
fn tier3_probe(
    view: &FileView,
    pat: &[&str],
    before: &[&str],
    after: &[&str],
) -> Tier3Probe {
    let fl = &view.lines;
    let p = pat.len();
    if p == 0 || p > fl.len() + 2 {
        return Tier3Probe {
            hit: None,
            best: 0.0,
            second: 0.0,
        };
    }
    let old_s = strip_indent(pat).join("\n");
    let b_s = strip_indent(before).join("\n");
    let a_s = strip_indent(after).join("\n");
    let w_old = 0.6f32;
    let w_b = if before.is_empty() { 0.0 } else { 0.2 };
    let w_a = if after.is_empty() { 0.0 } else { 0.2 };
    let w_total = w_old + w_b + w_a;

    let lo = p.saturating_sub(2).max(1);
    let hi = (p + 2).min(fl.len());
    let mut best: Option<(usize, usize, f32)> = None;
    let mut second_score: f32 = 0.0;
    for win in lo..=hi {
        for s in 0..=fl.len() - win {
            let win_s = strip_indent(&fl[s..s + win]).join("\n");
            let mut score = w_old * ratio(&old_s, &win_s);
            if w_b > 0.0 {
                let avail = strip_indent(&fl[s.saturating_sub(before.len())..s]).join("\n");
                score += w_b * ratio(&b_s, &avail);
            }
            if w_a > 0.0 {
                let a_end = (s + win + after.len()).min(fl.len());
                let avail = strip_indent(&fl[s + win..a_end]).join("\n");
                score += w_a * ratio(&a_s, &avail);
            }
            let score = score / w_total;
            match best {
                None => best = Some((s, win, score)),
                Some((_, _, bs)) if score > bs => {
                    second_score = bs;
                    best = Some((s, win, score));
                }
                Some((_, _, _)) => {
                    // 含并列：score == bs 时 second 被抬到 bs，margin 归零 → 拒绝。
                    if score > second_score {
                        second_score = score;
                    }
                }
            }
        }
    }
    match best {
        None => Tier3Probe {
            hit: None,
            best: 0.0,
            second: 0.0,
        },
        Some((s, w, sc)) => Tier3Probe {
            hit: (sc >= T3_THRESHOLD && (sc - second_score) >= T3_MARGIN).then_some((s, w, sc)),
            best: sc,
            second: second_score,
        },
    }
}

/// Tier3 全窗口候选（不判 margin），Top3。
fn tier3_candidates(view: &FileView, pat: &[&str], before: &[&str], after: &[&str]) -> Vec<Candidate> {
    let fl = &view.lines;
    let p = pat.len();
    if p == 0 || p > fl.len() + 2 {
        return Vec::new();
    }
    let old_s = strip_indent(pat).join("\n");
    let b_s = strip_indent(before).join("\n");
    let a_s = strip_indent(after).join("\n");
    let w_old = 0.6f32;
    let w_b = if before.is_empty() { 0.0 } else { 0.2 };
    let w_a = if after.is_empty() { 0.0 } else { 0.2 };
    let w_total = w_old + w_b + w_a;

    let lo = p.saturating_sub(2).max(1);
    let hi = (p + 2).min(fl.len());
    let mut all: Vec<(f32, usize, usize)> = Vec::new();
    for win in lo..=hi {
        for s in 0..=fl.len() - win {
            let win_s = strip_indent(&fl[s..s + win]).join("\n");
            let mut score = w_old * ratio(&old_s, &win_s);
            if w_b > 0.0 {
                let avail = strip_indent(&fl[s.saturating_sub(before.len())..s]).join("\n");
                score += w_b * ratio(&b_s, &avail);
            }
            if w_a > 0.0 {
                let a_end = (s + win + after.len()).min(fl.len());
                let avail = strip_indent(&fl[s + win..a_end]).join("\n");
                score += w_a * ratio(&a_s, &avail);
            }
            all.push((score / w_total, s, win));
        }
    }
    all.sort_by(|x, y| y.0.partial_cmp(&x.0).unwrap_or(std::cmp::Ordering::Equal));
    all.truncate(CANDIDATE_MAX);
    all.into_iter()
        .map(|(score, s, win)| Candidate {
            line_range: (s + 1, s + win),
            snippet: snippet_of(view, s, win),
            score,
            tier: 3,
            diff: pattern_vs_candidate(pat, &view.lines[s..s + win]),
        })
        .collect()
}

fn located(view: &FileView, s: usize, win: usize, tier: u8, score: f32, note: &str) -> Located {
    let range = view.char_range(s, win);
    Located {
        start_char: range.start,
        end_char: range.end,
        start_line: s,
        win_lines: win,
        tier,
        score,
        note: note.to_string(),
    }
}

/// 锚点定位（insert_after / insert_before 的 anchor，以及纯插入的 context）。
/// 无 context 消歧可用——多处命中即 Ambiguous。
fn locate_anchor(view: &FileView, anchor: &str) -> Result<Located, LocateError> {
    let pat = pattern_lines(anchor);
    if pat.is_empty() {
        return Err(LocateError::Underspecified);
    }
    let cands = tier1(view, &pat);
    if cands.len() == 1 {
        return Ok(located(view, cands[0], pat.len(), 1, 1.0, "exact"));
    }
    if cands.len() > 1 {
        return Err(LocateError::Ambiguous {
            candidates: make_candidates(view, &cands, pat.len(), 1, 1.0, &pat),
            detail: format!("anchor matches {} locations", cands.len()),
        });
    }
    let cands = tier2(view, &pat);
    if cands.len() == 1 {
        return Ok(located(view, cands[0], pat.len(), 2, 1.0, "indent-shape"));
    }
    if cands.len() > 1 {
        return Err(LocateError::Ambiguous {
            candidates: make_candidates(view, &cands, pat.len(), 2, 1.0, &pat),
            detail: format!("anchor matches {} locations (indent-shape)", cands.len()),
        });
    }
    let probe = tier3_probe(view, &pat, &[], &[]);
    if let Some((s, w, score)) = probe.hit {
        return Ok(located(view, s, w, 3, score, &format!("similarity {score:.2}")));
    }
    Err(LocateError::NoMatch {
        candidates: tier3_candidates(view, &pat, &[], &[]),
        detail: no_match_detail(&probe),
    })
}

/// 纯插入（old 为空）：context_before / context_after 各自定位，取交界处。
fn locate_pure_insert(view: &FileView, before: &str, after: &str) -> Result<Located, LocateError> {
    let b = pattern_lines(before);
    let a = pattern_lines(after);
    if b.is_empty() && a.is_empty() {
        return Err(LocateError::Underspecified);
    }
    let b_loc = if b.is_empty() {
        None
    } else {
        Some(locate_anchor(view, before)?)
    };
    let a_loc = if a.is_empty() {
        None
    } else {
        Some(locate_anchor(view, after)?)
    };
    let (pos, tier, score, note) = match (&b_loc, &a_loc) {
        (Some(bl), Some(al)) => {
            if bl.end_char > al.start_char {
                return Err(LocateError::Ambiguous {
                    candidates: Vec::new(),
                    detail: "context_before and context_after regions overlap or are inverted"
                        .to_string(),
                });
            }
            (
                bl.end_char,
                bl.tier.max(al.tier),
                bl.score.min(al.score),
                "pure insert (both contexts)".to_string(),
            )
        }
        (Some(bl), None) => (
            bl.end_char,
            bl.tier,
            bl.score,
            "pure insert (context_before)".to_string(),
        ),
        (None, Some(al)) => (
            al.start_char,
            al.tier,
            al.score,
            "pure insert (context_after)".to_string(),
        ),
        (None, None) => unreachable!("both contexts empty is checked above"),
    };
    Ok(Located {
        start_char: pos,
        end_char: pos,
        start_line: 0,
        win_lines: 0,
        tier,
        score,
        note,
    })
}

/// replace hunk 的定位：四层流水线。
fn locate_replace(
    view: &FileView,
    old: &str,
    before: &str,
    after: &str,
) -> Result<Located, LocateError> {
    let pat = pattern_lines(old);
    if pat.is_empty() {
        return locate_pure_insert(view, before, after);
    }
    let b = pattern_lines(before);
    let a = pattern_lines(after);

    // Tier1 精确
    let cands = tier1(view, &pat);
    if !cands.is_empty() {
        if cands.len() == 1 {
            return Ok(located(view, cands[0], pat.len(), 1, 1.0, "exact"));
        }
        let filtered = context_filter(view, &cands, pat.len(), &b, &a);
        if filtered.len() == 1 {
            return Ok(located(view, filtered[0], pat.len(), 1, 1.0, "exact (context)"));
        }
        return Err(LocateError::Ambiguous {
            candidates: make_candidates(view, &cands, pat.len(), 1, 1.0, &pat),
            detail: format!(
                "old matches {} locations; context_before/context_after did not disambiguate — pick one by its surrounding lines (see candidates) and add context_before/context_after",
                cands.len()
            ),
        });
    }

    // Tier2 缩进形状
    let cands = tier2(view, &pat);
    if !cands.is_empty() {
        if cands.len() == 1 {
            return Ok(located(view, cands[0], pat.len(), 2, 1.0, "indent-shape"));
        }
        let filtered = context_filter(view, &cands, pat.len(), &b, &a);
        if filtered.len() == 1 {
            return Ok(located(
                view,
                filtered[0],
                pat.len(),
                2,
                1.0,
                "indent-shape (context)",
            ));
        }
        return Err(LocateError::Ambiguous {
            candidates: make_candidates(view, &cands, pat.len(), 2, 1.0, &pat),
            detail: format!(
                "old matches {} locations (indent-shape); context did not disambiguate",
                cands.len()
            ),
        });
    }

    // Tier3 相似度评分
    let probe = tier3_probe(view, &pat, &b, &a);
    if let Some((s, w, score)) = probe.hit {
        return Ok(located(
            view,
            s,
            w,
            3,
            score,
            &format!("similarity {score:.2}"),
        ));
    }

    // Tier4 拒绝 + 候选
    Err(LocateError::NoMatch {
        candidates: tier3_candidates(view, &pat, &b, &a),
        detail: no_match_detail(&probe),
    })
}

fn locate_hunk(view: &FileView, hunk: &Hunk) -> Result<Located, LocateError> {
    match hunk {
        Hunk::Replace {
            old,
            context_before,
            context_after,
            ..
        } => locate_replace(view, old, context_before, context_after),
        Hunk::InsertAfter { anchor, .. } | Hunk::InsertBefore { anchor, .. } => {
            locate_anchor(view, anchor)
        }
        Hunk::PrependFile { .. } => Ok(Located {
            start_char: 0,
            end_char: 0,
            start_line: 0,
            win_lines: 0,
            tier: 1,
            score: 1.0,
            note: "prepend".to_string(),
        }),
        Hunk::AppendFile { .. } => Ok(Located {
            start_char: view.content.len(),
            end_char: view.content.len(),
            start_line: 0,
            win_lines: 0,
            tier: 1,
            score: 1.0,
            note: "append".to_string(),
        }),
    }
}

// ─────────────────────────────────────────────────────────────
// 应用：resolve → 重叠检测 → 倒序 rope 应用
// ─────────────────────────────────────────────────────────────

enum ResolvedOp {
    Replace { range: Range<usize>, new: String },
    Insert { pos: usize, text: String },
}

impl ResolvedOp {
    fn start(&self) -> usize {
        match self {
            ResolvedOp::Replace { range, .. } => range.start,
            ResolvedOp::Insert { pos, .. } => *pos,
        }
    }
}

/// Replace 的替换区间：行窗口 char range，末行尾换行按 `want_trailing_nl`
/// 决定是否纳入（文件尾无换行时自动退化）。纯插入（win_lines=0）→ 零长度区间。
fn replace_range(view: &FileView, loc: &Located, want_trailing_nl: bool) -> Range<usize> {
    if loc.win_lines == 0 {
        return loc.start_char..loc.end_char;
    }
    let start = view.line_starts[loc.start_line];
    let end = view.line_starts[loc.start_line + loc.win_lines];
    if want_trailing_nl || end <= start {
        return start..end;
    }
    let trimmed = if view.content.as_bytes().get(end.wrapping_sub(1)) == Some(&b'\n') {
        end - 1
    } else {
        end
    };
    start..trimmed
}

/// 命中窗口的公共缩进（首个非空行的前导空白，取到公共最小缩进长度）。
fn window_indent(view: &FileView, loc: &Located) -> String {
    if loc.win_lines == 0 {
        return String::new();
    }
    let lines = &view.lines[loc.start_line..loc.start_line + loc.win_lines];
    let min = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .find(|l| !l.trim().is_empty())
        .map(|l| l[..min].to_string())
        .unwrap_or_default()
}

/// 缩进补偿：new 先剥自身公共缩进，再逐非空行补回 `base`。
/// new 带不带缩进都给对，相对缩进结构保留，空行保持空。
fn reindent(new: &str, base: &str) -> String {
    if new.is_empty() || base.is_empty() {
        return new.to_string();
    }
    let lines: Vec<&str> = new.split('\n').collect();
    let stripped = strip_indent(&lines);
    stripped
        .iter()
        .map(|l| {
            if l.is_empty() {
                String::new()
            } else {
                format!("{base}{l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn resolve(view: &FileView, hunk: &Hunk, loc: &Located) -> ResolvedOp {
    match hunk {
        Hunk::Replace { old, new, .. } => {
            // 换行语义：old 以 '\n' 结尾 → 区间包含末行尾换行（模型显式声明）；
            // 否则不含（替换内容对称：old 不带换行，new 也不用带，不会粘行）。
            let range = replace_range(view, loc, !old.is_empty() && old.ends_with('\n'));
            // 缩进补偿：Tier2/3 按剥离公共缩进后的形状/相似度命中，
            // new 需补回文件窗口的基准缩进（new 自身可带可不带缩进，
            // 统一剥壳后补基准，相对缩进结构保留）。Tier1 精确命中不补偿。
            let new = if loc.tier >= 2 {
                reindent(new, &window_indent(view, loc))
            } else {
                new.clone()
            };
            ResolvedOp::Replace { range, new }
        }
        Hunk::InsertAfter { new, .. } => {
            let pos = view.line_starts[loc.start_line + loc.win_lines];
            // 锚点窗口是文件尾且无尾随换行：先补换行，避免新内容粘在最后一行上。
            let text = if pos == view.content.len() && !view.content.ends_with('\n') {
                format!("\n{new}")
            } else {
                new.clone()
            };
            ResolvedOp::Insert { pos, text }
        }
        Hunk::InsertBefore { new, .. } => ResolvedOp::Insert {
            pos: view.line_starts[loc.start_line],
            text: new.clone(),
        },
        Hunk::PrependFile { new } => ResolvedOp::Insert {
            pos: 0,
            text: new.clone(),
        },
        Hunk::AppendFile { new } => {
            let text = if !view.content.is_empty() && !view.content.ends_with('\n') {
                format!("\n{new}")
            } else {
                new.clone()
            };
            ResolvedOp::Insert {
                pos: view.content.len(),
                text,
            }
        }
    }
}

/// 重叠判定：
/// - 两个零长度插入点：同位置冲突；
/// - 插入点落入替换区间**严格内部**：冲突（落在边界上安全，结果与顺序无关）；
/// - 两个非零区间：按半开区间相交判定。
fn ranges_overlap(x: &ResolvedOp, y: &ResolvedOp) -> bool {
    let (x1, x2, xz) = match x {
        ResolvedOp::Replace { range, .. } => (range.start, range.end, false),
        ResolvedOp::Insert { pos, .. } => (*pos, *pos, true),
    };
    let (y1, y2, yz) = match y {
        ResolvedOp::Replace { range, .. } => (range.start, range.end, false),
        ResolvedOp::Insert { pos, .. } => (*pos, *pos, true),
    };
    if xz && yz {
        return x1 == y1;
    }
    if xz {
        return x1 > y1 && x1 < y2;
    }
    if yz {
        return y1 > x1 && y1 < x2;
    }
    x1 < y2 && y1 < x2
}

fn check_overlap(ops: &[(usize, ResolvedOp)]) -> Result<(), (usize, usize)> {
    for i in 0..ops.len() {
        for j in (i + 1)..ops.len() {
            if ranges_overlap(&ops[i].1, &ops[j].1) {
                return Err((ops[i].0, ops[j].0));
            }
        }
    }
    Ok(())
}

/// 倒序应用：位置大的先改，前面区间的 char index 不受后面编辑影响。
fn apply_ops(content: &str, ops: &[(usize, ResolvedOp)]) -> String {
    let mut rope = Rope::from_str(content);
    let mut ordered: Vec<&ResolvedOp> = ops.iter().map(|(_, op)| op).collect();
    ordered.sort_by_key(|op| std::cmp::Reverse(op.start()));
    for op in ordered {
        match op {
            ResolvedOp::Replace { range, new } => {
                // ropey 的 remove/insert 为 in-place 修改（返回 ()）——与 spec 假定
                // 的“返回新 rope”不同，以 crate 实际 API 为准。倒序应用保证前面
                // 区间的 char index 不受后面编辑影响，in-place 同样安全。
                let start = range.start;
                rope.remove(range.clone());
                rope.insert(start, new.as_str());
            }
            ResolvedOp::Insert { pos, text } => {
                rope.insert(*pos, text.as_str());
            }
        }
    }
    rope.to_string()
}

// ─────────────────────────────────────────────────────────────
// 执行结果模型
// ─────────────────────────────────────────────────────────────

struct HunkReport {
    index: usize,
    kind: String,
    status: String,
    tier: Option<u8>,
    score: Option<f32>,
    line_range: Option<(usize, usize)>,
    note: Option<String>,
    code: Option<String>,
    detail: Option<String>,
    candidates: Option<Vec<Candidate>>,
}

struct FileOutcome {
    edited: Option<String>,
    new_hash: Option<String>,
    diff: String,
    reports: Vec<HunkReport>,
    code: Option<String>,
    message: Option<String>,
    notes: Vec<String>,
}

/// 应用语义：strict = 全事务（任一 hunk 失败零改动）；partial = 成功的 hunk
/// 落盘、失败的报详情，模型用 new_hash 续接只重发失败项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Strict,
    Partial,
}

/// 核心纯逻辑：同一份未修改快照上定位全部 hunk → 重叠检测 → 倒序应用。
/// strict：任一 hunk 失败 → 整体拒绝（edited = None）；
/// partial：成功 hunk 应用（edited = Some + code = 首个失败码），报告含全部详情。
fn run_edit(content: &str, hunks: &[Hunk], notes: Vec<String>, mode: Mode) -> FileOutcome {
    let view = FileView::new(content);
    let mut reports: Vec<HunkReport> = Vec::with_capacity(hunks.len());
    let mut located: Vec<Option<Located>> = Vec::with_capacity(hunks.len());
    let mut first_code: Option<String> = None;

    for (i, hunk) in hunks.iter().enumerate() {
        match locate_hunk(&view, hunk) {
            Ok(loc) => {
                let line_range = (loc.win_lines > 0).then(|| (loc.start_line + 1, loc.start_line + loc.win_lines));
                reports.push(HunkReport {
                    index: i,
                    kind: hunk.kind_name().to_string(),
                    status: "ok".to_string(),
                    tier: Some(loc.tier),
                    score: Some(loc.score),
                    line_range,
                    note: Some(loc.note.clone()),
                    code: None,
                    detail: None,
                    candidates: None,
                });
                located.push(Some(loc));
            }
            Err(e) => {
                let (code, detail, candidates) = match e {
                    LocateError::NoMatch { candidates, detail } => {
                        ("NO_MATCH".to_string(), detail, Some(candidates))
                    }
                    LocateError::Ambiguous { candidates, detail } => {
                        ("AMBIGUOUS_MATCH".to_string(), detail, Some(candidates))
                    }
                    LocateError::Underspecified => (
                        "UNDERSPECIFIED".to_string(),
                        "old/anchor and contexts are all empty; use insert_after / insert_before / prepend_file / append_file instead"
                            .to_string(),
                        None,
                    ),
                };
                if first_code.is_none() {
                    first_code = Some(code.clone());
                }
                reports.push(HunkReport {
                    index: i,
                    kind: hunk.kind_name().to_string(),
                    status: "error".to_string(),
                    tier: None,
                    score: None,
                    line_range: None,
                    note: None,
                    code: Some(code),
                    detail: Some(detail),
                    candidates,
                });
                located.push(None);
            }
        }
    }

    if let Some(code) = &first_code {
        let failed = reports.iter().filter(|r| r.status == "error").count();
        match mode {
            Mode::Strict => {
                let message = format!(
                    "{failed} of {} hunk(s) failed to locate; all-or-nothing: no changes written",
                    hunks.len()
                );
                return FileOutcome {
                    edited: None,
                    new_hash: None,
                    diff: String::new(),
                    reports,
                    code: Some(code.clone()),
                    message: Some(message),
                    notes,
                };
            }
            Mode::Partial => {
                // 只应用定位成功的 hunk（仍做重叠检测与单快照倒序应用）。
                let mut ops: Vec<(usize, ResolvedOp)> = Vec::with_capacity(hunks.len());
                for (i, (hunk, loc)) in hunks.iter().zip(located.iter()).enumerate() {
                    if let Some(loc) = loc {
                        ops.push((i, resolve(&view, hunk, loc)));
                    }
                }
                if ops.is_empty() {
                    return FileOutcome {
                        edited: None,
                        new_hash: None,
                        diff: String::new(),
                        reports,
                        code: Some(code.clone()),
                        message: Some(format!("{failed}/{failed} hunk(s) failed; nothing applied")),
                        notes,
                    };
                }
                if let Err((a, b)) = check_overlap(&ops) {
                    return FileOutcome {
                        edited: None,
                        new_hash: None,
                        diff: String::new(),
                        reports,
                        code: Some("OVERLAPPING_HUNKS".to_string()),
                        message: Some(format!(
                            "successful hunk {a} and hunk {b} resolve to overlapping ranges; no changes written"
                        )),
                        notes,
                    };
                }
                let edited = apply_ops(content, &ops);
                let applied = ops.len();
                return FileOutcome {
                    diff: unified_diff(content, &edited, "file"),
                    new_hash: Some(content_hash(&edited)),
                    edited: Some(edited),
                    reports,
                    code: Some(code.clone()),
                    message: Some(format!(
                        "{applied} hunk(s) applied, {failed} failed — re-send ONLY the failed hunks with the returned new_hash as expected_hash"
                    )),
                    notes,
                };
            }
        }
    }

    let mut ops: Vec<(usize, ResolvedOp)> = Vec::with_capacity(hunks.len());
    for (i, (hunk, loc)) in hunks.iter().zip(located.iter()).enumerate() {
        ops.push((i, resolve(&view, hunk, loc.as_ref().unwrap())));
    }
    if let Err((a, b)) = check_overlap(&ops) {
        return FileOutcome {
            edited: None,
            new_hash: None,
            diff: String::new(),
            reports,
            code: Some("OVERLAPPING_HUNKS".to_string()),
            message: Some(format!(
                "hunk {a} and hunk {b} resolve to overlapping ranges; no changes written"
            )),
            notes,
        };
    }

    let edited = apply_ops(content, &ops);
    FileOutcome {
        diff: unified_diff(content, &edited, "file"),
        new_hash: Some(content_hash(&edited)),
        edited: Some(edited),
        reports,
        code: None,
        message: None,
        notes,
    }
}

// ─────────────────────────────────────────────────────────────
// 渲染
// ─────────────────────────────────────────────────────────────



fn render_text(path: &str, outcome: &FileOutcome) -> String {
    let mut out = String::new();
    match (&outcome.edited, &outcome.code) {
        (Some(_), None) => {
            // [OK] 全部成功
            let n = outcome.reports.len();
            let hash8 = outcome
                .new_hash
                .as_deref()
                .map(|h| &h[..h.len().min(8)])
                .unwrap_or("");
            out.push_str(&format!(
                "[OK] edit_file_v2 {path}\n  {n}/{n} hunks applied (new_hash {hash8})\n"
            ));
            for r in &outcome.reports {
                out.push_str(&format!("  {}\n", hunk_ok_line(r)));
            }
        }
        (Some(_), Some(code)) => {
            // [PARTIAL] 成功落盘 + 失败报告 + 续接指引
            let applied = outcome.reports.iter().filter(|r| r.status == "ok").count();
            let total = outcome.reports.len();
            let hash8 = outcome
                .new_hash
                .as_deref()
                .map(|h| &h[..h.len().min(8)])
                .unwrap_or("");
            out.push_str(&format!(
                "[PARTIAL] edit_file_v2 {path}\n  applied {applied}/{total} hunks (new_hash {hash8})\n"
            ));
            for r in &outcome.reports {
                if r.status == "ok" {
                    out.push_str(&format!("  {}\n", hunk_ok_line(r)));
                }
            }
            for r in &outcome.reports {
                if r.status != "ok" {
                    render_hunk_error(&mut out, r);
                }
            }
            out.push_str(&format!(
                "  next: re-send ONLY the failed hunks with \"expected_hash\": \"{}\" — already-applied hunks will conflict if repeated\n",
                outcome.new_hash.as_deref().unwrap_or_default()
            ));
            let _ = code;
        }
        (None, Some(code)) => {
            // [ERROR] 零改动 + 全部失败详情
            out.push_str(&format!(
                "[ERROR] edit_file_v2 {path}\n  no changes written ({code})\n"
            ));
            if let Some(m) = &outcome.message {
                out.push_str(&format!("  {m}\n"));
            }
            for r in &outcome.reports {
                if r.status == "ok" {
                    out.push_str(&format!(
                        "  hunk{} {}: ok (not applied — all-or-nothing)\n",
                        r.index, r.kind
                    ));
                } else {
                    render_hunk_error(&mut out, r);
                }
            }
        }
        (None, None) => {
            out.push_str(&format!("[ERROR] edit_file_v2 {path}\n  internal: no result\n"));
        }
    }
    for n in &outcome.notes {
        out.push_str(&format!("  note: {n}\n"));
    }
    out
}

fn hunk_ok_line(r: &HunkReport) -> String {
    let loc = match r.line_range {
        Some((a, b)) if a == b => format!("L{a}"),
        Some((a, b)) => format!("L{a}-L{b}"),
        None => "file".to_string(),
    };
    let tier = r.tier.map(|t| format!("tier{t}")).unwrap_or_default();
    let score = r
        .score
        .filter(|_| r.tier == Some(3))
        .map(|s| format!(" score {s:.2}"))
        .unwrap_or_default();
    let note = r.note.as_deref().unwrap_or("");
    format!(
        "hunk{} {}: {loc} {tier}{score} {note}",
        r.index, r.kind
    )
}

/// 单个失败 hunk 的详细渲染：错误码 + 为什么失败 + 候选与 -/+ 对照 + 下一步。
fn render_hunk_error(out: &mut String, r: &HunkReport) {
    out.push_str(&format!(
        "  hunk{} {}: {} — {}\n",
        r.index,
        r.kind,
        r.code.as_deref().unwrap_or("error"),
        r.detail.as_deref().unwrap_or("")
    ));
    if let Some(cands) = &r.candidates {
        for (i, c) in cands.iter().enumerate() {
            out.push_str(&format!(
                "    candidate #{} L{}-L{} score {:.2} tier{}\n",
                i + 1,
                c.line_range.0,
                c.line_range.1,
                c.score,
                c.tier
            ));
            if !c.diff.is_empty() {
                out.push_str(&c.diff);
                out.push('\n');
            }
        }
    }
}

fn truncate_content(content: &str) -> String {
    if content.len() <= CONTENT_CAP {
        return content.to_string();
    }
    let cut = content.floor_char_boundary(CONTENT_CAP);
    format!(
        "{}…[truncated: {} bytes total]",
        &content[..cut],
        content.len()
    )
}

fn hint_for(code: &str) -> Option<&'static str> {
    match code {
        "NO_MATCH" => Some("Refine 'old'/context from the candidates, or re-locate with rg; line-number edits are served by edit_file."),
        "AMBIGUOUS_MATCH" => Some("Add context_before/context_after to disambiguate the repeated occurrence."),
        "OVERLAPPING_HUNKS" => Some("Split or merge hunks so their resolved ranges do not overlap."),
        "UNDERSPECIFIED" => Some("Provide context, or use insert_after / insert_before / prepend_file / append_file."),
        "HASH_MISMATCH" => Some("Re-read the file with read_file and retry with the fresh hash."),
        "FILE_ALREADY_EXISTS" => Some("The file already exists; drop expected_hash to edit it, or choose a new path to create."),
        _ => None,
    }
}

// ─────────────────────────────────────────────────────────────
// 执行入口
// ─────────────────────────────────────────────────────────────

fn exec_edit_file_v2(args: &serde_json::Value) -> ToolResult {
    let fail = |code: &str, message: String, retryable: bool, hint: Option<&str>| {
        ToolResult::error_data(
            code,
            message.clone(),
            retryable,
            hint.map(str::to_string),
            json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "code": code,
                "message": message,
            }),
        )
    };

    let raw_path = match args.get("path").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        Some(p) => p,
        None => {
            return fail("PARSE_ERROR", "edit_file_v2: missing 'path'".into(), false, None);
        }
    };
    let path = crate::resolve_workspace_path(raw_path);
    let expected_hash = args
        .get("expected_hash")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let mut notes: Vec<String> = Vec::new();
    let hunks = match args.get("hunks").and_then(|v| v.as_array()) {
        Some(arr) if !arr.is_empty() => arr,
        _ => {
            return fail(
                "PARSE_ERROR",
                "edit_file_v2: missing non-empty 'hunks' array".into(),
                false,
                None,
            );
        }
    };
    if hunks.len() > MAX_HUNKS {
        return fail(
            "PARSE_ERROR",
            format!("edit_file_v2: too many hunks ({} > {MAX_HUNKS})", hunks.len()),
            false,
            None,
        );
    }
    let mut parsed: Vec<Hunk> = Vec::with_capacity(hunks.len());
    for (i, h) in hunks.iter().enumerate() {
        match Hunk::parse(h, &mut notes) {
            Ok(hunk) => parsed.push(hunk),
            Err(e) => {
                return fail(
                    "PARSE_ERROR",
                    format!("edit_file_v2: hunks[{i}]: {e}"),
                    false,
                    None,
                );
            }
        }
    }

    // ── 应用语义（参数校验先于任何 IO）──
    let mode = match args.get("mode").and_then(|v| v.as_str()).unwrap_or("strict") {
        "strict" => Mode::Strict,
        "partial" => Mode::Partial,
        other => {
            return fail(
                "PARSE_ERROR",
                format!("edit_file_v2: unknown mode '{other}' (expected 'strict' or 'partial')"),
                false,
                None,
            );
        }
    };

    // ── 读文件 ──
    let raw = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && expected_hash.is_none() => {
            // 创建新文件路径：空内容进入正常管线（仅 prepend/append 可定位）。
            Vec::new()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return fail(
                "FILE_NOT_FOUND",
                format!("edit_file_v2: {raw_path}: file not found (expected_hash provided but nothing to verify against)"),
                false,
                None,
            );
        }
        Err(e) => {
            return fail(
                "READ_FAILED",
                format!("edit_file_v2: cannot read {raw_path}: {e}"),
                false,
                None,
            );
        }
    };
    let raw = match String::from_utf8(raw) {
        Ok(s) => s,
        Err(_) => {
            return fail(
                "NOT_UTF8_TEXT",
                format!("edit_file_v2: {raw_path} is not valid UTF-8 text (treated as binary)"),
                false,
                None,
            );
        }
    };
    // LF 规范视图（与 read_file 的展示/hash 同视图）
    let (content, was_crlf) = normalize_newlines(&raw);

    // ── hash gate ──
    match &expected_hash {
        Some(h) => {
            let current = content_hash(&content);
            if &current != h {
                let cc = truncate_content(&content);
                let text = format!(
                    "[ERROR] edit_file_v2 {raw_path}\n  HASH_MISMATCH: content changed since the referenced read\n  current_hash: {current}\n  current content:\n{cc}\n"
                );
                let data = json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "code": "HASH_MISMATCH",
                    "path": raw_path,
                    "message": "File content changed since the referenced read",
                    "expected_hash": h,
                    "current_hash": current,
                    "current_content": cc,
                });
                return ToolResult::error_data(
                    "HASH_MISMATCH",
                    text,
                    true,
                    hint_for("HASH_MISMATCH").map(str::to_string),
                    data,
                );
            }
        }
        None => {
            // 创建路径：文件必须不存在（含空文件——存在即拒绝"创建"意图）。
            if path_exists(&path) {
                return fail(
                    "FILE_ALREADY_EXISTS",
                    format!(
                        "edit_file_v2: {raw_path} already exists; expected_hash is None (create-only path). Provide the current hash to edit it."
                    ),
                    true,
                    hint_for("FILE_ALREADY_EXISTS"),
                );
            }
        }
    }

    // ── 核心执行 ──
    let outcome = run_edit(&content, &parsed, notes, mode);

    match outcome.edited {
        None => {
            let code = outcome.code.as_deref().unwrap_or("EDIT_REJECTED");
            let text = render_text(raw_path, &outcome);
            let data = json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "path": raw_path,
                "code": code,
                "message": outcome.message,
                "hunks": outcome.reports.iter().map(|r| hunk_report_json(r)).collect::<Vec<_>>(),
            });
            ToolResult::error_data(
                code,
                text,
                true,
                hint_for(code).map(str::to_string),
                data,
            )
        }
        Some(ref edited) => {
            let write_content = if was_crlf {
                edited.replace('\n', "\r\n")
            } else {
                edited.clone()
            };
            if let Err(e) = atomic_write(&path, &write_content) {
                return fail(
                    "WRITE_FAILED",
                    format!("edit_file_v2: atomic write failed for {raw_path}: {e} — the file on disk was NOT modified"),
                    false,
                    None,
                );
            }
            let new_hash = outcome.new_hash.as_deref().unwrap_or_default();
            let text = render_text(raw_path, &outcome);
            let applied = outcome.reports.iter().filter(|r| r.status == "ok").count();
            let total = outcome.reports.len();
            let status = if outcome.code.is_some() { "partial" } else { "ok" };
            let mut data = json!({
                "timeis": crate::now_utc8(),
                "status": status,
                "path": raw_path,
                "new_hash": new_hash,
                "applied_hunks": applied,
                "total_hunks": total,
                "notes": outcome.notes,
                "hunks": outcome.reports.iter().map(|r| hunk_report_json(r)).collect::<Vec<_>>(),
            });
            if let Some(code) = &outcome.code {
                data["code"] = json!(code);
            }
            let mut result = ToolResult::ok_data(data, text);
            if !outcome.diff.is_empty() {
                result = result.with_diff(outcome.diff);
            }
            result
        }
    }
}

fn path_exists(path: &str) -> bool {
    std::path::Path::new(path).exists()
}

fn hunk_report_json(r: &HunkReport) -> Value {
    let mut v = json!({
        "index": r.index,
        "kind": r.kind,
        "status": r.status,
    });
    if let Some(t) = r.tier {
        v["tier"] = json!(t);
    }
    if let Some(s) = r.score {
        v["score"] = json!(s);
    }
    if let Some((a, b)) = r.line_range {
        v["line_range"] = json!([a, b]);
    }
    if let Some(n) = &r.note {
        v["note"] = json!(n);
    }
    if let Some(c) = &r.code {
        v["code"] = json!(c);
    }
    if let Some(d) = &r.detail {
        v["detail"] = json!(d);
    }
    if let Some(cands) = &r.candidates {
        v["candidates"] = json!(cands
            .iter()
            .map(|c| json!({
                "line_range": [c.line_range.0, c.line_range.1],
                "snippet": c.snippet,
                "score": c.score,
                "tier": c.tier,
                "diff": c.diff,
            }))
            .collect::<Vec<_>>());
    }
    v
}

fn handle_edit_file_v2(ctx: ToolCallCtx) -> ToolResult {
    exec_edit_file_v2(&ctx.args)
}

pub fn register(mgr: &mut ToolManager) {
    mgr.register_with_placement(
        ToolHandler {
            key: "edit_file_v2".to_string(),
            description: concat!(
                "Structured hunk editor (v2, complements edit_file). Kind-tagged hunks: ",
                "replace (old/new/context_before/context_after), insert_after / insert_before (anchor/new), ",
                "prepend_file / append_file (new). Four-tier matching: exact → indent-shape → similarity scoring ",
                "with margin (auto-applies only when the best candidate clearly wins) → top-3 candidates on failure. ",
                "All hunks are located on ONE unchanged snapshot and applied all-or-nothing; overlapping hunks are rejected. ",
                "Newline rule: the replaced range includes the trailing newline ONLY if 'old' ends with '\\n' — ",
                "normally give both old and new WITHOUT the trailing newline (it stays in place); ",
                "end 'old' with '\\n' only when you want to delete/replace the line break itself. ",
                "No line numbers, no replace_all, no regex — use edit_file for those. ",
                "expected_hash (from read_file) guards stale content; on mismatch the current content is returned. ",
                "On success returns new_hash to chain into the next call without re-reading. ",
                "mode 'partial' applies successful hunks and reports failures in detail, so you only re-send the failed ones "
                ,"(with expected_hash = returned new_hash) instead of the whole batch; mode 'strict' (default) is all-or-nothing."
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Target file path"},
                    "expected_hash": {
                        "type": "string",
                        "description": "Optional; hash returned by read_file. Omit only when creating a new file."
                    },
                    "hunks": {
                        "type": "array",
                        "description": "Edit hunks. Each hunk: {\"kind\":\"replace\",\"old\":…,\"new\":…,\"context_before\":…,\"context_after\":…} | {\"kind\":\"insert_after\"|\"insert_before\",\"anchor\":…,\"new\":…} | {\"kind\":\"prepend_file\"|\"append_file\",\"new\":…}. Empty 'old' with contexts = pure insert."
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["strict", "partial"],
                        "default": "strict",
                        "description": "strict = all-or-nothing; partial = successful hunks are applied and written, failed hunks are reported in detail — retry only the failed hunks with expected_hash = returned new_hash."
                    }
                },
                "required": ["path", "hunks"],
                "additionalProperties": false
            }),
            handler: handle_edit_file_v2,
            risk: ToolRisk::Write,
            category: crate::permission::ToolCategory::Write,
            default_timeout: std::time::Duration::from_secs(60),
        },
        ToolPlacement::Workspace,
    );
}

// ─────────────────────────────────────────────────────────────
// 测试
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn rp(old: &str, new: &str) -> Hunk {
        Hunk::Replace {
            old: old.to_string(),
            new: new.to_string(),
            context_before: String::new(),
            context_after: String::new(),
        }
    }

    fn rp_ctx(old: &str, new: &str, before: &str, after: &str) -> Hunk {
        Hunk::Replace {
            old: old.to_string(),
            new: new.to_string(),
            context_before: before.to_string(),
            context_after: after.to_string(),
        }
    }

    fn edit(content: &str, hunks: &[Hunk]) -> FileOutcome {
        run_edit(content, hunks, Vec::new(), Mode::Strict)
    }

    fn edit_partial(content: &str, hunks: &[Hunk]) -> FileOutcome {
        run_edit(content, hunks, Vec::new(), Mode::Partial)
    }

    fn err_code(o: &FileOutcome) -> &str {
        o.code.as_deref().unwrap_or("")
    }

    // 1. 精确匹配唯一命中
    #[test]
    fn exact_match_unique_hit() {
        let out = edit("a\nb\nc\n", &[rp("b", "B")]);
        assert_eq!(out.edited.as_deref(), Some("a\nB\nc\n"));
        assert_eq!(out.reports[0].tier, Some(1));
        assert_eq!(out.reports[0].line_range, Some((2, 2)));
    }

    // 2. 缩进基准不同 → Tier2 形状命中
    #[test]
    fn indent_shape_matches_on_tier2() {
        let content = "fn outer() {\n    fn inner() {\n        let x = 1;\n    }\n}\n";
        // 模型忘了 inner 体里多缩进两格：old 不带缩进，文件行带 8 空格
        let out = edit(content, &[rp("let x = 1;", "let x = 2;")]);
        assert_eq!(
            out.edited.as_deref(),
            Some("fn outer() {\n    fn inner() {\n        let x = 2;\n    }\n}\n")
        );
        assert_eq!(out.reports[0].tier, Some(2));
        assert_eq!(out.reports[0].note.as_deref(), Some("indent-shape"));
    }

    // 3. 细微出入 → Tier3 达标自动采纳
    #[test]
    fn tier3_auto_applies_when_score_and_margin_pass() {
        // 文件里是 "let foo = 1;"，模型记为 "let fo0 = 1;"（一字之差）；
        // 且忘了缩进。剥缩进后 ratio 0.92 ≥ 0.85，唯一高分候选 → Tier3 采纳。
        let content = "fn main() {\n    let foo = 1;\n    println!(\"{}\", foo);\n}\n";
        let out = edit(content, &[rp("let fo0 = 1;", "let foo = 1;")]);
        assert_eq!(
            out.edited.as_deref(),
            Some("fn main() {\n    let foo = 1;\n    println!(\"{}\", foo);\n}\n")
        );
        assert_eq!(out.reports[0].tier, Some(3));
        assert!(out.reports[0].score.unwrap() >= T3_THRESHOLD);
    }

    // 4. 双候选 margin 不足 → 拒绝
    #[test]
    fn tier3_rejects_when_margin_insufficient() {
        let content = "let alpha_value = 1;\nlet alpho_value = 1;\n";
        let out = edit(content, &[rp("let alphu_value = 1;", "let x = 1;")]);
        assert!(out.edited.is_none());
        assert_eq!(err_code(&out), "NO_MATCH");
        assert!(out.reports[0].candidates.as_ref().unwrap().len() >= 2);
    }

    // 5. 同一模式 ×3 + context_before 消歧
    #[test]
    fn context_before_disambiguates_repeated_pattern() {
        let content = "fn a() {\n    return Ok(());\n}\nfn b() {\n    return Ok(());\n}\nfn c() {\n    return Ok(());\n}\n";
        let out = edit(
            content,
            &[rp_ctx("return Ok(());", "return Err(());", "fn b() {", "")],
        );
        assert_eq!(
            out.edited.as_deref(),
            Some("fn a() {\n    return Ok(());\n}\nfn b() {\n    return Err(());\n}\nfn c() {\n    return Ok(());\n}\n")
        );
        // 行带缩进 → 精确不命中，Tier2 形状命中（3 处）→ context_before 消歧
        assert_eq!(out.reports[0].tier, Some(2));
    }

    // 6. 两 hunk 互不重叠，各自命中
    #[test]
    fn two_hunks_apply_independently() {
        let out = edit("a\nb\nc\nd\n", &[rp("a", "A"), rp("c", "C")]);
        assert_eq!(out.edited.as_deref(), Some("A\nb\nC\nd\n"));
        assert_eq!(out.reports.len(), 2);
    }

    // 7. 两 hunk 重叠 → 整体拒绝，零改动
    #[test]
    fn overlapping_hunks_rejected_atomically() {
        let content = "a\nb\nc\n";
        let before = content.to_string();
        // replace 窗口 [0,4)（"a\nb\nc" 不含尾换行），insert_before "c" 的
        // 插入点 = 行 2 行首 = char 4 ∈ [0,4) 边界…… 用更直白的严格内部冲突：
        // replace "a\nb\nc" 区间 [0,5)，insert_before "b" 的插入点 = 行 1 行首 = char 2 ∈ (0,5) 内部 → 冲突
        let out = edit(
            content,
            &[
                rp("a\nb\nc", "X"),
                Hunk::InsertBefore {
                    anchor: "b".into(),
                    new: "BB\n".into(),
                },
            ],
        );
        assert!(out.edited.is_none());
        assert_eq!(err_code(&out), "OVERLAPPING_HUNKS");
        assert_eq!(before, "a\nb\nc\n");
    }

    // 8. 空文件 PrependFile / AppendFile
    #[test]
    fn empty_file_prepend_and_append() {
        let out = edit("", &[Hunk::PrependFile { new: "hello\n".into() }]);
        assert_eq!(out.edited.as_deref(), Some("hello\n"));
        let out = edit("", &[Hunk::AppendFile { new: "world\n".into() }]);
        assert_eq!(out.edited.as_deref(), Some("world\n"));
    }

    // 9. 无结尾换行 AppendFile 沿用原约定（自动补换行）
    #[test]
    fn append_file_respects_missing_trailing_newline() {
        let out = edit("a\nb", &[Hunk::AppendFile { new: "c\n".into() }]);
        assert_eq!(out.edited.as_deref(), Some("a\nb\nc\n"));
    }

    // 10. CRLF 混入 → 归一化 + notes
    #[test]
    fn crlf_in_request_normalized_with_notes() {
        // 归一化在 Hunk::parse 层：CRLF → LF 并记 note，走真实解析路径。
        let mut notes = Vec::new();
        let hunk = Hunk::parse(
            &json!({"kind": "replace", "old": "a\r\nb", "new": "A\nB"}),
            &mut notes,
        )
        .unwrap();
        let out = run_edit("a\nb\n", &[hunk], notes, Mode::Strict);
        assert_eq!(out.edited.as_deref(), Some("A\nB\n"));
        assert_eq!(out.reports[0].status, "ok");
        assert!(out.notes.iter().any(|n| n.contains("CRLF")));
    }

    // 14. old 空 + context 全空 → Underspecified
    #[test]
    fn empty_old_without_context_is_underspecified() {
        let out = edit("a\nb\n", &[rp("", "X\n")]);
        assert_eq!(err_code(&out), "UNDERSPECIFIED");
        assert!(out.edited.is_none());
    }

    // 纯插入：context_before 定位
    #[test]
    fn pure_insert_with_context_before() {
        let out = edit(
            "a\nb\nc\n",
            &[rp_ctx("", "X\n", "b", "")],
        );
        assert_eq!(out.edited.as_deref(), Some("a\nb\nX\nc\n"));
    }

    // 纯插入：双 context 交界
    #[test]
    fn pure_insert_between_contexts() {
        let out = edit("a\nb\nc\n", &[rp_ctx("", "X\n", "b", "c")]);
        assert_eq!(out.edited.as_deref(), Some("a\nb\nX\nc\n"));
    }

    // 15. insert_after / insert_before 锚点
    #[test]
    fn insert_after_and_before_anchors() {
        let out = edit(
            "a\nb\nc\n",
            &[Hunk::InsertAfter {
                anchor: "b".into(),
                new: "b2\n".into(),
            }],
        );
        assert_eq!(out.edited.as_deref(), Some("a\nb\nb2\nc\n"));
        let out = edit(
            "a\nb\nc\n",
            &[Hunk::InsertBefore {
                anchor: "c".into(),
                new: "c0\n".into(),
            }],
        );
        assert_eq!(out.edited.as_deref(), Some("a\nb\nc0\nc\n"));
    }

    // 15a. 同一位置的两次插入 → 重叠拒绝（顺序无歧义才允许）
    #[test]
    fn two_inserts_at_same_position_rejected() {
        let out = edit(
            "a\nb\nc\n",
            &[
                Hunk::InsertAfter {
                    anchor: "b".into(),
                    new: "b2\n".into(),
                },
                Hunk::InsertBefore {
                    anchor: "c".into(),
                    new: "c0\n".into(),
                },
            ],
        );
        assert!(out.edited.is_none());
        assert_eq!(err_code(&out), "OVERLAPPING_HUNKS");
    }

    // 15b. 锚点多处命中 → Ambiguous
    #[test]
    fn ambiguous_anchor_rejected() {
        let out = edit(
            "a\nx\nb\nx\n",
            &[Hunk::InsertAfter {
                anchor: "x".into(),
                new: "y\n".into(),
            }],
        );
        assert_eq!(err_code(&out), "AMBIGUOUS_MATCH");
        assert!(out.edited.is_none());
    }

    // 16. CRLF 文件写回保持 CRLF（execute 层测试）
    #[test]
    fn crlf_file_roundtrip_via_execute() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crlf.txt");
        std::fs::write(&path, "a\r\nb\r\n").unwrap();
        let hash = content_hash("a\nb\n"); // LF 视图 hash
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "expected_hash": hash,
            "hunks": [{"kind": "replace", "old": "b", "new": "c"}],
        }));
        assert!(result.is_success(), "model text: {}", result.model.text);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\r\nc\r\n");
    }

    // 11. expected_hash 失配 → 拒绝 + current_content
    #[test]
    fn hash_mismatch_rejected_with_current_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a\nb\n").unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "expected_hash": "deadbeef",
            "hunks": [{"kind": "replace", "old": "a", "new": "A"}],
        }));
        assert!(!result.is_success());
        assert_eq!(result.data["code"], "HASH_MISMATCH");
        assert!(result.data["current_content"].as_str().unwrap().contains("a\nb\n"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\n");
    }

    // 12. expected_hash=None 但文件已存在 → FileAlreadyExists
    #[test]
    fn create_path_rejects_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "x\n").unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "hunks": [{"kind": "prepend_file", "new": "y\n"}],
        }));
        assert!(!result.is_success());
        assert_eq!(result.data["code"], "FILE_ALREADY_EXISTS");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x\n");
    }

    // 12b. 创建新文件成功
    #[test]
    fn create_new_file_with_prepend() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "hunks": [{"kind": "prepend_file", "new": "hello\n"}],
        }));
        assert!(result.is_success(), "model text: {}", result.model.text);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
    }

    // 13. 非 UTF-8 → NotUtf8Text
    #[test]
    fn binary_file_rejected_as_not_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bin.dat");
        std::fs::write(&path, [0xffu8, 0xfe, 0x00, 0x01]).unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "expected_hash": "whatever",
            "hunks": [{"kind": "replace", "old": "x", "new": "y"}],
        }));
        assert!(!result.is_success());
        assert_eq!(result.data["code"], "NOT_UTF8_TEXT");
    }

    // 17. new_hash 续接
    #[test]
    fn new_hash_chains_into_next_call() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a\nb\n").unwrap();
        let r1 = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "expected_hash": content_hash("a\nb\n"),
            "hunks": [{"kind": "replace", "old": "a", "new": "A"}],
        }));
        assert!(r1.is_success(), "model text: {}", r1.model.text);
        let new_hash = r1.data["new_hash"].as_str().unwrap().to_string();
        assert!(!new_hash.is_empty());
        let r2 = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "expected_hash": new_hash,
            "hunks": [{"kind": "replace", "old": "b", "new": "B"}],
        }));
        assert!(r2.is_success(), "model text: {}", r2.model.text);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "A\nB\n");
    }

    // 18. 多 hunk 失败时报告全部失败详情
    #[test]
    fn all_hunk_failures_reported_at_once() {
        let out = edit(
            "a\nb\nc\n",
            &[rp("a", "A"), rp("zzz", "Z"), rp("", "P")],
        );
        assert!(out.edited.is_none());
        assert_eq!(out.reports.len(), 3);
        assert_eq!(out.reports[0].status, "ok");
        assert_eq!(out.reports[1].status, "error");
        assert_eq!(out.reports[2].status, "error");
        assert_eq!(err_code(&out), "NO_MATCH");
    }

    // 19. 未知 kind → 拒绝
    #[test]
    fn unknown_hunk_kind_rejected() {
        let mut notes = Vec::new();
        let err = Hunk::parse(&json!({"kind": "magic", "new": "x"}), &mut notes).unwrap_err();
        assert!(err.contains("unknown hunk kind"));
    }



    // 20. 三 hunk 倒序应用（文件头/中/尾）
    #[test]
    fn three_hunks_apply_head_mid_tail() {
        let out = edit(
            "a\nb\nc\nd\ne\n",
            // old 无尾换行 → 区间不含尾换行 → new 对称地也不带
            &[rp("a", "A"), rp("c", "C"), rp("e", "E")],
        );
        assert_eq!(out.edited.as_deref(), Some("A\nb\nC\nd\nE\n"));
    }

    // 插入点与替换区间边界相邻（不冲突）→ 都应用
    #[test]
    fn insert_at_replace_boundary_is_allowed() {
        let out = edit(
            "a\nb\nc\n",
            &[
                rp("a", "A"),
                Hunk::InsertAfter {
                    anchor: "a".into(),
                    new: "a2\n".into(),
                },
            ],
        );
        // replace "a"（区间 [0,1)，不含尾换行）与 insert_after "a"（插入点 char 2，
        // 在区间边界）不相交 → 都应用；倒序应用结果确定。
        assert_eq!(out.edited.as_deref(), Some("A\na2\nb\nc\n"));
    }

    // 21. partial 模式：成功 hunk 应用、失败 hunk 只报告
    #[test]
    fn partial_mode_applies_successful_hunks() {
        let out = edit_partial(
            "a\nb\nc\n",
            &[rp("a", "A"), rp("zzz", "Z"), rp("c", "C")],
        );
        // 成功两处已应用，失败一处报错；code 为第一个失败码
        assert_eq!(out.edited.as_deref(), Some("A\nb\nC\n"));
        assert_eq!(out.reports.len(), 3);
        assert_eq!(out.reports[0].status, "ok");
        assert_eq!(out.reports[1].status, "error");
        assert_eq!(out.reports[2].status, "ok");
        assert_eq!(out.code.as_deref(), Some("NO_MATCH"));
        assert!(out.new_hash.is_some());
        // 渲染文本含续接指引
        let text = render_text("f.txt", &out);
        assert!(text.contains("[PARTIAL]"), "text: {text}");
        assert!(text.contains("re-send ONLY the failed hunks"), "text: {text}");
    }

    // 22. partial 模式：全部失败 → 零改动（不写空结果）
    #[test]
    fn partial_mode_all_failed_writes_nothing() {
        let out = edit_partial("a\nb\nc\n", &[rp("zzz", "Z")]);
        assert!(out.edited.is_none());
        assert_eq!(out.code.as_deref(), Some("NO_MATCH"));
    }

    // 23. partial 模式：成功 hunk 之间重叠 → 仍拒绝
    #[test]
    fn partial_mode_overlap_among_successful_still_rejected() {
        let out = edit_partial(
            "a\nb\nc\n",
            &[
                rp("a\nb", "X"),
                Hunk::InsertBefore {
                    anchor: "b".into(),
                    new: "BB\n".into(),
                },
                rp("zzz", "Z"), // 失败的 hunk 不参与重叠检测
            ],
        );
        assert!(out.edited.is_none());
        assert_eq!(out.code.as_deref(), Some("OVERLAPPING_HUNKS"));
    }

    // 24. NO_MATCH 诊断：候选带 -/+ 对照
    #[test]
    fn no_match_candidates_carry_pattern_diff() {
        let out = edit("let alpha_value = 1;\nlet alpho_value = 1;\n", &[rp("let alphu_value = 1;", "let x = 1;")]);
        let cands = out.reports[0].candidates.as_ref().unwrap();
        assert!(!cands.is_empty());
        let diff = &cands[0].diff;
        assert!(diff.contains("- let alphu_value = 1;"), "diff: {diff}");
        assert!(diff.contains("+ let alpha_value = 1;") || diff.contains("+ let alpho_value = 1;"), "diff: {diff}");
    }

    // 25. NO_MATCH 诊断：margin 不足时说明原因
    #[test]
    fn no_match_detail_explains_margin_shortfall() {
        let out = edit("let alpha_value = 1;\nlet alpho_value = 1;\n", &[rp("let alphu_value = 1;", "let x = 1;")]);
        let detail = out.reports[0].detail.as_deref().unwrap_or("");
        assert!(detail.contains("margin"), "detail: {detail}");
        assert!(detail.contains("context_before"), "detail: {detail}");
    }

    // 26. NO_MATCH 诊断：完全不像时说明原因
    #[test]
    fn no_match_detail_explains_total_mismatch() {
        let out = edit("a\nb\nc\n", &[rp("zzzz", "x")]);
        let detail = out.reports[0].detail.as_deref().unwrap_or("");
        assert!(detail.contains("no window had any similarity"), "detail: {detail}");
    }

    // 27. partial 执行链：partial 落盘后 new_hash 续接只重发失败 hunk
    #[test]
    fn partial_then_resend_failed_hunk_via_execute() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        let r1 = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "expected_hash": content_hash("a\nb\nc\n"),
            "mode": "partial",
            "hunks": [
                {"kind": "replace", "old": "a", "new": "A"},
                {"kind": "replace", "old": "zzz", "new": "Z"},
                {"kind": "replace", "old": "c", "new": "C"}
            ],
        }));
        assert!(r1.is_success(), "model text: {}", r1.model.text);
        assert_eq!(r1.data["status"], "partial");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "A\nb\nC\n");
        let new_hash = r1.data["new_hash"].as_str().unwrap().to_string();
        // 只重发失败的 hunk
        let r2 = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "expected_hash": new_hash,
            "hunks": [{"kind": "replace", "old": "zzz", "new": "Z"}],
        }));
        assert!(!r2.is_success(), "expected NO_MATCH, model text: {}", r2.model.text);
        // 文件未被第二次调用改动
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "A\nb\nC\n");
        // 修正后成功
        let r3 = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "expected_hash": new_hash,
            "hunks": [{"kind": "insert_after", "anchor": "b", "new": "b2\n"}],
        }));
        assert!(r3.is_success(), "model text: {}", r3.model.text);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "A\nb\nb2\nC\n");
    }

    // 28. 未知 mode → PARSE_ERROR
    #[test]
    fn unknown_mode_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a\n").unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "mode": "magic",
            "hunks": [{"kind": "replace", "old": "a", "new": "b"}],
        }));
        assert!(!result.is_success());
        assert_eq!(result.data["code"], "PARSE_ERROR");
    }
}
