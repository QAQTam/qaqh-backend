//! Tracking: file writes, tool code display, assistant content analysis.
use crate::agent::AgentState;

/// Track a file written/edited this turn for sandbox enforcement.
pub fn track_file_written(state: &mut AgentState, args: &str) {
    if let Some(p) = arg_parser::parse_file_arg(args) {
        // Store both the original relative path and the absolute form
        // so command-text matching works regardless of how the exec tool
        // references the file.
        if !state.files_written_this_turn.contains(&p) {
            state.files_written_this_turn.push(p.clone());
        }
        if let Ok(abs) = std::path::absolute(&p) {
            let abs_str = abs.to_string_lossy().to_string();
            if abs_str != p && !state.files_written_this_turn.contains(&abs_str) {
                state.files_written_this_turn.push(abs_str);
            }
        }
    }
}

use super::arg_parser;

/// Track tool execution outcome for bottom-left code preview panel.
pub fn track_tool_code(state: &mut AgentState, tool_name: &str, args: &str, result: &str) {
    let raw = result.trim();
    match tool_name {
        "file" | "read_file" | "write_file" | "edit_file" | "edit_file_diff" => {
            let action = arg_parser::tool_action(args);
            let path = arg_parser::parse_file_arg(args).unwrap_or_default();
            state.tool_code_path = path.clone();
            state.tool_code_action = if action.is_empty() {
                tool_name.trim_end_matches("_file").to_string()
            } else {
                action
            };
            if raw.is_empty() {
                state.tool_code_content = format!("── {} ──\n[PARTIAL] viewing {path}", state.tool_code_action);
            } else {
                state.tool_code_content = format!("── {} ──\n{raw}", state.tool_code_action);
            }
        }
        "exec" => {
            let action = arg_parser::tool_action(args);
            state.tool_code_action = "exec".into();
            if action == "explore" {
                state.tool_code_content = format!("── explore ──\n{raw}");
            }
        }
        "agent" => {
            let action = arg_parser::tool_action(args);
            state.tool_code_content = format!("── {action} ──\n{raw}");
        }
        _ => {}
    }
}

/// Get the last assistant message text content (for intent gate).
pub fn last_assistant_content(state: &AgentState) -> String {
    state.ctx.to_vec().iter().rev()
        .find(|m| m.role == "assistant" && m.content.is_some())
        .and_then(|m| m.content.clone())
        .unwrap_or_default()
}

/// Read the active plan text from session persistence.
pub fn read_active_plan(_state: &AgentState) -> String {
    let path = std::env::temp_dir().join("dsx_active_plan.txt");
    std::fs::read_to_string(path).unwrap_or_default()
}
