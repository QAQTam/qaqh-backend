//! Task management: create, update, delete, list tasks.
//! Format: "- [status] T{id}: subject — description"
//!
//! Persisted to `sessions/{seed}/tasks.md` (session-scoped).

use std::sync::Mutex;

static TASK_LOCK: Mutex<()> = Mutex::new(());

fn tasks_path() -> std::path::PathBuf {
    let session = crate::runtime::context()
        .map(|ctx| ctx.active_session)
        .unwrap_or_default();
    if session.is_empty() {
        // No active session — fall back to workspace .deepx/ for standalone tool use
        crate::workspace::deepx_dir().join("tasks.md")
    } else {
        deepx_types::platform::sessions_dir()
            .join(&session)
            .join("tasks.md")
    }
}

fn read_tasks() -> Vec<String> {
    let path = tasks_path();
    if !path.exists() {
        return Vec::new();
    }
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    content
        .lines()
        .filter(|l| l.starts_with("- [") && !l.trim().is_empty())
        .map(String::from)
        .collect()
}

fn write_tasks(lines: &[String]) {
    let path = tasks_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, lines.join("\n") + "\n");
}

fn next_id(lines: &[String]) -> u32 {
    lines
        .iter()
        .filter_map(|l| {
            let rest = l.trim_start_matches("- [").split_once("] ")?.1;
            let id_str = rest.trim_start().splitn(2, ':').next()?;
            id_str.strip_prefix('T')?.parse::<u32>().ok()
        })
        .max()
        .unwrap_or(0)
        + 1
}

fn find_task(lines: &[String], id: u32) -> Option<usize> {
    let prefix = format!("T{}:", id);
    lines.iter().position(|l| l.trim_start().contains(&prefix))
}

fn parse_id(args: &serde_json::Value) -> Result<u32, String> {
    let val = args.get("id").ok_or("missing 'id'")?;
    let n = val
        .as_u64()
        .or_else(|| val.as_str().and_then(|s| s.parse::<u64>().ok()))
        .ok_or("'id' must be a positive integer")?;
    if n == 0 || n > u32::MAX as u64 {
        return Err("'id' must be a positive integer".into());
    }
    Ok(n as u32)
}

/// Parse stored task lines into TaskInfo structs for dashboard/status.
pub fn get_task_infos() -> Vec<deepx_proto::TaskInfo> {
    let lines = read_tasks();
    lines
        .iter()
        .filter_map(|l| {
            let trimmed = l.trim_start();
            // Format: "- [status] T{id}: subject — description"
            if !trimmed.starts_with("- [") {
                return None;
            }
            let after_bracket = trimmed.split_once("] ")?.1;
            let status = &trimmed[3..trimmed.find(']')?];
            let (id_part, rest) = after_bracket.split_once(": ")?;
            let id = id_part.trim().to_string();
            let (subject, description) = rest
                .split_once(" — ")
                .map_or((rest.to_string(), String::new()), |(s, d)| {
                    (s.to_string(), d.to_string())
                });
            Some(deepx_proto::TaskInfo {
                id,
                subject,
                description,
                status: status.to_string(),
            })
        })
        .collect()
}

pub(super) fn exec_task_create(args: &serde_json::Value) -> String {
    let _guard = TASK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let subject = args.s("subject");
    let description = args.s("description");

    if subject.is_empty() || subject.chars().count() > 100 {
        return crate::json_err(
            "INVALID_INPUT",
            "task: subject must be 1-100 chars",
            "Keep the subject short and imperative, e.g. 'Add login API'",
        );
    }
    if description.chars().count() > 200 {
        return crate::json_err(
            "INVALID_INPUT",
            "task: description max 200 chars",
            "Write a concise 1-sentence description.",
        );
    }

    let mut lines = read_tasks();
    let id = next_id(&lines);
    let entry = format!("- [pending] T{}: {} — {}", id, subject, description);
    lines.push(entry);
    write_tasks(&lines);

    crate::json_ok(serde_json::json!({
        "task_id": format!("T{}", id),
        "subject": subject,
        "content": format!("Task T{} created [pending]: {}. Use task_update(id={}, status=in_progress) when you start working on it.", id, subject, id)
    }))
}

pub(super) fn exec_task_update(args: &serde_json::Value) -> String {
    let _guard = TASK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let id: u32 = match parse_id(args) {
        Ok(n) => n,
        Err(e) => {
            let msg = format!("task_update: {}", e);
            return crate::json_err(
                "INVALID_INPUT",
                &msg,
                "Check the task ID using task_list().",
            );
        }
    };
    let status = args.s("status");

    if !matches!(
        status.as_str(),
        "pending" | "in_progress" | "completed" | "cancelled"
    ) {
        return crate::json_err(
            "INVALID_INPUT",
            "task_update: status must be pending, in_progress, completed, or cancelled",
            "Use one of these status values: pending, in_progress, completed, cancelled",
        );
    }

    let mut lines = read_tasks();
    let idx = match find_task(&lines, id) {
        Some(i) => i,
        None => {
            let msg = format!("task_update: task T{} not found", id);
            return crate::json_err("NOT_FOUND", &msg, "Use task_list to see task IDs.");
        }
    };

    let old_markers = ["[pending]", "[in_progress]", "[completed]", "[cancelled]"];
    let new_marker = format!("[{}]", status);
    for marker in &old_markers {
        if lines[idx].contains(marker) {
            lines[idx] = lines[idx].replace(marker, &new_marker);
            break;
        }
    }
    write_tasks(&lines);
    crate::json_ok(
        serde_json::json!({"task_id": format!("T{}", id), "status": status, "content": format!("Task T{} → {}", id, status)}),
    )
}

pub(super) fn exec_task_delete(args: &serde_json::Value) -> String {
    let _guard = TASK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let id: u32 = match parse_id(args) {
        Ok(n) => n,
        Err(e) => {
            let msg = format!("task_delete: {}", e);
            return crate::json_err("INVALID_INPUT", &msg, "Check the task ID.");
        }
    };

    let mut lines = read_tasks();
    let idx = match find_task(&lines, id) {
        Some(i) => i,
        None => {
            let msg = format!("task_delete: task T{} not found", id);
            return crate::json_err("NOT_FOUND", &msg, "Use task_list to see task IDs.");
        }
    };

    let removed = lines.remove(idx);
    let subject = removed
        .split(" — ")
        .next()
        .and_then(|s| s.split(": ").nth(1))
        .unwrap_or("?");
    write_tasks(&lines);
    crate::json_ok(
        serde_json::json!({"task_id": format!("T{}", id), "subject": subject, "content": format!("Task T{} deleted: {}", id, subject)}),
    )
}

pub(super) fn exec_task_list(args: &serde_json::Value) -> String {
    let filter_status = args
        .get("status")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_default();

    let lines = read_tasks();
    if lines.is_empty() {
        return crate::json_ok(
            serde_json::json!({"content": "No tasks yet. Use task_create(subject=..., description=...) to create one."}),
        );
    }

    let marker = if filter_status.is_empty() {
        None
    } else {
        Some(format!("[{}]", filter_status))
    };
    let filtered: Vec<&String> = if let Some(ref m) = marker {
        lines.iter().filter(|l| l.contains(m.as_str())).collect()
    } else {
        lines.iter().collect()
    };

    if filtered.is_empty() {
        return if filter_status.is_empty() {
            crate::json_ok(serde_json::json!({"content": "No tasks."}))
        } else {
            crate::json_ok(
                serde_json::json!({"content": format!("No tasks with status '{}'.", filter_status)}),
            )
        };
    }

    let icon = |s: &str| match s {
        "pending" => "○",
        "in_progress" => "●",
        "completed" => "✓",
        "cancelled" => "✗",
        _ => "?",
    };

    let mut content = format!("Tasks ({}):\n", filtered.len());
    for line in &filtered {
        let trimmed = line.trim_start_matches("- ");
        let status = if trimmed.contains("[pending]") {
            "pending"
        } else if trimmed.contains("[in_progress]") {
            "in_progress"
        } else if trimmed.contains("[completed]") {
            "completed"
        } else if trimmed.contains("[cancelled]") {
            "cancelled"
        } else {
            "?"
        };
        content.push_str(&format!("{} {}\n", icon(status), trimmed));
    }
    crate::json_ok(serde_json::json!({"content": content}))
}

// ── Dispatcher ──

fn handle_task(ctx: ToolCallCtx) -> ToolResult {
    let action = ctx.get_str("action").unwrap_or("list");
    match action {
        "create" => ToolResult::ok(exec_task_create(&ctx.args)),
        "update" => ToolResult::ok(exec_task_update(&ctx.args)),
        "delete" => ToolResult::ok(exec_task_delete(&ctx.args)),
        "list"   => ToolResult::ok(exec_task_list(&ctx.args)),
        _ => ToolResult::error(format!("Unknown task action: {action}. Use create, update, delete, or list.")),
    }
}

// ── Registration ──

use crate::{JsonArgs, ToolCallCtx, ToolHandler, ToolResult, ToolRisk};
use std::time::Duration;

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register(ToolHandler {
        key: "task".to_string(),
        description: "[DEPRECATED] Use todo_create, todo_update, todo_cancel, and todo_list instead.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["create", "update", "delete", "list"], "description": "Operation to perform"},
                "id": {"type": "integer", "description": "Task ID for update/delete (T1 → 1)"},
                "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"], "description": "New status for update"},
                "subject": {"type": "string", "description": "Task subject for create, 1-100 chars"},
                "description": {"type": "string", "description": "Task description for create, 1-200 chars"}
            },
            "required": ["action"],
            "additionalProperties": false
        }),
        handler: handle_task,
        risk: ToolRisk::Write,
        default_timeout: Duration::from_secs(15),
    });
}
