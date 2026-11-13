//! Command execution: sync, async, sudo support.
//!
//! 保留旧函数（exec_command, exec_with_sudo, spawn_exec_async 等）供向后兼容。
//! 安全检测逻辑已移至 safety.rs。

use std::process::Command;

use crate::{ToolCallCtx, ToolResult};

/// Get system shell: prefer bash for feature parity, fall back to sh (Alpine/busybox).
fn shell() -> &'static str {
    if cfg!(target_os = "windows") { "cmd" }
    else if std::path::Path::new("/bin/bash").exists() { "bash" }
    else { "sh" }
}

// ── 旧函数：向后兼容 ──

pub fn exec_command(args: &str) -> String {
    let command = parse_arg(args, "command");
    if command.trim().is_empty() {
        return "[ERROR] exec: empty command\n[HINT] Provide a shell command in the `cmd` or `command` parameter.".into();
    }
    let cwd = parse_opt(args, "cwd");
    let timeout_secs = parse_opt_u64(args, "timeout_secs")
        .filter(|&n| n > 0 && n <= 3600)
        .unwrap_or(30);

    // Sudo detection: redirect to exec/sudo_run (uses dsx-sudo on Unix)
    if command.trim().starts_with("sudo ") && !cfg!(target_os = "windows") {
        let inner = command.trim_start().strip_prefix("sudo ").unwrap_or("").trim();
        return format!("[ERROR] Use exec/sudo_run not exec/run for sudo.\n[HINT] Call sudo_run with command=\"{}\" instead.", inner);
    }

    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.args(["/C", &command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", &command]);
        c
    };

    if let Some(dir) = &cwd {
        cmd.current_dir(dir);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return format!("[ERROR] exec '{}' failed to start\n[HINT] {}", command, e),
    };
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();

    std::thread::spawn(move || {
        let result = child.wait_with_output();
        let _ = tx.send(result);
    });

    use std::sync::atomic::Ordering;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let output = loop {
        if crate::CANCEL.load(Ordering::SeqCst) {
            dsx_types::platform::kill_process(pid);
            return "[CANCELLED] Command execution cancelled by user.".into();
        }
        let remaining = deadline.checked_duration_since(std::time::Instant::now()).unwrap_or_default();
        if remaining.is_zero() {
            dsx_types::platform::kill_process(pid);
            return format!("[ERROR] exec timed out after {}s\n[HINT] Increase timeout_secs or check if the command is stuck.", timeout_secs);
        }
        let poll = remaining.min(std::time::Duration::from_secs(1));
        match rx.recv_timeout(poll) {
            Ok(Ok(o)) => break o,
            Ok(Err(e)) => return format!("[ERROR] exec wait failed: {}", e),
            Err(_) => continue, // poll tick — check cancel next iteration
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let full_output = format!("{}\n{}", stdout.trim_end(), stderr.trim_end());
    let full_output = full_output.trim();
    let exit_code = output.status.code().unwrap_or(-1);
    let status = if exit_code == 0 { "OK" } else { "FAIL" };
    let mut result = format!("[{}] exec: {} (exit {})\n", status, command, exit_code);

    if full_output.is_empty() {
        result.push_str("(no output)");
        return result;
    }

    let lines: Vec<&str> = full_output.lines().collect();
    let total = lines.len();

    // Adaptive limits by command type
    let (head_count, tail_count) = if command.contains("make ") || command.contains("cargo build") || command.contains("cargo check") {
        (120, 80)
    } else if command.contains("cargo test") || command.contains("go test") || command.contains("pytest") {
        (20, 200)
    } else if command.contains("grep") || command.contains("find ") || command.contains("ls ") {
        (200, 30)
    } else {
        (80, 40)
    };

    // Error indices for context display
    let error_indices: Vec<usize> = lines.iter().enumerate()
        .filter(|(_, l)| l.contains("error") || l.contains("Error") || l.contains("ERROR")
            || l.contains("fail") || l.contains("FAIL"))
        .map(|(i, _)| i)
        .collect();
    let warning_indices: Vec<usize> = lines.iter().enumerate()
        .filter(|(_, l)| l.contains("warning") || l.contains("Warning") || l.contains("WARN"))
        .map(|(i, _)| i)
        .collect();

    if total > head_count + tail_count {
        for &l in &lines[..head_count] {
            result.push_str(l);
            result.push('\n');
        }
        if head_count + tail_count < total {
            let mid = &lines[head_count..total.saturating_sub(tail_count)];
            let compressed = compress_lines(mid);
            result.push_str(&compressed);
        }
        if tail_count > 0 {
            for &l in &lines[total.saturating_sub(tail_count)..] {
                result.push_str(l);
                result.push('\n');
            }
        }
        // ── Error summary with 2-line context ──
        if !error_indices.is_empty() {
            result.push_str("── errors ──\n");
            let mut last = 0usize;
            for &ei in error_indices.iter().take(20) {
                let start = ei.saturating_sub(2);
                if start <= last { continue; }
                if ei > 2 && ei != error_indices[0] {
                    result.push_str("┊\n");
                }
                for li in start..=ei.saturating_add(2).min(total - 1) {
                    result.push_str(lines[li]);
                    result.push('\n');
                }
                last = ei.saturating_add(2);
            }
            if error_indices.len() > 20 {
                result.push_str(&format!("... ({} more errors)\n", error_indices.len() - 20));
            }
        }

        // ── Warning summary ──
        if !warning_indices.is_empty() {
            result.push_str("── warnings ──\n");
            for &wi in warning_indices.iter().take(10) {
                result.push_str(lines[wi]);
                result.push('\n');
            }
            if warning_indices.len() > 10 {
                result.push_str(&format!("... ({} more warnings)\n", warning_indices.len() - 10));
            }
        }

        if exit_code != 0 {
            result.push_str(&format!("── exit: {} ──\n", exit_code));
        }
        result
    } else {
        for &l in &lines {
            result.push_str(l);
            result.push('\n');
        }
        if !error_indices.is_empty() {
            result.push_str("── errors ──\n");
            let mut last = 0usize;
            for &ei in error_indices.iter().take(20) {
                let start = ei.saturating_sub(2);
                if start <= last { continue; }
                for li in start..=ei.saturating_add(2).min(total - 1) {
                    result.push_str(lines[li]);
                    result.push('\n');
                }
                last = ei.saturating_add(2);
            }
        }
        if !warning_indices.is_empty() {
            result.push_str("── warnings ──\n");
            for &wi in warning_indices.iter().take(10) {
                result.push_str(lines[wi]);
                result.push('\n');
            }
        }
        if exit_code != 0 {
            result.push_str(&format!("── exit: {} ──\n", exit_code));
        }
        result
    }
}

pub fn exec_with_sudo(command: &str, password: &str) -> String {
    use std::io::Write;
    let mut child = match std::process::Command::new("sudo")
        .args(["-S", "-p", "", "--"])
        .arg(shell())
        .arg("-c")
        .arg(command.trim_start_matches("sudo ").trim())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return format!("[ERROR] Cannot spawn sudo: {}\n[HINT] Is sudo installed?", e),
    };

    if let Some(ref mut stdin) = child.stdin {
        let _ = stdin.write_all(password.as_bytes());
        let _ = stdin.write_all(b"\n");
    }

    match child.wait_with_output() {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let stderr = String::from_utf8_lossy(&o.stderr);
            if stderr.contains("incorrect password") || stderr.contains("3 incorrect") {
                return format!("[ERROR] Incorrect sudo password\n[HINT] Try again.");
            }
            if !o.status.success() {
                let se = if stderr.is_empty() { String::new() }
                    else { format!("\n── stderr ──\n{}", stderr.trim_end()) };
                format!("[FAIL] sudo {} (exit {})\n{}{}", command, o.status.code().unwrap_or(-1), stdout.trim_end(), se)
            } else {
                format!("[OK] sudo: {}\n{}", command, stdout.trim_end())
            }
        }
        Err(e) => format!("[ERROR] sudo wait failed: {}", e),
    }
}

pub fn spawn_exec_async(
    tool_call_id: &str,
    args: &str,
    tx: tokio::sync::mpsc::Sender<crate::stubs::StreamEvent>,
) {
    spawn_exec_async_with_sudo(tool_call_id, args, None, tx)
}

pub fn spawn_exec_async_with_sudo(
    tool_call_id: &str,
    args: &str,
    sudo_password: Option<&str>,
    tx: tokio::sync::mpsc::Sender<crate::stubs::StreamEvent>,
) {
    let command = parse_arg(args, "command");
    let cwd = parse_opt(args, "cwd");
    let timeout_secs = parse_opt_u64(args, "timeout_secs")
        .filter(|&n| n > 0 && n <= 3600)
        .unwrap_or(300);
    let id = tool_call_id.to_string();
    let is_sudo = command.trim().starts_with("sudo ");
    let has_password: Option<String> = sudo_password.filter(|p| !p.is_empty()).map(|s| s.to_string());

    if is_sudo && !cfg!(target_os = "windows") && has_password.is_none() {
        let needs_pwd = std::process::Command::new("sudo")
            .args(["-n", "true"])
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true);
        if needs_pwd {
            let _ = tx.try_send(crate::stubs::StreamEvent::ExecDone(
                id, format!("[SUDO_REQUIRED] {}", command),
            ));
            return;
        }
    }

    tokio::spawn(async move {
        let pwd_for_stdin = has_password.clone();
        let mut child: tokio::process::Child = if is_sudo && pwd_for_stdin.is_some() {
            let pwd = pwd_for_stdin.unwrap();
            use tokio::io::AsyncWriteExt;
            let mut cmd = tokio::process::Command::new("sudo");
            cmd.args(["-S", "-p", "", "--"])
                .arg(shell())
                .arg("-c")
                .arg(command.trim_start_matches("sudo ").trim())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            if let Some(dir) = cwd { cmd.current_dir(dir); }
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(crate::stubs::StreamEvent::ExecDone(
                        id, format!("[ERROR] Failed to spawn sudo: {}", e),
                    )).await;
                    return;
                }
            };
            if let Some(ref mut stdin) = child.stdin {
                let _ = stdin.write_all(pwd.as_bytes()).await;
                let _ = stdin.write_all(b"\n").await;
            }
            child
        } else {
            let mut cmd = tokio::process::Command::new(shell());
            cmd.args(["-c", &command])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            if let Some(dir) = cwd { cmd.current_dir(dir); }
            match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(crate::stubs::StreamEvent::ExecDone(
                        id, format!("[ERROR] Failed to spawn: {}", e),
                    )).await;
                    return;
                }
            }
        };

        if let Some(pid) = child.id() {
            let _ = tx.send(crate::stubs::StreamEvent::ExecStarted(id.clone(), pid)).await;
        }

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let tx_stdout = tx.clone();
        let tx_stderr = tx.clone();

        let stdout_handle = tokio::spawn(async move {
            if let Some(mut reader) = stdout {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(&mut reader).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = tx_stdout.send(crate::stubs::StreamEvent::ExecProgress(line)).await;
                }
            }
        });

        let stderr_handle = tokio::spawn(async move {
            if let Some(mut reader) = stderr {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(&mut reader).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = tx_stderr.send(crate::stubs::StreamEvent::ExecProgress(
                        format!("stderr: {}", line)
                    )).await;
                }
            }
        });

        let status = match tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            child.wait(),
        ).await {
            Ok(s) => s,
            Err(_) => {
                let _ = child.kill().await;
                let _ = tx.send(crate::stubs::StreamEvent::ExecDone(
                    id,
                    format!("[ERROR] exec timed out after {}s\n[HINT] Increase timeout_secs or check if the command is stuck.", timeout_secs),
                )).await;
                return;
            }
        };
        let _ = stdout_handle.await;
        let _ = stderr_handle.await;

        let exit_code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
        let status_label = if exit_code == 0 { "OK" } else { "FAIL" };
        let result = format!("[{}] exec: {} (exit {})\n(streaming output shown above)", status_label, command, exit_code);
        let _ = tx.send(crate::stubs::StreamEvent::ExecDone(id, result)).await;
    });
}

// ── Handler 函数（新 IPC 框架）──

pub fn handle_run(ctx: ToolCallCtx) -> ToolResult {
    let command = ctx.get_str("command").unwrap_or("").to_string();

    // Auto-route sudo commands to dsx-sudo for privilege separation
    if command.trim().starts_with("sudo ") && !cfg!(target_os = "windows") {
        return handle_sudo_run(ctx);
    }

    let args = serde_json::json!({
        "command": command,
        "cwd": ctx.get_str("cwd"),
        "timeout_secs": ctx.get_u64("timeout_secs"),
    });
    let result = exec_command(&args.to_string());
    let success = result.starts_with("[OK]");
    ToolResult { success, content: result }
}

pub fn handle_sudo_run(ctx: ToolCallCtx) -> ToolResult {
    if cfg!(target_os = "windows") {
        return ToolResult::err("sudo_run: not supported on Windows");
    }

    let command = ctx.get_str("command").unwrap_or("");
    let cwd = ctx.get_str("cwd");
    let timeout_secs = ctx.get_u64("timeout_secs")
        .filter(|&n| n > 0 && n <= 3600)
        .unwrap_or(300);

    if command.trim().is_empty() {
        return ToolResult::err("sudo_run: empty command");
    }

    // Find dsx-sudo binary: check several locations
    let sudo_bin = find_dsx_sudo().unwrap_or_else(|| "dsx-sudo".into());

    // Tokenize command string into args for dsx-sudo
    let parts: Vec<&str> = command.split_whitespace().collect();
    let (cmd_name, cmd_args) = parts.split_first().unwrap_or((&"", &[]));

    // Build dsx-sudo args: [cmd, args...]
    let mut dsx_args: Vec<&str> = vec![cmd_name];
    dsx_args.extend_from_slice(cmd_args);

    let mut cmd = std::process::Command::new(&sudo_bin);
    cmd.args(&dsx_args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return ToolResult::err(&format!("dsx-sudo spawn: {e}")),
    };
    let pid = child.id();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    use std::sync::atomic::Ordering;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let output = loop {
        if crate::CANCEL.load(Ordering::SeqCst) {
            dsx_types::platform::kill_process(pid);
            return ToolResult::err("sudo command cancelled by user");
        }
        let remaining = deadline.checked_duration_since(std::time::Instant::now()).unwrap_or_default();
        if remaining.is_zero() {
            dsx_types::platform::kill_process(pid);
            return ToolResult::err(&format!("sudo timed out after {}s", timeout_secs));
        }
        let poll = remaining.min(std::time::Duration::from_secs(1));
        match rx.recv_timeout(poll) {
            Ok(Ok(o)) => break o,
            Ok(Err(e)) => return ToolResult::err(&format!("sudo wait failed: {e}")),
            Err(_) => continue,
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let line = stdout.lines().next().unwrap_or("");
    if line.starts_with("{\"ok\":") {
        let success = line.contains("\"ok\":true");
        let content = if success {
            extract_json_field(line, "stdout").unwrap_or_else(|| line.to_string())
        } else {
            let err = extract_json_field(line, "error").unwrap_or_else(|| "unknown error".into());
            format!("[ERROR] sudo: {err}")
        };
        ToolResult { success, content }
    } else if output.status.success() {
        ToolResult::ok(&*stdout)
    } else {
        let err_msg = if !stderr.is_empty() { &*stderr } else { &*stdout };
        ToolResult::err(&format!("sudo failed: {err_msg}"))
    }
}

fn find_dsx_sudo() -> Option<String> {
    let candidates = [
        "/usr/local/lib/dsx/dsx-sudo",
        "/usr/lib/dsx/dsx-sudo",
        "/opt/dsx/dsx-sudo",
    ];
    for p in &candidates {
        if std::path::Path::new(p).exists() {
            return Some(p.to_string());
        }
    }
    None
}

fn extract_json_field(json: &str, field: &str) -> Option<String> {
    let search = format!("\"{field}\":\"");
    let start = json.find(&search)?;
    let val_start = start + search.len();
    let mut val = String::new();
    let mut chars = json[val_start..].chars();
    loop {
        match chars.next() {
            Some('"') if val.ends_with('\\') => { val.push('"'); }
            Some('"') => break,
            Some(c) => val.push(c),
            None => break,
        }
    }
    Some(val)
}

// ── 辅助函数 ──

pub(super) fn compress_lines(lines: &[&str]) -> String {
    if lines.is_empty() { return String::new(); }
    let mut out = String::new();
    let mut group_start = 0;
    for i in 1..=lines.len() {
        let same = if i < lines.len() { same_prefix(lines[group_start], lines[i]) } else { false };
        if !same {
            let count = i - group_start;
            if count > 3 {
                let prefix = common_prefix(&lines[group_start..i]);
                out.push_str(&format!("  ... ({} similar lines) ... {}\n", count, prefix.trim()));
            } else {
                for &l in &lines[group_start..i] {
                    out.push_str(l);
                    out.push('\n');
                }
            }
            group_start = i;
        }
    }
    out
}

fn same_prefix(a: &str, b: &str) -> bool {
    let a_trim = a.trim_start();
    let b_trim = b.trim_start();
    if a_trim.len() < 10 || b_trim.len() < 10 { return false; }
    let len = 40.min(a_trim.len()).min(b_trim.len());
    match (a_trim.get(..len), b_trim.get(..len)) {
        (Some(a_pref), Some(b_pref)) => a_pref == b_pref,
        _ => false,
    }
}

fn common_prefix(lines: &[&str]) -> String {
    if lines.is_empty() { return String::new(); }
    let first = lines[0].trim_start();
    let mut prefix_len = first.len().min(60);
    for &l in &lines[1..] {
        let trimmed = l.trim_start();
        for (i, (a, b)) in first.chars().zip(trimmed.chars()).enumerate() {
            if a != b {
                prefix_len = prefix_len.min(i);
                break;
            }
        }
    }
    first.chars().take(prefix_len).collect()
}

// ── 参数解析（委托至 dsx-types）──

use dsx_types::arg::{parse_opt, parse_opt_u64};

fn parse_arg(args: &str, key: &str) -> String {
    dsx_types::arg::parse_arg(args, key).unwrap_or_default()
}

// ── 注册入口 ──

use crate::{ToolHandler, ToolKey, SafetyVerdict};
use std::time::Duration;

pub fn register(mgr: &mut crate::ToolManager) {
    // exec/run
    mgr.register(ToolHandler {
        key: ToolKey::new("exec", "run"),
        description: "Execute a shell command synchronously. Supports timeout_secs and cwd.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command"},
                "cwd": {"type": "string"},
                "timeout_secs": {"type": "integer"}
            },
            "required": ["command"],
            "additionalProperties": false
        }),
        handler: handle_run,
        safety: |ctx| {
            let cmd = ctx.get_str("command").unwrap_or("");
            crate::safety::classify_execution(cmd)
        },
        default_timeout: Duration::from_secs(300),
    });

    // exec/execute (alias for orchestrator compat)
    mgr.register(ToolHandler {
        key: ToolKey::new("exec", "execute"),
        description: "Execute a shell command synchronously. Supports timeout_secs and cwd.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command"},
                "cwd": {"type": "string"},
                "timeout_secs": {"type": "integer"}
            },
            "required": ["command"],
            "additionalProperties": false
        }),
        handler: handle_run,
        safety: |ctx| {
            let cmd = ctx.get_str("command").unwrap_or("");
            crate::safety::classify_execution(cmd)
        },
        default_timeout: Duration::from_secs(300),
    });

    // exec/sudo_run
    mgr.register(ToolHandler {
        key: ToolKey::new("exec", "sudo_run"),
        description: "Execute a command with sudo via dsx-sudo (setuid helper). No password needed.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "cwd": {"type": "string"},
                "timeout_secs": {"type": "integer"}
            },
            "required": ["command"],
            "additionalProperties": false
        }),
        handler: handle_sudo_run,
        safety: |_| SafetyVerdict::Allow,
        default_timeout: Duration::from_secs(300),
    });

}
