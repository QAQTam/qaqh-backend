//! Phase detection from AI reasoning streams.
//!
//! Currently only task phase detection (Plan/Coding/Debug) for auto-mode routing.
//! AgentPhase was removed — it was detected but never consumed.

use dsx_types::TaskPhase;

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
    else { TaskPhase::Coding }
}
