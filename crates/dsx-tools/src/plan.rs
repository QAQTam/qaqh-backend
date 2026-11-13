//! Plan management: create, update, read, list plans.

use crate::CURRENT_SESSION;
use dsx_types::ToolDef;
use super::{parse_arg, parse_opt};

// ── Plan tools ──

#[allow(dead_code)]
pub(super) fn plan_create_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: dsx_types::ToolFunction {
            name: "plan_create".into(),
            description: "Create a plan for multi-step tasks. Saved to plans/ and injected as context.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Plan name. Keep it short (3-8 words), lowercase. E.g. 'add-auth-middleware'"},
                    "goal": {"type": "string", "description": "What this plan aims to accomplish. 1-3 sentences describing the outcome and approach."}
                },
                "required": ["name", "goal"],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_plan_create(args: &str) -> String {
    let name = parse_arg(args, "name");
    let goal = parse_arg(args, "goal");

    if name.is_empty() || name.chars().count() > 80 {
        return "[ERROR] plan_create: name must be 1-80 chars".to_string();
    }
    if goal.is_empty() {
        return "[ERROR] plan_create: goal is required".to_string();
    }

    let seed = CURRENT_SESSION.get().cloned().unwrap_or_default();
    if seed.is_empty() {
        return "[ERROR] plan_create: no active session. Start a conversation first.".to_string();
    }

    match crate::stubs::write_plan(&seed, &name, &goal) {
        Some(path) => {
            let display = path.to_string_lossy().to_string();
            format!("[OK] Plan created: {}\nPath: {}\nNext: elaborate the Steps section in the plan file using write_file, then call plan_update to set status=active when ready to execute.", name, display)
        }
        None => "[ERROR] plan_create: failed to create plan file".to_string(),
    }
}

#[allow(dead_code)]
pub(super) fn plan_update_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: dsx_types::ToolFunction {
            name: "plan_update".into(),
            description: "Update plan status: draft→active→done. Auto-summarizes on completion.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Plan name (the slug used in plan_create)"},
                    "status": {"type": "string", "enum": ["draft", "active", "done", "cancelled"], "description": "New plan status"}
                },
                "required": ["name", "status"],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_plan_update(args: &str) -> String {
    let name = parse_arg(args, "name");
    let status = parse_arg(args, "status");

    if !matches!(status.as_str(), "draft" | "active" | "done" | "cancelled") {
        return "[ERROR] plan_update: status must be draft, active, done, or cancelled".to_string();
    }

    let seed = CURRENT_SESSION.get().cloned().unwrap_or_default();
    if seed.is_empty() {
        return "[ERROR] plan_update: no active session. Start a conversation first.".to_string();
    }

    match crate::stubs::update_plan_status(&seed, &name, &status) {
        Some(_updated) => {
            let mut result = format!("[OK] Plan '{}' status → {}\n", name, status);
            if status == "done" {
                // Auto-extract plan completion into long-term memory
                if let Some(content) = crate::stubs::read_plan_content(&seed, &name) {
                    let goal_line = content.lines()
                        .find(|l| l.starts_with("## Goal"))
                        .and_then(|_| content.lines().skip_while(|l| !l.starts_with("## Goal")).nth(1))
                        .unwrap_or("completed");
                    let decision = format!("Plan completed: {} — {}", name, goal_line.trim());
                    crate::stubs::append_memory(&seed, "long", &format!("- DECISION: {}", decision));
                }
                result.push_str("[MEMORY] Plan summary extracted to long-term memory.");
            }
            result
        }
        None => format!("[ERROR] plan_update: plan '{}' not found. Use plan_list to see available plans.", name),
    }
}

#[allow(dead_code)]
pub(super) fn plan_read_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: dsx_types::ToolFunction {
            name: "plan_read".into(),
            description: "Read a plan by name. Defaults to most recent active/draft plan.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Plan name to read. Leave empty to get the most recent active plan."}
                },
                "required": [],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_plan_read(args: &str) -> String {
    let name = parse_opt(args, "name").unwrap_or_default();

    let seed = CURRENT_SESSION.get().cloned().unwrap_or_default();
    if seed.is_empty() {
        return "[ERROR] plan_read: no active session. Start a conversation first.".to_string();
    }

    if name.is_empty() {
        // Find most recent active or draft plan
        let plans = crate::stubs::list_plans(&seed);
        let active = plans.iter().find(|(_, s, _)| s == "active" || s == "draft");
        match active {
            Some((n, s, _)) => {
                if let Some(content) = crate::stubs::read_plan_content(&seed, n) {
                    return format!("[OK] Plan '{}' ({}):\n{}", n, s, content);
                }
            }
            None => return "[OK] No active plans. Use plan_create to start one.".to_string(),
        }
    }

    match crate::stubs::read_plan_content(&seed, &name) {
        Some(content) => format!("[OK] Plan '{}':\n{}", name, content),
        None => format!("[ERROR] plan_read: plan '{}' not found. Use plan_list to see available plans.", name),
    }
}

#[allow(dead_code)]
pub(super) fn plan_list_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: dsx_types::ToolFunction {
            name: "plan_list".into(),
            description: "List all plans for current session with statuses.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_plan_list(_args: &str) -> String {
    let seed = CURRENT_SESSION.get().cloned().unwrap_or_default();
    if seed.is_empty() {
        return "[ERROR] plan_list: no active session. Start a conversation first.".to_string();
    }

    let plans = crate::stubs::list_plans(&seed);
    if plans.is_empty() {
        return "[OK] No plans for this session.".to_string();
    }

    let mut result = format!("[OK] Plans ({} total):\n", plans.len());
    for (name, status, path) in &plans {
        let marker = match status.as_str() {
            "active" => "●",
            "draft" => "○",
            "done" => "✓",
            "cancelled" => "✗",
            _ => "?",
        };
        result.push_str(&format!("{} [{}] {} — {}\n", marker, status, name, path.display()));
    }
    result
}

// ── Registration ──

use crate::{ToolHandler, ToolKey, SafetyVerdict, ToolCallCtx, ToolResult};
use std::time::Duration;

fn args_to_string(ctx: &ToolCallCtx) -> String {
    serde_json::to_string(&ctx.args).unwrap_or_default()
}

fn handle_plan_create(ctx: ToolCallCtx) -> ToolResult {
    ToolResult::ok(exec_plan_create(&args_to_string(&ctx)))
}
fn handle_plan_update(ctx: ToolCallCtx) -> ToolResult {
    ToolResult::ok(exec_plan_update(&args_to_string(&ctx)))
}
fn handle_plan_read(ctx: ToolCallCtx) -> ToolResult {
    ToolResult::ok(exec_plan_read(&args_to_string(&ctx)))
}
fn handle_plan_list(ctx: ToolCallCtx) -> ToolResult {
    ToolResult::ok(exec_plan_list(&args_to_string(&ctx)))
}

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register(ToolHandler {
        key: ToolKey::new("plan_create", ""),
        description: "Create a plan for multi-step tasks.",
        input_schema: serde_json::json!({
            "type": "object", "properties": {
                "name": {"type": "string", "description": "Plan name, lowercase, 3-8 words"},
                "goal": {"type": "string", "description": "What this plan aims to accomplish"}
            }, "required": ["name", "goal"], "additionalProperties": false
        }),
        handler: handle_plan_create,
        safety: |_| SafetyVerdict::Allow,
        default_timeout: Duration::from_secs(15),
    });
    mgr.register(ToolHandler {
        key: ToolKey::new("plan_update", ""),
        description: "Update plan status: draft->active->done.",
        input_schema: serde_json::json!({
            "type": "object", "properties": {
                "name": {"type": "string"},
                "status": {"type": "string", "enum": ["draft", "active", "done", "cancelled"]}
            }, "required": ["name", "status"], "additionalProperties": false
        }),
        handler: handle_plan_update,
        safety: |_| SafetyVerdict::Allow,
        default_timeout: Duration::from_secs(15),
    });
    mgr.register(ToolHandler {
        key: ToolKey::new("plan_read", ""),
        description: "Read a plan by name.",
        input_schema: serde_json::json!({
            "type": "object", "properties": {
                "name": {"type": "string", "description": "Plan name to read"}
            }, "required": [], "additionalProperties": false
        }),
        handler: handle_plan_read,
        safety: |_| SafetyVerdict::Allow,
        default_timeout: Duration::from_secs(15),
    });
    mgr.register(ToolHandler {
        key: ToolKey::new("plan_list", ""),
        description: "List all plans for current session with statuses.",
        input_schema: serde_json::json!({
            "type": "object", "properties": {}, "required": [], "additionalProperties": false
        }),
        handler: handle_plan_list,
        safety: |_| SafetyVerdict::Allow,
        default_timeout: Duration::from_secs(15),
    });
}
