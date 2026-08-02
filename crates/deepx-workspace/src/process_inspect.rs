//! Process inspection tools — check, wait, kill, write for tracked processes.
//!
//! Registered under the `check_process`, `wait_process`, `kill_process` names.
//! These let the LLM inspect long-running exec/subagent processes that
//! hit their timeout, instead of blindly retrying or killing.

use crate::{ToolCallCtx, ToolPlacement, ToolResult, ToolRisk, process_registry::ProcessRegistry};

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_with_placement(crate::ToolHandler {
        key: "process_check".to_string(),
        description: "Inspect a running background process (exec or subagent). \
            Returns status, elapsed time, and the last output tail. \
            Use when a previous exec/subagent timed out — you can peek at whether it's \
            still making progress or has stalled.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "integer", "description": "Process ID returned by the timed-out exec/subagent call."}
            },
            "required": ["id"],
            "additionalProperties": false
        }),
        handler: handle_check,
        risk: ToolRisk::ReadOnly,
        default_timeout: std::time::Duration::from_secs(10),
    }, ToolPlacement::Workspace);

    mgr.register_with_placement(crate::ToolHandler {
        key: "process_wait".to_string(),
        description: "Wait for a background process to complete, with a timeout. \
            Returns the final output when the process exits, or a snapshot if timeout is reached. \
            Use when you want to give a previously-timed-out process more time to finish.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "integer", "description": "Process ID to wait for."},
                "timeout_secs": {"type": "integer", "description": "Max seconds to wait. Default 120."}
            },
            "required": ["id"],
            "additionalProperties": false
        }),
        handler: handle_wait,
        risk: ToolRisk::ReadOnly,
        default_timeout: std::time::Duration::from_secs(180),
    }, ToolPlacement::Workspace);

    mgr.register_with_placement(crate::ToolHandler {
        key: "process_kill".to_string(),
        description: "Kill a process",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "integer", "description": "Process ID to kill."}
            },
            "required": ["id"],
            "additionalProperties": false
        }),
        handler: handle_kill,
        risk: ToolRisk::Administrative,
        default_timeout: std::time::Duration::from_secs(15),
    }, ToolPlacement::Workspace);

    mgr.register_with_placement(crate::ToolHandler {
        key: "process_write".to_string(),
        description: "Write text to the stdin of a running interactive process. \
            Use when a background exec process is waiting for input (e.g. password prompt, [Y/n] confirmation). \
            Append \\n to submit the input.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "integer", "description": "Process ID to write to."},
                "text": {"type": "string", "description": "Text to send to stdin. Use \\n for newline."}
            },
            "required": ["id", "text"],
            "additionalProperties": false
        }),
        handler: handle_write,
        risk: ToolRisk::Write,
        default_timeout: std::time::Duration::from_secs(15),
    }, ToolPlacement::Workspace);
}

/// Format a process info payload as a flat structured response.
///
/// `ProcessRegistry::get_info` already returns a structured JSON object
/// (id/name/status/exit_code/output…). Serializing it into a `content` string
/// would double-escape it (JSON inside a JSON string), so we spread the
/// fields at the top level and add a short human-readable `content` summary
/// that the context-fold logic can later replace with a hint.
fn process_info_ok(id: u32, info: serde_json::Value) -> String {
    let mut v = info;
    if let serde_json::Value::Object(ref mut map) = v {
        map.insert("timeis".to_string(), serde_json::json!(crate::now_utc8()));
        if !map.contains_key("content") {
            let status = map.get("status").and_then(|s| s.as_str()).unwrap_or("");
            map.insert(
                "content".to_string(),
                serde_json::json!(format!("process {id}: {status}")),
            );
        }
    }
    v.to_string()
}

fn handle_check(ctx: ToolCallCtx) -> ToolResult {
    let id: u32 = match ctx.args.get("id").and_then(|v| v.as_u64()) {
        Some(v) if v <= u32::MAX as u64 => v as u32,
        _ => {
            return ToolResult {
                success: false,
                content: crate::json_err(
                    "MISSING_ID",
                    "check_process: id required",
                    "Provide the process ID returned by exec or spawn_subagent.",
                ),
            };
        }
    };

    match ProcessRegistry::get_info(id) {
        Some(info) => ToolResult {
            success: true,
            content: process_info_ok(id, info),
        },
        None => ToolResult {
            success: false,
            content: crate::json_err(
                "NOT_FOUND",
                &format!("check_process: process {id} not found"),
                "Process may have already exited and been cleaned up.",
            ),
        },
    }
}

fn handle_wait(ctx: ToolCallCtx) -> ToolResult {
    let id: u32 = match ctx.args.get("id").and_then(|v| v.as_u64()) {
        Some(v) if v <= u32::MAX as u64 => v as u32,
        _ => {
            return ToolResult {
                success: false,
                content: crate::json_err(
                    "MISSING_ID",
                    "wait_process: id required",
                    "Provide the process ID returned by exec or spawn_subagent.",
                ),
            };
        }
    };
    let timeout_secs: u64 = ctx
        .args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(120);

    match ProcessRegistry::wait_for(id, timeout_secs) {
        Some(info) => ToolResult {
            success: true,
            content: process_info_ok(id, info),
        },
        None => ToolResult {
            success: false,
            content: crate::json_err(
                "NOT_FOUND",
                &format!("wait_process: process {id} not found"),
                "Check that the process ID is correct.",
            ),
        },
    }
}

fn handle_kill(ctx: ToolCallCtx) -> ToolResult {
    let id: u32 = match ctx.args.get("id").and_then(|v| v.as_u64()) {
        Some(v) if v <= u32::MAX as u64 => v as u32,
        _ => {
            return ToolResult {
                success: false,
                content: crate::json_err(
                    "MISSING_ID",
                    "kill_process: id required",
                    "Provide the process ID.",
                ),
            };
        }
    };

    if ProcessRegistry::kill(id) {
        ToolResult {
            success: true,
            content: crate::json_ok(
                serde_json::json!({"content": format!("Process {id} killed.")}),
            ),
        }
    } else {
        ToolResult {
            success: false,
            content: crate::json_err(
                "NOT_FOUND",
                &format!("kill_process: process {id} not found or already exited"),
                "Check the process ID.",
            ),
        }
    }
}

fn handle_write(ctx: ToolCallCtx) -> ToolResult {
    let id: u32 = match ctx.args.get("id").and_then(|v| v.as_u64()) {
        Some(v) if v <= u32::MAX as u64 => v as u32,
        _ => {
            return ToolResult {
                success: false,
                content: crate::json_err(
                    "MISSING_ID",
                    "process write: id required",
                    "Provide the process ID.",
                ),
            };
        }
    };
    let text = match ctx.args.get("text").and_then(|v| v.as_str()) {
        Some(t) if !t.is_empty() => t,
        _ => {
            return ToolResult {
                success: false,
                content: crate::json_err(
                    "MISSING_TEXT",
                    "process write: text required",
                    "Provide the text to write to stdin.",
                ),
            };
        }
    };

    match ProcessRegistry::write_to(id, text) {
        Ok(n) => ToolResult {
            success: true,
            content: crate::json_ok(
                serde_json::json!({"content": format!("Wrote {n} bytes to process {id}.")}),
            ),
        },
        Err(e) => ToolResult {
            success: false,
            content: crate::json_err(
                "WRITE_FAILED",
                &format!("process write: {e}"),
                "Check that the process is still running.",
            ),
        },
    }
}
