//! System prompt generation.
//!
//! 1. Try ~/.dsx/prompt.json (user-customizable):
//!    ```json
//!    {
//!      "en": { "base": "...", "rules": ["...", "..."], "style": "..." },
//!      "zh": { "base": "...", "rules": ["...", "..."], "style": "..." }
//!    }
//!    ```
//! 2. Fall back to built-in strings if file missing.

use std::path::PathBuf;

fn prompts_dir() -> PathBuf {
    dsx_types::platform::home_dir().join(".dsx")
}

const BUILTIN_EN: &str = "You are DeepSeek V4 — a 1M-token long-context code architect, running in DeepSeekX terminal as a peer engineering partner.\n\nRULES:\n- DO NOT ask \"how can I help\" or offer options. The user knows what they want — just execute.\n- Once the task is clear, act immediately. Don't ask for permission.\n- Prefer precise, minimal edits over large reads/writes — save tokens.\n- Assume user claims may be inaccurate. Use tool output to verify or correct.\n- Trust source code and tool output. Push back when the user is wrong.\n- Tool fails → read HINT → adapt. Never retry the same call blindly.\n- At the end of your response, state the next concrete action — don't ask \"what else\".\n- Trust what's on disk over what the user says.\n- Be ruthlessly concise: no greetings, no sign-offs, no explaining what you're about to do — just do it.\n- Reason entirely in English.";

const BUILTIN_ZH: &str = "你是 DeepSeek V4 — 1M 长上下文代码架构工程师，运行在 DeepSeekX 终端中，作为高效的结对编程伙伴与用户合作。\n\n规则：\n- 直接执行，不要问「要不要我帮你」或提供可选项。用户明确知道要什么。\n- 任务明确后立即行动，不要征求许可。先 Explore 再修改。\n- 减少大段 Read 和 Edit，精细化修改以节省 Token。\n- 用户描述可能不准确，用工具调用结果验证或纠正。\n- 相信源码和工具输出，勇敢指出用户的错误假设。\n- 工具失败→看 HINT→调整，不要盲目重试。同一个方法不要连续失败三次。\n- 每次回复结束给出下一步具体行动，不要反问「还需要什么」。\n- 以磁盘代码为准。\n- 保持极简：不要开场白、不要结束语、不要解释你在做什么——直接做。\n- 必须完全使用中文思考。";

/// Load the system prompt for the given language.
///
/// Priority: ~/.dsx/prompt.json → built-in.
pub fn system_prompt(lang: &str) -> String {
    let path = prompts_dir().join("prompt.json");
    if let Ok(data) = std::fs::read_to_string(&path) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) {
            let section = if lang == "zh" { &v["zh"] } else { &v["en"] };
            if !section.is_null() {
                let base = section["base"].as_str().unwrap_or("");
                let rules: Vec<&str> = section["rules"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|r| r.as_str()).collect())
                    .unwrap_or_default();
                let style = section["style"].as_str().unwrap_or("");
                let mut result = base.to_string();
                if !rules.is_empty() {
                    result.push_str("\n\nRULES:\n");
                    for r in &rules {
                        result.push_str(&format!("- {r}\n"));
                    }
                }
                if !style.is_empty() {
                    result.push_str(&format!("\n\n{style}"));
                }
                return result;
            }
        }
    }
    // Fallback: return built-in
    match lang {
        "zh" => BUILTIN_ZH.to_string(),
        _ => BUILTIN_EN.to_string(),
    }
}
