//! Tool dispatch: confirm, reject, cancel streaming, cancel exec.
//! Uses ToolResultAppender as the single entry point for tool result writes.

use crate::api::StreamEvent;
use crate::health::monitor::{post_tool_record, pre_tool_gate};
use crate::tools::{self, execute_tool};
use crate::agent::{AgentState, ToolResultAppender};
use crate::orchestrator::agent_loop;
use crate::orchestrator::gates;
use crate::orchestrator::tracker;
use crate::orchestrator::arg_parser;
use std::time::Instant;
use tokio::sync::mpsc;

/// User confirmed the pending tools — execute them.
pub fn handle_confirm_tools(state: &mut AgentState, tx: mpsc::Sender<StreamEvent>) {
    let tools = std::mem::take(&mut state.pending_tools);
    let mut has_long_exec = false;
    let mut appender = ToolResultAppender::new(state);

    for (tc, _, _) in &tools {
        if tc.function.name == "exec" {
            // Limit concurrent execs
            if appender.state.exec_pending >= 2 {
                appender.append("exec", &tc.id, &tc.function.arguments,
                    "[CANCELLED] Max 2 concurrent execs per turn.");
                continue;
            }
            has_long_exec = true;
            appender.state.exec_pending += 1;
            if appender.state.exec_started_at.is_none() {
                appender.state.exec_started_at = Some(Instant::now());
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&tc.function.arguments) {
                if let Some(cmd) = v.get("command").and_then(|c| c.as_str()) {
                    appender.state.tool_code_path = cmd.to_string();
                    appender.state.tool_code_action = "exec".into();
                    appender.state.tool_code_content = format!("$ {}\n", cmd);
                }
            }
            tools::spawn_exec_async(&tc.id, &tc.function.arguments, tx.clone());
            continue;
        }

        // Pre-tool safety gate
        match pre_tool_gate(&tc.function.name, &tc.function.arguments, &appender.state.monitor) {
            crate::health::monitor::PreToolResult::Block { reason } => {
                appender.append(&tc.function.name, &tc.id, &tc.function.arguments,
                    &format!("[ERROR] Blocked: {}", reason));
                continue;
            }
            _ => {}
        }

        let raw = if gates::phase_check_tool(appender.state, &tc.function.name, &tc.id)
            || gates::explore_gate(appender.state, &tc.function.name, &tc.id, &tc.function.arguments) {
            continue;
        } else {
            execute_tool(&tc.function.name, "", &tc.function.arguments)
        };

        let success = !raw.starts_with("[ERROR]") && !raw.starts_with("[FAIL]");
        post_tool_record(&tc.function.name, success, &mut appender.state.monitor);

        if tc.function.name == "exec" && arg_parser::tool_action(&tc.function.arguments) == "explore" {
            appender.state.has_explored = true;
        }
        if tc.function.name == "read_file"
            || (tc.function.name == "file" && arg_parser::tool_action(&tc.function.arguments) == "read") {
            appender.state.turns_since_last_read = 0;
        }

        tracker::track_tool_code(appender.state, &tc.function.name, &tc.function.arguments, &raw);

        if raw.starts_with("[SUDO_REQUIRED]") {
            let sudo_cmd = raw[16..].trim().to_string();
            appender.state.sudo_pending.push((tc.clone(), sudo_cmd));
            continue;
        }

        if !success {
            appender.state.health.record_error(&tc.function.name, &raw);
        } else {
            tracker::track_file_written(appender.state, &tc.function.arguments);
        }

        // Append result via ToolResultAppender (single entry point)
        appender.append(&tc.function.name, &tc.id, &tc.function.arguments, &raw);
    }

    let state = appender.into_inner();

    if !state.sudo_pending.is_empty() {
        return; // caller handles sudo mode switch
    }

    if has_long_exec {
        state.stream_content.clear();
        state.stream_reasoning.clear();
        return;
    }

    state.stream_content.clear();
    state.stream_reasoning.clear();
    agent_loop::handle_start_agent_loop(state, tx);
}

/// User rejected the pending tools — roll back context.
pub fn handle_reject_tools(state: &mut AgentState) {
    state.pending_tools.clear();
    state.ctx.remove_last_step();

    state.tool_results.clear();
    state.stream_content.clear();
    state.stream_reasoning.clear();
}

/// User pressed Esc during streaming — cancel the in-flight request.
pub fn handle_cancel_stream(state: &mut AgentState) {
    crate::tools::CANCEL.store(true, std::sync::atomic::Ordering::Relaxed);
    state.stream_cancelled = true;

    if state.exec_pending > 0 {
        handle_cancel_exec(state);
    }

    let had_text = !state.stream_content.is_empty();
    let had_tools = !state.stream_tool_progress.is_empty();

    if had_text || had_tools {
        state.ctx.remove_last_step_if_incomplete();
        let mut partial = dsx_types::Message::assistant_empty();
        if had_text {
            partial.content = Some(format!("{}\n\n*(interrupted by user)*", state.stream_content));
        }
        if !state.stream_reasoning.is_empty() {
            partial.reasoning_content = Some(state.stream_reasoning.clone());
        }
        let _ = state.ctx.push_assistant_restore(partial);
        state.turn_annotations.push("[health] User interrupted the AI during output".to_string());
    } else if !state.stream_reasoning.is_empty() {
        state.turn_annotations.push("[health] User interrupted the AI during thinking".to_string());
    }


    state.stream_content.clear();
    state.stream_reasoning.clear();
    state.stream_tool_progress.clear();
}

/// Cancel running exec child processes.
pub fn handle_cancel_exec(state: &mut AgentState) {
    for &pid in &state.exec_child_pids {
        dsx_types::platform::terminate_process(pid);
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    for &pid in &state.exec_child_pids {
        dsx_types::platform::kill_process(pid);
    }
    state.exec_child_pids.clear();
    state.exec_pending = 0;
    state.exec_started_at = None;
}
