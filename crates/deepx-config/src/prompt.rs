//! System prompt — compiled from embedded markdown.
//!
//! `backend_prompt.md`  defines the agent identity and rules.
//! `os_env.md`           carries runtime environment info (OS, shells, date).

use std::sync::OnceLock;

const DEFAULT_PROMPT: &str = include_str!("../prompts/backend_prompt.md");
const OS_ENV_TEMPLATE: &str = include_str!("../prompts/os_env.md");

/// Cached OS info string. Set at startup.
pub static OS_INFO: OnceLock<String> = OnceLock::new();

/// Cached toolchain versions. Set at startup.
pub static TOOLS_INFO: OnceLock<String> = OnceLock::new();

/// Full system prompt from embedded backend_prompt.md (identity + rules only).
pub fn full_system_prompt() -> String {
    DEFAULT_PROMPT.to_string()
}

/// Full system prompt with runtime environment injected from os_env.md.
///
/// Placeholders in os_env.md:
///   {{DATE}}   → today's date
///   {{OS}}     → OS_INFO (set at startup via agent_bridge)
///   {{SHELLS}} → auto-detected shells available on this machine
///   {{TOOLS}}  → TOOLS_INFO (toolchain versions detected at startup)
pub fn full_system_prompt_with_date(today: &str, os_info: &str) -> String {
    let shells = detect_shells();
    let tools = TOOLS_INFO
        .get()
        .map(|s| s.as_str())
        .unwrap_or("(not detected)");
    let os = if os_info.is_empty() {
        std::env::consts::OS
    } else {
        os_info
    };
    let env_block = OS_ENV_TEMPLATE
        .replace("{{DATE}}", today)
        .replace("{{OS}}", os)
        .replace("{{SHELLS}}", &shells)
        .replace("{{TOOLS}}", tools);
    format!("{}\n\n{}", DEFAULT_PROMPT, env_block)
}

/// Detect available shells on this machine.
fn detect_shells() -> String {
    let mut shells: Vec<&str> = Vec::new();
    if cfg!(windows) {
        shells.push("cmd");
        // pwsh (PowerShell 7) — check both common install locations
        let pwsh_paths = [
            r"C:\Program Files\WindowsApps\Microsoft.PowerShell_7.6.4.0_x64__8wekyb3d8bbwe\pwsh.exe",
            r"C:\Program Files (x86)\PowerShell\7\pwsh.exe",
        ];
        if pwsh_paths.iter().any(|p| std::path::Path::new(p).exists()) {
            shells.push("pwsh (PowerShell 7)");
        }
        // Legacy Windows PowerShell
        if std::path::Path::new(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe")
            .exists()
        {
            shells.push("powershell (Windows PowerShell 5)");
        }
    } else {
        shells.push("bash");
        shells.push("sh");
        if std::path::Path::new("/bin/zsh").exists() {
            shells.push("zsh");
        }
    }
    shells.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_is_not_empty() {
        assert!(!full_system_prompt().is_empty());
    }

    #[test]
    fn prompt_contains_identity() {
        assert!(full_system_prompt().contains("[IDENTITY]"));
    }

    #[test]
    fn prompt_contains_mermaid_visualization_formats() {
        let prompt = full_system_prompt();
        assert!(prompt.contains("[VISUALIZATION FORMATS]"));
        assert!(prompt.contains("```mermaid"));
        assert!(prompt.contains("flowchart TD"));
        assert!(prompt.contains("sequenceDiagram"));
        assert!(prompt.contains("mindmap"));
    }
}
