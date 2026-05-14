//! Skill listing, lookup, and file metadata.
//!
//! Three standalone tools: list_skills, read_skill_ref, glance.
//! Registered via `register()` as individual ToolHandlers.

use std::time::Duration;

use crate::types::ToolDef;
// NOTE: crate::session and crate::skills are in dsx-agent, not dsx-tools.
// These will be provided via IPC in Phase 4. Stubbed for now.
use super::parse_arg;
use super::{ToolManager, ToolHandler, ToolKey, ToolCallCtx, ToolResult, SafetyVerdict};

// ═══════════════════════════════════════════════════════════════════════════
// Individual tool implementations (unchanged internals)
// ═══════════════════════════════════════════════════════════════════════════

#[allow(dead_code)]
pub(super) fn list_skills_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "list_skills".into(),
            description: "List all installed skills and their descriptions.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        },
    }
}

#[allow(dead_code)]
pub(super) fn read_skill_ref_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "read_skill_ref".into(),
            description: "Read a skill's files to load domain knowledge for your task.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Skill name (e.g. postgresql)"},
                    "path": {"type": "string", "description": "File path within the skill (default: SKILL.md). Use references/<file> for reference docs."}
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_list_skills(_args: &str) -> String {
    // TODO Phase 4: return actual skill list via IPC from agent
    String::new()
}

pub(super) fn exec_read_skill_ref(args: &str) -> String {
    // TODO Phase 4: read skill via IPC from agent
    format!("[HINT] Skill system unavailable in standalone mode. Try again after Phase 4 integration.")
}

// ── Glance tool (file metadata preview) ──

#[allow(dead_code)]
pub(super) fn glance_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "glance".into(),
            description: "Preview file metadata: symbols, purpose, language, size. Use after explore.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path to preview. Relative to project root."}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_glance(args: &str) -> String {
    let path = parse_arg(args, "path");
    if path.is_empty() {
        return "[ERROR] glance: path is required".to_string();
    }

    // TODO Phase 4: glance receives seed + semantic data via IPC from agent.
    // In standalone mode, always return the "no cached summary" message.
    format!(
        "[OK] {} — No cached summary. Use read() to load and auto-cache, or explore() to scan the directory first.",
        path
    )
}

/// Find the last round number that touched a given file path.
fn find_last_round_for_file(short_mem: &str, file_path: &str) -> Option<usize> {
    for line in short_mem.lines().rev() {
        if !line.starts_with("- R") { continue; }
        if !line.contains(file_path) { continue; }
        if let Some(rest) = line.strip_prefix("- R") {
            if let Some(num_str) = rest.split(':').next() {
                if let Ok(n) = num_str.trim().parse::<usize>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

// ═══════════════════════════════════════════════════════════════════════════
// Handler wrappers
// ═══════════════════════════════════════════════════════════════════════════

fn handle_list_skills(ctx: ToolCallCtx) -> ToolResult {
    let args = serde_json::to_string(&ctx.args).unwrap_or_default();
    ToolResult::ok(exec_list_skills(&args))
}

fn handle_read_skill_ref(ctx: ToolCallCtx) -> ToolResult {
    let args = serde_json::to_string(&ctx.args).unwrap_or_default();
    ToolResult::ok(exec_read_skill_ref(&args))
}

fn handle_glance(ctx: ToolCallCtx) -> ToolResult {
    let args = serde_json::to_string(&ctx.args).unwrap_or_default();
    ToolResult::ok(exec_glance(&args))
}

// ═══════════════════════════════════════════════════════════════════════════
// Safety classifiers — all read-only metadata, always safe
// ═══════════════════════════════════════════════════════════════════════════

fn safety_list_skills(_ctx: &ToolCallCtx) -> SafetyVerdict {
    SafetyVerdict::Allow
}

fn safety_read_skill_ref(_ctx: &ToolCallCtx) -> SafetyVerdict {
    SafetyVerdict::Allow
}

fn safety_glance(_ctx: &ToolCallCtx) -> SafetyVerdict {
    SafetyVerdict::Allow
}

// ═══════════════════════════════════════════════════════════════════════════
// Registration entry point
// ═══════════════════════════════════════════════════════════════════════════

pub fn register(mgr: &mut ToolManager) {
    mgr.register(ToolHandler {
        key: ToolKey::new("list_skills", ""),
        description: "List all installed skills and their descriptions.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
        handler: handle_list_skills,
        safety: safety_list_skills,
        default_timeout: Duration::from_secs(15),
    });

    mgr.register(ToolHandler {
        key: ToolKey::new("read_skill_ref", ""),
        description: "Read a skill's files to load domain knowledge for your task.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Skill name (e.g. postgresql)"},
                "path": {"type": "string", "description": "File path within the skill (default: SKILL.md). Use references/<file> for reference docs."}
            },
            "required": ["name"],
            "additionalProperties": false
        }),
        handler: handle_read_skill_ref,
        safety: safety_read_skill_ref,
        default_timeout: Duration::from_secs(15),
    });

    mgr.register(ToolHandler {
        key: ToolKey::new("glance", ""),
        description: "Preview file metadata: symbols, purpose, language, size. Use after explore.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path to preview. Relative to project root."}
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        handler: handle_glance,
        safety: safety_glance,
        default_timeout: Duration::from_secs(15),
    });
}
