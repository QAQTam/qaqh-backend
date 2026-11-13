//! Tool execution via IPC to dsx-tools subprocess.
//!
//! The IPC connection is initialised once by runner.rs after spawning dsx-tools.
//! All tool calls go through `execute_tool()` which sends a JSON-LP frame over
//! the pipe and reads the response.

use crate::api::StreamEvent;
use dsx_proto::{self, AgentToTools, ToolsToAgent};
use dsx_types::{SafetyLevel, TaskPhase, ToolDef};
use std::io::{BufRead, Write};
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;
use tokio::process::Command as TokioCommand;

/// Tools whitelist that the agent exposes to the LLM.
/// Used by runner.rs and tools_spawn.rs during init/respawn.
pub const ESSENTIAL_TOOLS: &[&str] = &[
    "exec", "read_file", "write_file", "edit_file", "edit_file_diff",
    "explore", "search", "list_dir",
    "task_create", "task_update", "task_list",
    "plan_create", "plan_update", "plan_read", "plan_list",
    "web_fetch", "web_search",
];

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
///
/// Returns the tool's output content, possibly with an `[OK]`, `[ERROR]` or `[FAIL]` prefix.
pub fn execute_tool(name: &str, action: &str, args: &str) -> String {
    execute_tool_with_id(name, action, args, "")
}

/// Like `execute_tool` but passes a tool_call_id for streaming exec progress.
pub fn execute_tool_with_id(name: &str, action: &str, args: &str, tool_call_id: &str) -> String {
    // ── Direct exec: bypass IPC to isolate crashes from other tools ──
    if name == "exec" || name.starts_with("exec/") {
        let cmd = serde_json::from_str::<serde_json::Value>(args)
            .ok()
            .and_then(|v| v.get("command").and_then(|c| c.as_str().map(String::from)))
            .unwrap_or_default();
        let (level, reason) = classify_exec_command(&cmd);
        if level == dsx_types::SafetyLevel::Danger {
            return format!("[ERROR] Blocked: {}", reason);
        }
        return exec_direct(args, tool_call_id);
    }

    let args_val: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
    let call_id = if tool_call_id.is_empty() {
        format!("agent_{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0))
    } else {
        tool_call_id.to_string()
    };
    let has_id = !tool_call_id.is_empty();

    let result = with_ipc(|state| {
        // Check cancel before proceeding
        if CANCEL.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("[CANCELLED]".to_string());
        }

        // Send CallReq
        let effective_action = if action.is_empty() { name } else { action };
        let call = AgentToTools::CallReq {
            id: call_id.clone(),
            name: name.to_string(),
            action: effective_action.to_string(),
            args: args_val.clone(),
            timeout_secs: Some(60),
        };
        if dsx_proto::write_frame(&mut state.writer, &call).is_err() {
            return Err("tools IPC write failed".to_string());
        }

        // Loop: read frames, forward Progress, return on final result
        loop {
            let response: ToolsToAgent = match dsx_proto::read_frame(&mut state.reader) {
                Ok(Some(r)) => r,
                Ok(None) => return Err("tools IPC: connection closed (EOF)".to_string()),
                Err(e) => return Err(format!("tools IPC read error: {}", e)),
            };

            // Check cancel
            if CANCEL.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok("[CANCELLED] Tool execution cancelled.".to_string());
            }

            // Forward Progress frames to Tauri
            if let ToolsToAgent::Progress { id, content, stream_type } = &response {
                if has_id {
                    let frame = serde_json::json!({
                        "type": "tool_progress",
                        "id": id,
                        "content": content,
                        "stream_type": stream_type,
                    });
                    if let Ok(s) = serde_json::to_string(&frame) {
                        let _ = writeln!(std::io::stdout(), "{s}");
                        let _ = std::io::stdout().flush();
                    }
                }
                continue;
            }

            // Final result
            return match response {
                ToolsToAgent::Result { content, .. } => Ok(content),
                ToolsToAgent::ToolResultMessage { content, .. } => Ok(content),
                ToolsToAgent::ToolError { error, .. } => Ok(format!("[ERROR] {}", error)),
                _ => Ok("[ERROR] unexpected IPC frame type (expected tool result)".to_string()),
            };
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
                _ => (SafetyLevel::Safe, String::new()),
            }
        }
        // Read-only tools — always safe
        n if ["read_file", "list_dir", "search", "web",
                "explore", "list_skills", "read_skill_ref"].contains(&n) => {
            (SafetyLevel::Safe, String::new())
        }
        // Write tools — safe
        n if ["write_file", "edit_file", "edit_file_diff"].contains(&n) => {
            (SafetyLevel::Safe, String::new())
        }
        _ => (SafetyLevel::Safe, String::new()),
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
        // Windows destructive commands
        "format ", "diskpart", "del /f /s", "rmdir /s /q",
        "rd /s /q", "reg delete", "takeown /f",
    ];
    if dangerous.iter().any(|d| cmd.contains(d)) {
        return (SafetyLevel::Danger, format!("Potentially destructive command: {}", cmd));
    }

    let safe_prefixes = [
        // Unix
        "ls", "cat ", "grep ", "find ", "head ", "tail ", "wc ", "sort", "uniq",
        "echo ", "date", "pwd", "whoami", "uname", "which ", "type ", "env",
        "git status", "git diff", "git log", "git branch", "git show",
        "du ", "df ", "free", "uptime", "ps ", "pgrep",
        "cargo check", "cargo build --check",
        // Windows
        "dir", "type ", "help", "mkdir", "copy ", "move ",
        "echo ", "cd ", "set ", "where ",
    ];
    if safe_prefixes.iter().any(|p| cmd.starts_with(p)) {
        return (SafetyLevel::Safe, String::new());
    }

    (SafetyLevel::Safe, String::new())
}

// ── Session seed (forwarded via Init frame at connection time) ──

/// Set current session seed for tools subprocess.
pub fn set_current_session(_seed: &str) {
    // Session seed is sent in the AgentToTools::Init frame during connection setup;
    // subsequent seeds are propagated implicitly.
}

// ── Wrap helper ──

/// Wrap raw tool output with the tool name header.
/// Tool outputs already carry their own [OK]/[ERROR]/[FAIL] prefix,
/// so we just add the tool name line.
pub fn wrap_tool_result(name: &str, raw: &str) -> String {
    format!("{}:\n{}", name, raw)
}

// ── Async exec (tokio-based, used by orchestrator paths) ──

/// Parse `command` and `cwd` from exec args JSON.
fn parse_exec_args(args: &str) -> (String, Option<String>, u64) {
    match serde_json::from_str::<serde_json::Value>(args) {
        Ok(v) => (
            v.get("command").or_else(|| v.get("cmd")).and_then(|c| c.as_str()).unwrap_or("").to_string(),
            v.get("cwd").and_then(|c| c.as_str()).map(|s| s.to_string()),
            v.get("timeout").or_else(|| v.get("timeout_secs")).and_then(|t| t.as_u64()).filter(|&n| n > 0 && n <= 3600).unwrap_or(30),
        ),
        Err(_) => (String::new(), None, 30),
    }
}

/// Run a command via tokio::process::Command and return output.
/// Sends ExecStarted with PID for cancellation support.
async fn run_command_async(
    command: &str,
    cwd: Option<&str>,
    timeout_secs: u64,
    id: &str,
    tx: &tokio::sync::mpsc::Sender<StreamEvent>,
) -> String {
    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = TokioCommand::new("cmd");
        c.args(["/C", command]);
        c
    } else {
        let mut c = TokioCommand::new("sh");
        c.args(["-c", command]);
        c
    };
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    // Spawn first to get PID for cancellation
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return format!("[ERROR] exec: spawn failed: {}", e),
    };

    // Notify PID for cancellation support
    if let Some(pid) = child.id() {
        let _ = tx.send(StreamEvent::ExecStarted(id.to_string(), pid)).await;
    }

    // Wait with timeout
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        child.wait_with_output(),
    ).await;

    match output {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if out.status.success() {
                format!("[OK] exec (exit 0)")
            } else {
                let err_output = if stderr.trim().is_empty() { stdout.to_string() } else { stderr.to_string() };
                let tail: Vec<&str> = err_output.lines().rev().take(10).collect();
                let summary = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
                format!("[ERROR] exec (exit {})\n{}", out.status.code().unwrap_or(-1), summary)
            }
        }
        Ok(Err(e)) => format!("[ERROR] exec: wait failed: {}", e),
        Err(_) => "[CANCELLED] exec timed out".into(),
    }
}

/// Spawn an exec via PTY. Real implementation using tokio::process.
pub fn spawn_exec_pty(id: &str, args: &str, tx: tokio::sync::mpsc::Sender<StreamEvent>) {
    let id = id.to_string();
    let args = args.to_string();
    tokio::spawn(async move {
        let (command, cwd, timeout) = parse_exec_args(&args);
        if command.is_empty() {
            let _ = tx.send(StreamEvent::ExecDone(id, "[ERROR] exec: empty command".into())).await;
            return;
        }
        let result = run_command_async(&command, cwd.as_deref(), timeout, &id, &tx).await;
        let _ = tx.send(StreamEvent::ExecDone(id, result)).await;
    });
}

/// Spawn an exec async. Real implementation using tokio::process.
pub fn spawn_exec_async(id: &str, args: &str, tx: tokio::sync::mpsc::Sender<StreamEvent>) {
    spawn_exec_pty(id, args, tx);
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

/// Send cancel to the tools subprocess for the current operation.
pub fn cancel_current_tool() {
    with_ipc(|state| {
        let cancel = AgentToTools::Cancel { id: None };
        let _ = dsx_proto::write_frame(&mut state.writer, &cancel);
    });
}

// ── Direct exec (bypasses IPC to isolate exec crashes) ──

/// Execute a shell command directly in the agent process, bypassing the dsx-tools IPC pipe.
/// Streams each output line to Tauri as `exec_progress` JSON-LP frames when tool_call_id is non-empty.
fn exec_direct(args: &str, tool_call_id: &str) -> String {
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

    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", command]);
        c
    } else {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", command]);
        c
    };
    if let Some(dir) = cwd { cmd.current_dir(dir); }

    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return format!("[ERROR] exec spawn failed: {}", e),
    };
    let pid = child.id();

    // ── Reader threads: read lines, write to Tauri via stdout JSON-LP, accumulate in shared buffers ──
    use std::sync::{Arc, Mutex};

    let out_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let err_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let has_id = !tool_call_id.is_empty();
    let id_owned = tool_call_id.to_string();

    if let Some(out) = child.stdout.take() {
        let buf = out_buf.clone();
        let id = id_owned.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines() {
                let Ok(l) = line else { break };
                if has_id {
                    let frame = serde_json::json!({
                        "type": "exec_progress", "id": id, "line": l,
                    });
                    if let Ok(s) = serde_json::to_string(&frame) {
                        let _ = writeln!(std::io::stdout(), "{s}");
                        let _ = std::io::stdout().flush();
                    }
                }
                buf.lock().unwrap().push_str(&l);
                buf.lock().unwrap().push('\n');
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        let buf = err_buf.clone();
        let id = id_owned;
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(err).lines() {
                let Ok(l) = line else { break };
                if has_id {
                    let frame = serde_json::json!({
                        "type": "exec_progress", "id": id, "line": format!("[stderr] {l}"),
                    });
                    if let Ok(s) = serde_json::to_string(&frame) {
                        let _ = writeln!(std::io::stdout(), "{s}");
                        let _ = std::io::stdout().flush();
                    }
                }
                buf.lock().unwrap().push_str(&l);
                buf.lock().unwrap().push('\n');
            }
        });
    }

    // ── Main thread: wait with cancel + timeout support ──
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(timeout);

    loop {
        if CANCEL.load(Ordering::SeqCst) {
            dsx_types::platform::kill_process(pid);
            return "[CANCELLED] exec cancelled by user.".into();
        }
        if Instant::now() >= deadline {
            return format!("[ERROR] exec timed out after {}s", timeout);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let exit_code = status.code().unwrap_or(0) as i32;
                let stdout = out_buf.lock().unwrap().clone();
                let stderr = err_buf.lock().unwrap().clone();

                if exit_code == 0 {
                    return format!("[OK] exec: {} (exit 0)", command);
                }

                // Failure: return exit code + last 10 lines of error output
                let err_output = if !stderr.trim().is_empty() { stderr } else { stdout };
                let tail: Vec<&str> = err_output.lines().rev().take(10).collect();
                let summary = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
                return format!("[ERROR] exec: {} (exit {})\n{}", command, exit_code, summary);
            }
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                return format!("[ERROR] exec wait failed: {e}");
            }
        }
    }
}

/// Execute exec with streaming progress to Tauri.
pub fn exec_with_streaming(args: &str, tool_call_id: &str) -> String {
    exec_direct(args, tool_call_id)
}
