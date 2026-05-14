//! Git operations: status, diff, log, commit.

use std::process::Command;

use crate::types::ToolDef;
use super::{parse_arg, parse_opt};
#[allow(dead_code)]
pub(super) fn git_status_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "git_status".into(),
            description: "Show git working tree status.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_git_status() -> String {
    run_cmd("git", &["status", "--short", "-b"])
}

#[allow(dead_code)]
pub(super) fn git_diff_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "git_diff".into(),
            description: "Show git diff. Default unstaged; set staged=true for staged changes.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "staged": {"type": "boolean", "description": "Show staged changes instead", "default": false}
                },
                "required": [],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_git_diff() -> String {
    run_cmd("git", &["diff", "--stat"])
}

#[allow(dead_code)]
pub(super) fn git_log_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "git_log".into(),
            description: "Show recent git commit history.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "count": {"type": "integer", "description": "Number of recent commits", "default": 10}
                },
                "required": [],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_git_log(args: &str) -> String {
    let count = parse_opt(args, "count").unwrap_or_else(|| "10".to_string());
    run_cmd("git", &["log", "--oneline", "-n", &count])
}

#[allow(dead_code)]
pub(super) fn git_commit_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "git_commit".into(),
            description: "Create a git commit. Requires user confirmation.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "message": {"type": "string", "description": "Commit message"}
                },
                "required": ["message"],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_git_commit(args: &str) -> String {
    let msg = parse_arg(args, "message");
    run_cmd("git", &["commit", "-m", &msg])
}
pub(super) fn run_cmd(prog: &str, args: &[&str]) -> String {
    match Command::new(prog).args(args).output() {
        Ok(o) => {
            let out = String::from_utf8_lossy(&o.stdout).to_string();
            if out.is_empty() { "(no output)".into() } else { out }
        }
        Err(e) => format!("{} error: {}", prog, e),
    }
}
