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

/// Cached shell inventory. Discovery must remain side-effect free because this
/// code runs synchronously before a newly spawned agent enters its input loop.
static SHELLS_INFO: OnceLock<String> = OnceLock::new();

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
        .replace("{{SHELLS}}", shells)
        .replace("{{TOOLS}}", tools);
    format!("{}\n\n{}", DEFAULT_PROMPT, env_block)
}

/// Detect available shells on this machine.
fn detect_shells() -> &'static str {
    SHELLS_INFO.get_or_init(|| {
        let mut shells: Vec<&str> = Vec::new();
        if cfg!(windows) {
            // Never spawn a shell as a capability probe here. Git Bash startup
            // can block for tens of seconds under concurrent agent creation.
            // 顺序与默认 shell 一致：pwsh 优先。
            if executable_on_path("pwsh") {
                shells.push("pwsh (PowerShell 7)");
            }
            if executable_on_path("bash") {
                shells.push("bash (Git for Windows)");
            }
            shells.push("cmd");
        } else {
            shells.push("bash");
            shells.push("sh");
            if std::path::Path::new("/bin/zsh").exists() {
                shells.push("zsh");
            }
        }
        shells.join(", ")
    })
}

fn executable_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    executable_in_dirs(name, std::env::split_paths(&path))
}

fn executable_in_dirs(name: &str, dirs: impl IntoIterator<Item = std::path::PathBuf>) -> bool {
    #[cfg(windows)]
    let candidates = if std::path::Path::new(name).extension().is_some() {
        vec![name.to_string()]
    } else {
        ["exe", "cmd", "bat", "com"]
            .into_iter()
            .map(|extension| format!("{name}.{extension}"))
            .collect()
    };
    #[cfg(not(windows))]
    let candidates = vec![name.to_string()];

    dirs.into_iter().any(|dir| {
        candidates
            .iter()
            .any(|candidate| is_executable_file(&dir.join(candidate)))
    })
}

fn is_executable_file(path: &std::path::Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return path
            .metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
    }
    #[cfg(not(unix))]
    true
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
    fn prompt_visualization_is_optional() {
        let prompt = full_system_prompt();
        assert!(prompt.contains("[OPTIONAL VISUALIZATION]"));
        assert!(prompt.contains("不要把图表教程或 Mermaid 作为默认输出"));
    }

    #[test]
    fn prompt_teaches_edit_file() {
        let prompt = full_system_prompt();
        assert!(prompt.contains("[FILE EDITING]"));
        assert!(prompt.contains("edit_file"));
    }

    #[test]
    fn prompt_contains_task_management_section() {
        let prompt = full_system_prompt();
        assert!(prompt.contains("[TASK MANAGEMENT]"));
        assert!(prompt.contains("统一 `todo` 工具"));
        assert!(prompt.contains("create_batch"));
        assert!(prompt.contains("不要并行发多个 create"));
    }

    #[test]
    fn executable_discovery_reads_directories_without_starting_the_candidate() {
        let root = std::env::temp_dir().join(format!("deepx-shell-probe-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        #[cfg(windows)]
        let candidate = root.join("probe-shell.exe");
        #[cfg(not(windows))]
        let candidate = root.join("probe-shell");
        #[cfg(windows)]
        std::fs::write(&candidate, b"not an executable").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&candidate, b"#!/bin/sh\n: > \"$0.ran\"\n").unwrap();
            std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        assert!(executable_in_dirs(
            "probe-shell",
            std::iter::once(root.clone())
        ));
        assert!(!root.join("probe-shell.ran").exists());

        let _ = std::fs::remove_file(candidate);
        let _ = std::fs::remove_dir(root);
    }
}
