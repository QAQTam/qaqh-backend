//! Gate functions: explore-before-read, phase checks, health checks.
use crate::agent::AgentState;

/// Phase gate: block destructive tools in Explore/Plan mode.
pub fn phase_check_tool(state: &mut AgentState, tool_name: &str, tc_id: &str) -> bool {
    let is_readonly = state.current_task_phase == crate::types::TaskPhase::Explore
        || state.current_task_phase == crate::types::TaskPhase::Plan;
    if !is_readonly { return false; }
    let blocked = ["file", "exec", "agent"];
    if !blocked.contains(&tool_name) { return false; }
    let target = "coding";
    let msg = format!(
        "[ERROR] '{}' blocked in {:?} mode — not allowed.\n[HINT] Call status(state=\"{}\") to switch, then retry.",
        tool_name, state.current_task_phase, target);
    let _ = state.ctx.push_tool_result(tc_id, &msg);
    true
}

/// Pre-tool health check: circuit breaker.
pub fn pre_tool_health_check(state: &mut AgentState, name: &str) -> Option<String> {
    state.health.record_tool_call(name);
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
    let is_exec = tool_name == "exec" && action == "execute";
    let is_explore = tool_name == "exec" && action == "explore";
    if is_read || is_write {
        if !state.has_explored {
            let _ = state.ctx.push_tool_result(tc_id, &format!("[ERROR] '{}' blocked: you haven't explored the project yet.\n[HINT] Call exec(explore) first.", tool_name));
            return true;
        }
        if is_write {
            if let Some(ref path) = parse_file_arg(args) {
                let declared = last_assistant_mentions(state, path);
                let is_critical = turn_scorer::classify_path(path) == turn_scorer::PathTrust::HighStake;
                state.health.record_intent_compliance(path, declared, is_critical);
                if let Some(note) = state.health.intent_note(path) {
                    state.ctx.annotate("intent", &note);
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
            state.ctx.annotate("exec", &format!("'{}' — next time, say what you're running so the log captures it.", cmd.chars().take(60).collect::<String>()));
        }
        if let Some((tool_match, cmd_summary)) = turn_scorer::detect_tool_equivalent(&cmd) {
            state.ctx.annotate("exec", &format!("'{}' looks like {}() — if {}() is insufficient, tell us why.", cmd_summary, tool_match, tool_match));
        }
        for written in &state.files_written_this_turn {
            if cmd.contains(written) && turn_scorer::classify_path(written) != turn_scorer::PathTrust::Trusted {
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
    if let Some(last) = state.messages.iter().rev().find(|m| m.role == "assistant" && m.content.is_some()) {
        last.content.as_ref().map(|c| c.contains(path)).unwrap_or(false)
    } else {
        false
    }
}
