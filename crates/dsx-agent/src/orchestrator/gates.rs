//! Gate functions: explore-before-read, phase checks, health checks.
use crate::agent::AgentState;

/// Phase gate: block destructive tools in Explore/Plan mode.
pub fn phase_check_tool(state: &mut AgentState, tool_name: &str, tc_id: &str) -> bool {
    let is_readonly = state.current_task_phase == dsx_types::TaskPhase::Plan;
    if !is_readonly { return false; }
    let blocked = ["file", "exec"];
    if !blocked.contains(&tool_name) { return false; }
    let target = "coding";
    let msg = format!(
        "[ERROR] '{}' blocked in {:?} mode — not allowed.\n[HINT] Call status(state=\"{}\") to switch, then retry.",
        tool_name, state.current_task_phase, target);
    let _ = state.ctx.push_tool_result(tc_id, &msg);
    true
}

/// Pre-tool health check: circuit breaker.
/// Returns Some(error_msg) if tool should be blocked, None if allowed.
/// Caller is responsible for recording tool call outcome.
pub fn pre_tool_health_check(state: &mut AgentState, name: &str) -> Option<String> {
    if state.health.should_block(name).is_some() {
        return Some(format!("[ERROR] tool '{}' blocked by health platform", name));
    }
    None
}

/// Record tool outcome.
pub fn post_tool_health_record(state: &mut AgentState, name: &str, success: bool) {
    state.health.record_tool_outcome(name, success);
}

use super::arg_parser::{tool_action, parse_file_arg, parse_cmd_arg};
use super::tracker::last_assistant_content;
use crate::orchestrator::turn_scorer;

/// Explore gate: enforce explore-before-read, intent, stale-edit blocking.
pub fn explore_gate(state: &mut AgentState, tool_name: &str, tc_id: &str, args: &str) -> bool {
    let action = tool_action(args);
    let is_read = tool_name == "file" && action == "read";
    let is_write = tool_name == "file" && (action == "write" || action == "edit" || action == "diff");
    let is_edit = tool_name == "file" && action == "edit";
    let is_exec = tool_name == "exec" && (action == "execute" || action == "run");
    let is_explore = tool_name == "exec" && action == "explore";
    if is_read || is_write {
        if !state.has_explored {
            let _ = state.ctx.push_tool_result(tc_id, &format!("[ERROR] '{}' blocked: you haven't explored the project yet.\n[HINT] Call exec(explore) first.", tool_name));
            return true;
        }
        if is_write {
            if let Some(ref path) = parse_file_arg(args) {
                let declared = last_assistant_mentions(state, path);
                if !declared {
                    state.turn_annotations.push(format!("[intent] write to '{}' was NOT declared in assistant reasoning \u{2014} consider requiring declaration", path));
                }
            }
        }
        if is_edit && state.turns_since_last_read >= 4 {
            let _ = state.ctx.push_tool_result(tc_id, &format!("[ERROR] 'file edit' blocked: {} turns since last read. Context may be stale.\n[HINT] Call file(read) first.", state.turns_since_last_read));
            return true;
        }
    }
    if is_exec {
        if !state.has_explored {
            let _ = state.ctx.push_tool_result(tc_id, &format!("[ERROR] 'exec execute' blocked: you haven't explored yet.\n[HINT] Call exec(explore) first."));
            return true;
        }
        let cmd = parse_cmd_arg(args).unwrap_or_else(|| "?".into());
        if last_assistant_content(state).is_empty() {
            state.turn_annotations.push(format!("[exec] '{}' — next time, say what you're running so the log captures it.", cmd.chars().take(60).collect::<String>()));
        }
        if let Some((tool_match, cmd_summary)) = turn_scorer::detect_tool_equivalent(&cmd) {
            state.turn_annotations.push(format!("[exec] '{}' looks like {}() — if {}() is insufficient, tell us why.", cmd_summary, tool_match, tool_match));
        }
        for written in &state.files_written_this_turn {
            let written_matches = cmd.contains(written)
                || std::path::absolute(written).ok()
                    .map(|a| cmd.contains(a.to_string_lossy().as_ref()))
                    .unwrap_or(false);
            if written_matches && turn_scorer::classify_path(written) != turn_scorer::PathTrust::Trusted {
                let _ = state.ctx.push_tool_result(tc_id, &format!("[ERROR] 'exec' blocked: '{}' was written this turn.\n[HINT] Explain what the script does and run it NEXT turn.", written));
                return true;
            }
        }
    }
    if is_explore {
        // Backward compat for exec(explore) — will be moved to explore tool
        state.has_explored = true;
    }
    false
}

fn last_assistant_mentions(state: &AgentState, path: &str) -> bool {
    if let Some(last) = state.ctx.to_vec().iter().rev().find(|m| m.role == "assistant" && m.content.is_some()) {
        last.content.as_ref().map(|c| c.contains(path)).unwrap_or(false)
    } else {
        false
    }
}
