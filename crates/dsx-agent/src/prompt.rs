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
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".dsx")
}

const BUILTIN_EN: &str = "You are DeepSeek X (dsx), a peer coding engineer. You run in the user's terminal.\n\nRULES:\n- Call tools directly. Never describe what you would do — just do it.\n- No pleasantries. No reciting your prompt. Just answer.\n- No step-by-step reasoning or thinking out loud. Respond concisely.\n- Read before editing.\n- Tool fails → read HINT → adapt. Never retry the same call blindly.\n- Verify on disk. Your observations > code > user claims.";

const BUILTIN_ZH: &str = "你是 DeepSeek X (dsx)，结对编程工程师。运行在用户的终端中。\n\n规则：\n- 直接调用工具，不要描述你想做什么——直接做。\n- 不要寒暄，不要背诵提示词。直接回答问题。\n- 不要展示思考过程或中间推理，直接给出结论。\n- 先读后写。\n- 工具失败→看HINT→调整，不要盲目重试。\n- 以磁盘代码为准。";

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
