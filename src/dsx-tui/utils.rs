//! Pure utility functions for string/display operations.

use crate::types::AgentPhase;

/// Strip ANSI escape sequences from a string (color codes, cursor movements, etc.)
pub(crate) fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == 0x1b { // ESC
            // Consume until we hit a letter (end of CSI sequence)
            while let Some(c) = bytes.next() {
                if c.is_ascii_alphabetic() || c == 0x9c { // ST string terminator
                    break;
                }
            }
        } else {
            out.push(b as char);
        }
    }
    out
}

pub(super) fn display_width(s: &str) -> usize {
    s.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
}

pub(super) fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 { format!("{}B", bytes) }
    else if bytes < 1024*1024 { format!("{:.1}K", bytes as f64 / 1024.0) }
    else { format!("{:.1}M", bytes as f64 / (1024.0*1024.0)) }
}

pub(super) fn pad_to_width(s: &str, target: usize) -> String {
    let w = display_width(s);
    if w < target {
        format!("{}{}", s, " ".repeat(target.saturating_sub(w)))
    } else {
        s.to_string()
    }
}

pub(super) fn textwrap(text: &str, max_width: usize) -> Vec<String> {
    let max = max_width.max(20);
    text.lines().flat_map(|line| {
        let w = display_width(line);
        if w <= max { return vec![line.to_string()]; }
        let mut r = Vec::new();
        let mut start = 0;
        while start < line.len() {
            // Walk forward accumulating display width until we hit max
            let mut col = 0;
            let mut end = start;
            for (i, c) in line[start..].char_indices() {
                let cw = if c.is_ascii() { 1 } else { 2 };
                if col + cw > max { break; }
                col += cw;
                end = start + i + c.len_utf8();
            }
            if end == start { end = start + line[start..].chars().next().map(|c| c.len_utf8()).unwrap_or(0); }
            r.push(line[start..end].to_string());
            start = end;
        }
        r
    }).collect()
}

pub(super) fn truncate(s: &str, max: usize) -> String {
    if display_width(s) <= max { s.to_string() }
    else {
        let mut col = 0;
        let mut end = 0;
        for (i, c) in s.char_indices() {
            col += if c.is_ascii() { 1 } else { 2 };
            if col > max { break; }
            end = i + c.len_utf8();
        }
        format!("{}…", &s[..end])
    }
}

// ── Phase detection from reasoning chain ──

pub(crate) fn detect_phase(reasoning: &str) -> (AgentPhase, f64) {
    let tail: String = reasoning.lines().rev().take(15).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
    let tail_lower = tail.to_lowercase();

    // Keyword scoring (ZH + EN combined)
    let analyze = score(&tail_lower, &[
        "analyze", "understand", "first", "look at", "check the",
        "explore", "see what", "investigate",
        "分析", "理解", "看一下", "看看", "了解", "检查", "研究",
    ]);
    let plan = score(&tail_lower, &[
        "plan", "approach", "step", "strategy", "outline",
        "going to", "will need to", "should first", "let me",
        "方案", "步骤", "规划", "计划", "思路", "方法是", "先",
    ]);
    let verify = score(&tail_lower, &[
        "output", "result", "shows", "confirmed", "looks correct",
        "error", "warning", "compilation", "test", "pass",
        "this means", "indicates", "suggests", "proves",
        "输出", "结果", "说明", "表明", "确认", "验证", "错误",
        "编译", "测试", "通过",
    ]);
    let respond = score(&tail_lower, &[
        "done", "complete", "finished", "summary", "conclusion",
        "in summary", "to conclude", "finally",
        "完成", "总结", "结论", "以上", "如上所述",
    ]);

    let (phase, conf) = if analyze > plan && analyze > verify && analyze > respond {
        (AgentPhase::Analyzing, analyze)
    } else if plan > analyze && plan > verify && plan > respond {
        (AgentPhase::Planning, plan)
    } else if verify > analyze && verify > plan && verify > respond {
        (AgentPhase::Verifying, verify)
    } else if respond > analyze && respond > plan && respond > verify {
        (AgentPhase::Responding, respond)
    } else {
        (AgentPhase::Analyzing, 0.0) // default
    };
    (phase, conf)
}

pub(super) fn score(text: &str, keywords: &[&str]) -> f64 {
    let count = keywords.iter().filter(|k| text.contains(*k)).count();
    count as f64 / keywords.len() as f64
}

// ── Char-boundary-safe cursor helpers (UTF-8 multi-byte safety) ──

/// Ensure pos is on a char boundary (clamp left)
pub(crate) fn char_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() { return s.len(); }
    let mut p = pos;
    while p > 0 && !s.is_char_boundary(p) { p -= 1; }
    p
}

/// Move cursor one char left
pub(super) fn prev_char_boundary(s: &str, pos: usize) -> usize {
    if pos == 0 { return 0; }
    let mut p = pos - 1;
    while p > 0 && !s.is_char_boundary(p) { p -= 1; }
    p
}

/// Detect task phase from AI reasoning stream (auto mode).
/// Scans the first ~300 chars of reasoning for phase-indicating keywords.
pub(super) fn detect_task_phase_from_reasoning(reasoning: &str) -> crate::types::TaskPhase {
    let scan: String = reasoning.chars().take(300).collect::<String>().to_lowercase();
    let plan = ["plan", "design", "architect", "approach", "analyze", "outline",
        "方案", "设计", "架构", "分析", "规划"];
    let code = ["implement", "write", "create", "build", "add", "modify", "edit",
        "code", "实现", "编写", "创建", "添加", "修改", "写", "编码"];
    let debug = ["error", "bug", "crash", "wrong", "failed", "debug", "issue",
        "fix", "错误", "bug", "崩溃", "调试", "修复"];

    let p_score = plan.iter().filter(|kw| scan.contains(*kw)).count();
    let c_score = code.iter().filter(|kw| scan.contains(*kw)).count();
    let d_score = debug.iter().filter(|kw| scan.contains(*kw)).count();

    if p_score >= c_score && p_score >= d_score && p_score >= 2 { crate::types::TaskPhase::Plan }
    else if d_score >= p_score && d_score >= c_score && d_score >= 2 { crate::types::TaskPhase::Debug }
    else if c_score >= p_score && c_score >= d_score && c_score >= 2 { crate::types::TaskPhase::Coding }
    else { crate::types::TaskPhase::Chat }
}

/// Move cursor one char right
pub(super) fn next_char_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() { return s.len(); }
    let mut p = pos + 1;
    while p < s.len() && !s.is_char_boundary(p) { p += 1; }
    p
}
