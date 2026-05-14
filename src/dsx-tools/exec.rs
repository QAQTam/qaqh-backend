//! Command execution: sync, async, sudo, PTY support.
//!
//! 保留旧函数（exec_command, exec_with_sudo, spawn_exec_async 等）供向后兼容。
//! 新增 handler 函数（handle_run, handle_sudo_run, handle_pty）适配新 IPC 框架。
//! 安全检测逻辑已移至 safety.rs。

use std::process::Command;

use crate::tools::{ToolCallCtx, ToolResult};

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

    // Sudo detection: if password needed, signal TUI to prompt user
    let is_sudo = command.trim().starts_with("sudo ");
    if is_sudo && !cfg!(target_os = "windows") {
        let needs_pwd = std::process::Command::new("sudo")
            .args(["-n", "true"])
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true);
        if needs_pwd {
            return format!("[SUDO_REQUIRED] {}", command);
        }
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

    let output = match rx.recv_timeout(std::time::Duration::from_secs(timeout_secs)) {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return format!("[ERROR] exec wait failed: {}", e),
        Err(_) => {
            let _ = std::process::Command::new("kill").args([&pid.to_string()]).output();
            std::thread::sleep(std::time::Duration::from_millis(200));
            let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).output();
            return format!("[ERROR] exec timed out after {}s\n[HINT] Increase timeout_secs or check if the command is stuck.", timeout_secs);
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

    let errors: Vec<&str> = full_output.lines()
        .filter(|l| l.contains("error") || l.contains("Error") || l.contains("ERROR")
            || l.contains("fail") || l.contains("FAIL"))
        .collect();
    let warnings: Vec<&str> = full_output.lines()
        .filter(|l| l.contains("warning") || l.contains("Warning") || l.contains("WARN"))
        .collect();
    let lines: Vec<&str> = full_output.lines().collect();
    let total = lines.len();
    let limit = 160;

    if total > limit {
        let head_count = 80;
        let tail_count = 40;
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
        if !errors.is_empty() {
            result.push_str("── errors ──\n");
            for &e in errors.iter().take(15) {
                result.push_str(e);
                result.push('\n');
            }
            if errors.len() > 15 {
                result.push_str(&format!("... ({} more error lines)\n", errors.len() - 15));
            }
        }
        if warnings.len() > 3 {
            result.push_str("── warnings ──\n");
            for &w in warnings.iter().take(10) {
                result.push_str(w);
                result.push('\n');
            }
            if warnings.len() > 10 {
                result.push_str(&format!("... ({} more warning lines)\n", warnings.len() - 10));
            }
        } else if !warnings.is_empty() {
            for &w in &warnings {
                result.push_str(w);
                result.push('\n');
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
    tx: tokio::sync::mpsc::Sender<crate::api::StreamEvent>,
) {
    spawn_exec_async_with_sudo(tool_call_id, args, None, tx)
}

pub fn spawn_exec_async_with_sudo(
    tool_call_id: &str,
    args: &str,
    sudo_password: Option<&str>,
    tx: tokio::sync::mpsc::Sender<crate::api::StreamEvent>,
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
            let _ = tx.try_send(crate::api::StreamEvent::ExecDone(
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
                    let _ = tx.send(crate::api::StreamEvent::ExecDone(
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
                    let _ = tx.send(crate::api::StreamEvent::ExecDone(
                        id, format!("[ERROR] Failed to spawn: {}", e),
                    )).await;
                    return;
                }
            }
        };

        if let Some(pid) = child.id() {
            let _ = tx.send(crate::api::StreamEvent::ExecStarted(id.clone(), pid)).await;
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
                    let _ = tx_stdout.send(crate::api::StreamEvent::ExecProgress(line)).await;
                }
            }
        });

        let stderr_handle = tokio::spawn(async move {
            if let Some(mut reader) = stderr {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(&mut reader).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = tx_stderr.send(crate::api::StreamEvent::ExecProgress(
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
                let _ = tx.send(crate::api::StreamEvent::ExecDone(
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
        let _ = tx.send(crate::api::StreamEvent::ExecDone(id, result)).await;
    });
}

pub fn spawn_exec_pty(
    tool_call_id: &str,
    args: &str,
    tx: tokio::sync::mpsc::Sender<crate::api::StreamEvent>,
) {
    let command = parse_arg(args, "command");
    let cwd = parse_opt(args, "cwd");
    let timeout_secs = parse_opt_u64(args, "timeout_secs")
        .filter(|&n| n > 0 && n <= 3600)
        .unwrap_or(300);
    let id = tool_call_id.to_string();

    if command.contains("sudo ") {
        return spawn_exec_async(tool_call_id, args, tx);
    }

    tokio::spawn(async move {
        let (mut pty, pts) = match pty_process::open() {
            Ok(p) => p,
            Err(e) => {
                let _ = tx.send(crate::api::StreamEvent::ExecDone(
                    id, format!("[ERROR] PTY unavailable: {}", e),
                )).await;
                return;
            }
        };
        let _ = pty.resize(pty_process::Size::new(24, 80));

        let mut pty_cmd = pty_process::Command::new(shell());
        pty_cmd = pty_cmd.args(["-c", &command]);
        if let Some(ref dir) = cwd {
            pty_cmd = pty_cmd.current_dir(dir);
        }

        let mut child = match pty_cmd.spawn(pts) {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(crate::api::StreamEvent::ExecDone(
                    id, format!("[ERROR] Failed to spawn on PTY: {}", e),
                )).await;
                return;
            }
        };
        if let Some(pid) = child.id() {
            let _ = tx.send(crate::api::StreamEvent::ExecStarted(id.clone(), pid)).await;
        }

        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 8192];
        let mut output_buf = String::new();

        let read_future = async {
            loop {
                match pty.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = String::from_utf8_lossy(&buf[..n]);
                        output_buf.push_str(&chunk);
                        let _ = tx.send(crate::api::StreamEvent::ExecProgress(chunk.to_string())).await;
                    }
                    Err(_) => break,
                }
            }
        };

        tokio::select! {
            _ = read_future => {}
            _ = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)) => {
                let _ = tx.send(crate::api::StreamEvent::ExecDone(
                    id,
                    format!("[ERROR] exec timed out after {}s\n[HINT] Increase timeout_secs or check if the command is stuck.", timeout_secs),
                )).await;
                return;
            }
        }

        let exit_code = child.wait().await
            .map(|s| s.code().unwrap_or(-1))
            .unwrap_or(-1);
        let status_label = if exit_code == 0 { "OK" } else { "FAIL" };
        let _ = tx.send(crate::api::StreamEvent::ExecDone(
            id,
            format!("[{}] exec: {} (exit {})\n{}", status_label, command, exit_code, output_buf.trim()),
        )).await;
    });
}

// ── Handler 函数（新 IPC 框架）──

pub fn handle_run(ctx: ToolCallCtx) -> ToolResult {
    let command = ctx.get_str("command").unwrap_or("").to_string();
    // 重建 args JSON 以兼容 old 函数签名
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
    let command = ctx.get_str("command").unwrap_or("");
    let password = ctx.get_str("password").unwrap_or("");
    let result = exec_with_sudo(command, password);
    let success = result.starts_with("[OK]");
    ToolResult { success, content: result }
}

pub fn handle_pty(ctx: ToolCallCtx) -> ToolResult {
    let command = ctx.get_str("command").unwrap_or("");
    let cwd = ctx.get_str("cwd");
    let timeout = ctx.get_u64("timeout_secs");
    let args = serde_json::json!({
        "command": command,
        "cwd": cwd,
        "timeout_secs": timeout,
    });
    // PTY 是异步的，同步版本 fallback 到同步 exec
    let result = exec_command(&args.to_string());
    let success = result.starts_with("[OK]");
    ToolResult { success, content: result }
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

// ── 参数解析（兼容垫片）──

fn parse_arg(args: &str, key: &str) -> String {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v.get(key).and_then(|v| v.as_str()).map(String::from))
        .unwrap_or_default()
}

fn parse_opt(args: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v.get(key).and_then(|v| v.as_str()).map(String::from))
}

fn parse_opt_u64(args: &str, key: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(args).ok()?;
    let val = v.get(key)?;
    val.as_u64().or_else(|| val.as_str().and_then(|s| s.parse::<u64>().ok()))
}

// ── 注册入口 ──

use crate::tools::{ToolHandler, ToolKey, SafetyVerdict};
use std::time::Duration;

pub fn register(mgr: &mut crate::tools::ToolManager) {
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
            crate::tools::safety::classify_execution(cmd)
        },
        default_timeout: Duration::from_secs(300),
    });

    // exec/sudo_run
    mgr.register(ToolHandler {
        key: ToolKey::new("exec", "sudo_run"),
        description: "Execute a command with sudo. Requires password parameter.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "password": {"type": "string"},
                "cwd": {"type": "string"},
                "timeout_secs": {"type": "integer"}
            },
            "required": ["command", "password"],
            "additionalProperties": false
        }),
        handler: handle_sudo_run,
        safety: |_| SafetyVerdict::confirm("sudo_exec"),
        default_timeout: Duration::from_secs(300),
    });

    // exec/pty
    mgr.register(ToolHandler {
        key: ToolKey::new("exec", "pty"),
        description: "Execute a command in a PTY (pseudo-terminal) for interactive programs.",
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
        handler: handle_pty,
        safety: |_| SafetyVerdict::confirm("pty_exec"),
        default_timeout: Duration::from_secs(300),
    });
}
