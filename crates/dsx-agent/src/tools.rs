//! Tool execution via IPC to dsx-tools subprocess.
//!
//! The IPC connection is initialised once by runner.rs after spawning dsx-tools.
//! All tool calls go through `execute_tool()` which sends a JSON-LP frame over
//! the pipe and reads the response. Confirmation requests are auto-approved
//! (pre-gated by the orchestrator's safety checks).

use crate::api::StreamEvent;
use dsx_proto::{self, AgentToTools, ToolsToAgent};
use dsx_types::{SafetyLevel, TaskPhase, ToolDef};
use std::io::{BufRead, Write};
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

// ── Global flags ──

/// Global auto-mode flag, read by tool wrappers.
pub static AUTO_MODE: AtomicBool = AtomicBool::new(false);

/// Global cancel flag, set when user presses Esc.
pub static CANCEL: AtomicBool = AtomicBool::new(false);

// ── IPC state ──

struct ToolsIpcState {
    reader: Box<dyn BufRead + Send>,
    writer: Box<dyn Write + Send>,
    tool_defs: Vec<ToolDef>,
}

static TOOLS_IPC: Mutex<Option<ToolsIpcState>> = Mutex::new(None);

/// Initialise or replace the tools IPC connection.
/// Called by runner after spawning dsx-tools and completing the Init/Ready handshake.
pub fn init_tools_ipc(
    reader: Box<dyn BufRead + Send>,
    writer: Box<dyn Write + Send>,
    tool_defs: Vec<ToolDef>,
) {
    if let Ok(mut guard) = TOOLS_IPC.lock() {
        *guard = Some(ToolsIpcState { reader, writer, tool_defs });
    }
}

// ── Tool definition accessors (backed by cached Init/Ready data) ──

fn with_ipc<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut ToolsIpcState) -> R,
{
    let mut guard = TOOLS_IPC.lock().ok()?;
    guard.as_mut().map(|state| f(state))
}

/// Get all available tools from the cached definitions.
pub fn all_tools() -> Vec<ToolDef> {
    with_ipc(|state| state.tool_defs.clone()).unwrap_or_default()
}

/// Get tools for a given phase (currently returns all tools — no phase filtering).
pub fn tools_for_phase(_phase: TaskPhase) -> Vec<ToolDef> {
    all_tools()
}

// ── Execute ──

/// Execute a tool synchronously via IPC to dsx-tools.
///
/// Sends an `AgentToTools::CallReq` frame and reads the response.
/// Confirmation requests (`ToolConfirmReq`) are auto-approved — the caller's
/// gate layer is expected to have cleared the tool before calling here.
///
/// Returns the tool's output content, possibly with an `[OK]`, `[ERROR]` or `[FAIL]` prefix.
pub fn execute_tool(name: &str, action: &str, args: &str) -> String {
    // ── Direct exec: bypass IPC to isolate crashes from other tools ──
    if name == "exec" || name.starts_with("exec/") {
        return exec_direct(args);
    }

    let (call_id, args_val) = {
        let id = format!("agent_{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0));
        let val: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
        (id, val)
    };

    let result = with_ipc(|state| {
        // Check cancel before proceeding
        if CANCEL.load(std::sync::atomic::Ordering::SeqCst) {
            CANCEL.store(false, std::sync::atomic::Ordering::SeqCst);
            return Err("[CANCELLED]".to_string());
        }

        // Send CallReq
        let effective_action = if action.is_empty() { name } else { action };
        let call = AgentToTools::CallReq {
            id: call_id.clone(),
            name: name.to_string(),
            action: effective_action.to_string(),
            args: args_val,
            timeout_secs: Some(60),
        };
        if dsx_proto::write_frame(&mut state.writer, &call).is_err() {
            return Err("tools IPC write failed".to_string());
        }

        // Read first response frame
        let response: ToolsToAgent = match dsx_proto::read_frame(&mut state.reader) {
            Ok(Some(r)) => r,
            Ok(None) => return Err("tools IPC: connection closed (EOF)".to_string()),
            Err(e) => return Err(format!("tools IPC read error: {}", e)),
        };

        // Check cancel after read
        if CANCEL.load(std::sync::atomic::Ordering::SeqCst) {
            CANCEL.store(false, std::sync::atomic::Ordering::SeqCst);
            return Ok("[CANCELLED] Tool execution cancelled.".to_string());
        }

        // Handle confirm request — auto-approve (gates have already checked safety)
        if let ToolsToAgent::ToolConfirmReq { id, .. } = &response {
            let confirm = AgentToTools::ConfirmResp {
                id: id.clone(),
                approved: true,
            };
            let _ = dsx_proto::write_frame(&mut state.writer, &confirm);
            // Read actual result
            return match dsx_proto::read_frame::<ToolsToAgent>(&mut state.reader) {
                Ok(Some(ToolsToAgent::Result { content, .. })) => Ok(content),
                Ok(Some(ToolsToAgent::ToolResultMessage { content, .. })) => Ok(content),
                Ok(Some(ToolsToAgent::ToolError { error, .. })) => Ok(format!("[ERROR] {}", error)),
                Ok(Some(_)) => Ok("[ERROR] unexpected IPC frame type".to_string()),
                Ok(None) => Err("tools IPC: EOF after confirmation".to_string()),
                Err(e) => Err(format!("tools IPC read error after confirmation: {}", e)),
            };
        }

        // Normal result
        match response {
            ToolsToAgent::Result { content, .. } => Ok(content),
            ToolsToAgent::ToolResultMessage { content, .. } => Ok(content),
            ToolsToAgent::ToolError { error, .. } => Ok(format!("[ERROR] {}", error)),
            ToolsToAgent::Progress { content, .. } => {
                // Sync execution shouldn't produce progress, but consume it anyway
                Ok(format!("[OK] (no result, progress: {})", content))
            }
            _ => Ok("[ERROR] unexpected IPC frame type (expected tool result)".to_string()),
        }
    });

    match result {
        Some(Ok(content)) => content,
        Some(Err(msg)) => {
            // IPC error — reset connection so runner can reinitialize
            if msg.contains("IPC:") || msg.contains("IPC write") || msg.contains("read error") || msg.contains("EOF") {
                reset_tools_ipc();
            }
            format!("[ERROR] {}", msg)
        }
        None => "[ERROR] tools IPC not initialised — call init_tools_ipc() first".to_string(),
    }
}

// ── Safety classification (compat heuristic; real safety lives in dsx-tools/safety.rs) ──

/// Classify a tool call's safety level based on name and args.
pub fn classify_tool(name: &str, args: &str) -> (SafetyLevel, String) {
    match name {
        "exec" => {
            let cmd = serde_json::from_str::<serde_json::Value>(args)
                .ok()
                .and_then(|v| v.get("command").and_then(|c| c.as_str()).map(|s| s.to_string()))
                .unwrap_or_default();
            classify_exec_command(&cmd)
        }
        "file" => {
            let action = serde_json::from_str::<serde_json::Value>(args)
                .ok()
                .and_then(|v| v.get("action").and_then(|a| a.as_str()).map(|s| s.to_string()))
                .unwrap_or_default();
            match action.as_str() {
                "read" => (SafetyLevel::Safe, String::new()),
                "write" | "edit" | "diff" => (SafetyLevel::Confirm, format!("file/{} needs confirmation", action)),
                _ => (SafetyLevel::Confirm, format!("{} needs confirmation", name)),
            }
        }
        "agent" => (SafetyLevel::Danger, "agent tool needs confirmation".to_string()),
        // Read-only tools — always safe
        n if ["read_file", "list_dir", "search", "glance", "web", "git",
                "explore", "list_skills", "read_skill_ref"].contains(&n) => {
            (SafetyLevel::Safe, String::new())
        }
        // Write tools — confirm
        n if ["write_file", "edit_file", "edit_file_diff"].contains(&n) => {
            (SafetyLevel::Confirm, format!("{} needs confirmation", n))
        }
        _ => (SafetyLevel::Confirm, format!("{} needs confirmation", name)),
    }
}

/// Heuristic safety classification for exec commands.
fn classify_exec_command(cmd: &str) -> (SafetyLevel, String) {
    let dangerous = [
        "sudo rm -rf /", "sudo rm -r /", "sudo rm /", "sudo rm -rf",
        "rm -rf /", "rm -rf ~", "rm -rf .",
        "dd if=", "mkfs.", "fdisk", ":(){ :|:& };:",
        "chmod 777 /", "chmod -R 777 /", "chown -R",
        "> /dev/sda", "mv /", "rm -r /",
        "shutdown", "reboot", "halt", "poweroff",
    ];
    if dangerous.iter().any(|d| cmd.contains(d)) {
        return (SafetyLevel::Danger, format!("Potentially destructive command: {}", cmd));
    }

    let safe_prefixes = [
        "sudo ", "ls", "cat ", "grep ", "find ", "head ", "tail ", "wc ", "sort", "uniq",
        "echo ", "date", "pwd", "whoami", "uname", "which ", "type ", "env",
        "git status", "git diff", "git log", "git branch", "git show",
        "du ", "df ", "free", "uptime", "ps ", "pgrep",
        "cargo check", "cargo build --check",
    ];
    if safe_prefixes.iter().any(|p| cmd.starts_with(p)) {
        return (SafetyLevel::Safe, String::new());
    }

    (SafetyLevel::Confirm, format!("Run: {}?", cmd))
}

// ── Session seed (forwarded via Init frame at connection time) ──

/// Set current session seed for tools subprocess.
pub fn set_current_session(_seed: &str) {
    // Session seed is sent in the AgentToTools::Init frame during connection setup;
    // subsequent seeds are propagated implicitly.
}

// ── Wrap helper ──

/// Wrap raw tool output with the tool name header.
pub fn wrap_tool_result(name: &str, raw: &str) -> String {
    let prefix = if raw.starts_with("[ERROR]") || raw.starts_with("[FAIL]") {
        ""
    } else {
        "[OK] "
    };
    format!("{}{}:\n{}", prefix, name, raw)
}

// ── Async exec stubs (used by old orchestrator code paths; not yet wired via IPC) ──

/// Spawn an exec via PTY. Stub — fires ExecDone immediately.
pub fn spawn_exec_pty(id: &str, _args: &str, tx: tokio::sync::mpsc::Sender<StreamEvent>) {
    let id = id.to_string();
    tokio::spawn(async move {
        let _ = tx.send(StreamEvent::ExecDone(id, "[OK] exec (stub)".to_string())).await;
    });
}

/// Spawn an exec async. Stub — fires ExecDone immediately.
pub fn spawn_exec_async(id: &str, _args: &str, tx: tokio::sync::mpsc::Sender<StreamEvent>) {
    let id = id.to_string();
    tokio::spawn(async move {
        let _ = tx.send(StreamEvent::ExecDone(id, "[OK] exec (stub)".to_string())).await;
    });
}

// ── Shutdown ──

/// Send a graceful shutdown frame to the tools subprocess.
pub fn shutdown_tools() {
    with_ipc(|state| {
        let _ = dsx_proto::write_frame(&mut state.writer, &AgentToTools::Shutdown);
    });
}

/// Reset the tools IPC state (mark as disconnected). Next with_ipc call will return None.
pub fn reset_tools_ipc() {
    if let Ok(mut guard) = TOOLS_IPC.lock() {
        *guard = None;
    }
}

/// Handle a tool confirmation response from TUI — forward ConfirmResp to tools subprocess.
pub fn handle_tool_confirm(id: String, approved: bool) {
    with_ipc(|state| {
        let confirm = AgentToTools::ConfirmResp { id, approved };
        let _ = dsx_proto::write_frame(&mut state.writer, &confirm);
    });
}

/// Send cancel to the tools subprocess for the current operation.
pub fn cancel_current_tool() {
    with_ipc(|state| {
        let cancel = AgentToTools::Cancel { id: None };
        let _ = dsx_proto::write_frame(&mut state.writer, &cancel);
    });
}

// ── Direct exec (bypasses IPC to isolate exec crashes) ──

/// Execute a shell command directly in the agent process, bypassing the dsx-tools IPC pipe.
/// This prevents exec crashes (OOM, segfault, kill-9) from taking down other tools.
fn exec_direct(args: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(_) => return "[ERROR] exec: invalid JSON args".into(),
    };
    let command = v.get("command").and_then(|c| c.as_str()).unwrap_or("");
    if command.trim().is_empty() {
        return "[ERROR] exec: empty command".into();
    }
    let cwd = v.get("cwd").and_then(|c| c.as_str());
    let timeout = v.get("timeout_secs")
        .and_then(|t| t.as_u64())
        .filter(|&n| n > 0 && n <= 3600)
        .unwrap_or(30);

    let mut cmd = std::process::Command::new("sh");
    cmd.args(["-c", command]);
    if let Some(dir) = cwd { cmd.current_dir(dir); }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return format!("[ERROR] exec spawn failed: {}", e),
    };
    let pid = child.id();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    use std::sync::atomic::Ordering;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
    let output = loop {
        if CANCEL.load(Ordering::SeqCst) {
            CANCEL.store(false, Ordering::SeqCst);
            let _ = std::process::Command::new("kill").args([&pid.to_string()]).output();
            return "[CANCELLED] exec cancelled by user.".into();
        }
        let remaining = deadline.checked_duration_since(std::time::Instant::now()).unwrap_or_default();
        if remaining.is_zero() {
            return format!("[ERROR] exec timed out after {}s", timeout);
        }
        let poll = remaining.min(std::time::Duration::from_secs(1));
        match rx.recv_timeout(poll) {
            Ok(Ok(o)) => break o,
            Ok(Err(e)) => return format!("[ERROR] exec wait failed: {}", e),
            Err(_) => continue,
        }
    };

    let exit_code = output.status.code().unwrap_or(-1);
    let status = if exit_code == 0 { "OK" } else { "FAIL" };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut result = format!("[{}] exec: {} (exit {})\n", status, command, exit_code);
    if stdout.trim().is_empty() && stderr.trim().is_empty() {
        result.push_str("(no output)");
    } else {
        result.push_str(stdout.trim_end());
        if !stderr.trim().is_empty() {
            result.push_str("\n── stderr ──\n");
            result.push_str(stderr.trim_end());
        }
    }
    result
}
