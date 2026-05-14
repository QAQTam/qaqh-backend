//! Tracking: file writes, tool code display, assistant content analysis.
use crate::agent::AgentState;
use crate::types::Message;

/// Track a file written/edited this turn for sandbox enforcement.
pub fn track_file_written(state: &mut AgentState, args: &str) {
    let path = arg_parser::parse_file_arg(args);
    if let Some(ref p) = path {
        if !state.files_written_this_turn.contains(p) {
            state.files_written_this_turn.push(p.clone());
        }
    }
}

use super::arg_parser;
use super::arg_parser::{tool_action, parse_file_arg, parse_cmd_arg};

/// Track tool execution outcome for bottom-left code preview panel.
pub fn track_tool_code(state: &mut AgentState, tool_name: &str, args: &str, result: &str) {
    let raw = result.trim();
    match tool_name {
        "file" => {
            let action = arg_parser::tool_action(args);
            let path = arg_parser::parse_file_arg(args).unwrap_or_default();
            state.tool_code_path = path.clone();
            state.tool_code_action = action.clone();
            if raw.is_empty() {
                state.tool_code_content = format!("── {action} ──\n[PARTIAL] viewing {path}");
            } else {
                state.tool_code_content = format!("── {action} ──\n{raw}");
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
    state.messages.iter().rev()
        .find(|m| m.role == "assistant" && m.content.is_some())
        .and_then(|m| m.content.clone())
        .unwrap_or_default()
}

/// Read the active plan text from session persistence.
pub fn read_active_plan(state: &AgentState) -> String {
    let path = std::env::temp_dir().join("dsx_active_plan.txt");
    std::fs::read_to_string(path).unwrap_or_default()
}
