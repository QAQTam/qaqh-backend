//! Safety verdicts for tool invocation.
//!
//! Each tool's safety function returns a `SafetyVerdict` that the
//! ToolManager/Agent uses to decide whether to auto-execute
//! or block outright.
//!
//! Centralized classification logic from exec.rs (is_danger_command,
//! is_safe_command) lives here.

/// The result of a tool safety check.
#[derive(Debug, Clone)]
pub enum SafetyVerdict {
    /// Automatically allow execution.
    Allow,
    /// Block execution with the given reason.
    Block(String),
}

impl SafetyVerdict {
    /// Auto-allow shorthand.
    pub const fn allowed() -> Self {
        SafetyVerdict::Allow
    }

    /// Block with a reason.
    pub fn blocked(reason: impl Into<String>) -> Self {
        SafetyVerdict::Block(reason.into())
    }
}

// ── Exec 安全分类（从 exec.rs 移入）──

pub fn is_danger_command(cmd: &str) -> bool {
    let dangerous = [
        "sudo rm -rf", "sudo rm -r /", "sudo rm /", "sudo rm -rf /",
        "rm -rf /", "rm -rf ~", "rm -rf .",
        "dd if=", "mkfs.", "fdisk", ":(){ :|:& };:",
        "chmod 777 /", "chmod -R 777 /", "chown -R",
        "> /dev/sda", "mv /", "rm -r /",
        "shutdown", "reboot", "halt", "poweroff",
        // Windows destructive commands
        "format ", "diskpart", "del /f /s", "rmdir /s /q",
        "rd /s /q", "reg delete", "takeown /f",
    ];
    dangerous.iter().any(|d| cmd.contains(d))
}

pub fn is_safe_command(cmd: &str) -> bool {
    let safe_prefixes = [
        // Unix
        "ls", "cat ", "grep ", "find ", "head ", "tail ", "wc ", "sort", "uniq",
        "echo ", "date", "pwd", "whoami", "uname", "which ", "type ", "env",
        "git status", "git diff", "git log", "git branch", "git show",
        "du ", "df ", "free", "uptime", "ps ", "pgrep",
        "cargo check", "cargo build --check",
        // Windows
        "dir", "type ", "help", "mkdir", "copy ", "move ", "del ",
        "echo ", "cd ", "set ", "powershell -Command get-", "where ",
    ];
    safe_prefixes.iter().any(|p| cmd.starts_with(p))
}

/// Classify an exec/run action based on the command string.
pub fn classify_execution(command: &str) -> SafetyVerdict {
    let cmd = command.trim().to_lowercase();
    if cmd.is_empty() {
        return SafetyVerdict::Block("empty command".into());
    }
    if is_danger_command(&cmd) {
        SafetyVerdict::Block(format!("Potentially destructive command: {}", command))
    } else {
        SafetyVerdict::Allow
    }
}

