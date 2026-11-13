//! Task management: create, update, list tasks.

use crate::CURRENT_SESSION;
use dsx_types::ToolDef;
use super::{parse_arg, parse_opt};

// ── Task tools ──

#[allow(dead_code)]
pub(super) fn task_create_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: dsx_types::ToolFunction {
            name: "task_create".into(),
            description: "Create a tracked task. Break plans into actionable steps.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "subject": {"type": "string", "description": "Task subject, imperative form. E.g. 'Add plan paths to session.rs'"},
                    "description": {"type": "string", "description": "What needs to be done. 1 sentence."}
                },
                "required": ["subject", "description"],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_task_create(args: &str) -> String {
    let subject = parse_arg(args, "subject");
    let description = parse_arg(args, "description");

    if subject.is_empty() || subject.chars().count() > 100 {
        return "[ERROR] task_create: subject must be 1-100 chars\n[HINT] Keep the subject short and imperative, e.g. 'Add login API'".to_string();
    }
    if description.chars().count() > 200 {
        return "[ERROR] task_create: description max 200 chars\n[HINT] Write a concise 1-sentence description.".to_string();
    }

    let seed = CURRENT_SESSION.get().cloned().unwrap_or_default();
    if seed.is_empty() {
        return "[ERROR] task_create: no active session. Start a conversation first.".to_string();
    }

    let entry = format!("- [pending] {} — {}", subject, description);
    crate::stubs::append_memory(&seed, "tasks", &entry);

    format!("[OK] Task created [pending]: {}\nUse task_update(status=in_progress) when you start working on it.", subject)
}

#[allow(dead_code)]
pub(super) fn task_update_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: dsx_types::ToolFunction {
            name: "task_update".into(),
            description: "Update task status: pending→in_progress→completed. Updates short-term memory.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "subject": {"type": "string", "description": "Task subject to update (must match exactly)"},
                    "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"], "description": "New task status"}
                },
                "required": ["subject", "status"],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_task_update(args: &str) -> String {
    let subject = parse_arg(args, "subject");
    let status = parse_arg(args, "status");

    if !matches!(status.as_str(), "pending" | "in_progress" | "completed" | "cancelled") {
        return "[ERROR] task_update: status must be pending, in_progress, completed, or cancelled".to_string();
    }

    let seed = CURRENT_SESSION.get().cloned().unwrap_or_default();
    if seed.is_empty() {
        return "[ERROR] task_update: no active session. Start a conversation first.".to_string();
    }

    // Read tasks.md, find the task line, replace status
    let content = crate::stubs::read_memory(&seed, "tasks");
    let old_markers = ["[pending]", "[in_progress]", "[completed]", "[cancelled]"];
    let new_marker = format!("[{}]", status);
    let mut found = false;
    let mut updated = String::with_capacity(content.len());

    let subject_match = format!("] {} —", subject);
    for line in content.lines() {
        if line.contains(&subject_match) {
            let mut replaced = line.to_string();
            for marker in &old_markers {
                if replaced.contains(marker) {
                    replaced = replaced.replace(marker, &new_marker);
                    found = true;
                    break;
                }
            }
            updated.push_str(&replaced);
        } else {
            updated.push_str(line);
        }
        updated.push('\n');
    }

    if !found {
        return format!("[ERROR] task_update: task '{}' not found. Use task_list to see tasks.", subject);
    }

    crate::stubs::write_memory(&seed, "tasks", &updated);
    format!("[OK] Task '{}' → {}", subject, status)
}

#[allow(dead_code)]
pub(super) fn task_list_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: dsx_types::ToolFunction {
            name: "task_list".into(),
            description: "List tasks filtered by status. Review progress and plan next work.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"], "description": "Filter by status. Leave empty for all tasks."}
                },
                "required": [],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_task_list(args: &str) -> String {
    let filter_status = parse_opt(args, "status").unwrap_or_default();

    let seed = CURRENT_SESSION.get().cloned().unwrap_or_default();
    if seed.is_empty() {
        return "[ERROR] task_list: no active session. Start a conversation first.".to_string();
    }

    let content = crate::stubs::read_memory(&seed, "tasks");
    let task_lines: Vec<&str> = content
        .lines()
        .filter(|l| l.starts_with("- [") && (l.contains("[pending]") || l.contains("[in_progress]") || l.contains("[completed]") || l.contains("[cancelled]")))
        .collect();

    if task_lines.is_empty() {
        return "[OK] No tasks yet. Use task_create to create one.".to_string();
    }

    let filtered: Vec<&&str> = if filter_status.is_empty() {
        task_lines.iter().collect()
    } else {
        let marker = format!("[{}]", filter_status);
        task_lines.iter().filter(|l| l.contains(&marker)).collect()
    };

    if filtered.is_empty() {
        return format!("[OK] No tasks with status '{}'.", filter_status);
    }

    let icon = |s: &str| match s {
        "pending" => "○",
        "in_progress" => "●",
        "completed" => "✓",
        _ => "?",
    };

    let mut result = format!("[OK] Tasks ({}) :\n", filtered.len());
    for line in &filtered {
        let line = line.trim_start_matches("- ");
        let status = if line.contains("[pending]") { "pending" }
            else if line.contains("[in_progress]") { "in_progress" }
            else if line.contains("[completed]") { "completed" }
            else { "?" };
        result.push_str(&format!("{} {}\n", icon(status), line));
    }
    result
}

// ── Registration ──

use crate::{ToolHandler, ToolKey, SafetyVerdict, ToolCallCtx, ToolResult};
use std::time::Duration;

fn handle_task_create(ctx: ToolCallCtx) -> ToolResult {
    ToolResult::ok(exec_task_create(&serde_json::to_string(&ctx.args).unwrap_or_default()))
}
fn handle_task_update(ctx: ToolCallCtx) -> ToolResult {
    ToolResult::ok(exec_task_update(&serde_json::to_string(&ctx.args).unwrap_or_default()))
}
fn handle_task_list(ctx: ToolCallCtx) -> ToolResult {
    ToolResult::ok(exec_task_list(&serde_json::to_string(&ctx.args).unwrap_or_default()))
}

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register(ToolHandler {
        key: ToolKey::new("task_create", ""),
        description: "Create a tracked task.",
        input_schema: serde_json::json!({
            "type": "object", "properties": {
                "subject": {"type": "string", "description": "Task subject, imperative form"},
                "description": {"type": "string", "description": "What needs to be done"}
            }, "required": ["subject", "description"], "additionalProperties": false
        }),
        handler: handle_task_create,
        safety: |_| SafetyVerdict::Allow,
        default_timeout: Duration::from_secs(15),
    });
    mgr.register(ToolHandler {
        key: ToolKey::new("task_update", ""),
        description: "Update task status: pending->in_progress->completed.",
        input_schema: serde_json::json!({
            "type": "object", "properties": {
                "subject": {"type": "string"},
                "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"]}
            }, "required": ["subject", "status"], "additionalProperties": false
        }),
        handler: handle_task_update,
        safety: |_| SafetyVerdict::Allow,
        default_timeout: Duration::from_secs(15),
    });
    mgr.register(ToolHandler {
        key: ToolKey::new("task_list", ""),
        description: "List tasks filtered by status.",
        input_schema: serde_json::json!({
            "type": "object", "properties": {
                "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"]}
            }, "required": [], "additionalProperties": false
        }),
        handler: handle_task_list,
        safety: |_| SafetyVerdict::Allow,
        default_timeout: Duration::from_secs(15),
    });
}
