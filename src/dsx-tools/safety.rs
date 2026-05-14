//! Safety verdicts for tool invocation.
//!
//! Each tool's safety function returns a `SafetyVerdict` that the
//! ToolManager/Agent uses to decide whether to auto-execute, ask for
//! confirmation, or block outright.
//!
//! Centralized classification logic from exec.rs (is_danger_command,
//! is_safe_command) lives here.

/// The result of a tool safety check.
#[derive(Debug, Clone)]
pub enum SafetyVerdict {
    /// Automatically allow execution (e.g. read, list, search).
    Allow,
    /// Ask the user for confirmation with the given prompt.
    Confirm(String),
    /// Block execution with the given reason.
    Block(String),
}

impl SafetyVerdict {
    /// Auto-allow shorthand.
    pub const fn allowed() -> Self {
        SafetyVerdict::Allow
    }

    /// Confirm with a default prompt.
    pub fn confirm(name: &str) -> Self {
        SafetyVerdict::Confirm(format!("Allow {}?", name))
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
    ];
    dangerous.iter().any(|d| cmd.contains(d))
}

pub fn is_safe_command(cmd: &str) -> bool {
    let safe_prefixes = [
        "sudo ", "ls", "cat ", "grep ", "find ", "head ", "tail ", "wc ", "sort", "uniq",
        "echo ", "date", "pwd", "whoami", "uname", "which ", "type ", "env",
        "git status", "git diff", "git log", "git branch", "git show",
        "du ", "df ", "free", "uptime", "ps ", "pgrep",
        "cargo check", "cargo build --check",
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
    } else if is_safe_command(&cmd) {
        SafetyVerdict::Allow
    } else {
        SafetyVerdict::Confirm(format!("Run: {}?", command))
    }
}

/// Heuristic: is this command likely long-running?
pub fn is_long_command(args: &serde_json::Value) -> bool {
    let command = args.get("command")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_lowercase();
    let long_indicators = [
        "make", "cargo build", "cargo test", "npm install", "npm run build",
        "pip install", "cmake", "gradle", "mvn", "docker build",
        "sudo apt", "brew install", "rustup",
    ];
    long_indicators.iter().any(|i| command.contains(i))
}
