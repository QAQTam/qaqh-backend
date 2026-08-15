//! edit_file_v2 — 差分实现的第二代编辑工具。
//!
//! 定位（见 docs/nextdev/PLAN-EDIT-FILE-V2.md）：**只实现 v1 没有的部分**。
//! v1（edit_file：行号定位、replace_all、regex、每 op 独立事务与诊断体系）
//! 已下线删除——v2 是唯一编辑入口。
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

/// Tier3 相似度采纳阈值（spec 建议 0.85；0.90 过度保守 → 拒绝多 → 模型重试多）。
const T3_THRESHOLD: f32 = 0.85;
/// Tier3 胜出边际：最高分必须显著超过次高分才自动采纳（spec 建议 0.10）。
const T3_MARGIN: f32 = 0.10;
/// 单次调用 hunk 数上限。
const MAX_HUNKS: usize = 64;
/// HashMismatch 时回传内容的截断字节数。
const CONTENT_CAP: usize = 64 * 1024;

/// 读路径（空 hunks）预算，与 read_file 对齐。
const READ_MAX_LINES: usize = 400;
const READ_MAX_CHARS: usize = 24_000;
const READ_MAX_CONTEXT: usize = 100;/// 失败候选列表条数上限。
const CANDIDATE_MAX: usize = 3;
/// 候选 snippet 展示长度（字符）。
const SNIPPET_MAX: usize = 120;

// ─────────────────────────────────────────────────────────────
// 请求模型
// ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Hunk {
    /// 替换；old 为空字符串 = 纯插入（context_before/after 至少一侧非空）。
    /// `replace_all=true` 时替换 old 的全部**精确**匹配位置（仅 Tier1 生效，
    /// 不降级模糊匹配——模糊应用于多位置风险不可控）。
    Replace {
        old: String,
        new: String,
        context_before: String,
        context_after: String,
        replace_all: bool,
        /// 宽松行号提示（1-based，±10 窗口）：四层定位全失败后，仅在
        /// [hint-10, hint+10] 内重试 Tier1 精确匹配，唯一命中才应用。
        /// 不触碰默认路径（未提供时行为与原来完全一致）。
        hint_line: Option<usize>,
    },
    /// 整文件覆盖（等价 write 的 content 语义）：不要求 `old`，恒成功；
    /// **独占**——必须单独调用，与其它 hunk 混用报 OVERWRITE_EXCLUSIVE。
    Overwrite { new: String },
    InsertAfter {
        anchor: String,
        new: String,
        hint_line: Option<usize>,
    },
    InsertBefore {
        anchor: String,
        new: String,
        hint_line: Option<usize>,
    },
    PrependFile { new: String },
    AppendFile { new: String },
    /// 行内替换（sed `s///` 语义）：anchor 定位行窗口后，**仅在窗口内**做子串/
    /// 正则替换——不跨行、不改行结构。`replace_all=false` 只替换第一处（按行序）；
    /// `regex=true` 时 `old` 为正则（regex crate 语法，大小写敏感）。
    ReplaceInline {
        anchor: String,
        old: String,
        new: String,
        replace_all: bool,
        regex: bool,
        hint_line: Option<usize>,
    },
}

impl Hunk {
    fn kind_name(&self) -> &'static str {
        match self {
            Hunk::Replace { .. } => "replace",
            Hunk::Overwrite { .. } => "overwrite",
            Hunk::InsertAfter { .. } => "insert_after",
            Hunk::InsertBefore { .. } => "insert_before",
            Hunk::PrependFile { .. } => "prepend_file",
            Hunk::AppendFile { .. } => "append_file",
            Hunk::ReplaceInline { .. } => "replace_inline",
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
                let replace_all = v
                    .get("replace_all")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false);
                Ok(Hunk::Replace {
                    old: norm(old, notes),
                    new: norm(new, notes),
                    context_before: norm(context_before, notes),
                    context_after: norm(context_after, notes),
                    replace_all,
                    hint_line: parse_hint_line(v),
                })
            }
            "overwrite" => {
                let new = v
                    .get("new")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| "overwrite hunk requires 'new'".to_string())?;
                // 整文件语义：忽略可能误传的 old/context（write 语义不看旧内容）。
                Ok(Hunk::Overwrite { new: norm(new, notes) })
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
                    Ok(Hunk::InsertAfter {
                        anchor,
                        new,
                        hint_line: parse_hint_line(v),
                    })
                } else {
                    Ok(Hunk::InsertBefore {
                        anchor,
                        new,
                        hint_line: parse_hint_line(v),
                    })
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
            "replace_inline" => {
                let anchor = v
                    .get("anchor")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "replace_inline hunk requires non-empty 'anchor'".to_string())?;
                let old = v
                    .get("old")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "replace_inline hunk requires non-empty 'old'".to_string())?;
                if old.contains('\n') {
                    return Err(
                        "replace_inline 'old' must be a single-line substring (use 'replace' for line-level edits)"
                            .to_string(),
                    );
                }
                let new = v
                    .get("new")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| "replace_inline hunk requires 'new'".to_string())?;
                let replace_all = v
                    .get("replace_all")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false);
                let regex = v.get("regex").and_then(|x| x.as_bool()).unwrap_or(false);
                Ok(Hunk::ReplaceInline {
                    anchor: norm(anchor, notes),
                    old: norm(old, notes),
                    new: norm(new, notes),
                    replace_all,
                    regex,
                    hint_line: parse_hint_line(v),
                })
            }
            other => Err(format!(
                "unknown hunk kind '{other}' (expected replace / overwrite / insert_after / insert_before / prepend_file / append_file / replace_inline)"
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
    /// 每行行首**字节**偏移；len = lines.len() + 1，末项 = content.len()（str 切片用）。
    byte_starts: Vec<usize>,
    /// 每行行首 **char** index；len = lines.len() + 1，末项 = 总字符数（ropey 操作用）。
    ///
    /// ⚠ 二者不可混用：`char_indices()` 返回的是字节偏移，此前误当 char 索引存入，
    /// 中文文件（字节数 > 字符数）下 ropey remove/insert 越界 panic、区间错位。
    char_starts: Vec<usize>,
}

impl<'a> FileView<'a> {
    fn new(content: &'a str) -> Self {
        let mut lines = Vec::new();
        let mut byte_starts = Vec::new();
        let mut char_starts = Vec::new();
        let mut byte_start = 0usize;
        let mut char_start = 0usize;
        let mut char_count = 0usize;
        for (i, ch) in content.char_indices() {
            if ch == '\n' {
                lines.push(&content[byte_start..i]);
                byte_starts.push(byte_start);
                char_starts.push(char_start);
                byte_start = i + 1;
                char_start = char_count + 1;
            }
            char_count += 1;
        }
        if byte_start <= content.len() {
            lines.push(&content[byte_start..]);
            byte_starts.push(byte_start);
            char_starts.push(char_start);
        }
        byte_starts.push(content.len());
        char_starts.push(char_count);
        FileView {
            content,
            lines,
            byte_starts,
            char_starts,
        }
    }

    /// 行窗口 [s, s+win) 的 **char** 区间（ropey remove/insert 用）。
    fn char_range(&self, s: usize, win: usize) -> Range<usize> {
        self.char_starts[s]..self.char_starts[s + win]
    }

    /// 总字符数。
    fn char_len(&self) -> usize {
        self.char_starts[self.char_starts.len() - 1]
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
    /// hint_line 兜底命中时记录模型提示的 1-based 行号（透明回传用）。
    hint_line: Option<usize>,
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
    /// replace_inline 的 `regex=true` 时正则编译失败。
    InvalidRegex(String),
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
        hint_line: None,
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
        hint_line: None,
    })
}

/// 解析可选的宽松行号提示（hint_line，1-based；0 视为 1）。
fn parse_hint_line(v: &Value) -> Option<usize> {
    v.get("hint_line")
        .and_then(|x| x.as_u64())
        .map(|x| x.max(1) as usize)
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

/// replace_all：Tier1 精确匹配的**全部**位置（一个 hunk → 多个操作）。
/// 不降级 Tier2/3——模糊匹配应用于多个位置风险不可控。零命中 → NoMatch
/// （附 Tier3 候选供参考）。
fn locate_replace_all(view: &FileView, old: &str) -> Result<Vec<Located>, LocateError> {
    let pat = pattern_lines(old);
    if pat.is_empty() {
        return Err(LocateError::Underspecified);
    }
    let cands = tier1(view, &pat);
    if cands.is_empty() {
        return Err(LocateError::NoMatch {
            candidates: tier3_candidates(view, &pat, &[], &[]),
            detail: "replace_all: no exact match found — re-check 'old' against the file (see candidates below)"
                .into(),
        });
    }
    Ok(cands
        .into_iter()
        .map(|s| located(view, s, pat.len(), 1, 1.0, "exact (replace_all)"))
        .collect())
}

fn locate_hunk(view: &FileView, hunk: &Hunk) -> Result<Vec<Located>, LocateError> {
    match hunk {
        Hunk::Replace {
            old,
            replace_all: true,
            ..
        } => locate_replace_all(view, old),
        Hunk::Replace {
            old,
            context_before,
            context_after,
            ..
        } => locate_replace(view, old, context_before, context_after).map(|l| vec![l]),
        // 整文件区间；空文件（创建路径）自动退化为零长度区间。
        Hunk::Overwrite { .. } => Ok(vec![Located {
            start_char: 0,
            end_char: view.content.len(),
            start_line: 0,
            win_lines: 0,
            tier: 1,
            score: 1.0,
            note: "overwrite".to_string(),
            hint_line: None,
        }]),
        Hunk::InsertAfter { anchor, .. } | Hunk::InsertBefore { anchor, .. } => {
            locate_anchor(view, anchor).map(|l| vec![l])
        }
        Hunk::PrependFile { .. } => Ok(vec![Located {
            start_char: 0,
            end_char: 0,
            start_line: 0,
            win_lines: 0,
            tier: 1,
            score: 1.0,
            note: "prepend".to_string(),
            hint_line: None,
        }]),
        Hunk::AppendFile { .. } => Ok(vec![Located {
            start_char: view.content.len(),
            end_char: view.content.len(),
            start_line: 0,
            win_lines: 0,
            tier: 1,
            score: 1.0,
            note: "append".to_string(),
            hint_line: None,
        }]),
        Hunk::ReplaceInline {
            anchor,
            old,
            replace_all,
            regex,
            ..
        } => {
            let loc = locate_anchor(view, anchor)?;
            // 窗口内验证可命中：replace_inline 的 NO_MATCH 语义限定在 anchor 窗口内。
            let win_lines = loc.win_lines.max(1);
            let win = &view.lines[loc.start_line..loc.start_line + win_lines];
            let hit = if *regex {
                match regex::Regex::new(old) {
                    Ok(re) => win.iter().any(|l| re.is_match(l)),
                    Err(e) => return Err(LocateError::InvalidRegex(e.to_string())),
                }
            } else {
                win.iter().any(|l| l.contains(old.as_str()))
            };
            if !hit {
                let _ = replace_all; // 命中计数在 resolve 里标注
                return Err(LocateError::NoMatch {
                    candidates: Vec::new(),
                    detail: format!(
                        "replace_inline: 'old' not found within the anchor window (L{}-L{}); the anchor located the window but the substring/regex does not occur in it",
                        loc.start_line + 1,
                        loc.start_line + win_lines
                    ),
                });
            }
            Ok(vec![loc])
        }
    }
}

/// Tier1 精确匹配限定在 [lo, hi)（0-based 行窗口）内。
fn tier1_in_window(view: &FileView, pat: &[&str], lo: usize, hi: usize) -> Vec<usize> {
    tier1(view, pat)
        .into_iter()
        .filter(|&s| s >= lo && s < hi)
        .collect()
}

/// hint_line 兜底定位（宽松行号）：多命中（Ambiguous）时按行号窗口消歧。
///
/// 语义：行号 = 提示/窗口（±10 行），内容 = 唯一通行证——窗口内 Tier1 精确
/// 匹配，**唯一命中才应用**（0 个 → None，调用方保留原 Ambiguous；多个 →
/// 新 Ambiguous 附窗口信息）。不触碰默认路径：hint_line 为 None 返回 None。
const HINT_WINDOW: usize = 10;

fn locate_with_hint(view: &FileView, hunk: &Hunk) -> Option<Result<Located, LocateError>> {
    let hint = match hunk {
        Hunk::Replace {
            hint_line: Some(h),
            replace_all: false,
            ..
        }
        | Hunk::InsertAfter {
            hint_line: Some(h), ..
        }
        | Hunk::InsertBefore {
            hint_line: Some(h), ..
        }
        | Hunk::ReplaceInline {
            hint_line: Some(h), ..
        } => (*h).max(1), // 0 视为 1（与 parse 层一致）
        _ => return None,
    };
    let total = view.lines.len();
    // 1-based hint → 0-based 窗口 [hint-11, hint+10)，钳制到文件范围。
    let lo = hint.saturating_sub(HINT_WINDOW + 1).min(total);
    let hi = (hint + HINT_WINDOW).min(total);
    if lo >= hi {
        return None; // 窗口为空 → hint 无意义，保留原错误
    }

    match hunk {
        Hunk::Replace { old, .. } => {
            let pat = pattern_lines(old);
            if pat.is_empty() {
                return None; // 纯插入走 locate_pure_insert，不做 hint 兜底
            }
            let cands = tier1_in_window(view, &pat, lo, hi);
            match cands.len() {
                0 => None,
                1 => {
                    let mut loc = located(
                        view,
                        cands[0],
                        pat.len(),
                        4,
                        1.0,
                        &format!("hint_line {hint} (window, exact)"),
                    );
                    loc.hint_line = Some(hint);
                    Some(Ok(loc))
                }
                _ => Some(Err(LocateError::Ambiguous {
                    candidates: make_candidates(view, &cands, pat.len(), 1, 1.0, &pat),
                    detail: format!(
                        "hint_line {hint}: 'old' still matches {} locations within window L{}-L{}; add context_before/context_after or a more specific hint",
                        cands.len(),
                        lo + 1,
                        hi
                    ),
                })),
            }
        }
        Hunk::InsertAfter { anchor, .. } | Hunk::InsertBefore { anchor, .. } => {
            let pat = pattern_lines(anchor);
            let cands = tier1_in_window(view, &pat, lo, hi);
            match cands.len() {
                0 => None,
                1 => {
                    let mut loc = located(
                        view,
                        cands[0],
                        pat.len(),
                        4,
                        1.0,
                        &format!("hint_line {hint} (window, exact)"),
                    );
                    loc.hint_line = Some(hint);
                    Some(Ok(loc))
                }
                _ => Some(Err(LocateError::Ambiguous {
                    candidates: make_candidates(view, &cands, pat.len(), 1, 1.0, &pat),
                    detail: format!(
                        "hint_line {hint}: anchor still matches {} locations within window L{}-L{}; refine the anchor",
                        cands.len(),
                        lo + 1,
                        hi
                    ),
                })),
            }
        }
        Hunk::ReplaceInline {
            anchor,
            old,
            regex,
            ..
        } => {
            let pat = pattern_lines(anchor);
            let cands = tier1_in_window(view, &pat, lo, hi);
            match cands.len() {
                0 => None,
                1 => {
                    let mut loc = located(
                        view,
                        cands[0],
                        pat.len(),
                        4,
                        1.0,
                        &format!("hint_line {hint} (window, exact)"),
                    );
                    // 窗口内验证子串/regex 可命中（与 locate_hunk 的 ReplaceInline 一致）。
                    let win_lines = loc.win_lines.max(1);
                    let win = &view.lines[loc.start_line..loc.start_line + win_lines];
                    let hit = if *regex {
                        match regex::Regex::new(old) {
                            Ok(re) => win.iter().any(|l| re.is_match(l)),
                            Err(e) => {
                                return Some(Err(LocateError::InvalidRegex(e.to_string())));
                            }
                        }
                    } else {
                        win.iter().any(|l| l.contains(old.as_str()))
                    };
                    if !hit {
                        return None; // 窗口内无子串 → hint 无效，保留原错误
                    }
                    loc.hint_line = Some(hint);
                    Some(Ok(loc))
                }
                _ => Some(Err(LocateError::Ambiguous {
                    candidates: make_candidates(view, &cands, pat.len(), 1, 1.0, &pat),
                    detail: format!(
                        "hint_line {hint}: anchor still matches {} locations within window L{}-L{}",
                        cands.len(),
                        lo + 1,
                        hi
                    ),
                })),
            }
        }
        _ => None,
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
    let start = view.char_starts[loc.start_line];
    let end = view.char_starts[loc.start_line + loc.win_lines];
    if want_trailing_nl || end <= start {
        return start..end;
    }
    // 窗口最后一行是否以 '\n' 结尾：用字节索引检查（end 的字节位置前一个字节）。
    // `end - 1` 是 char 索引（'\n' 为单字符，安全）。
    let end_byte = view.byte_starts[loc.start_line + loc.win_lines];
    let trimmed = if view.content.as_bytes().get(end_byte.wrapping_sub(1)) == Some(&b'\n') {
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

/// sed `s///` 语义：窗口内行内替换。不跨行（逐行处理）、不改行结构；
/// `replace_all=false` 只替换第一处（按行序）。regex=true 时 old 为正则。
/// 窗口末尾无换行时（文件尾）保持无换行。
fn inline_replace(
    window: &str,
    old: &str,
    new: &str,
    replace_all: bool,
    regex: bool,
) -> Result<String, String> {
    if regex {
        let re = regex::Regex::new(old).map_err(|e| e.to_string())?;
        let mut out = String::with_capacity(window.len() + new.len());
        // done = 首次替换已完成（仅 replace_all=false 时置位；=true 时每行都换）。
        let mut done = false;
        for line in window.split_inclusive('\n') {
            if done {
                out.push_str(line);
                continue;
            }
            let (body, nl) = if line.ends_with('\n') {
                (&line[..line.len() - 1], "\n")
            } else {
                (line, "")
            };
            if replace_all {
                out.push_str(&re.replace_all(body, new));
            } else {
                match re.find(body) {
                    Some(_) => {
                        // re.replace 替换第一处并展开 $1/$2 捕获组（sed s/// 语义）。
                        out.push_str(&re.replace(body, new));
                        done = true;
                    }
                    None => out.push_str(body),
                }
            }
            out.push_str(nl);
        }
        Ok(out)
    } else {
        let mut out = String::with_capacity(window.len() + new.len());
        // done = 首次替换已完成（仅 replace_all=false 时置位；=true 时每行都换）。
        let mut done = false;
        for line in window.split_inclusive('\n') {
            if done {
                out.push_str(line);
                continue;
            }
            let (body, nl) = if line.ends_with('\n') {
                (&line[..line.len() - 1], "\n")
            } else {
                (line, "")
            };
            if replace_all {
                out.push_str(&body.replace(old, new));
            } else {
                match body.find(old) {
                    Some(pos) => {
                        let mut replaced = String::with_capacity(body.len() + new.len());
                        replaced.push_str(&body[..pos]);
                        replaced.push_str(new);
                        replaced.push_str(&body[pos + old.len()..]);
                        out.push_str(&replaced);
                        done = true;
                    }
                    None => out.push_str(body),
                }
            }
            out.push_str(nl);
        }
        Ok(out)
    }
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
        Hunk::Overwrite { new } => ResolvedOp::Replace {
            // 无缩进补偿：new 是全文，Tier1 恒命中。
            range: 0..view.content.len(),
            new: new.clone(),
        },
        Hunk::InsertAfter { new, .. } => {
            let pos = view.char_starts[loc.start_line + loc.win_lines];
            // 锚点窗口是文件尾且无尾随换行：先补换行，避免新内容粘在最后一行上。
            let text = if pos == view.char_len() && !view.content.ends_with('\n') {
                format!("\n{new}")
            } else {
                new.clone()
            };
            ResolvedOp::Insert { pos, text }
        }
        Hunk::InsertBefore { new, .. } => ResolvedOp::Insert {
            pos: view.char_starts[loc.start_line],
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
                pos: view.char_len(),
                text,
            }
        }
        Hunk::ReplaceInline {
            old,
            new,
            replace_all,
            regex,
            ..
        } => {
            // 窗口 = anchor 命中行区间（含行间换行，不含文件尾的尾空行行号）。
            let start = view.char_starts[loc.start_line];
            let end = view.char_starts[loc.start_line + loc.win_lines.max(1)];
            let window = &view.content[start..end];
            let replaced = inline_replace(window, old, new, *replace_all, *regex)
                .expect("locate_hunk verified the window contains a match; regex already compiled");
            ResolvedOp::Replace { range: start..end, new: replaced }
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
/// 返回 (编辑后文本, 行号偏移表)——偏移基于同一快照（单快照两阶段），
/// 供账本行号修正（read 用旧行号时自动补偿）。
fn apply_ops(content: &str, ops: &[(usize, ResolvedOp)]) -> (String, Vec<(usize, i64)>) {
    let mut rope = Rope::from_str(content);
    let mut shifts: Vec<(usize, i64)> = Vec::with_capacity(ops.len());
    let mut ordered: Vec<&ResolvedOp> = ops.iter().map(|(_, op)| op).collect();
    ordered.sort_by_key(|op| std::cmp::Reverse(op.start()));
    for op in ordered {
        match op {
            ResolvedOp::Replace { range, new } => {
                // 行号增量 = 新增换行数 - 移除换行数（行级替换/行内子串统一）。
                let before_line = rope.char_to_line(range.start) + 1;
                let removed_lf = rope
                    .slice(range.clone())
                    .chars()
                    .filter(|&c| c == '\n')
                    .count() as i64;
                let added_lf = new.chars().filter(|&c| c == '\n').count() as i64;
                shifts.push((before_line, added_lf - removed_lf));
                // ropey 的 remove/insert 为 in-place 修改（返回 ()）——与 spec 假定
                // 的“返回新 rope”不同，以 crate 实际 API 为准。倒序应用保证前面
                // 区间的 char index 不受后面编辑影响，in-place 同样安全。
                let start = range.start;
                rope.remove(range.clone());
                rope.insert(start, new.as_str());
            }
            ResolvedOp::Insert { pos, text } => {
                let before_line = rope.char_to_line(*pos) + 1;
                let added_lf = text.chars().filter(|&c| c == '\n').count() as i64;
                shifts.push((before_line, added_lf));
                rope.insert(*pos, text.as_str());
            }
        }
    }
    (rope.to_string(), shifts)
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
    /// hint_line 兜底命中时记录模型提示的 1-based 行号（透明回传）。
    used_hint: Option<usize>,
}

struct FileOutcome {
    edited: Option<String>,
    new_hash: Option<String>,
    diff: String,
    reports: Vec<HunkReport>,
    code: Option<String>,
    message: Option<String>,
    notes: Vec<String>,
    /// 账本行号修正：每 hunk 的 (编辑前 1-based 起始行, 行号增量)。
    /// 仅成功路径非空；错误/未落盘路径为空。
    shifts: Vec<(usize, i64)>,
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
    // overwrite 是整文件独占语义：单个调用里只能有它自己。
    let overwrite_count = hunks
        .iter()
        .filter(|h| matches!(h, Hunk::Overwrite { .. }))
        .count();
    if overwrite_count > 1 || (overwrite_count == 1 && hunks.len() > 1) {
        return FileOutcome {
            edited: None,
            new_hash: None,
            diff: String::new(),
            reports: Vec::new(),
            code: Some("OVERWRITE_EXCLUSIVE".to_string()),
            message: Some(
                "overwrite replaces the WHOLE file and must be the only hunk in the call; use one overwrite alone, or prepend_file/append_file/replace hunks for incremental edits"
                    .to_string(),
            ),
            notes,
            shifts: Vec::new(),
        };
    }
    let view = FileView::new(content);
    let mut reports: Vec<HunkReport> = Vec::with_capacity(hunks.len());
    let mut located: Vec<Vec<Located>> = Vec::with_capacity(hunks.len());
    let mut first_code: Option<String> = None;

    for (i, hunk) in hunks.iter().enumerate() {
        // hint_line 消歧：多命中（AMBIGUOUS_MATCH）时按提示行号 ±10 开窗口
        // 过滤，窗口内唯一命中才应用（默认路径零变化：无 hint 原样报错）。
        let located_result = match locate_hunk(&view, hunk) {
            Ok(locs) => Ok(locs),
            Err(LocateError::Ambiguous { candidates, detail }) => {
                match locate_with_hint(&view, hunk) {
                    Some(Ok(loc)) => Ok(vec![loc]),
                    Some(Err(e)) => Err(e),
                    None => Err(LocateError::Ambiguous { candidates, detail }),
                }
            }
            Err(e) => Err(e),
        };
        match located_result {
            Ok(locs) => {
                // replace_all 一个 hunk 可产出多个位置；报告仍按 hunk 一条，
                // 位置/分数取首个，note 标注命中数量。
                let loc = &locs[0];
                let line_range =
                    (loc.win_lines > 0).then(|| (loc.start_line + 1, loc.start_line + loc.win_lines));
                let note = if locs.len() > 1 {
                    format!("{} location(s) replaced", locs.len())
                } else {
                    loc.note.clone()
                };
                reports.push(HunkReport {
                    index: i,
                    kind: hunk.kind_name().to_string(),
                    status: "ok".to_string(),
                    tier: Some(loc.tier),
                    score: Some(loc.score),
                    line_range,
                    note: Some(note),
                    code: None,
                    detail: None,
                    candidates: None,
                    used_hint: loc.hint_line,
                });
                located.push(locs);
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
                    LocateError::InvalidRegex(e) => (
                        "INVALID_REGEX".to_string(),
                        format!("replace_inline: invalid regex: {e}"),
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
                    used_hint: None,
                });
                located.push(Vec::new());
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
                    shifts: Vec::new(),
                };
            }
            Mode::Partial => {
                // 只应用定位成功的 hunk（仍做重叠检测与单快照倒序应用）。
                let mut ops: Vec<(usize, ResolvedOp)> = Vec::with_capacity(hunks.len());
                for (i, (hunk, locs)) in hunks.iter().zip(located.iter()).enumerate() {
                    for loc in locs {
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
                        shifts: Vec::new(),
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
                        shifts: Vec::new(),
                    };
                }
                let (edited, shifts) = apply_ops(content, &ops);
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
                    shifts,
                };
            }
        }
    }

    let mut ops: Vec<(usize, ResolvedOp)> = Vec::with_capacity(hunks.len());
    for (i, (hunk, locs)) in hunks.iter().zip(located.iter()).enumerate() {
        for loc in locs {
            ops.push((i, resolve(&view, hunk, loc)));
        }
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
            shifts: Vec::new(),
        };
    }

    let (edited, shifts) = apply_ops(content, &ops);
    FileOutcome {
        diff: unified_diff(content, &edited, "file"),
        new_hash: Some(content_hash(&edited)),
        edited: Some(edited),
        reports,
        code: None,
        message: None,
        notes,
        shifts,
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
        "NO_MATCH" => Some("Refine 'old'/context from the candidates, or re-locate with rg."),
        "AMBIGUOUS_MATCH" => Some("Add context_before/context_after to disambiguate the repeated occurrence."),
        "OVERLAPPING_HUNKS" => Some("Split or merge hunks so their resolved ranges do not overlap."),
        "UNDERSPECIFIED" => Some("Provide context, or use insert_after / insert_before / prepend_file / append_file."),
        "OVERWRITE_EXCLUSIVE" => Some("overwrite replaces the whole file — send it as the only hunk in the call."),
        "HASH_MISMATCH" => Some("Re-read the file with read_file and retry with the fresh hash (or omit expected_hash to edit without verification)."),
        "INVALID_REGEX" => Some("replace_inline: fix the regex (regex crate syntax) and retry."),
        _ => None,
    }
}

// ─────────────────────────────────────────────────────────────
// 读路径（空 hunks）：整文件 / 行号范围 / 锚定读
// ─────────────────────────────────────────────────────────────

/// 行号范围读（1-based，与 read_file 同语义）：grep `path:line:` 输出直连。
fn read_range(
    content: &str,
    total_lines: usize,
    raw_path: &str,
    hash: &str,
    start: Option<usize>,
    end: Option<usize>,
    fail: &dyn Fn(&str, String, bool, Option<&str>) -> ToolResult,
) -> ToolResult {
    let first = start.unwrap_or(1).saturating_sub(1);
    if first > total_lines || end.is_some_and(|e| e > total_lines) {
        return ToolResult::error_data(
            "LINE_OUT_OF_RANGE",
            format!("edit_file_v2: requested lines are outside '{raw_path}' ({total_lines} total lines)"),
            false,
            Some("Use the total_lines value and retry.").map(str::to_string),
            json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "code": "LINE_OUT_OF_RANGE",
                "path": raw_path,
                "message": format!("requested lines are outside '{raw_path}' ({total_lines} total lines)"),
                "total_lines": total_lines,
                "hash": hash,
            }),
        );
    }
    let end_0 = end.unwrap_or(total_lines).min(total_lines);
    let view = FileView::new(content);
    let body_chars: usize = view.lines[first..end_0]
        .iter()
        .map(|l| l.chars().count() + 8)
        .sum();
    if body_chars > READ_MAX_CHARS {
        return fail(
            "RANGE_TOO_LARGE",
            "edit_file_v2: requested range exceeds the model output budget".into(),
            false,
            Some("Split the range into smaller contiguous reads."),
        );
    }
    let body = view.lines[first..end_0]
        .iter()
        .enumerate()
        .map(|(off, line)| format!("L{}: {line}", first + off + 1))
        .collect::<Vec<_>>()
        .join("\n");
    ToolResult::ok_data(
        json!({
            "status": "ok",
            "read_only": true,
            "path": raw_path,
            "start_line": first + 1,
            "end_line": end_0,
            "total_lines": total_lines,
            "hash": hash,
            "truncated": false,
        }),
        body,
    )
}

/// 锚定读：复用 hunk 的四层定位引擎——读时的 anchor 命中 = 改时的 hunk 定位，
/// 读到的上下文就是将要编辑的上下文。模糊/未命中返回候选（与 hunk 同款）。
fn read_anchored(
    content: &str,
    total_lines: usize,
    raw_path: &str,
    hash: &str,
    anchor: &str,
    ctx_before: usize,
    ctx_after: usize,
    fail: &dyn Fn(&str, String, bool, Option<&str>) -> ToolResult,
) -> ToolResult {
    let view = FileView::new(content);
    let loc = match locate_anchor(&view, anchor) {
        Ok(l) => l,
        Err(LocateError::Ambiguous { candidates, detail }) => {
            return ToolResult::error_data(
                "ANCHOR_AMBIGUOUS",
                render_read_candidates(raw_path, "ANCHOR_AMBIGUOUS", &detail, &candidates),
                true,
                Some("Add context_before/context_after to narrow the window, or read by line range (start_line/end_line).").map(str::to_string),
                json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "code": "ANCHOR_AMBIGUOUS",
                    "path": raw_path,
                    "message": detail,
                    "candidates": candidates.iter().map(candidate_json).collect::<Vec<_>>(),
                    "hash": hash,
                }),
            );
        }
        Err(LocateError::NoMatch { candidates, detail }) => {
            return ToolResult::error_data(
                "NO_MATCH",
                render_read_candidates(raw_path, "NO_MATCH", &detail, &candidates),
                true,
                Some("Refine the anchor from the candidates, or locate with grep first.").map(str::to_string),
                json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "code": "NO_MATCH",
                    "path": raw_path,
                    "message": detail,
                    "candidates": candidates.iter().map(candidate_json).collect::<Vec<_>>(),
                    "hash": hash,
                }),
            );
        }
        Err(LocateError::Underspecified) => {
            return fail(
                "UNDERSPECIFIED",
                "edit_file_v2: anchor is empty (underspecified read)".into(),
                false,
                Some("Provide a non-empty anchor, or use start_line/end_line."),
            );
        }
        // locate_anchor 不编译正则，InvalidRegex 只可能来自 replace_inline 的 locate_hunk。
        Err(LocateError::InvalidRegex(_)) => unreachable!("locate_anchor never compiles a regex"),
    };
    let s = loc.start_line;
    let w = loc.win_lines.max(1);
    let before = ctx_before.min(s);
    let after = ctx_after.min(total_lines.saturating_sub(s + w));
    let first = s - before;
    let end_0 = (s + w + after).min(total_lines);
    let body_chars: usize = view.lines[first..end_0]
        .iter()
        .map(|l| l.chars().count() + 8)
        .sum();
    if body_chars > READ_MAX_CHARS {
        return fail(
            "RANGE_TOO_LARGE",
            "edit_file_v2: anchored window exceeds the model output budget".into(),
            false,
            Some("Reduce context_before/context_after."),
        );
    }
    let body = view.lines[first..end_0]
        .iter()
        .enumerate()
        .map(|(off, line)| format!("L{}: {line}", first + off + 1))
        .collect::<Vec<_>>()
        .join("\n");
    ToolResult::ok_data(
        json!({
            "status": "ok",
            "read_only": true,
            "path": raw_path,
            "anchor_line": s + 1,
            "anchor_lines": w,
            "start_line": first + 1,
            "end_line": end_0,
            "total_lines": total_lines,
            "tier": loc.tier,
            "score": loc.score,
            "note": loc.note,
            "hash": hash,
            "truncated": false,
        }),
        body,
    )
}

fn render_read_candidates(path: &str, code: &str, detail: &str, cands: &[Candidate]) -> String {
    let mut out = format!("[ERROR] edit_file_v2 {path}\n  {code}: {detail}\n");
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
    out
}

fn candidate_json(c: &Candidate) -> serde_json::Value {
    json!({
        "line_range": [c.line_range.0, c.line_range.1],
        "snippet": c.snippet,
        "score": c.score,
        "tier": c.tier,
        "diff": c.diff,
    })
}

/// 读路径入口（hunks 缺失/空）：整文件 / start_line..end_line / anchor 三种形态。
fn read_path(path: &str, raw_path: &str, args: &serde_json::Value) -> ToolResult {
    let read_fail = |code: &str, message: String, retryable: bool, hint: Option<&str>| {
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
    let fail = &read_fail;

    let start = args.get("start_line").and_then(|v| v.as_u64()).map(|v| v as usize);
    let end = args.get("end_line").and_then(|v| v.as_u64()).map(|v| v as usize);
    let anchor = args
        .get("anchor")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let has_context = args.get("context_before").is_some() || args.get("context_after").is_some();
    let ctx_before = args
        .get("context_before")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(10);
    let ctx_after = args
        .get("context_after")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(10);

    let has_range = start.is_some() || end.is_some();
    if has_range && (anchor.is_some() || has_context) {
        return fail(
            "PARSE_ERROR",
            "edit_file_v2: start_line/end_line and anchor are mutually exclusive".into(),
            false,
            None,
        );
    }
    if anchor.is_none() && has_context {
        return fail(
            "PARSE_ERROR",
            "edit_file_v2: context_before/context_after require 'anchor'".into(),
            false,
            None,
        );
    }
    if start == Some(0) || end == Some(0) {
        return fail(
            "PARSE_ERROR",
            "edit_file_v2: read line numbers start at 1".into(),
            false,
            None,
        );
    }
    if let (Some(s), Some(e)) = (start, end) {
        if e < s {
            return fail(
                "PARSE_ERROR",
                "edit_file_v2: end_line must be >= start_line".into(),
                false,
                None,
            );
        }
        if e - s + 1 > READ_MAX_LINES {
            return fail(
                "RANGE_TOO_LARGE",
                format!("edit_file_v2: requested range exceeds {READ_MAX_LINES} lines"),
                false,
                Some("Use smaller contiguous ranges."),
            );
        }
    }
    if anchor.is_some() && (ctx_before > READ_MAX_CONTEXT || ctx_after > READ_MAX_CONTEXT) {
        return fail(
            "PARSE_ERROR",
            format!("edit_file_v2: context window per side exceeds {READ_MAX_CONTEXT} lines"),
            false,
            None,
        );
    }

    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return fail(
                "FILE_NOT_FOUND",
                format!("edit_file_v2: {raw_path}: file not found (read mode; creation requires explicit hunks)"),
                false,
                Some("Provide prepend_file / append_file / overwrite hunks to create the file"),
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
    let raw = match String::from_utf8(bytes) {
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
    let (content, _was_crlf) = normalize_newlines(&raw);
    let hash = content_hash(&content);
    let total_lines = content.lines().count();

    if !has_range && anchor.is_none() {
        // 整文件读（现状语义：CONTENT_CAP 截断，hash 仍为全文件）
        let truncated = content.len() > CONTENT_CAP;
        let shown = truncate_content(&content);
        let text = if truncated {
            format!("{shown}\n(read-only: {total_lines} lines, truncated)\n")
        } else {
            content
        };
        return ToolResult::ok_data(
            json!({
                "status": "ok",
                "read_only": true,
                "path": raw_path,
                "hash": hash,
                "line_count": total_lines,
                "truncated": truncated,
                // 注意：内容走 model_text（compact_data 会删 data 里的
                // content 键），data 只放元数据——与 read_file 同构。
            }),
            text,
        );
    }

    if has_range {
        return read_range(&content, total_lines, raw_path, &hash, start, end, fail);
    }
    read_anchored(
        &content,
        total_lines,
        raw_path,
        &hash,
        anchor.unwrap(),
        ctx_before,
        ctx_after,
        fail,
    )
}
// ─────────────────────────────────────────────────────────────
// 执行入口
// ─────────────────────────────────────────────────────────────

pub(super) fn exec_edit_file_v2(args: &serde_json::Value) -> ToolResult {
    let dry_run = args
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
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
    // ── 读路径（二元组场景：edit 兼读）──
    // hunks 缺失/空 = 无编辑意图 → 退化为读：返回 content + hash + line_count
    // （对齐 read_file 结构，hash 可直接作 expected_hash 续接编辑，闭环在同一
    // 工具内完成）。文件必须存在——创建需要显式 hunks，防"读"误当"建"。
    // 不写盘、不 record、无 diff；权限层按空 hunks 动态分类为 Read（permission.rs）。
    if args.get("hunks").and_then(|v| v.as_array()).is_none_or(|a| a.is_empty()) {
        // 读路径：整文件 / 行号范围（grep 直连）/ 锚定读（复用定位引擎）。
        // 不写盘、不 record、无 diff；权限层按空 hunks 动态分类为 Read（permission.rs）。
        return read_path(&path, raw_path, args);
    }
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
    let mut file_was_missing = false;
    let raw = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && expected_hash.is_none() => {
            // 创建新文件路径：空内容进入正常管线（仅 prepend/append/overwrite 可定位）。
            file_was_missing = true;
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
            // 无 hash：直接编辑。v2 全部是内容定位（无行号盲定位），
            // 命中即安全——与 v1 的内容定位语义一致。模型从 read 拿不到
            // hash（hash 只在 data 元数据里，模型正文里没有），门禁只会堵死。
            // 文件不存在 → 创建路径（读文件分支已处理 NotFound）。
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
            // 文件不存在 + 内容定位失败 → 提示创建路径（否则模型会反复重试 replace）。
            let hint = if file_was_missing && code == "NO_MATCH" {
                Some("file does not exist — create it with prepend_file / append_file (or overwrite) hunks".to_string())
            } else {
                hint_for(code).map(str::to_string)
            };
            ToolResult::error_data(
                code,
                text,
                true,
                hint,
                data,
            )
        }
        Some(ref edited) => {
            if dry_run {
                // 只定位+计算，不写盘；暂存参数供 confirm_apply 内存直提
                // （模型无需重发 hunks）。expected_hash 注入 dry-run 读到的
                // LF 视图 hash：重放时 hash gate 拦截期间的外部改动。
                let new_hash = outcome.new_hash.as_deref().unwrap_or_default();
                let mut pending_args = args.clone();
                if let Some(obj) = pending_args.as_object_mut() {
                    obj.remove("dry_run");
                    obj.insert("expected_hash".into(), json!(content_hash(&content)));
                }
                let pending_id = crate::pending::store("edit_file_v2", &pending_args);
                let text = render_text(raw_path, &outcome);
                let applied = outcome.reports.iter().filter(|r| r.status == "ok").count();
                let total = outcome.reports.len();
                let status = if outcome.code.is_some() { "partial" } else { "ok" };
                let mut data = json!({
                    "timeis": crate::now_utc8(),
                    "status": status,
                    "dry_run": true,
                    "path": raw_path,
                    "pending_id": pending_id,
                    "new_hash": new_hash,
                    "applied_hunks": applied,
                    "total_hunks": total,
                    "hunks": outcome.reports.iter().map(|r| hunk_report_json(r)).collect::<Vec<_>>(),
                });
                if let Some(code) = &outcome.code {
                    data["code"] = json!(code);
                }
                let mut result = ToolResult::ok_data(data, text);
                result.summary.push_str(&format!(
                    "pending_id={pending_id} — confirm with confirm_apply {{\"pending_id\":\"{pending_id}\",\"action\":\"apply\"}}\n"
                ));
                return result;
            }
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
            // 台账 + 行号偏移链：局部 hunk 编辑记录偏移（read 旧行号可自动修正）；
            // 全覆盖/零偏移路径退化为 write 语义（清偏移链 = 行号全部失效）。
            if outcome.shifts.is_empty() {
                crate::file_state::record_write(&path, &write_content);
            } else {
                crate::file_state::record_edit_with_shifts(&path, &write_content, &outcome.shifts);
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
    // hint_line 兜底命中：透明回传 提示行号 / 实际行号 / 偏差（实际 - 提示）。
    if let Some(h) = r.used_hint {
        v["used_hint"] = json!(h);
        if let Some((a, _)) = r.line_range {
            v["actual_line"] = json!(a);
            v["line_offset"] = json!(a as i64 - h as i64);
        }
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
                "Structured hunk editor (v2, the ONLY file editor). Kind-tagged hunks: ",
                "replace (old/new/context_before/context_after), overwrite (new, whole-file replacement), insert_after / insert_before (anchor/new), replace_inline (anchor + old/new substring or regex replace, sed s/// semantics, window-limited), ",
                "prepend_file / append_file (new). Four-tier matching: exact → indent-shape → similarity scoring ",
                "with margin (auto-applies only when the best candidate clearly wins) → top-3 candidates on failure. ",
                "overwrite replaces the ENTIRE file (equivalent to write) and must be the only hunk in the call. ",
                "Keep 'old' SHORT: use the smallest unique 1-5 line fragment (whitespace/indent differences are auto-tolerated; ",
                "a whole function body is more likely to mismatch). ",
                "replace_all=true on a replace hunk substitutes EVERY exact occurrence of 'old' (Tier-1 exact only). ",
                "All hunks are located on ONE unchanged snapshot and applied all-or-nothing; overlapping hunks are rejected. ",
                "Newline rule: the replaced range includes the trailing newline ONLY if 'old' ends with '\\n' — ",
                "normally give both old and new WITHOUT the trailing newline (it stays in place); ",
                "end 'old' with '\\n' only when you want to delete/replace the line break itself. ",
                "expected_hash (optional; from read_file data, if your provider exposes it) guards stale content; omit it to edit without verification — the content match itself is the safety net. ",
                "On success returns new_hash to chain into the next call without re-reading. ",
                "On failure, candidates come back with line ranges — refine 'old'/context from them, or re-read the file first if the view may be stale. ",
                "mode 'partial' applies successful hunks and reports failures in detail, so you only re-send the failed ones "
                ,"(with expected_hash = returned new_hash) instead of the whole batch; mode 'strict' (default) is all-or-nothing. "
                ,"dry_run=true locates and computes without writing and returns a pending_id — ask the user for confirmation, then commit via confirm_apply without re-sending the hunks. Omit hunks (or send []) to READ the file: read-only result with content + hash + line_count — the hash chains straight into expected_hash for the next edit. Read mode also supports grep-driven precision reads: start_line/end_line (1-based; line ranges connect straight to grep `path:line:` output; each line prefixed L<number>:) and anchor + context_before/context_after (anchored read reusing the SAME four-tier locator as hunks — the anchor you read with is the anchor you edit with; ambiguous anchors return candidates). All read modes return the full-file hash for expected_hash chaining."
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Target file path"},
                    "expected_hash": {
                        "type": "string",
                        "description": "Optional; hash from read_file's data. Omit to edit without verification (content match itself is the safety net). Mismatch rejects with current content returned."
                    },
                    "hunks": {
                        "type": "array",
                        "description": "Edit hunks. Each hunk: {\"kind\":\"replace\",\"old\":…,\"new\":…,\"context_before\":…,\"context_after\":…,\"replace_all\":false} | {\"kind\":\"overwrite\",\"new\":…} (whole-file replacement; must be the only hunk) | {\"kind\":\"insert_after\"|\"insert_before\",\"anchor\":…,\"new\":…} | {\"kind\":\"prepend_file\"|\"append_file\",\"new\":…} | {\"kind\":\"replace_inline\",\"anchor\":…,\"old\":…,\"new\":…,\"replace_all\":false,\"regex\":false} (sed s/// semantics: substring/regex replace ONLY inside the anchor's located window, never across lines; replace_all=false replaces the first occurrence only; regex uses regex-crate syntax, case-sensitive). Omit hunks or send [] to READ the file (read-only: content + hash + line_count, aligned with read_file; creation requires explicit hunks). Empty 'old' with contexts = pure insert. replace_all=true substitutes every exact occurrence of 'old' (Tier-1 exact only)."
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["strict", "partial"],
                        "default": "strict",
                        "description": "strict = all-or-nothing; partial = successful hunks are applied and written, failed hunks are reported in detail — retry only the failed hunks with expected_hash = returned new_hash."
                    },
                    "dry_run": {
                        "type": "boolean",
                        "default": false,
                        "description": "Locate + compute every hunk but write NOTHING. On success returns a pending_id; confirm with confirm_apply to commit without re-sending the hunks."
                    },
                    "start_line": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Read mode only (omit hunks): first line, 1-based — connects straight to grep line numbers."
                    },
                    "end_line": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Read mode only: last line, 1-based, inclusive; omit to read to EOF."
                    },
                    "anchor": {
                        "type": "string",
                        "description": "Read mode only: content anchor, located with the same four-tier engine as hunks; returns the surrounding window (context_before/context_after)."
                    },
                    "context_before": {
                        "type": "integer",
                        "minimum": 0,
                        "default": 10,
                        "description": "Read mode only: lines of context before the anchor match."
                    },
                    "context_after": {
                        "type": "integer",
                        "minimum": 0,
                        "default": 10,
                        "description": "Read mode only: lines of context after the anchor match."
                    }
                },
                "required": ["path"],
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
            replace_all: false,
            hint_line: None,
        }
    }

    fn rp_all(old: &str, new: &str) -> Hunk {
        Hunk::Replace {
            old: old.to_string(),
            new: new.to_string(),
            context_before: String::new(),
            context_after: String::new(),
            replace_all: true,
            hint_line: None,
        }
    }

    fn rp_ctx(old: &str, new: &str, before: &str, after: &str) -> Hunk {
        Hunk::Replace {
            old: old.to_string(),
            new: new.to_string(),
            context_before: before.to_string(),
            context_after: after.to_string(),
            replace_all: false,
            hint_line: None,
        }
    }

    fn rp_hint(old: &str, new: &str, hint: usize) -> Hunk {
        Hunk::Replace {
            old: old.to_string(),
            new: new.to_string(),
            context_before: String::new(),
            context_after: String::new(),
            replace_all: false,
            hint_line: Some(hint),
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

    // 0. replace_all：全部精确匹配位置一次替换
    #[test]
    fn replace_all_substitutes_every_exact_occurrence() {
        let out = edit("a\nx\nb\nx\nc\n", &[rp_all("x", "X")]);
        assert_eq!(out.edited.as_deref(), Some("a\nX\nb\nX\nc\n"));
        assert_eq!(out.reports.len(), 1);
        assert_eq!(out.reports[0].tier, Some(1));
        assert_eq!(
            out.reports[0].note.as_deref(),
            Some("2 location(s) replaced")
        );
    }

    // 0b. replace_all：零精确命中 → NO_MATCH，不降级模糊匹配（多位置风险不可控）
    #[test]
    fn replace_all_without_exact_match_rejects() {
        // old 与文件内容有缩进差异 → Tier2 可单点命中，但 replace_all 拒绝降级。
        let out = edit("a\n    x\nb\n", &[rp_all("x", "X")]);
        assert_eq!(err_code(&out), "NO_MATCH");
        assert!(out.edited.is_none());
    }

    // 0c. replace_all：与其它 hunk 共存（区间互不重叠）
    #[test]
    fn replace_all_coexists_with_other_hunks() {
        let out = edit(
            "a\nx\nb\nx\nc\n",
            &[rp_all("x", "X"), rp("b", "B")],
        );
        assert_eq!(out.edited.as_deref(), Some("a\nX\nB\nX\nc\n"));
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
                    hint_line: None,
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
                hint_line: None,
            }],
        );
        assert_eq!(out.edited.as_deref(), Some("a\nb\nb2\nc\n"));
        let out = edit(
            "a\nb\nc\n",
            &[Hunk::InsertBefore {
                anchor: "c".into(),
                new: "c0\n".into(),
                hint_line: None,
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
                    hint_line: None,
                },
                Hunk::InsertBefore {
                    anchor: "c".into(),
                    new: "c0\n".into(),
                    hint_line: None,
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
                hint_line: None,
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

    // 12. 无 hash 编辑已有文件 → 直接编辑（内容定位命中即安全）
    #[test]
    fn edit_existing_file_without_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "x\n").unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "hunks": [{"kind": "replace", "old": "x", "new": "y"}],
        }));
        assert!(result.is_success(), "model text: {}", result.model.text);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "y\n");
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
                    hint_line: None,
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
                    hint_line: None,
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

    // ── 多字节（中文）回归：FileView 字节/字符索引混淆曾导致 ropey 越界
    //    panic 与区间错位（char_indices 返回字节偏移被误当 char 索引）。
    // ────────────────────────────────────────────────────────────────

    // 中文行 Tier1 精确替换：多字节行命中 + 区间正确（修复前会 panic/错位）。
    #[test]
    fn multibyte_exact_replace() {
        let content = "fn main() {\n    // 处理用户输入\n    let x = 1;\n}\n";
        let out = edit(content, &[rp("    // 处理用户输入", "    // 处理用户请求")]);
        assert_eq!(
            out.edited.as_deref(),
            Some("fn main() {\n    // 处理用户请求\n    let x = 1;\n}\n")
        );
        assert_eq!(out.reports[0].tier, Some(1));
    }

    // 中文文件多 hunk 倒序应用：两处中文替换 + 一处 ASCII 替换，倒序区间都正确。
    #[test]
    fn multibyte_multi_hunk_reverse_apply() {
        let content = "// 配置说明\nlet a = 1;\n// 环境变量\nlet b = 2;\n";
        let out = edit(
            content,
            &[
                rp("// 配置说明", "// 配置文档"),
                rp("let b = 2;", "let b = 3;"),
                rp("// 环境变量", "// 环境参数"),
            ],
        );
        assert_eq!(
            out.edited.as_deref(),
            Some("// 配置文档\nlet a = 1;\n// 环境参数\nlet b = 3;\n")
        );
        assert_eq!(err_code(&out), "");
    }

    // 中文锚点 InsertAfter：插入位置按 char 索引定位，中文行后插入正确。
    // （语义：new 自带换行，与 ASCII 用例 15 一致。）
    #[test]
    fn multibyte_insert_after_anchor() {
        let content = "// 头部注释\nbody();\n";
        let out = edit(
            content,
            &[Hunk::InsertAfter {
                anchor: "// 头部注释".to_string(),
                new: "// 追加注释\n".to_string(),
                hint_line: None,
            }],
        );
        assert_eq!(
            out.edited.as_deref(),
            Some("// 头部注释\n// 追加注释\nbody();\n")
        );
    }

    // 中文文件无尾换行 AppendFile：自动补换行（new 给纯内容，不带前导 \n）。
    #[test]
    fn multibyte_append_no_trailing_newline() {
        let content = "// 中文内容";
        let out = edit(
            content,
            &[Hunk::AppendFile {
                new: "// 追加行".to_string(),
            }],
        );
        assert_eq!(out.edited.as_deref(), Some("// 中文内容\n// 追加行"));
    }

    // 中文 + 尾换行语义：old 带 \n 的整段替换（删除尾行时区间含 \n）。
    #[test]
    fn multibyte_replace_with_trailing_newline() {
        let content = "第一行\n第二行\n第三行\n";
        let out = edit(content, &[rp("第二行\n", "第二行（改）\n")]);
        assert_eq!(
            out.edited.as_deref(),
            Some("第一行\n第二行（改）\n第三行\n")
        );
    }

    // ── overwrite（D10）──

    // O1. 整文件覆盖：new 全文替换，Tier1 恒命中
    #[test]
    fn overwrite_replaces_whole_file() {
        let out = edit("a\nb\nc\n", &[Hunk::Overwrite { new: "x\ny\n".into() }]);
        assert_eq!(out.edited.as_deref(), Some("x\ny\n"));
        assert_eq!(out.reports[0].tier, Some(1));
        assert_eq!(out.reports[0].kind, "overwrite");
        assert_eq!(out.reports[0].status, "ok");
    }

    // O2. 空内容（创建路径）上 overwrite = 创建
    #[test]
    fn overwrite_on_empty_content_creates() {
        let out = edit("", &[Hunk::Overwrite { new: "x\n".into() }]);
        assert_eq!(out.edited.as_deref(), Some("x\n"));
    }

    // O3. overwrite 与其他 hunk 混用 → OVERWRITE_EXCLUSIVE（独占语义）
    #[test]
    fn overwrite_mixed_with_other_hunks_rejected() {
        let out = edit(
            "a\n",
            &[rp("a", "b"), Hunk::Overwrite { new: "x\n".into() }],
        );
        assert_eq!(err_code(&out), "OVERWRITE_EXCLUSIVE");
        assert!(out.edited.is_none());
        assert_eq!(out.reports.len(), 0);
    }

    // O4. 两个 overwrite → OVERWRITE_EXCLUSIVE
    #[test]
    fn two_overwrites_rejected() {
        let out = edit(
            "a\n",
            &[
                Hunk::Overwrite { new: "x\n".into() },
                Hunk::Overwrite { new: "y\n".into() },
            ],
        );
        assert_eq!(err_code(&out), "OVERWRITE_EXCLUSIVE");
        assert!(out.edited.is_none());
    }

    // O5. parse：overwrite 只要 new；误传的 old 被忽略（整文件语义不看旧内容）
    #[test]
    fn overwrite_parse_ignores_old() {
        let mut notes = Vec::new();
        let h = Hunk::parse(
            &json!({"kind": "overwrite", "new": "x\ny\n", "old": "irrelevant"}),
            &mut notes,
        );
        assert_eq!(h, Ok(Hunk::Overwrite { new: "x\ny\n".into() }));
        // 缺 new → 报错
        let h2 = Hunk::parse(&json!({"kind": "overwrite"}), &mut notes);
        assert!(h2.is_err());
        // unknown kind 消息包含 overwrite
        let h3 = Hunk::parse(&json!({"kind": "nope", "new": "x"}), &mut notes);
        assert!(h3
            .unwrap_err()
            .contains("expected replace / overwrite / insert_after"));
    }

    // O6. exec 层：overwrite 创建新文件
    #[test]
    fn overwrite_creates_new_file_via_exec() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "hunks": [{"kind": "overwrite", "new": "hello\nworld\n"}],
        }));
        assert!(result.is_success(), "model text: {}", result.model.text);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\nworld\n");
        // 台账已记录（创建）
        assert!(crate::file_state::last_hash(&path.to_string_lossy()).is_some());
    }

    // O7. exec 层：overwrite 覆盖已有文件
    #[test]
    fn overwrite_replaces_existing_file_via_exec() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "old content\n").unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "hunks": [{"kind": "overwrite", "new": "brand new\n"}],
        }));
        assert!(result.is_success(), "model text: {}", result.model.text);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "brand new\n");
    }

    // O8. overwrite + expected_hash：错值拒绝（防覆盖竞争写），对值通过
    #[test]
    fn overwrite_with_expected_hash_guards_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a\nb\n").unwrap();
        let bad = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "expected_hash": "deadbeef",
            "hunks": [{"kind": "overwrite", "new": "x\n"}],
        }));
        assert!(!bad.is_success());
        assert_eq!(bad.data["code"], "HASH_MISMATCH");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\n");

        let good = content_hash("a\nb\n");
        let ok = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "expected_hash": good,
            "hunks": [{"kind": "overwrite", "new": "x\n"}],
        }));
        assert!(ok.is_success(), "model text: {}", ok.model.text);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x\n");
    }

    // O9. 不存在文件上误用 replace → NO_MATCH + 创建提示（hint 含 does not exist）
    #[test]
    fn replace_on_missing_file_hints_creation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ghost.txt");
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "hunks": [{"kind": "replace", "old": "x", "new": "y"}],
        }));
        assert!(!result.is_success());
        assert_eq!(result.data["code"], "NO_MATCH");
        let hint = result
            .error
            .as_ref()
            .and_then(|e| e.hint.as_deref())
            .unwrap_or("");
        assert!(
            hint.contains("does not exist"),
            "hint should guide creation, got: {hint}"
        );
        assert!(!path.exists());
    }

    // O10. exec 层混用 → OVERWRITE_EXCLUSIVE 透传
    #[test]
    fn overwrite_mixed_rejected_via_exec() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a\n").unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "hunks": [
                {"kind": "replace", "old": "a", "new": "b"},
                {"kind": "overwrite", "new": "x\n"}
            ],
        }));
        assert!(!result.is_success());
        assert_eq!(result.data["code"], "OVERWRITE_EXCLUSIVE");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n");
    }

    // ── 读路径（空 hunks = 读，二元组场景）──

    // R1. 空 hunks 读已有文件：read_only + content + hash + line_count，不写盘
    #[test]
    fn empty_hunks_reads_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        let mtime_before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "hunks": [],
        }));
        assert!(result.is_success(), "model text: {}", result.model.text);
        assert_eq!(result.data["read_only"], true);
        // 内容在 model_text（对齐 read_file：data 只放元数据）
        assert!(result.model_text().contains("a\nb\nc\n"));
        assert_eq!(result.data["line_count"], 3);
        assert_eq!(result.data["hash"], content_hash("a\nb\nc\n"));
        assert_eq!(result.data["truncated"], false);
        // 不写盘（mtime 不变）
        let mtime_after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after);
    }

    // R2. 省略 hunks 字段同样走读路径
    #[test]
    fn missing_hunks_reads_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "hello\n").unwrap();
        let result = exec_edit_file_v2(&json!({ "path": path.to_string_lossy() }));
        assert!(result.is_success(), "model text: {}", result.model.text);
        assert_eq!(result.data["read_only"], true);
        assert_eq!(result.data["hash"], content_hash("hello\n"));
    }

    // R3. 空 hunks 读不存在的文件 → FILE_NOT_FOUND，且不创建
    #[test]
    fn empty_hunks_on_missing_file_reports_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ghost.txt");
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "hunks": [],
        }));
        assert!(!result.is_success());
        assert_eq!(result.data["code"], "FILE_NOT_FOUND");
        assert!(!path.exists());
    }

    // R4. 读→编辑闭环：读的 hash 直接作 expected_hash 编辑成功
    #[test]
    fn read_hash_chains_into_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "x\n").unwrap();
        let read = exec_edit_file_v2(&json!({ "path": path.to_string_lossy(), "hunks": [] }));
        assert!(read.is_success());
        let h = read.data["hash"].as_str().unwrap().to_string();
        let edit = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "expected_hash": h,
            "hunks": [{"kind": "replace", "old": "x", "new": "y"}],
        }));
        assert!(edit.is_success(), "model text: {}", edit.model.text);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "y\n");
    }

    // ── 读路径扩展：行号范围读（grep 直连）──

    // R5. 范围读：L 前缀行 + 元数据，hash 为全文件 hash
    #[test]
    fn range_read_returns_prefixed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "start_line": 2,
            "end_line": 4,
        }));
        assert!(result.is_success(), "model text: {}", result.model.text);
        assert!(result.model_text().contains("L2: b\nL3: c\nL4: d"));
        assert_eq!(result.data["start_line"], 2);
        assert_eq!(result.data["end_line"], 4);
        assert_eq!(result.data["total_lines"], 5);
        assert_eq!(result.data["hash"], content_hash("a\nb\nc\nd\ne\n"));
        assert_eq!(result.data["truncated"], false);
    }

    // R6. 只给 start_line：读到文件尾
    #[test]
    fn range_read_start_only_reads_to_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "start_line": 4,
        }));
        assert!(result.is_success(), "model text: {}", result.model.text);
        assert!(result.model_text().contains("L4: d\nL5: e"));
        assert_eq!(result.data["end_line"], 5);
    }

    // R7. 越界 → LINE_OUT_OF_RANGE（带 total_lines + hash）
    #[test]
    fn range_read_out_of_range_reports_total_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "start_line": 6,
        }));
        assert!(!result.is_success());
        assert_eq!(result.data["code"], "LINE_OUT_OF_RANGE");
        assert_eq!(result.data["total_lines"], 3);
        assert_eq!(result.data["hash"], content_hash("a\nb\nc\n"));
    }

    // R8. end < start → PARSE_ERROR
    #[test]
    fn range_read_inverted_bounds_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "start_line": 4,
            "end_line": 2,
        }));
        assert!(!result.is_success());
        assert_eq!(result.data["code"], "PARSE_ERROR");
    }

    // R9. 行号从 1 开始
    #[test]
    fn range_read_zero_line_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a\n").unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "start_line": 0,
        }));
        assert!(!result.is_success());
        assert_eq!(result.data["code"], "PARSE_ERROR");
    }

    // R10. 行数超上限 → RANGE_TOO_LARGE
    #[test]
    fn range_read_too_many_lines_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "x\n".repeat(450)).unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "start_line": 1,
            "end_line": 450,
        }));
        assert!(!result.is_success());
        assert_eq!(result.data["code"], "RANGE_TOO_LARGE");
    }

    // R11. 字符预算超限 → RANGE_TOO_LARGE
    #[test]
    fn range_read_exceeding_char_budget_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        let line = "y".repeat(100);
        let content = (0..300).map(|_| line.clone()).collect::<Vec<_>>().join("\n");
        std::fs::write(&path, content).unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "start_line": 1,
            "end_line": 300,
        }));
        assert!(!result.is_success());
        assert_eq!(result.data["code"], "RANGE_TOO_LARGE");
    }

    // ── 读路径扩展：锚定读（复用定位引擎）──

    // R12. 锚定读基本：唯一命中 → 窗口 + anchor_line + tier + 全文件 hash
    #[test]
    fn anchored_read_locates_unique_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.rs");
        std::fs::write(
            &path,
            "line one\nfn foo(a: i32) -> i32 {\n    a + 1\n}\nline five\n",
        )
        .unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "anchor": "fn foo(a: i32) -> i32 {",
        }));
        assert!(result.is_success(), "model text: {}", result.model.text);
        assert!(result.model_text().contains("L2: fn foo(a: i32) -> i32 {"));
        assert_eq!(result.data["anchor_line"], 2);
        assert_eq!(result.data["anchor_lines"], 1);
        assert_eq!(result.data["tier"], 1);
        assert_eq!(result.data["start_line"], 1);
        assert_eq!(result.data["end_line"], 5);
        assert_eq!(result.data["total_lines"], 5);
        assert_eq!(
            result.data["hash"],
            content_hash("line one\nfn foo(a: i32) -> i32 {\n    a + 1\n}\nline five\n")
        );
    }

    // R13. 锚定读上下文窗口：context_before/context_after 控制展示范围
    #[test]
    fn anchored_read_context_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.rs");
        std::fs::write(
            &path,
            "line one\nfn foo(a: i32) -> i32 {\n    a + 1\n}\nline five\n",
        )
        .unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "anchor": "fn foo(a: i32) -> i32 {",
            "context_before": 1,
            "context_after": 1,
        }));
        assert!(result.is_success(), "model text: {}", result.model.text);
        assert_eq!(
            result.model_text(),
            "L1: line one\nL2: fn foo(a: i32) -> i32 {\nL3:     a + 1"
        );
        assert_eq!(result.data["start_line"], 1);
        assert_eq!(result.data["end_line"], 3);
    }

    // R14. 模糊锚 → ANCHOR_AMBIGUOUS + 候选（读阶段消歧，改阶段不再失败）
    #[test]
    fn anchored_read_ambiguous_returns_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "dup\ndup\n").unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "anchor": "dup",
        }));
        assert!(!result.is_success());
        assert_eq!(result.data["code"], "ANCHOR_AMBIGUOUS");
        let cands = result.data["candidates"].as_array().unwrap();
        assert!(!cands.is_empty());
        assert!(result
            .error
            .as_ref()
            .and_then(|e| e.hint.as_deref())
            .is_some_and(|h| h.contains("context_before")));
    }

    // R15. 未命中 → NO_MATCH + 候选
    #[test]
    fn anchored_read_no_match_returns_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "alpha\nbeta\n").unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "anchor": "zzz_nope",
        }));
        assert!(!result.is_success());
        assert_eq!(result.data["code"], "NO_MATCH");
    }

    // R16. 参数互斥与依赖校验
    #[test]
    fn read_mode_parameter_conflicts_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a\n").unwrap();
        // start_line 与 anchor 互斥
        let r1 = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "start_line": 1,
            "anchor": "a",
        }));
        assert!(!r1.is_success());
        assert_eq!(r1.data["code"], "PARSE_ERROR");
        // context 必须配 anchor
        let r2 = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "context_before": 3,
        }));
        assert!(!r2.is_success());
        assert_eq!(r2.data["code"], "PARSE_ERROR");
        // context 窗口超上限
        let r3 = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "anchor": "a",
            "context_after": 200,
        }));
        assert!(!r3.is_success());
        assert_eq!(r3.data["code"], "PARSE_ERROR");
    }

    // R17. 范围读的 hash 链进编辑（grep → 读 → 改 闭环）
    #[test]
    fn range_read_hash_chains_into_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        let read = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "start_line": 2,
            "end_line": 2,
        }));
        assert!(read.is_success());
        let h = read.data["hash"].as_str().unwrap().to_string();
        let edit = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "expected_hash": h,
            "hunks": [{"kind": "replace", "old": "b", "new": "B"}],
        }));
        assert!(edit.is_success(), "model text: {}", edit.model.text);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nB\nc\n");
    }

    // R18. 锚定读的 hash 链进编辑（锚定读 → 同锚编辑 闭环）
    #[test]
    fn anchored_read_hash_chains_into_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "pre\ntarget\npost\n").unwrap();
        let read = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "anchor": "target",
            "context_before": 1,
            "context_after": 1,
        }));
        assert!(read.is_success());
        let h = read.data["hash"].as_str().unwrap().to_string();
        let edit = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "expected_hash": h,
            "hunks": [{"kind": "insert_after", "anchor": "target", "new": "inserted\n"}],
        }));
        assert!(edit.is_success(), "model text: {}", edit.model.text);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "pre\ntarget\ninserted\npost\n"
        );
    }

    // ── replace_inline（sed `s///` 语义）──

    // R19. 基本：anchor 窗口内子串替换第一处，窗口外不受影响
    #[test]
    fn replace_inline_replaces_first_occurrence_in_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.rs");
        std::fs::write(&path, "fn main() {\n    let x = input.clone();\n    input\n}\n").unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "hunks": [{
                "kind": "replace_inline",
                "anchor": "let x = input.clone();",
                "old": "input",
                "new": "payload",
            }],
        }));
        assert!(result.is_success(), "model text: {}", result.model.text);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn main() {\n    let x = payload.clone();\n    input\n}\n"
        );
    }

    // R20. replace_all：窗口内全部替换（仍不跨窗口）
    #[test]
    fn replace_inline_replace_all_in_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.rs");
        std::fs::write(&path, "fn f() {\n    let a = alpha + alpha;\n    alpha\n}\n").unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "hunks": [{
                "kind": "replace_inline",
                "anchor": "let a = alpha + alpha;",
                "old": "alpha",
                "new": "beta",
                "replace_all": true,
            }],
        }));
        assert!(result.is_success(), "model text: {}", result.model.text);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn f() {\n    let a = beta + beta;\n    alpha\n}\n"
        );
    }

    // R21. 超长行场景（backend_prompt.md 案例）：整行锚 + 行内子串替换
    #[test]
    fn replace_inline_on_long_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        let long_line = format!("start {} end", "y".repeat(200));
        std::fs::write(&path, format!("a\n{long_line}\nb\n")).unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "hunks": [{
                "kind": "replace_inline",
                "anchor": long_line,
                "old": "yyy",
                "new": "ZZZ",
            }],
        }));
        assert!(result.is_success(), "model text: {}", result.model.text);
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, format!("a\nstart {} end\nb\n", "ZZZ".to_string() + &"y".repeat(197)));
    }

    // R22. 窗口内无 old → NO_MATCH
    #[test]
    fn replace_inline_missing_in_window_reports_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.rs");
        std::fs::write(&path, "fn foo() {\n    bar()\n}\n").unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "hunks": [{
                "kind": "replace_inline",
                "anchor": "fn foo() {",
                "old": "baz",
                "new": "qux",
            }],
        }));
        assert!(!result.is_success());
        assert_eq!(result.data["code"], "NO_MATCH");
        // 事务：文件未被修改
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn foo() {\n    bar()\n}\n");
    }

    // R23. regex 替换 + 捕获组引用
    #[test]
    fn replace_inline_regex_with_captures() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.rs");
        std::fs::write(&path, "fn f() {\n    let _a = 1;\n}\n").unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "hunks": [{
                "kind": "replace_inline",
                "anchor": "let _a = 1;",
                "old": r"let (\w+) = (\d+)",
                "new": "let $2 = $1",
                "regex": true,
            }],
        }));
        assert!(result.is_success(), "model text: {}", result.model.text);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn f() {\n    let 1 = _a;\n}\n"
        );
    }

    // R24. 非法正则 → INVALID_REGEX
    #[test]
    fn replace_inline_invalid_regex_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.rs");
        std::fs::write(&path, "fn f() {\n    x\n}\n").unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "hunks": [{
                "kind": "replace_inline",
                "anchor": "x",
                "old": "(",
                "new": "y",
                "regex": true,
            }],
        }));
        assert!(!result.is_success());
        assert_eq!(result.data["code"], "INVALID_REGEX");
    }

    // R25. 模糊锚 → AMBIGUOUS_MATCH（与 replace/insert 同款消歧）
    #[test]
    fn replace_inline_ambiguous_anchor_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "dup\ndup\n").unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "hunks": [{
                "kind": "replace_inline",
                "anchor": "dup",
                "old": "dup",
                "new": "DUP",
            }],
        }));
        assert!(!result.is_success());
        assert_eq!(result.data["code"], "AMBIGUOUS_MATCH");
    }

    // R26. 多 hunk 混用：replace_inline + replace 同批事务，失败整体拒绝
    #[test]
    fn replace_inline_mixed_with_replace_is_transactional() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.rs");
        std::fs::write(&path, "fn f() {\n    let a = alpha;\n    beta\n}\n").unwrap();
        let result = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "hunks": [
                {
                    "kind": "replace_inline",
                    "anchor": "let a = alpha;",
                    "old": "alpha",
                    "new": "gamma",
                },
                {"kind": "replace", "old": "zzz_nope", "new": "x"},
            ],
        }));
        assert!(!result.is_success());
        assert_eq!(result.data["code"], "NO_MATCH");
        // 整体拒绝：零改动
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn f() {\n    let a = alpha;\n    beta\n}\n"
        );
    }

    // R27. replace_inline 走 hash 链：读 → 改，返回 new_hash
    #[test]
    fn replace_inline_hash_chains() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.rs");
        std::fs::write(&path, "fn f() {\n    let x = input;\n}\n").unwrap();
        let read = exec_edit_file_v2(&json!({ "path": path.to_string_lossy() }));
        assert!(read.is_success());
        let h = read.data["hash"].as_str().unwrap().to_string();
        let edit = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "expected_hash": h,
            "hunks": [{
                "kind": "replace_inline",
                "anchor": "let x = input;",
                "old": "input",
                "new": "payload",
            }],
        }));
        assert!(edit.is_success(), "model text: {}", edit.model.text);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn f() {\n    let x = payload;\n}\n");
        assert!(edit.data.get("new_hash").is_some());
    }

    #[test]
    fn shifts_are_computed_for_line_level_edits() {
        let content = "a\nb\nc\nd\ne\n";
        // L2 的 b 换成两行 → (2, +1)；L4-5 的 d+e 行块换 e → (4, -1)
        let outcome = edit(content, &[rp("b", "b1\nb2"), rp("d\ne", "e")]);
        assert!(outcome.edited.is_some(), "strict edit should apply");
        let mut shifts = outcome.shifts.clone();
        shifts.sort(); // 应用序 = 倒序（位置大者先）；断言与顺序无关
        assert_eq!(shifts, vec![(2, 1), (4, -1)]);
    }

    #[test]
    fn inline_replace_has_zero_shift() {
        let content = "aaa\nbbb\nccc\n";
        let outcome = edit(content, &[rp("bbb", "BBB")]);
        assert!(outcome.edited.is_some());
        assert_eq!(outcome.shifts, vec![(2, 0)]);
    }

    #[test]
    fn failed_edit_has_empty_shifts() {
        let content = "aaa\nbbb\n";
        let outcome = edit(content, &[rp("zzz", "yyy")]);
        assert!(outcome.edited.is_none());
        assert!(outcome.shifts.is_empty());
    }

    // ── R30-R35：hint_line 宽松行号 ──────────────────────────────

    #[test]
    fn hint_line_disambiguates_repeated_content() {
        // x 出现两次（L3 与 L14），hint=14 的窗口 [4,24) 只含第二个 → 唯一命中。
        let content = "a\nb\nx\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nx\nm\n";
        let out = edit(content, &[rp_hint("x", "X", 14)]);
        assert!(out.edited.is_some(), "hint window should disambiguate");
        // 只替换 L14 的 x（L3 保持小写）。
        assert_eq!(
            out.edited.as_deref(),
            Some("a\nb\nx\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nX\nm\n")
        );
        let r = &out.reports[0];
        assert_eq!(r.status, "ok");
        assert_eq!(r.tier, Some(4));
        assert_eq!(r.used_hint, Some(14));
        assert_eq!(r.line_range, Some((14, 14)));
    }

    #[test]
    fn hint_line_window_still_ambiguous_keeps_error() {
        // 两个 x 都在窗口 [1,12) 内 → hint 无法消歧 → 保持 AMBIGUOUS_MATCH。
        let content = "x\nx\ny\n";
        let out = edit(content, &[rp_hint("x", "X", 2)]);
        assert!(out.edited.is_none());
        assert_eq!(err_code(&out), "AMBIGUOUS_MATCH");
        assert_eq!(out.reports[0].used_hint, None);
    }

    #[test]
    fn hint_line_miss_keeps_original_error() {
        // hint 偏太远（窗口内无 x）→ 保留原 Ambiguous（不误改）。
        // x 位于 L2 与 L25；hint=13 的窗口 [3,23) 不含任何 x。
        let content = "a\nx\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nt\nu\nv\nw\nx\ny\n";
        let out = edit(content, &[rp_hint("x", "X", 13)]);
        assert!(out.edited.is_none());
        assert_eq!(err_code(&out), "AMBIGUOUS_MATCH");
    }

    #[test]
    fn hint_line_disambiguates_anchor_insert() {
        // anchor "x" 两处，hint=14 窗口只含第二个 → insert_before 落到 L14。
        let content = "a\nb\nx\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nx\nm\n";
        let out = edit(
            content,
            &[Hunk::InsertBefore {
                anchor: "x".into(),
                new: "INS\n".into(),
                hint_line: Some(14),
            }],
        );
        assert!(out.edited.is_some(), "hint should disambiguate the anchor");
        assert_eq!(
            out.edited.as_deref(),
            Some("a\nb\nx\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nINS\nx\nm\n")
        );
        assert_eq!(out.reports[0].used_hint, Some(14));
    }

    #[test]
    fn no_hint_line_keeps_default_behavior() {
        // 无 hint_line：多命中 → 原样 AMBIGUOUS_MATCH（默认路径零变化）。
        let content = "a\nx\nb\nx\n";
        let out = edit(content, &[rp("x", "X")]);
        assert!(out.edited.is_none());
        assert_eq!(err_code(&out), "AMBIGUOUS_MATCH");
    }

    #[test]
    fn hint_line_zero_is_treated_as_one() {
        // hint_line=0（LSP 习惯）→ 视为 1；窗口 [1,11) 含第一个 x（L2 的 0-based 1）。
        let content = "a\nx\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\nx\n";
        let out = edit(content, &[rp_hint("x", "X", 0)]);
        assert!(out.edited.is_some());
        // 替换的是 0-based 1（L2）的 x。
        assert_eq!(out.edited.as_deref(), Some(content.replacen("x", "X", 1).as_str()));
        assert_eq!(out.reports[0].line_range, Some((2, 2)));
        assert_eq!(out.reports[0].used_hint, Some(1));
    }
    #[test]
    fn hint_line_via_json_parses_and_reports() {
        // JSON 解析层："hint_line" 字段 → parse_hint_line → 窗口消歧 → JSON 回传。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.txt");
        std::fs::write(&path, "a\nb\nx\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nx\nm\n").unwrap();
        let r = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "hunks": [{"kind": "replace", "old": "x", "new": "X", "hint_line": 14}],
        }));
        assert!(r.is_success(), "model text: {}", r.model.text);
        // 只改 L14 的 x（L3 保持小写）。
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "a\nb\nx\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nX\nm\n"
        );
        let hr = &r.data["hunks"][0];
        assert_eq!(hr["used_hint"], 14);
        assert_eq!(hr["actual_line"], 14);
        assert_eq!(hr["line_offset"], 0);
    }

    #[test]
    fn hint_line_zero_via_json_is_normalized_to_one() {
        // 两个 x 相距 >20 行（L2 与 L16），hint_line=0 → 归一化 1 → 窗口只含
        // 第一个 → 消歧成功，used_hint 回传归一化后的 1。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h0.txt");
        std::fs::write(
            &path,
            "a\nx\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\nx\n",
        )
        .unwrap();
        let r = exec_edit_file_v2(&json!({
            "path": path.to_string_lossy(),
            "hunks": [{"kind": "replace", "old": "x", "new": "X", "hint_line": 0}],
        }));
        assert!(r.is_success(), "model text: {}", r.model.text);
        // 只改 L2 的 x（L16 保持小写）。
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "a\nX\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\nx\n"
        );
        assert_eq!(r.data["hunks"][0]["used_hint"], 1);
        assert_eq!(r.data["hunks"][0]["actual_line"], 2);
    }
}

