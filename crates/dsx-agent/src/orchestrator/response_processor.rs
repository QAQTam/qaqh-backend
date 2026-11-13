//! Tool response finalizer: after streaming completes, handle tool calls.
//!
//! Uses ToolResultAppender as the single entry point for tool result writes,
//! eliminating the scattered push_tool calls that caused orphan tool_use 400s.

use crate::api::StreamEvent;
use crate::tools::{self, classify_tool, execute_tool};
use dsx_types::{Message, SafetyLevel, ToolCall, UsageInfo};
use crate::health::monitor::{pre_tool_gate, post_tool_record};
use crate::agent::{AgentState, ToolResultAppender};
use crate::orchestrator::gates;
use crate::orchestrator::tracker;
use crate::orchestrator::turn_scorer;
use crate::orchestrator::arg_parser;
use crate::orchestrator::session_persistence;
use std::time::Instant;
use tokio::sync::mpsc;

// ── Shared tool execution helper ──

/// Execute a single tool through the full gates + tracking pipeline.
///
/// Shared between `Safe` and `skip_all` modes, eliminating the old
/// nearly-identical code blocks.
fn process_single_tool(
    appender: &mut ToolResultAppender,
    tc: &ToolCall,
) {
    // Pre-tool safety gate
    match pre_tool_gate(&tc.function.name, &tc.function.arguments, &appender.state.monitor) {
        crate::health::monitor::PreToolResult::Block { reason } => {
            appender.append(&tc.function.name, &tc.id, &tc.function.arguments,
                &format!("[ERROR] Blocked: {}", reason));
            return;
        }
        _ => {}
    }

    // Phase check + explore gate + health check → execute
    let raw = if gates::phase_check_tool(appender.state, &tc.function.name, &tc.id)
        || gates::explore_gate(appender.state, &tc.function.name, &tc.id, &tc.function.arguments)
    {
        return;
    } else if let Some(reason) = gates::pre_tool_health_check(appender.state, &tc.function.name) {
        appender.append(&tc.function.name, &tc.id, &tc.function.arguments, &reason);
        return;
    } else {
        execute_tool(&tc.function.name, "", &tc.function.arguments)
    };

    // Post-execution tracking
    let success = !raw.starts_with("[ERROR]") && !raw.starts_with("[FAIL]");
    post_tool_record(&tc.function.name, success, &mut appender.state.monitor);
    gates::post_tool_health_record(appender.state, &tc.function.name, success);

    // Side-effect tracking
    if tc.function.name == "exec" && arg_parser::tool_action(&tc.function.arguments) == "explore" {
        appender.state.has_explored = true;
    }
    if tc.function.name == "read_file"
        || (tc.function.name == "file" && arg_parser::tool_action(&tc.function.arguments) == "read")
    {
        appender.state.turns_since_last_read = 0;
    }

    // Project map
    if raw.starts_with("[PROJECT_MAP]") {
        appender.state.project_map = raw.clone();
    }

    // Track tool code
    tracker::track_tool_code(appender.state, &tc.function.name, &tc.function.arguments, &raw);

    // Sudo required
    if raw.starts_with("[SUDO_REQUIRED]") {
        appender.state.sudo_pending.push((tc.clone(), raw[16..].trim().to_string()));
        return;
    }

    // Failure tracking
    if !success {
        appender.state.tool_failures += 1;
        appender.state.health.record_error(&tc.function.name, &raw);
    } else {
        appender.state.tool_failures = 0;
        tracker::track_file_written(appender.state, &tc.function.arguments);
    }

    // Append result to context (single entry point)
    appender.append(&tc.function.name, &tc.id, &tc.function.arguments, &raw);
}

/// Result of finalize_stream_response — tells the caller what to do next.
pub enum LoopDecision {
    /// No tools, or all auto-executed → restart agent loop
    ContinueAgentLoop,
    /// Tools need user confirmation
    ConfirmTools(Vec<(ToolCall, SafetyLevel, String)>),
    /// Sudo prompt needed
    AwaitSudo(Vec<(ToolCall, String)>),
    /// Intent picker (ask_user)
    AwaitIntent,
    /// Safety gate triggered — force text response
    ForceTextResponse(String),
    /// Done for this turn
    EndTurn,
}

/// Process the final stream response: save the assistant message, handle tool calls.
pub fn finalize_stream_response(
    state: &mut AgentState,
    raw_message: Message,
    usage: Option<UsageInfo>,
    tx: mpsc::Sender<StreamEvent>,
) -> LoopDecision {
    let _ = state.ctx.push_assistant(raw_message.clone());
    state.api_usage = usage.clone();
    if let Some(ref u) = usage {
        state.session_tokens += u.total_tokens as u64;
    }
    state.health.record_api_success(&state.config.model);
    state.stream_tool_progress.clear();

    let tool_calls = raw_message.tool_calls.clone().unwrap_or_default();

    if tool_calls.is_empty() {
        state.turn_scores.push(turn_scorer::score_current_turn(state));
        state.stream_content.clear();
        state.stream_reasoning.clear();
        session_persistence::maybe_save_session(state);
        return LoopDecision::EndTurn;
    }

    state.tool_calls_this_turn += tool_calls.len() as u32;
    let tc_names: Vec<&str> = tool_calls.iter().map(|t| t.function.name.as_str()).collect();
    log::info!("finalize_stream: {} tool calls [{}] (turn={}, failures={})",
        tool_calls.len(), tc_names.join(", "), state.tool_calls_this_turn, state.tool_failures);

    let mut needs_confirm: Vec<(ToolCall, SafetyLevel, String)> = Vec::new();
    let mut appender = ToolResultAppender::new(state);

    for (idx, tc) in tool_calls.iter().enumerate() {
        let (safety, prompt) = classify_tool(&tc.function.name, &tc.function.arguments);

        if safety == SafetyLevel::Safe {
            // ── ask_user: special case, must be sole tool call ──
            if tc.function.name == "agent" && arg_parser::tool_action(&tc.function.arguments) == "ask" {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&tc.function.arguments) {
                    appender.state.intent_question = v.get("question").and_then(|q| q.as_str()).unwrap_or("").to_string();
                    if let Some(arr) = v.get("options").and_then(|o| o.as_array()) {
                        appender.state.intent_options = arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
                    }
                }
                // Cancel all other tools (ask_user must be the only one)
                for (pending_tc, _, _) in &needs_confirm {
                    appender.append(&pending_tc.function.name, &pending_tc.id, &pending_tc.function.arguments,
                        "[CANCELLED] ask_user must be the only tool call. Re-call in next turn.");
                }
                for remaining_tc in tool_calls.iter().skip(idx + 1) {
                    appender.append(&remaining_tc.function.name, &remaining_tc.id, &remaining_tc.function.arguments,
                        "[CANCELLED] ask_user must be the only tool call. Re-call in next turn.");
                }
                return LoopDecision::AwaitIntent;
            }

            // ── exec(execute/run): long-running async spawn ──
            if tc.function.name == "exec" && (arg_parser::tool_action(&tc.function.arguments) == "execute" || arg_parser::tool_action(&tc.function.arguments) == "run") {
                if gates::explore_gate(appender.state, &tc.function.name, &tc.id, &tc.function.arguments) {
                    continue;
                }
                if appender.state.exec_pending >= 2 {
                    appender.append("exec", &tc.id, &tc.function.arguments,
                        "[CANCELLED] Max 2 concurrent execs per turn.");
                    continue;
                }
                appender.state.exec_pending += 1;
                if appender.state.exec_started_at.is_none() {
                    appender.state.exec_started_at = Some(Instant::now());
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&tc.function.arguments) {
                    if let Some(cmd) = v.get("command").and_then(|c| c.as_str()) {
                        appender.state.tool_code_path = cmd.to_string();
                        appender.state.tool_code_action = "exec".into();
                        appender.state.tool_code_status = None;
                        appender.state.tool_code_content = format!("$ {}\n", cmd);
                    }
                }
                tools::spawn_exec_pty(&tc.id, &tc.function.arguments, tx.clone());
                continue;
            }

            // ── Health gate: track tool, check circuit breaker ──
            if let Some(warn) = appender.state.health.track_tool(&tc.function.name, &tc.function.arguments) {
                let interrupt = format!("[HEALTH] {} Forcing fallback to text response.", warn);
                appender.append(&tc.function.name, &tc.id, &tc.function.arguments, &interrupt);
                continue;
            }

            // ── Common: gates, execute, track ──
            process_single_tool(&mut appender, tc);

        } else if appender.state.skip_all {
            // ── Skip-all mode: auto-execute dangerous tools ──
            process_single_tool(&mut appender, tc);
        } else {
            needs_confirm.push((tc.clone(), safety, prompt));
        }
    }

    // Recover state from appender
    let state = appender.into_inner();

    // ── Health escalation ──
    if let Some(escalation) = state.health.should_escalate() {
        log::warn!("health escalation: {}", escalation);
        state.turn_annotations.push(format!("[SYSTEM] {}", escalation));
        state.flush_notes();
        session_persistence::maybe_save_session(state);
        return LoopDecision::ForceTextResponse(escalation);
    }

    // ── 100-call safety gate ──
    if state.tool_calls_this_turn >= 100 {
        log::warn!("safety gate: 100 tool calls this turn, forcing text response");
        state.turn_scores.push(turn_scorer::score_current_turn(state));
        state.tool_failures = 0;
        state.tool_calls_this_turn = 0;
        let interrupt = "[System] Tool loop limit reached (100 calls in this turn). Respond with what you have, without calling more tools.";
        if state.exec_pending == 0 {
            state.turn_annotations.push(interrupt.to_string());
            state.flush_notes();
            session_persistence::maybe_save_session(state);
        } else {
            state.gate_message = Some(interrupt.into());
        }
        return LoopDecision::ForceTextResponse(interrupt.into());
    }

    // ── 3-consecutive-failure gate ──
    if state.tool_failures >= 3 {
        log::warn!("safety gate: 3 consecutive failures, forcing text response");
        state.turn_scores.push(turn_scorer::score_current_turn(state));
        state.tool_failures = 0;
        state.tool_calls_this_turn = 0;
        let interrupt = "3 consecutive tool failures occurred. Respond with your analysis — do not call more tools.";
        if state.exec_pending == 0 {
            state.turn_annotations.push(format!("[System] {}", interrupt));
            state.flush_notes();
            session_persistence::maybe_save_session(state);
        } else {
            state.gate_message = Some(interrupt.into());
        }
        return LoopDecision::ForceTextResponse(interrupt.into());
    }

    // ── Tool-only turn counting ──
    if needs_confirm.is_empty() {
        state.consecutive_tool_turns += 1;
    } else {
        state.consecutive_tool_turns = 0;
    }

    if state.consecutive_tool_turns >= 30 {
        state.turn_scores.push(turn_scorer::score_current_turn(state));
        state.consecutive_tool_turns = 0;
        state.turn_annotations.push("[SYSTEM] 30 consecutive tool-only turns. Tools may be broken or stuck. Stop calling tools and respond with your analysis.".to_string());
        state.flush_notes();
        session_persistence::maybe_save_session(state);
        return LoopDecision::ForceTextResponse("30 consecutive tool-only turns.".into());
    }
    if state.consecutive_tool_turns == 5 {
        state.turn_annotations.push("[REMINDER] 5 consecutive tool-only turns. Are tools working correctly? Consider explaining what you're doing in text.".to_string());
        state.flush_notes();
    }

    // ── Sudo prompt gate ──
    if !state.sudo_pending.is_empty() {
        state.stream_content.clear();
        state.stream_reasoning.clear();
        return LoopDecision::AwaitSudo(std::mem::take(&mut state.sudo_pending));
    }

    // ── Auto-verify queue ──
    if needs_confirm.is_empty() && !state.auto_verify.is_empty() {
        let pending: Vec<String> = state.auto_verify.drain(..).collect();
        for cmd in pending {
            let r = execute_tool("exec", "", &format!("{{\"command\":\"{}\"}}", cmd));
            log::info!("auto-verify: {} → {}", cmd, r.lines().next().unwrap_or("?"));
        }
    }

    // ── Next action ──
    if needs_confirm.is_empty() {
        state.stream_content.clear();
        state.stream_reasoning.clear();
        LoopDecision::ContinueAgentLoop
    } else {
        state.pending_tools = needs_confirm;
        LoopDecision::ConfirmTools(state.pending_tools.clone())
    }
}
