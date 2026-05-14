//! Phase detection from AI reasoning streams.
//!
//! These functions analyze the tail of the reasoning chain to determine
//! the agent's current working phase (Analyzing / Planning / Verifying / Responding)
//! and task phase (Plan / Coding / Debug / Chat).
//!
//! Migrated from dsx-tui/utils.rs — they are agent logic, not UI logic.

use crate::types::{AgentPhase, TaskPhase};

/// Detect agent phase from the reasoning tail (last ~15 lines).
/// Returns (phase, confidence) where confidence is a keyword match ratio.
pub fn detect_phase(reasoning: &str) -> (AgentPhase, f64) {
    let tail: String = reasoning.lines().rev().take(15).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
    let tail_lower = tail.to_lowercase();

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
        (AgentPhase::Analyzing, 0.0)
    };
    (phase, conf)
}

/// Keyword scoring helper — fraction of keywords found in text
fn score(text: &str, keywords: &[&str]) -> f64 {
    let count = keywords.iter().filter(|k| text.contains(*k)).count();
    count as f64 / keywords.len() as f64
}

/// Detect task phase from AI reasoning stream (first ~300 chars).
/// Used for auto-mode phase tracking.
pub fn detect_task_phase_from_reasoning(reasoning: &str) -> TaskPhase {
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

    if p_score >= c_score && p_score >= d_score && p_score >= 2 { TaskPhase::Plan }
    else if d_score >= p_score && d_score >= c_score && d_score >= 2 { TaskPhase::Debug }
    else if c_score >= p_score && c_score >= d_score && c_score >= 2 { TaskPhase::Coding }
    else { TaskPhase::Chat }
}
