//! 统一文件编辑工具 `edit_file`。
//!
//! 设计原则（替代移植自 codex 的 apply_patch）：
//!
//! - **包容（格式）**：三种定位原语任意组合——
//!   - `old_string`：字符串定位（Claude edit_file 风格；单行=行内子串，含换行=行窗口）
//!   - `old_lines`：行数组定位（行序列窗口匹配）
//!   - `start_line`/`end_line`：行号定位（与 old_lines/old_string/expected_hash 交叉校验）
//!   单文件多块用 `ops` 数组；多文件并行用 `files` 数组。CRLF 自动归一化；
//!   默认 trim_end 容错，`allow_fuzzy` 启用空白折叠（tab/多空格）与 Unicode 归一化（NFC、智能标点）兜底。
//!
//! - **严格（命中）**：多 candidate 且无法用 `context_before/after` 消歧 → 拒绝并
//!   列出全部位置（绝不猜测）；行号模式逐行交叉校验（LINE_MISMATCH 带逐行对比）；
//!   `old_string` 与 `old_lines` 同时给出时校验定位一致（CROSS_CHECK）；
//!   `expected_hash` 防文件漂移。
//!
//! - **每 op 独立事务**：一个 op 失败只报告该 op（含 closest_line 提示），其余 op
//!   照常应用；文件内全部成功 op 应用后一次原子写盘。失败恢复只重试失败项，
//!   不需要整包重发（apply_patch 整批拒绝的痛点）。

use serde::Serialize;
use std::path::PathBuf;

use super::file_shared::{
    atomic_write, closest_line, content_hash, disambiguate_match, normalize_newlines, unified_diff,
    verify_expected_hash,
};
use crate::{ToolHandler, ToolResult, ToolRisk};

// ─────────────────────────────────────────────────────────────
// Op 解析
// ─────────────────────────────────────────────────────────────

/// 单个编辑操作（定位原语任意组合；new 侧 new_string 优先于 new_lines）。
#[derive(Debug, Clone, Default)]
struct EditOp {
    old_string: Option<String>,
    new_string: Option<String>,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    start_line: Option<usize>,
    end_line: Option<usize>,
    context_before: Vec<String>,
    context_after: Vec<String>,
    replace_all: bool,
    use_regex: bool,
    allow_fuzzy: bool,
    description: String,
}

impl EditOp {
    fn parse(v: &serde_json::Value) -> Result<Self, String> {
        let op = EditOp {
            old_string: v
                .get("old_string")
                .and_then(|x| x.as_str())
                .map(str::to_string),
            new_string: v
                .get("new_string")
                .and_then(|x| x.as_str())
                .map(str::to_string),
            old_lines: str_array(v, "old_lines"),
            new_lines: str_array(v, "new_lines"),
            start_line: v
                .get("start_line")
                .and_then(|x| x.as_u64())
                .map(|n| n as usize),
            end_line: v
                .get("end_line")
                .and_then(|x| x.as_u64())
                .map(|n| n as usize),
            context_before: str_array(v, "context_before"),
            context_after: str_array(v, "context_after"),
            replace_all: v
                .get("replace_all")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
            use_regex: v.get("regex").and_then(|x| x.as_bool()).unwrap_or(false),
            allow_fuzzy: v
                .get("allow_fuzzy")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
            description: v
                .get("description")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        };
        if op.old_string.is_none()
            && op.old_lines.is_empty()
            && op.start_line.is_none()
            && op.new_string.is_none()
            && op.new_lines.is_empty()
        {
            return Err(
                "op is empty: provide old_string/new_string, old_lines/new_lines, or start_line"
                    .into(),
            );
        }
        if op.new_string.is_none() && op.new_lines.is_empty() {
            return Err("op is missing a replacement: provide new_string or new_lines".into());
        }
        Ok(op)
    }
}

fn str_array(v: &serde_json::Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// 单文件编辑请求（files 数组的一项，或单文件模式的顶层参数）。
struct FileRequest {
    path: PathBuf,
    display: String,
    ops: Vec<EditOp>,
    expected_hash: Option<String>,
    dry_run: bool,
}

fn parse_file_request(v: &serde_json::Value) -> Result<FileRequest, String> {
    let path = v
        .get("path")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "edit_file: missing 'path'".to_string())?;
    let display = path.to_string();
    let path = crate::resolve_workspace_path(&path);
    let expected_hash = v
        .get("expected_hash")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let dry_run = v.get("dry_run").and_then(|x| x.as_bool()).unwrap_or(false);
    let ops = if let Some(ops) = v.get("ops").and_then(|x| x.as_array()) {
        if ops.is_empty() {
            return Err(format!("edit_file: ops array for '{display}' is empty"));
        }
        ops.iter()
            .enumerate()
            .map(|(i, op)| EditOp::parse(op).map_err(|e| format!("ops[{i}] of '{display}': {e}")))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        vec![EditOp::parse(v).map_err(|e| format!("'{display}': {e}"))?]
    };
    Ok(FileRequest {
        path: PathBuf::from(path),
        display,
        ops,
        expected_hash,
        dry_run,
    })
}

// ─────────────────────────────────────────────────────────────
// 匹配引擎（严格命中）
// ─────────────────────────────────────────────────────────────

#[derive(Debug)]
enum OpError {
    /// 模式未命中（携带最近行与最佳前缀诊断，模型可据此自纠）
    NoMatch {
        closest: Option<(usize, String)>,
        diagnostic: Option<String>,
    },
    /// 多处命中且无法消歧（严格：拒绝，绝不猜测）
    Ambiguous { candidates: Vec<usize> },
    /// 行号模式交叉校验失败（逐行对比）
    LineMismatch { detail: String },
    /// old_string 与 old_lines 定位不一致
    CrossCheck { detail: String },
    /// 其他（regex 错误等）
    Other { message: String },
}

/// 行窗口匹配：返回所有 candidate 起始行（0-based）与是否启用了 fuzzy。
/// 层级：trim_end 精确 → (allow_fuzzy) trim → (allow_fuzzy) Unicode NFC + 行内空白折叠。
fn find_windows(file_lines: &[&str], pattern: &[String], allow_fuzzy: bool) -> (Vec<usize>, bool) {
    if pattern.is_empty() || pattern.len() > file_lines.len() {
        return (Vec::new(), false);
    }
    let norm: Vec<String> = pattern.iter().map(|l| l.trim_end().to_string()).collect();
    let mut candidates: Vec<usize> = Vec::new();
    for i in 0..=file_lines.len() - pattern.len() {
        let win: Vec<String> = file_lines[i..i + pattern.len()]
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect();
        if win == norm {
            candidates.push(i);
        }
    }
    if !candidates.is_empty() {
        return (candidates, false);
    }
    if !allow_fuzzy {
        return (candidates, false);
    }
    let trim_norm: Vec<String> = pattern.iter().map(|l| l.trim().to_string()).collect();
    for i in 0..=file_lines.len() - pattern.len() {
        let win: Vec<String> = file_lines[i..i + pattern.len()]
            .iter()
            .map(|l| l.trim().to_string())
            .collect();
        if win == trim_norm {
            candidates.push(i);
        }
    }
    if !candidates.is_empty() {
        return (candidates, true);
    }
    let uni_norm: Vec<String> = pattern.iter().map(|l| normalise(l)).collect();
    for i in 0..=file_lines.len() - pattern.len() {
        let win: Vec<String> = file_lines[i..i + pattern.len()]
            .iter()
            .map(|l| normalise(l))
            .collect();
        if win == uni_norm {
            candidates.push(i);
        }
    }
    (candidates, true)
}

/// Unicode/空白归一化（allow_fuzzy 兜底层）：
/// trim → NFC 组合字符折叠（é 与 e+◌́ 等价）→ 智能标点/破折号/空格类 → ASCII → 行内空白折叠（tab/多空格 → 单空格）。
fn normalise(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    s.trim()
        .nfc()
        .map(|c| match c {
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
            | '\u{3000}' => ' ',
            other => other,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 字节位置 → 1-based 行号（按换行符计数，行首位置正确）。
fn line_of(content: &str, byte_pos: usize) -> usize {
    content[..byte_pos].matches('\n').count() + 1
}

/// 多行字符串 → 行数组：只去掉首尾空元素（由换行边界产生），**保留中间空行**。
/// old_lines / 多行 old_string / 多行 new_string 统一走这里，
/// 避免空行被整体过滤导致行窗口错位（旧行为）。
fn multiline_string_lines(s: &str) -> Vec<String> {
    let lines: Vec<String> = s.split('\n').map(str::to_string).collect();
    let mut start = 0;
    let mut end = lines.len();
    while start < end && lines[start].trim().is_empty() {
        start += 1;
    }
    while end > start && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    lines[start..end].to_vec()
}
/// NO_MATCH 诊断：以 closest_line 为锚，在附近窗口找"最佳前缀匹配"，
/// 报告匹配进度、首个失配行对比与转义差异提示。模型据此可直接修正
/// 定位（转义/空白/内容差异），无需整文件重读。
fn no_match_diagnostic(content: &str, pattern: &[String]) -> Option<String> {
    if pattern.is_empty() {
        return None;
    }
    let lines: Vec<&str> = content.lines().collect();
    let needle = pattern.first().cloned().unwrap_or_default();
    let anchor = closest_line(content, &needle)?;
    let anchor_idx = anchor.0.saturating_sub(1);
    let lo = anchor_idx.saturating_sub(8);
    let hi = (anchor_idx + 8).min(lines.len().saturating_sub(1));
    let mut best_start = lo;
    let mut best_common = 0usize;
    for s in lo..=hi {
        let mut common = 0usize;
        for (k, p) in pattern.iter().enumerate() {
            if s + k >= lines.len() {
                break;
            }
            if lines[s + k].trim_end() == p.trim_end() {
                common += 1;
            } else {
                break;
            }
        }
        if common > best_common {
            best_common = common;
            best_start = s;
        }
    }
    if best_common == 0 {
        let mut out = format!(
            "no line matches; closest line is L{}: {}",
            anchor.0,
            truncate_line(&anchor.1, 200)
        );
        // 单行模式（pattern 只有一行）最常在这里命中转义差异
        let anchor_trim = anchor.1.trim();
        if anchor_trim.replace('\\', "") == needle.replace('\\', "") && anchor_trim != needle {
            out.push_str(
                "\n  [HINT] escape mismatch: the file contains backslash-escaped characters (e.g. \\\") that differ from your pattern — copy the exact characters from read_file output.",
            );
        }
        return Some(out);
    }
    if best_common >= pattern.len() {
        return None; // 理论不可达：NO_MATCH 已排除全匹配
    }
    let mismatch_idx = best_start + best_common;
    let actual = lines.get(mismatch_idx).copied().unwrap_or("");
    let expected = &pattern[best_common];
    let mut out = format!(
        "best partial match: {} of {} lines at L{}; first mismatch at pattern line {}:\n  L{} actual:   {}\n  L{} expected: {}",
        best_common,
        pattern.len(),
        best_start + 1,
        best_common + 1,
        mismatch_idx + 1,
        truncate_line(actual, 200),
        mismatch_idx + 1,
        truncate_line(expected, 200)
    );
    // 转义差异提示：文件里是 \" 而模式里是 "（或反之）——直接命中复制粘贴问题
    if actual.replace('\\', "") == expected.replace('\\', "") && actual != expected {
        out.push_str(
            "\n  [HINT] escape mismatch: the file contains backslash-escaped characters (e.g. \\\") that differ from your pattern — copy the exact characters from read_file output.",
        );
    }
    Some(out)
}
fn truncate_line(s: &str, max: usize) -> String {
    let cut = s.floor_char_boundary(s.len().min(max));
    let mut out = s[..cut].to_string();
    if cut < s.len() {
        out.push('…');
    }
    out
}
/// 定位单个 op 在内容中的命中窗口。返回 (start_line 0-based, 行数, was_fuzzy)。
fn locate_op(
    content: &str,
    file_lines: &[&str],
    op: &EditOp,
) -> Result<(usize, usize, bool), OpError> {
    // replace_all 的合法场景（单行子串 + 非 regex）已被 apply_op 快路径短路；
    // 到达这里说明是 unsupported 组合——显式拒绝而非静默降级。
    if op.replace_all {
        if op.use_regex {
            return Err(OpError::Other {
                message: "replace_all=true is not supported with regex=true — regex mode replaces only the first match; drop replace_all or use old_string (substring mode)".into(),
            });
        }
        return Err(OpError::Other {
            message: "replace_all=true is only supported for a single-line old_string (substring mode); use ops to repeat targeted edits".into(),
        });
    }
    if op.use_regex {
        return locate_regex(content, file_lines, op);
    }

    // ── 行号定位（交叉校验）──
    if let Some(start) = op.start_line {
        let s = start.saturating_sub(1);
        let e = op.end_line.map(|n| n.saturating_sub(1)).unwrap_or(s);
        if s >= file_lines.len() {
            return Err(OpError::Other {
                message: format!(
                    "start_line {start} is past end of file ({} lines)",
                    file_lines.len()
                ),
            });
        }
        let e = e.min(file_lines.len().saturating_sub(1));
        if s > e {
            return Err(OpError::Other {
                message: format!(
                    "start_line {start} > end_line {}",
                    op.end_line.unwrap_or(start)
                ),
            });
        }
        // 行号 + 无任何内容校验时，必须提供 expected_hash（严格：防漂移定位）
        if op.old_lines.is_empty() && op.old_string.is_none() {
            // execute_file 层已做 UNVERIFIED_LINE_EDIT 预检（需要 expected_hash）；
            // 到达这里说明文件级 expected_hash 已校验通过，直接按窗口替换。
            return Ok((s, e - s + 1, false));
        }
        // 校验行号窗口内容（若有 old_lines / old_string）
        if !op.old_lines.is_empty() {
            let actual: Vec<&str> = file_lines[s..=e].to_vec();
            let norm_actual: Vec<String> =
                actual.iter().map(|l| l.trim_end().to_string()).collect();
            let norm_old: Vec<String> = op
                .old_lines
                .iter()
                .map(|l| l.trim_end().to_string())
                .collect();
            if norm_actual != norm_old {
                let mut ctx = String::new();
                for (i, line) in actual.iter().enumerate() {
                    if i >= norm_old.len() || line.trim_end() != norm_old[i] {
                        ctx.push_str(&format!("  L{} actual: {}\n", s + i + 1, line));
                        if i < norm_old.len() {
                            ctx.push_str(&format!("  L{} old_lines: {}\n", s + i + 1, norm_old[i]));
                        }
                    }
                }
                return Err(OpError::LineMismatch {
                    detail: format!(
                        "start_line={start}: old_lines do not match actual content at lines {}-{}\n{ctx}",
                        s + 1,
                        e + 1
                    ),
                });
            }
            return Ok((s, e - s + 1, false));
        }
        if let Some(old) = &op.old_string {
            let window: Vec<&str> = file_lines[s..=e].to_vec();
            let want = multiline_string_lines(old);
            if !want.is_empty() {
                let norm_win: Vec<String> =
                    window.iter().map(|l| l.trim_end().to_string()).collect();
                let norm_want: Vec<String> =
                    want.iter().map(|l| l.trim_end().to_string()).collect();
                if norm_win != norm_want {
                    return Err(OpError::LineMismatch {
                        detail: format!(
                            "start_line={start}: old_string does not match actual content at lines {}-{}",
                            s + 1,
                            e + 1
                        ),
                    });
                }
            }
        }
        // 行号 + 内容校验通过（或 expected_hash 已校验）：按窗口替换
        return Ok((s, e - s + 1, false));
    }

    // ── 单行子串定位（old_string 无换行，Claude 风格）──
    if let Some(old) = &op.old_string
        && !old.contains('\n')
        && !old.is_empty()
    {
        // old_string 与 old_lines 同时给出 → 交叉校验（严格：两处定位必须一致）
        if !op.old_lines.is_empty() {
            let (s_lines, _, _) = locate_line_window(file_lines, op)?;
            let positions: Vec<usize> = content.match_indices(old).map(|(i, _)| i).collect();
            let s_sub = match positions.len() {
                0 => {
                    return Err(OpError::NoMatch {
                        closest: closest_line(content, old),
                        diagnostic: no_match_diagnostic(content, &[old.to_string()]),
                    });
                }
                1 => line_of(content, positions[0]).saturating_sub(1),
                _ if op.replace_all => line_of(content, positions[0]).saturating_sub(1),
                _ => {
                    let lines: Vec<usize> =
                        positions.iter().map(|&p| line_of(content, p)).collect();
                    return Err(OpError::Ambiguous { candidates: lines });
                }
            };
            if s_sub != s_lines {
                return Err(OpError::CrossCheck {
                    detail: format!(
                        "old_string locates L{} but old_lines locates L{}",
                        s_sub + 1,
                        s_lines + 1
                    ),
                });
            }
            return Ok((s_sub, 1, false));
        }
        let positions: Vec<usize> = content.match_indices(old).map(|(i, _)| i).collect();
        if !positions.is_empty() {
            if positions.len() > 1 && !op.replace_all {
                // 多候选：先用 context_before/context_after 做行级消歧（Claude 风格）
                let mut cand_lines: Vec<usize> = positions
                    .iter()
                    .map(|&p| line_of(content, p).saturating_sub(1))
                    .collect();
                cand_lines.sort_unstable();
                cand_lines.dedup();
                let mut per_line: std::collections::HashMap<usize, usize> =
                    std::collections::HashMap::new();
                for &p in &positions {
                    *per_line
                        .entry(line_of(content, p).saturating_sub(1))
                        .or_insert(0) += 1;
                }
                if !op.context_before.is_empty() || !op.context_after.is_empty() {
                    if let Ok(line) = disambiguate_match(
                        &cand_lines,
                        &op.context_before,
                        &op.context_after,
                        file_lines,
                        "",
                        1,
                    ) {
                        // 选中行内仍有多个命中 → context 无法消歧到具体位置，保持拒绝
                        if per_line.get(&line).copied().unwrap_or(1) == 1 {
                            return Ok((line, 1, false));
                        }
                    }
                }
                let lines: Vec<usize> = cand_lines.iter().map(|l| l + 1).collect();
                return Err(OpError::Ambiguous { candidates: lines });
            }
            return Ok((line_of(content, positions[0]).saturating_sub(1), 1, false));
        }
        // 子串未命中 → 回退单行窗口（trim_end/fuzzy 容错）
        let pattern = vec![old.clone()];
        let (candidates, fuzzy) = find_windows(file_lines, &pattern, op.allow_fuzzy);
        let idx = disambiguate_op(candidates, file_lines, op, &pattern)?;
        return Ok((idx, 1, fuzzy));
    }

    // ── 行窗口定位（old_lines 或多行 old_string）──
    let (start, win, fuzzy) = locate_line_window(file_lines, op)?;
    Ok((start, win, fuzzy))
}

/// 行窗口定位：old_lines 或多行 old_string → (start, win, fuzzy)。
fn locate_line_window(file_lines: &[&str], op: &EditOp) -> Result<(usize, usize, bool), OpError> {
    let pattern: Vec<String> = if !op.old_lines.is_empty() {
        op.old_lines.clone()
    } else if let Some(old) = &op.old_string {
        multiline_string_lines(old)
    } else {
        return Err(OpError::Other {
            message: "op has no locator: provide old_string, old_lines, or start_line".into(),
        });
    };
    if pattern.is_empty() {
        return Err(OpError::NoMatch {
            closest: None,
            diagnostic: None,
        });
    }
    let (candidates, fuzzy) = find_windows(file_lines, &pattern, op.allow_fuzzy);
    let idx = disambiguate_op(candidates, file_lines, op, &pattern)?;
    Ok((idx, pattern.len(), fuzzy))
}

/// 行窗口 candidate 消歧（严格：无法唯一确定 → Ambiguous）。
fn disambiguate_op(
    candidates: Vec<usize>,
    file_lines: &[&str],
    op: &EditOp,
    pattern: &[String],
) -> Result<usize, OpError> {
    if candidates.is_empty() {
        let needle = pattern.first().cloned().unwrap_or_default();
        let content = file_lines.join("\n");
        let closest = closest_line(&content, &needle);
        return Err(OpError::NoMatch {
            closest,
            diagnostic: no_match_diagnostic(&content, pattern),
        });
    }
    if candidates.len() == 1 {
        return Ok(candidates[0]);
    }
    match disambiguate_match(
        &candidates,
        &op.context_before,
        &op.context_after,
        file_lines,
        "",
        pattern.len(),
    ) {
        Ok(idx) => Ok(idx),
        Err(_) => Err(OpError::Ambiguous { candidates }),
    }
}

/// 正则定位（保留 edit 的 regex 能力）。
fn locate_regex(
    content: &str,
    file_lines: &[&str],
    op: &EditOp,
) -> Result<(usize, usize, bool), OpError> {
    let old = op.old_string.as_deref().ok_or_else(|| OpError::Other {
        message: "regex mode requires old_string".into(),
    })?;
    let re = regex::Regex::new(old).map_err(|e| OpError::Other {
        message: format!("invalid regex: {e}"),
    })?;
    let mut positions: Vec<usize> = re.find_iter(content).map(|m| m.start()).collect();
    if positions.is_empty() {
        return Err(OpError::NoMatch {
            closest: closest_line(content, old),
            diagnostic: no_match_diagnostic(content, &[old.to_string()]),
        });
    }
    if positions.len() > 1 && !op.replace_all {
        let lines: Vec<usize> = positions.iter().map(|&p| line_of(content, p)).collect();
        return Err(OpError::Ambiguous { candidates: lines });
    }
    positions.truncate(1);
    let line = line_of(content, positions[0]);
    let _ = file_lines;
    Ok((line.saturating_sub(1), 1, false))
}

// ─────────────────────────────────────────────────────────────
// 应用引擎
// ─────────────────────────────────────────────────────────────

/// op 应用结果（成功或失败，独立事务）。
#[derive(Debug, Serialize)]
struct OpReport {
    index: usize,
    status: &'static str,
    // 成功
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fuzzy: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff: Option<String>,
    // 失败
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    closest_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    closest_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    candidates: Option<Vec<usize>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mismatch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

impl OpReport {
    fn ok(
        index: usize,
        line: usize,
        added: usize,
        removed: usize,
        fuzzy: bool,
        diff: Option<String>,
        description: String,
    ) -> Self {
        Self {
            index,
            status: "ok",
            line: Some(line),
            summary: Some(format!("+{added} -{removed}")),
            description: (!description.is_empty()).then_some(description),
            fuzzy: Some(fuzzy),
            diff,
            code: None,
            closest_line: None,
            closest_text: None,
            candidates: None,
            mismatch: None,
            diagnostic: None,
            hint: None,
        }
    }

    fn err(index: usize, e: OpError, display: &str) -> Self {
        let (code, closest_line, closest_text, candidates, mismatch, diagnostic, hint) = match e {
            OpError::NoMatch {
                closest,
                diagnostic,
            } => (
                "NO_MATCH",
                closest.as_ref().map(|(l, _)| *l),
                closest.map(|(_, t)| t),
                None,
                None,
                diagnostic,
                Some(format!(
                    "pattern not found in {display}. Use read_file to check current content, then retry with corrected old_* or start_line."
                )),
            ),
            OpError::Ambiguous { candidates } => (
                "AMBIGUOUS_MATCH",
                None,
                None,
                Some(candidates.clone()),
                None,
                None,
                Some(format!(
                    "pattern matches at {} locations: {} — add context_before/context_after to disambiguate, or use start_line, or set replace_all=true.",
                    candidates.len(),
                    candidates.iter().take(8).map(|l| format!("L{l}")).collect::<Vec<_>>().join(", ")
                )),
            ),
            OpError::LineMismatch { detail } => (
                "LINE_MISMATCH",
                None,
                None,
                None,
                Some(detail.clone()),
                None,
                Some("File content has changed since the referenced read. Use read_file to re-read and retry with corrected old_lines/start_line.".into()),
            ),
            OpError::CrossCheck { detail } => (
                "CROSS_CHECK",
                None,
                None,
                None,
                Some(detail),
                None,
                Some("old_string and old_lines locate different positions — keep only one locator per op, or align them.".into()),
            ),
            OpError::Other { message } => (
                "OP_ERROR",
                None,
                None,
                None,
                Some(message.clone()),
                None,
                Some(format!("op #{index} of {display} failed: {message}")),
            ),
        };
        Self {
            index,
            status: "error",
            line: None,
            summary: None,
            description: None,
            fuzzy: None,
            diff: None,
            code: Some(code.into()),
            closest_line,
            closest_text,
            candidates,
            mismatch,
            diagnostic,
            hint: hint.map(|h| {
                format!("{h}\n       (op #{index} of {display} failed; other ops are unaffected)")
            }),
        }
    }
}

/// 对当前内容应用单个 op。返回 (新内容, 报告)。
fn apply_op(
    content: &str,
    op: &EditOp,
    index: usize,
    display: &str,
    dry_run: bool,
) -> (String, OpReport) {
    // replace_all 子串全量替换（不经过行窗口，直接 content.replace）
    if op.replace_all
        && !op.use_regex
        && let Some(old) = &op.old_string
        && !old.contains('\n')
    {
        let count = content.matches(old).count();
        if count == 0 {
            return (
                content.to_string(),
                OpReport::err(
                    index,
                    OpError::NoMatch {
                        closest: closest_line(content, old),
                        diagnostic: no_match_diagnostic(content, &[old.to_string()]),
                    },
                    display,
                ),
            );
        }
        let new_content = content.replace(old, op.new_string.as_deref().unwrap_or(""));
        return finish_op(
            content,
            &new_content,
            0,
            op.new_string.as_deref().map_or(0, |s| s.lines().count()),
            count,
            false,
            index,
            display,
            dry_run,
            op,
        );
    }

    // 定位
    let file_lines: Vec<&str> = content.lines().collect();
    let locate = locate_op(content, &file_lines, op);
    let (start, win, fuzzy) = match locate {
        Ok(v) => v,
        Err(e) => return (content.to_string(), OpReport::err(index, e, display)),
    };

    // 应用：行窗口替换（统一路径；单行子串命中时行内替换）
    let new_lines: Vec<String> = if !op.new_lines.is_empty() {
        op.new_lines.clone()
    } else if let Some(new) = &op.new_string {
        if new.contains('\n') {
            multiline_string_lines(new)
        } else {
            vec![new.clone()]
        }
    } else {
        Vec::new()
    };

    let mut out_lines: Vec<&str> = file_lines.clone();
    let replaced_lines: Vec<&str> = out_lines[start..start + win].to_vec();
    out_lines.splice(start..start + win, std::iter::empty());
    for (j, line) in new_lines.iter().enumerate() {
        out_lines.insert(start + j, line.as_str());
    }
    // 单行子串命中（old_string 无换行、行窗口未启用 fuzzy、win==1 且替换只有一行）：
    // 在命中行内做子串替换，避免整行重写导致行内多余内容被覆盖。
    let mut final_lines = out_lines;
    if let Some(old) = &op.old_string
        && !old.contains('\n')
        && !op.use_regex
        && win == 1
        && new_lines.len() == 1
    {
        let line_before = replaced_lines
            .first()
            .map(|l| l.to_string())
            .unwrap_or_default();
        if line_before.contains(old) {
            let replaced = line_before.replacen(old, &new_lines[0], 1);
            final_lines[start] = replaced.as_str();
            // 重新 join 以反映行内替换
            let mut rebuilt = final_lines.join("\n");
            if content.ends_with('\n') && !rebuilt.ends_with('\n') {
                rebuilt.push('\n');
            }
            return finish_op(
                content, &rebuilt, start, 1, 1, fuzzy, index, display, dry_run, op,
            );
        }
    }

    let mut new_content = final_lines.join("\n");
    if content.ends_with('\n') && !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    let added = new_lines.len();
    let removed = win;
    finish_op(
        content,
        &new_content,
        start,
        added,
        removed,
        fuzzy,
        index,
        display,
        dry_run,
        op,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish_op(
    original: &str,
    new_content: &str,
    start: usize,
    added: usize,
    removed: usize,
    fuzzy: bool,
    index: usize,
    display: &str,
    dry_run: bool,
    _op: &EditOp,
) -> (String, OpReport) {
    // 成功 op 只回报位置与增减统计（模型知道改了什么，省 token）；
    // dry_run 时给完整 diff 供确认命中位置。
    let diff = if dry_run {
        Some(unified_diff(original, new_content, display))
    } else {
        None
    };
    (
        new_content.to_string(),
        OpReport::ok(
            index,
            start + 1,
            added,
            removed,
            fuzzy,
            diff,
            _op.description.clone(),
        ),
    )
}

// ─────────────────────────────────────────────────────────────
// 文件级执行
// ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct FileReport {
    path: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    write_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<String>,
    ops: Vec<OpReport>,
}

fn execute_file(req: &FileRequest) -> FileReport {
    let raw = match std::fs::read_to_string(&req.path) {
        Ok(c) => c,
        Err(e) => {
            return FileReport {
                path: req.display.clone(),
                status: "error",
                diff: None,
                write_error: None,
                total: None,
                ops: vec![OpReport {
                    index: 0,
                    status: "error",
                    line: None,
                    summary: None,
                    description: None,
                    fuzzy: None,
                    diff: None,
                    code: Some("READ_FAILED".into()),
                    closest_line: None,
                    closest_text: None,
                    candidates: None,
                    mismatch: Some(e.to_string()),
                    diagnostic: None,
                    hint: Some(format!(
                        "Cannot read {}. Use exec with argv [\"ls\", \"-la\"] (or rg --files) to inspect the parent directory first.",
                        req.display
                    )),
                }],
            };
        }
    };
    // 换行统一契约（LF canonical view）：
    // read 的展示、行号、hash 全部基于 LF 归一化视图（file_query.rs 同款
    // replace("\r\n","\n") + replace('\r',"\n")）；edit_file 的匹配、hash
    // 校验必须在同一视图上进行，否则 CRLF 文件下 expected_hash 永远失配。
    // 写回时按 was_crlf 还原原始换行（最小 diff），新内容与 diff 均在 LF 视图。
    let (mut content, was_crlf) = normalize_newlines(&raw);
    if let Err(error) = verify_expected_hash(&req.display, &content, req.expected_hash.as_deref()) {
        return FileReport {
            path: req.display.clone(),
            status: "error",
            diff: None,
            write_error: None,
            total: None,
            ops: vec![OpReport {
                index: 0,
                status: "error",
                line: None,
                summary: None,
                description: None,
                fuzzy: None,
                diff: None,
                code: Some("STALE_FILE".into()),
                closest_line: None,
                closest_text: None,
                candidates: None,
                mismatch: Some(error),
                diagnostic: None,
                hint: Some(
                    "Use read_file to obtain current content and hash, then retry the edit.".into(),
                ),
            }],
        };
    }

    // ── 工具侧账本（自动防漂移，模型无需回传 hash）──
    // disk_hash：当前磁盘的 LF 视图指纹；ledger_hash：工具最近一次
    // read/edit/write 该文件时记录的同视图指纹。两者仅用于行号盲定位
    // （start_line 且无 old_string/old_lines）的安全放行——内容定位自带校验，
    // 从不依赖账本。账本失配不拒绝内容定位（命中即安全），只挡盲定位。
    let disk_hash = content_hash(&content);
    let ledger_hash = crate::file_state::last_hash(&req.path.to_string_lossy());

    let original_lf = content.clone();
    let mut reports: Vec<OpReport> = Vec::with_capacity(req.ops.len());
    for (i, op) in req.ops.iter().enumerate() {
        // 行号盲定位（start_line 且无内容校验）的防漂移：
        // 1) 模型显式带 expected_hash → 已由文件级校验覆盖；
        // 2) 否则用工具侧账本自动校验：账本匹配 → 放行；失配（工具外修改）
        //    → STALE_FILE；无账本（本会话从未 read）→ UNVERIFIED_LINE_EDIT，
        //    提示先 read 一次（read 后自动建立账本，**无需手动回传 hash**）。
        if op.start_line.is_some() && op.old_lines.is_empty() && op.old_string.is_none() {
            if req.expected_hash.is_none() {
                match &ledger_hash {
                    None => {
                        reports.push(OpReport {
                            index: i,
                            status: "error",
                            line: None,
                            summary: None,
                        description: None,
                            fuzzy: None,
                            diff: None,
                            code: Some("UNVERIFIED_LINE_EDIT".into()),
                            closest_line: None,
                            closest_text: None,
                            candidates: None,
                            mismatch: Some(format!(
                                "op #{i}: start_line without old_lines/old_string has no verification baseline — this file was not read this session"
                            )),
                            diagnostic: None,
                            hint: Some(format!(
                                "Use read_file on this file once — the tool then tracks its state automatically and start_line becomes safe (no hash needed). (op #{i} of {} failed; other ops are unaffected)",
                                req.display
                            )),
                        });
                        continue;
                    }
                    Some(known) if known != &disk_hash => {
                        reports.push(OpReport {
                            index: i,
                            status: "error",
                            line: None,
                            summary: None,
                        description: None,
                            fuzzy: None,
                            diff: None,
                            code: Some("STALE_FILE".into()),
                            closest_line: None,
                            closest_text: None,
                            candidates: None,
                            mismatch: Some(format!(
                                "op #{i}: file was modified outside the tool since the last read/edit (ledger hash mismatch)"
                            )),
                            diagnostic: None,
                            hint: Some(format!(
                                "Use read_file to refresh the tool's view of the file, then retry. (op #{i} of {} failed; other ops are unaffected)",
                                req.display
                            )),
                        });
                        continue;
                    }
                    Some(_) => {} // 账本匹配 → 放行
                }
            }
        }
        let (next, report) = apply_op(&content, op, i, &req.display, req.dry_run);
        content = next;
        reports.push(report);
    }

    let ok_count = reports.iter().filter(|r| r.status == "ok").count();
    let file_diff = if ok_count > 0 && content != original_lf {
        unified_diff(&original_lf, &content, &req.display)
    } else {
        String::new()
    };

    let mut write_error: Option<String> = None;
    if ok_count > 0 && !req.dry_run {
        let write_content = if was_crlf {
            content.replace('\n', "\r\n")
        } else {
            content.clone()
        };
        match atomic_write(&req.path.to_string_lossy(), &write_content) {
            Ok(_) => {
                // 账本记录 LF 视图内容（与 read 的 hash 同视图），
                // 即使文件是 CRLF 写回，账本 hash 仍与 read 一致。
                crate::file_state::record_edit(&req.path.to_string_lossy(), &content);
            }
            Err(e) => {
                // 写盘失败必须上报：ops 定位/应用成功 ≠ 改动落地。
                // 模型需要知道磁盘上的文件并未被修改，才能决定重试或改路径。
                write_error = Some(format!(
                    "atomic write failed after {ok_count}/{} op(s) applied: {e} — the file on disk was NOT modified",
                    req.ops.len()
                ));
            }
        }
    }

    // 写盘失败 → 整体失败（改动未落地），即使所有 op 定位/应用都成功。
    let status = if write_error.is_some() {
        "error"
    } else if ok_count == req.ops.len() {
        "ok"
    } else if ok_count > 0 {
        "partial"
    } else {
        "error"
    };
    FileReport {
        path: req.display.clone(),
        status,
        diff: if req.dry_run && ok_count > 0 {
            Some(file_diff)
        } else {
            None
        },
        write_error,
        total: if ok_count > 0 {
            Some(format!(
                "{ok_count}/{} op(s) applied at {}",
                req.ops.len(),
                req.display
            ))
        } else {
            None
        },
        ops: reports,
    }
}

/// 提取目标路径（权限审批用）：`path` + `files[].path`。
pub(crate) fn extract_target_paths(args: &serde_json::Value) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        paths.push(PathBuf::from(path));
    }
    if let Some(files) = args.get("files").and_then(|v| v.as_array()) {
        for f in files {
            if let Some(path) = f.get("path").and_then(|v| v.as_str()) {
                paths.push(PathBuf::from(path));
            }
        }
    }
    paths
}

// ─────────────────────────────────────────────────────────────
// Handler
// ─────────────────────────────────────────────────────────────

fn exec_edit_file(args: &serde_json::Value) -> ToolResult {
    let dry_run = args
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // 多文件模式：files 数组；单文件模式：顶层 path
    let mut requests: Vec<FileRequest> = Vec::new();
    if let Some(files) = args.get("files").and_then(|v| v.as_array()) {
        if args
            .get("path")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty())
        {
            return crate::ToolResult::error(serde_json::json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "code": "PARAM_CONFLICT",
                "message": "use either 'files' (multi-file) or 'path' (single file), not both",
                "hint": "For multi-file edits pass files: [{path, ops}]; for single-file edits pass path + old_*/new_* or ops.",
            }).to_string());
        }
        if files.is_empty() {
            return crate::ToolResult::error(
                serde_json::json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "code": "EMPTY_FILES",
                    "message": "files array is empty",
                    "hint": "Provide at least one file entry.",
                })
                .to_string(),
            );
        }
        for (i, f) in files.iter().enumerate() {
            let mut f = f.clone();
            if dry_run {
                f["dry_run"] = serde_json::Value::Bool(true);
            }
            match parse_file_request(&f) {
                Ok(req) => requests.push(req),
                Err(e) => {
                    return crate::ToolResult::error(serde_json::json!({
                        "timeis": crate::now_utc8(),
                        "status": "error",
                        "code": "PARSE_ERROR",
                        "message": format!("files[{i}]: {e}"),
                        "hint": "Each file entry needs a path and ops (or top-level old_*/new_* fields).",
                    }).to_string());
                }
            }
        }
    } else {
        let mut top = args.clone();
        if dry_run {
            top["dry_run"] = serde_json::Value::Bool(true);
        }
        match parse_file_request(&top) {
            Ok(req) => requests.push(req),
            Err(e) => {
                return crate::ToolResult::error(serde_json::json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "code": "PARSE_ERROR",
                    "message": e,
                    "hint": "edit_file accepts: path + old_string/new_string (string mode), path + old_lines/new_lines or start_line (line mode), ops (multi-block), files (multi-file).",
                }).to_string());
            }
        }
    }

    let reports: Vec<FileReport> = requests.iter().map(execute_file).collect();
    let all_ok = reports.iter().all(|r| r.status == "ok");
    let none_ok = reports.iter().all(|r| r.status == "error");
    let status = if all_ok {
        "ok"
    } else if none_ok {
        "error"
    } else {
        "partial"
    };

    let text = render_text(&reports, status, dry_run);
    crate::ToolResult::ok_data(
        serde_json::json!({
            "timeis": crate::now_utc8(),
            "status": status,
            "dry_run": dry_run,
            "files": reports,
        }),
        text,
    )
}

/// 模型可见的紧凑文本报告（data 字段携带结构化 JSON 供审计/前端）。
fn render_text(reports: &[FileReport], status: &str, dry_run: bool) -> String {
    let mut out = String::new();
    if dry_run {
        out.push_str("[DRY RUN] edit_file — preview, no changes written\n");
    }
    out.push_str(&format!("[{}] edit_file\n", status.to_uppercase()));
    for f in reports {
        match f.status {
            "ok" => out.push_str(&format!(
                "  {}: {}\n",
                f.path,
                f.total.as_deref().unwrap_or("applied")
            )),
            "partial" => out.push_str(&format!(
                "  {}: {} (partial)\n",
                f.path,
                f.total.as_deref().unwrap_or("some ops applied")
            )),
            _ => out.push_str(&format!("  {}: failed\n", f.path)),
        }
        for op in &f.ops {
            if op.status == "ok" {
                out.push_str(&format!(
                    "    op{}: ok L{} {}{}\n",
                    op.index,
                    op.line.unwrap_or(0),
                    op.summary.as_deref().unwrap_or(""),
                    op.fuzzy
                        .unwrap_or(false)
                        .then_some(" (fuzzy)")
                        .unwrap_or("")
                ));
            } else {
                out.push_str(&format!(
                    "    op{}: {} {}\n",
                    op.index,
                    op.code.as_deref().unwrap_or("error"),
                    op.mismatch.as_deref().unwrap_or("")
                ));
                if let (Some(line), Some(text)) = (op.closest_line, &op.closest_text) {
                    let cap = text.floor_char_boundary(text.len().min(200));
                    out.push_str(&format!("      closest: L{line}: {}\n", &text[..cap]));
                }
                if let Some(diag) = &op.diagnostic {
                    out.push_str(&format!("      diagnostic: {diag}\n"));
                }
                if let Some(hint) = &op.hint {
                    out.push_str(&format!("      hint: {hint}\n"));
                }
            }
        }
        if let Some(we) = &f.write_error {
            out.push_str(&format!("    write error: {we}\n"));
        }
    }
    out
}

fn handle_edit_file(ctx: crate::ToolCallCtx) -> ToolResult {
    exec_edit_file(&ctx.args)
}

// ─────────────────────────────────────────────────────────────
// Registration
// ─────────────────────────────────────────────────────────────

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_with_placement(ToolHandler {
        key: "edit_file".to_string(),
        description: concat!(
            "Unified file editor: string mode (old_string/new_string), line mode (old_lines/new_lines), ",
            "or line-number mode (start_line/end_line). Multi-block: pass ops array; multi-file: pass files array. ",
            "Every op is an independent transaction — a failed op is reported with its closest_line and other ops still apply. ",
            "Trim-end whitespace is tolerated by default; allow_fuzzy=true adds trim + intra-line whitespace collapsing (tab/multiple spaces) + Unicode normalization (NFC combining-character folding, smart punctuation). ",
            "Ambiguous matches (multiple locations, no disambiguating context) are REJECTED with all candidate lines — never guessed: ",
            "context_before/context_after disambiguate (substring and line modes), or use start_line / replace_all=true. ",
            "Expected_hash (from read) guards against stale content — optional: when omitted, the tool verifies against its own last-known state automatically (read once, then edit freely). ",
            "For patch-style edits (unified diff), use the apply_patch tool. Use write for whole-file creation; delete for removal."
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Target file (single-file mode; exclusive with files)"},
                "files": {"type": "array", "description": "Multi-file mode: [{path, ops?: [...], old_string?, new_string?, old_lines?, new_lines?, start_line?, expected_hash?}]"},
                "ops": {"type": "array", "description": "Multi-block mode: list of ops, each with its own locator fields. Applied in order; each op is independent"},
                "old_string": {"type": "string", "description": "String locator (Claude style). Single line = in-line substring; multi-line = line window"},
                "new_string": {"type": "string", "description": "Replacement text"},
                "old_lines": {"type": "array", "items": {"type": "string"}, "description": "Line-window locator"},
                "new_lines": {"type": "array", "items": {"type": "string"}, "description": "Replacement lines"},
                "start_line": {"type": "integer", "description": "1-based line locator (cross-checked against old_lines/old_string when provided; otherwise requires expected_hash)"},
                "end_line": {"type": "integer", "description": "Inclusive end line (defaults to start_line)"},
                "context_before": {"type": "array", "items": {"type": "string"}, "description": "Lines just before the change, for disambiguation"},
                "context_after": {"type": "array", "items": {"type": "string"}, "description": "Lines just after the change, for disambiguation"},
                "replace_all": {"type": "boolean", "description": "Replace all occurrences (substring mode only)", "default": false},
                "regex": {"type": "boolean", "description": "Treat old_string as regex", "default": false},
                "allow_fuzzy": {"type": "boolean", "description": "Whitespace collapsing + Unicode normalization fallback (trim, tab/multi-space folding, NFC)", "default": false},
                "expected_hash": {"type": "string", "description": "Optional. When omitted, the tool auto-verifies against its own last-known state (from read/edit/write) and rejects stale line-number edits — no need to pass the hash back"},
                "dry_run": {"type": "boolean", "description": "Preview only (with diffs), do not write", "default": false},
                "description": {"type": "string", "description": "Brief note explaining why this change is needed (optional)"}
            },
            "additionalProperties": false
        }),
        handler: handle_edit_file,
        risk: ToolRisk::Write,
        default_timeout: std::time::Duration::from_secs(60),
    },
    crate::ToolPlacement::Workspace,
);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_shared::content_hash;
    use std::path::Path;

    fn write_tmp(dir: &Path, name: &str, content: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().to_string()
    }

    fn run(path: &str, extra: serde_json::Value) -> serde_json::Value {
        let mut args = serde_json::json!({ "path": path });
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                args[k] = v.clone();
            }
        }
        let result = exec_edit_file(&args);
        let data = result.data.clone();
        if data.is_null() {
            serde_json::json!({ "status": "error", "raw": result.model_text() })
        } else {
            data
        }
    }

    #[test]
    fn substring_mode_replaces_in_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "a.rs", "fn main() {\n    let x = 1;\n}\n");
        let out = run(
            &p,
            serde_json::json!({
                "old_string": "let x = 1;", "new_string": "let y = 2;",
            }),
        );
        assert_eq!(out["status"], "ok");
        assert_eq!(out["files"][0]["status"], "ok");
        assert_eq!(out["files"][0]["ops"][0]["line"], 2);
        let content = std::fs::read_to_string(&p).unwrap();
        assert_eq!(content, "fn main() {\n    let y = 2;\n}\n");
    }

    #[test]
    fn line_window_mode_replaces_block() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "b.rs", "a\nb\nc\nd\n");
        let out = run(
            &p,
            serde_json::json!({
                "old_lines": ["b", "c"], "new_lines": ["B", "C", "C2"],
            }),
        );
        assert_eq!(out["status"], "ok");
        assert_eq!(out["files"][0]["ops"][0]["summary"], "+3 -2");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nB\nC\nC2\nd\n");
    }

    #[test]
    fn start_line_with_old_lines_cross_check() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "c.rs", "a\nb\nc\n");
        // 行号 + 正确内容 → 成功
        let out = run(
            &p,
            serde_json::json!({
                "start_line": 2, "old_lines": ["b"], "new_lines": ["B"],
            }),
        );
        assert_eq!(out["status"], "ok");
        // 行号 + 错误内容 → LINE_MISMATCH（严格）
        let p2 = write_tmp(dir.path(), "d.rs", "a\nb\nc\n");
        let out = run(
            &p2,
            serde_json::json!({
                "start_line": 2, "old_lines": ["WRONG"], "new_lines": ["B"],
            }),
        );
        assert_eq!(out["files"][0]["ops"][0]["status"], "error");
        assert_eq!(out["files"][0]["ops"][0]["code"], "LINE_MISMATCH");
    }

    #[test]
    fn start_line_without_content_requires_expected_hash() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "e.rs", "a\nb\nc\n");
        let content = std::fs::read_to_string(&p).unwrap();
        let hash = content_hash(&content);
        let out = run(
            &p,
            serde_json::json!({
                "start_line": 2, "end_line": 2, "new_lines": ["B"], "expected_hash": hash,
            }),
        );
        assert_eq!(out["status"], "ok");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nB\nc\n");
    }

    #[test]
    fn ambiguous_match_is_rejected_with_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "f.rs", "x\nmark\nx\nmark\nx\n");
        let out = run(
            &p,
            serde_json::json!({
                "old_lines": ["mark"], "new_lines": ["replaced"],
            }),
        );
        assert_eq!(out["files"][0]["ops"][0]["status"], "error");
        assert_eq!(out["files"][0]["ops"][0]["code"], "AMBIGUOUS_MATCH");
        let candidates = out["files"][0]["ops"][0]["candidates"].as_array().unwrap();
        assert_eq!(candidates.len(), 2);
        // 未写入
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "x\nmark\nx\nmark\nx\n"
        );
    }

    #[test]
    fn context_disambiguates() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "g.rs", "x\nmark\nfirst\nx\nmark\nsecond\n");
        let out = run(
            &p,
            serde_json::json!({
                "old_lines": ["mark"], "new_lines": ["replaced"], "context_after": ["first"],
            }),
        );
        assert_eq!(out["status"], "ok");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "x\nreplaced\nfirst\nx\nmark\nsecond\n"
        );
    }

    #[test]
    fn ops_are_independent_transactions() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "h.rs", "a\nb\nc\n");
        let out = run(
            &p,
            serde_json::json!({
                "ops": [
                    {"old_lines": ["a"], "new_lines": ["A"]},
                    {"old_lines": ["DOES_NOT_EXIST"], "new_lines": ["X"]},
                    {"old_lines": ["c"], "new_lines": ["C"]},
                ],
            }),
        );
        assert_eq!(out["status"], "partial");
        let ops = out["files"][0]["ops"].as_array().unwrap();
        assert_eq!(ops[0]["status"], "ok");
        assert_eq!(ops[1]["status"], "error");
        assert_eq!(ops[1]["code"], "NO_MATCH");
        assert_eq!(ops[2]["status"], "ok");
        // 成功 op 已应用，失败 op 被跳过
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "A\nb\nC\n");
    }

    #[test]
    fn multi_file_mode() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = write_tmp(dir.path(), "i.rs", "one\n");
        let p2 = write_tmp(dir.path(), "j.rs", "two\n");
        let args = serde_json::json!({
            "files": [
                {"path": p1, "old_string": "one", "new_string": "ONE"},
                {"path": p2, "old_lines": ["two"], "new_lines": ["TWO"]},
            ],
        });
        let result = exec_edit_file(&args);
        let out: serde_json::Value = result.data.clone();
        assert_eq!(out["status"], "ok");
        assert_eq!(out["files"].as_array().unwrap().len(), 2);
        assert_eq!(std::fs::read_to_string(&p1).unwrap(), "ONE\n");
        assert_eq!(std::fs::read_to_string(&p2).unwrap(), "TWO\n");
    }

    #[test]
    fn cross_check_string_vs_lines() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "k.rs", "a\nb\nc\n");
        // 一致 → ok
        let out = run(
            &p,
            serde_json::json!({
                "old_string": "b", "old_lines": ["b"], "new_string": "B",
            }),
        );
        assert_eq!(out["status"], "ok");
        // 不一致 → CROSS_CHECK
        let p2 = write_tmp(dir.path(), "l.rs", "a\nb\nc\n");
        let out = run(
            &p2,
            serde_json::json!({
                "old_string": "c", "old_lines": ["a"], "new_string": "X",
            }),
        );
        assert_eq!(out["files"][0]["ops"][0]["code"], "CROSS_CHECK");
    }

    #[test]
    fn replace_all_substring() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "m.rs", "x y x\n");
        let out = run(
            &p,
            serde_json::json!({
                "old_string": "x", "new_string": "z", "replace_all": true,
            }),
        );
        assert_eq!(out["status"], "ok");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "z y z\n");
    }

    #[test]
    fn fuzzy_fallback_normalizes_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "n.rs", "fn main() {\n    call();\n}\n");
        // 缩进错误 + allow_fuzzy → 命中
        let out = run(
            &p,
            serde_json::json!({
                "old_lines": ["fn main() {", "call();"], "new_lines": ["fn main() {", "  call();"],
                "allow_fuzzy": true,
            }),
        );
        assert_eq!(out["status"], "ok");
        assert_eq!(out["files"][0]["ops"][0]["fuzzy"], true);
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "fn main() {\n  call();\n}\n"
        );
    }

    #[test]
    fn dry_run_does_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "o.rs", "a\n");
        let out = run(
            &p,
            serde_json::json!({
                "old_string": "a", "new_string": "b", "dry_run": true,
            }),
        );
        assert_eq!(out["status"], "ok");
        assert_eq!(out["dry_run"], true);
        assert!(out["files"][0]["ops"][0]["diff"].is_string());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\n");
    }

    #[test]
    fn stale_hash_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "p.rs", "a\n");
        let out = run(
            &p,
            serde_json::json!({
                "old_string": "a", "new_string": "b", "expected_hash": "deadbeef",
            }),
        );
        assert_eq!(out["files"][0]["ops"][0]["code"], "STALE_FILE");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\n");
    }

    #[test]
    fn crlf_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "q.rs", "a\r\nb\r\n");
        let out = run(
            &p,
            serde_json::json!({
                "old_string": "b", "new_string": "B",
            }),
        );
        assert_eq!(out["status"], "ok");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\r\nB\r\n");
    }

    #[test]
    fn crlf_expected_hash_matches_read_view() {
        // read 的 hash 基于 LF 归一化视图（file_query.rs L144-145）；
        // edit_file 的 expected_hash 校验必须在同一视图，否则 CRLF 文件永远 STALE_FILE。
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "q2.rs", "a\r\nb\r\n");
        let raw = std::fs::read_to_string(&p).unwrap();
        let lf_view = raw.replace("\r\n", "\n");
        let hash = content_hash(&lf_view);
        let out = run(
            &p,
            serde_json::json!({
                "old_string": "b", "new_string": "B", "expected_hash": hash,
            }),
        );
        assert_eq!(out["files"][0]["ops"][0]["status"], "ok");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\r\nB\r\n");
    }

    #[test]
    fn crlf_diff_is_minimal_not_whole_file() {
        // 换行差异不得污染 diff：CRLF 文件单行修改，diff 只含变化行。
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "q3.rs", "line1\r\nline2\r\nline3\r\n");
        let out = run(
            &p,
            serde_json::json!({
                "old_string": "line2", "new_string": "LINE2", "dry_run": true,
            }),
        );
        assert_eq!(out["status"], "ok");
        let diff = out["files"][0]["ops"][0]["diff"].as_str().unwrap();
        // line1/line3 允许作为上下文行（' ' 前缀）出现，但绝不能被标记为删除
        assert!(diff.contains("-line2"), "diff must remove line2: {diff}");
        assert!(diff.contains("+LINE2"), "diff must add LINE2: {diff}");
        assert!(
            !diff
                .lines()
                .any(|l| l.starts_with("-line1") || l.starts_with("-line3")),
            "unchanged lines must not be deleted: {diff}"
        );
        // 未写入
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "line1\r\nline2\r\nline3\r\n"
        );
    }

    #[test]
    fn multi_line_string_locator() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "r.rs", "a\nb\nc\nd\n");
        let out = run(
            &p,
            serde_json::json!({
                "old_string": "a\nb", "new_string": "A\nB",
            }),
        );
        assert_eq!(out["status"], "ok");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "A\nB\nc\nd\n");
    }

    #[test]
    fn extract_paths_includes_files_array() {
        let args = serde_json::json!({
            "files": [{"path": "a.rs"}, {"path": "b.rs", "ops": []}],
        });
        let paths = extract_target_paths(&args);
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], PathBuf::from("a.rs"));
        assert_eq!(paths[1], PathBuf::from("b.rs"));
    }

    #[test]
    fn substring_context_disambiguates() {
        // 子串模式多处命中：context_after 参与消歧（Claude 风格）
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "s.rs", "x\nmark\nfirst\nx\nmark\nsecond\n");
        let out = run(
            &p,
            serde_json::json!({
                "old_string": "mark", "new_string": "replaced", "context_after": ["second"],
            }),
        );
        assert_eq!(out["status"], "ok");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "x\nmark\nfirst\nx\nreplaced\nsecond\n"
        );
    }

    #[test]
    fn substring_same_line_multi_hit_with_context_stays_ambiguous() {
        // 同一行内多处命中：context 只能定位到行，无法消歧到行内位置 → 保持拒绝
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "t.rs", "a\nmark x mark\nb\n");
        let out = run(
            &p,
            serde_json::json!({
                "old_string": "mark", "new_string": "M", "context_after": ["b"],
            }),
        );
        assert_eq!(out["status"], "error");
        assert_eq!(out["files"][0]["ops"][0]["code"], "AMBIGUOUS_MATCH");
    }

    #[test]
    fn fuzzy_collapses_intra_line_whitespace_and_tab() {
        // allow_fuzzy：行内多空格与 tab 等价
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "u.rs", "let x = 1;\n");
        let out = run(
            &p,
            serde_json::json!({
                "old_lines": ["let  x\t=\t1;"], "new_lines": ["let y = 2;"], "allow_fuzzy": true,
            }),
        );
        assert_eq!(out["status"], "ok");
        assert_eq!(out["files"][0]["ops"][0]["fuzzy"], true);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "let y = 2;\n");
    }

    #[test]
    fn fuzzy_nfc_combining_characters() {
        // allow_fuzzy：NFC 组合字符折叠（é == e + U+0301）
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "v.rs", "café\n");
        let out = run(
            &p,
            serde_json::json!({
                "old_lines": ["cafe\u{301}"], "new_lines": ["CAFE"], "allow_fuzzy": true,
            }),
        );
        assert_eq!(out["status"], "ok");
        assert_eq!(out["files"][0]["ops"][0]["fuzzy"], true);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "CAFE\n");
    }
    #[test]
    fn multiline_old_string_with_blank_lines_matches() {
        // 多行 old_string 中间的空白行必须参与匹配（与 old_lines 语义一致）
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "w.rs", "fn a() {\n\n    let x = 1;\n}\n");
        let out = run(
            &p,
            serde_json::json!({
                "old_string": "fn a() {\n\n    let x = 1;",
                "new_string": "fn a() {\n\n    let y = 2;",
            }),
        );
        assert_eq!(out["status"], "ok", "got: {out}");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "fn a() {\n\n    let y = 2;\n}\n"
        );
    }
    #[test]
    fn multiline_new_string_preserves_blank_lines() {
        // 多行 new_string 中间的空白行不能被过滤
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "x.rs", "a\nb\n");
        let out = run(
            &p,
            serde_json::json!({
                "old_lines": ["a", "b"], "new_lines": ["A", "", "B"],
            }),
        );
        assert_eq!(out["status"], "ok");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "A\n\nB\n");
    }
    #[test]
    fn replace_all_with_regex_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "y.rs", "a a\n");
        let out = run(
            &p,
            serde_json::json!({
                "old_string": "a", "new_string": "b", "regex": true, "replace_all": true,
            }),
        );
        assert_eq!(out["status"], "error");
        assert_eq!(out["files"][0]["ops"][0]["code"], "OP_ERROR");
        assert!(
            out["files"][0]["ops"][0]["hint"]
                .as_str()
                .unwrap_or("")
                .contains("replace_all=true is not supported with regex=true"),
            "got: {out}"
        );
        // 文件未被修改（显式拒绝而非静默降级）
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a a\n");
    }
    #[test]
    fn replace_all_with_multiline_old_string_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "z.rs", "a\nb\n");
        let out = run(
            &p,
            serde_json::json!({
                "old_string": "a\nb", "new_string": "c", "replace_all": true,
            }),
        );
        assert_eq!(out["status"], "error");
        assert!(
            out["files"][0]["ops"][0]["hint"]
                .as_str()
                .unwrap_or("")
                .contains("only supported for a single-line old_string"),
            "got: {out}"
        );
    }
    #[test]
    fn no_match_reports_partial_match_diagnostic() {
        // NO_MATCH 时给出最佳前缀诊断：已匹配行数 + 首个失配行对比
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "aa.rs", "fn a() {\n    let x = 1;\n}\n");
        let out = run(
            &p,
            serde_json::json!({
                "old_string": "fn a() {\n    let x = 999;\n}",
                "new_string": "fn a() {\n    let x = 0;\n}",
            }),
        );
        assert_eq!(out["status"], "error");
        let diag = out["files"][0]["ops"][0]["diagnostic"]
            .as_str()
            .unwrap_or("");
        assert!(diag.contains("best partial match"), "diag: {diag}");
        assert!(diag.contains("1 of 3 lines"), "diag: {diag}");
        assert!(diag.contains("actual"), "diag: {diag}");
        assert!(diag.contains("expected"), "diag: {diag}");
    }
    #[test]
    fn no_match_diagnostic_detects_escape_mismatch() {
        // 文件里是普通引号，模式里带了反斜杠转义 → 专门提示
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "ab.rs", "print!(\"hi\");\n");
        let out = run(
            &p,
            serde_json::json!({
                "old_string": "print!(\\\"hi\\\");", "new_string": "print!(\"yo\");",
            }),
        );
        assert_eq!(out["status"], "error");
        let diag = out["files"][0]["ops"][0]["diagnostic"]
            .as_str()
            .unwrap_or("");
        assert!(diag.contains("escape mismatch"), "diag: {diag}");
    }

    // ── 工具侧账本：模型无需回传 hash ──

    #[test]
    fn line_edit_without_hash_uses_ledger() {
        // 账本是进程全局状态：与 init_tools（清账本）互斥，避免并行 flaky
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // read 建立账本后，start_line 盲定位无需 expected_hash 即可安全放行
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "ledger1.rs", "a\nb\nc\n");
        let content = std::fs::read_to_string(&p).unwrap();
        crate::file_state::record_read(&p, &content, 3); // 模拟 read_file
        let out = run(
            &p,
            serde_json::json!({
                "start_line": 2, "new_string": "B",
            }),
        );
        assert_eq!(out["status"], "ok", "got: {out}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nB\nc\n");
    }

    #[test]
    fn line_edit_without_ledger_is_rejected_with_read_hint() {
        // 账本是进程全局状态：与 init_tools（清账本）互斥，避免并行 flaky
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 从未 read（无账本）→ UNVERIFIED_LINE_EDIT；hint 指向 read 而非传 hash
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "ledger2.rs", "a\nb\nc\n");
        let out = run(
            &p,
            serde_json::json!({
                "start_line": 2, "new_string": "B",
            }),
        );
        assert_eq!(out["files"][0]["ops"][0]["status"], "error");
        assert_eq!(out["files"][0]["ops"][0]["code"], "UNVERIFIED_LINE_EDIT");
        let hint = out["files"][0]["ops"][0]["hint"].as_str().unwrap_or("");
        assert!(
            hint.contains("read_file"),
            "hint should point to read: {hint}"
        );
        assert!(
            !hint.contains("expected_hash"),
            "hint must not demand hash: {hint}"
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nb\nc\n");
    }

    #[test]
    fn line_edit_rejected_when_file_changed_externally() {
        // read 后文件被工具外修改 → 账本失配 → 行号盲定位拒绝（防漂移）
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "ledger3.rs", "a\nb\nc\n");
        let content = std::fs::read_to_string(&p).unwrap();
        crate::file_state::record_read(&p, &content, 3);
        std::fs::write(&p, "a\nB-EXTERNAL\nc\n").unwrap();
        let out = run(
            &p,
            serde_json::json!({
                "start_line": 2, "new_string": "B",
            }),
        );
        assert_eq!(out["files"][0]["ops"][0]["status"], "error");
        assert_eq!(out["files"][0]["ops"][0]["code"], "STALE_FILE");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nB-EXTERNAL\nc\n");
    }

    #[test]
    fn substring_edit_succeeds_after_external_change() {
        // 内容定位自带校验：外部修改后 old_string 命中即安全，无需 hash
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "ledger4.rs", "a\nb\nc\n");
        let content = std::fs::read_to_string(&p).unwrap();
        crate::file_state::record_read(&p, &content, 3);
        std::fs::write(&p, "a\nb\nc\n// ext\n").unwrap(); // 外部追加
        let out = run(
            &p,
            serde_json::json!({
                "old_string": "b", "new_string": "B",
            }),
        );
        assert_eq!(out["status"], "ok", "got: {out}");
        // 写盘后账本已刷新为最新内容（下次盲定位继续安全）
        let new_content = std::fs::read_to_string(&p).unwrap();
        let ledger = crate::file_state::last_hash(&p);
        assert_eq!(
            ledger,
            Some(crate::file_shared::content_hash(&new_content)),
            "ledger must refresh after edit"
        );
    }

    #[test]
    fn write_rejected_when_file_changed_externally_without_hash() {
        // 账本是进程全局状态：与 init_tools（清账本）互斥，避免并行 flaky
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // write 未带 hash：账本失配（外部修改）→ STALE_FILE，不覆盖外部改动
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "ledger5.rs", "a\nb\n");
        let content = std::fs::read_to_string(&p).unwrap();
        crate::file_state::record_read(&p, &content, 2);
        std::fs::write(&p, "external\n").unwrap();
        let out = crate::file_mutate::exec_write_file(&serde_json::json!({
            "path": p, "content": "new\n",
        }));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["code"], "STALE_FILE", "got: {out}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "external\n");
    }

    #[test]
    fn write_updates_ledger_without_hash() {
        // 账本是进程全局状态：与 init_tools（清账本）互斥，避免并行 flaky
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // write 成功后账本自动更新为新内容指纹
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "ledger6.rs", "a\n");
        let content = std::fs::read_to_string(&p).unwrap();
        crate::file_state::record_read(&p, &content, 1);
        let out = crate::file_mutate::exec_write_file(&serde_json::json!({
            "path": p, "content": "NEW\n",
        }));
        assert!(out.contains("[OK]"), "got: {out}");
        assert_eq!(
            crate::file_state::last_hash(&p),
            Some(crate::file_shared::content_hash("NEW\n"))
        );
    }

    #[test]
    fn delete_rejected_when_file_changed_externally_without_hash() {
        // 账本是进程全局状态：与 init_tools（清账本）互斥，避免并行 flaky
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // delete 是破坏性操作：账本失配（外部修改）→ STALE_FILE，文件保留
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "ledger7.rs", "a\n");
        let content = std::fs::read_to_string(&p).unwrap();
        crate::file_state::record_read(&p, &content, 1);
        std::fs::write(&p, "external\n").unwrap();
        let out = crate::file_mutate::exec_delete_file(&serde_json::json!({ "path": p }));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["code"], "STALE_FILE", "got: {out}");
        assert!(std::path::Path::new(&p).exists());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "external\n");
    }
}
