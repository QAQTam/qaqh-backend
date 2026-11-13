//! Git operations: status, diff, log, commit.
//!
//! Handler functions for new IPC framework + backward compat with old run_cmd.

use std::process::Command;

use crate::{ToolCallCtx, ToolResult};

// ── Handler 函数（新 IPC 框架）──

pub fn handle_status() -> ToolResult {
    let out = run_cmd("git", &["status", "--short", "-b"]);
    ToolResult::ok(out)
}

pub fn handle_diff(ctx: ToolCallCtx) -> ToolResult {
    let staged = ctx.get_bool("staged").unwrap_or(false);
    if staged {
        ToolResult::ok(run_cmd("git", &["diff", "--cached", "--stat"]))
    } else {
        ToolResult::ok(run_cmd("git", &["diff", "--stat"]))
    }
}

pub fn handle_log(ctx: ToolCallCtx) -> ToolResult {
    let count = ctx.get_str("count").unwrap_or("10");
    ToolResult::ok(run_cmd("git", &["log", "--oneline", "-n", count]))
}

pub fn handle_commit(ctx: ToolCallCtx) -> ToolResult {
    let msg = ctx.get_str("message").unwrap_or("");
    if msg.is_empty() {
        return ToolResult::err("[ERROR] git commit: message is required");
    }
    ToolResult::ok(run_cmd("git", &["commit", "-m", msg]))
}

// ── 旧函数（向后兼容）──

pub(super) fn exec_git_status() -> String {
    run_cmd("git", &["status", "--short", "-b"])
}

pub(super) fn exec_git_diff() -> String {
    run_cmd("git", &["diff", "--stat"])
}

pub(super) fn exec_git_log(args: &str) -> String {
    let count = parse_opt(args, "count").unwrap_or_else(|| "10".to_string());
    run_cmd("git", &["log", "--oneline", "-n", &count])
}

pub(super) fn exec_git_commit(args: &str) -> String {
    let msg = parse_arg(args, "message");
    run_cmd("git", &["commit", "-m", &msg])
}

pub(super) fn run_cmd(prog: &str, args: &[&str]) -> String {
    match Command::new(prog).args(args).output() {
        Ok(o) => {
            let out = String::from_utf8_lossy(&o.stdout).to_string();
            let err = String::from_utf8_lossy(&o.stderr).to_string();
            if out.is_empty() && err.is_empty() {
                "(no output)".into()
            } else if out.is_empty() {
                err
            } else if err.is_empty() {
                out
            } else {
                format!("{}\n{}", out.trim(), err.trim())
            }
        }
        Err(e) => format!("{} error: {}", prog, e),
    }
}

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

// ── 注册入口 ──

use crate::{ToolHandler, ToolKey, SafetyVerdict};
use std::time::Duration;

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register(ToolHandler {
        key: ToolKey::new("git", "status"),
        description: "Show git working tree status.",
        input_schema: serde_json::json!({"type":"object","properties":{},"required":[],"additionalProperties":false}),
        handler: |_| handle_status(),
        safety: |_| SafetyVerdict::allowed(),
        default_timeout: Duration::from_secs(10),
    });
    mgr.register(ToolHandler {
        key: ToolKey::new("git", "diff"),
        description: "Show git diff (unstaged, or staged with staged=true).",
        input_schema: serde_json::json!({"type":"object","properties":{"staged":{"type":"boolean"}},"required":[],"additionalProperties":false}),
        handler: handle_diff,
        safety: |_| SafetyVerdict::allowed(),
        default_timeout: Duration::from_secs(10),
    });
    mgr.register(ToolHandler {
        key: ToolKey::new("git", "log"),
        description: "Show recent git commit history.",
        input_schema: serde_json::json!({"type":"object","properties":{"count":{"type":"integer"}},"required":[],"additionalProperties":false}),
        handler: handle_log,
        safety: |_| SafetyVerdict::allowed(),
        default_timeout: Duration::from_secs(10),
    });
    mgr.register(ToolHandler {
        key: ToolKey::new("git", "commit"),
        description: "Create a git commit.",
        input_schema: serde_json::json!({"type":"object","properties":{"message":{"type":"string"}},"required":["message"],"additionalProperties":false}),
        handler: handle_commit,
        safety: |_| SafetyVerdict::confirm("git_commit"),
        default_timeout: Duration::from_secs(10),
    });
}
