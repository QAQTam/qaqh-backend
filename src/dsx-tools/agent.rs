//! Agent tool: git, plan, task, memory, status, ask, health, skills.
use crate::types::{SafetyLevel, ToolDef};

pub use super::meta::*;
pub use super::ask::*;
pub use super::status::*;
pub use super::plan::*;
pub use super::task::*;
pub use super::memory::*;
pub use super::git::*;
pub use crate::health::exec_health;
use super::parse_arg;

/// Agent self-management tool definition.
pub fn agent_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "agent".into(),
            description: "Agent self-management. Actions: git_status, git_diff, git_log, git_commit, ask (ask user), status (declare working mode), mem_save, mem_read, mem_forget, recall (search session history), plan_create, plan_update, plan_read, plan_list, task_create, task_update, task_list, glance (file metadata), health (system health), pitfall_save, pitfall_guide, list_skills, read_skill_ref.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "description": "Action to perform"},
                    "name": {"type": "string"},
                    "subject": {"type": "string"},
                    "status": {"type": "string"},
                    "description": {"type": "string"},
                    "question": {"type": "string"},
                    "options": {"type": "array", "items": {"type": "string"}},
                    "query": {"type": "string"},
                    "kind": {"type": "string"},
                    "tier": {"type": "string", "description": "Memory tier: short/tasks"},
                    "key": {"type": "string"},
                    "content": {"type": "string"},
                    "path": {"type": "string"},
                    "goal": {"type": "string"},
                    "message": {"type": "string"},
                    "state": {"type": "string"},
                    "difficulty": {"type": "string"},
                    "tool": {"type": "string"},
                    "error_kind": {"type": "string"},
                    "lesson": {"type": "string"},
                    "max_results": {"type": "integer"},
                    "count": {"type": "integer"},
                    "staged": {"type": "boolean"},
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        },
    }
}

/// Execute agent action by dispatching to sub-modules.
pub fn exec_agent(args: &str) -> String {
    let action = parse_arg(args, "action");
    match action.as_str() {
        "git_status" => exec_git_status(),
        "git_diff" => exec_git_diff(),
        "git_log" => exec_git_log(args),
        "git_commit" => exec_git_commit(args),
        "ask" => exec_ask_user(args),
        "status" => exec_status(args),
        "mem_save" => exec_mem_save(args),
        "mem_read" => exec_mem_read(args),
        "mem_forget" => exec_mem_forget(args),
        "recall" => exec_recall(args),
        "plan_create" => exec_plan_create(args),
        "plan_update" => exec_plan_update(args),
        "plan_read" => exec_plan_read(args),
        "plan_list" => exec_plan_list(args),
        "task_create" => exec_task_create(args),
        "task_update" => exec_task_update(args),
        "task_list" => exec_task_list(args),
        "glance" => exec_glance(args),
        "health" => exec_health(args),
        "pitfall_save" => exec_pitfall_save(args),
        "pitfall_guide" => exec_pitfall_guide(args),
        "list_skills" => exec_list_skills(args),
        "read_skill_ref" => exec_read_skill_ref(args),
        _ => format!("[ERROR] Unknown agent action: {}", action),
    }
}

/// Agent actions are all Safe (no destructive side effects).
pub fn classify_agent(_args: &str) -> SafetyLevel {
    SafetyLevel::Safe
}
