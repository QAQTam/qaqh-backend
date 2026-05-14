//! Tool response finalizer: after streaming completes, handle tool calls.
use crate::api::StreamEvent;
use crate::tools::{self, classify_tool, execute_tool, wrap_tool_result};
use crate::tui::app::App;
use crate::tui::utils::{char_boundary, strip_ansi};
use crate::tui::Mode;
use crate::orchestrator::agent_loop;
use crate::orchestrator::gates;
use crate::orchestrator::tracker;
use crate::orchestrator::turn_scorer;
use crate::orchestrator::{session_persistence};
use crate::orchestrator::arg_parser;
use crate::types::{AgentPhase, Message, SafetyLevel, ToolCall, UsageInfo};
use crate::health::monitor::{pre_tool_gate, post_tool_record};
use std::time::Instant;
use tokio::sync::mpsc;

/// Process the final stream response: save the assistant message, handle tool calls.
pub fn finalize_stream_response(
    app: &mut App, raw_message: Message, usage: Option<UsageInfo>, tx: mpsc::Sender<StreamEvent>,
) {
    app.mode = Mode::Chat;
    let _ = app.ctx.push_assistant(raw_message.clone());
    app.api_usage = usage.clone();
    if let Some(ref u) = usage {
        app.session_tokens += u.total_tokens as u64;
    }
    app.health.record_api_success(&app.config.model);
    app.stream_tool_progress.clear();
    app.status_msg = String::new();

    let tool_calls = raw_message.tool_calls.clone().unwrap_or_default();

    if tool_calls.is_empty() {
        app.turn_scores.push(turn_scorer::score_current_turn(app));
        app.mode = Mode::Chat;
        app.stream_content.clear();
        app.stream_reasoning.clear();
        app.phase = AgentPhase::Idle;
        app.phase_count = 0;
        session_persistence::maybe_save_session(app);
        return;
    }

    app.tool_calls_this_turn += tool_calls.len() as u32;
    let tc_names: Vec<&str> = tool_calls.iter().map(|t| t.function.name.as_str()).collect();
    log::info!("finalize_stream: {} tool calls [{}] (turn={}, failures={})",
        tool_calls.len(), tc_names.join(", "), app.tool_calls_this_turn, app.tool_failures);

    let mut needs_confirm: Vec<(ToolCall, SafetyLevel, String)> = Vec::new();

    for (idx, tc) in tool_calls.iter().enumerate() {
        let (safety, prompt) = classify_tool(&tc.function.name, &tc.function.arguments);
        if safety == SafetyLevel::Safe {
            if tc.function.name == "agent" && arg_parser::tool_action(&tc.function.arguments) == "ask" {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&tc.function.arguments) {
                    app.intent_question = v.get("question").and_then(|q| q.as_str()).unwrap_or("").to_string();
                    if let Some(arr) = v.get("options").and_then(|o| o.as_array()) {
                        app.intent_options = arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
                    }
                }
                app.mode = Mode::IntentPicker;
                app.status_msg = "Choose an option or type your own".into();
                // Cancel any tools already queued for confirmation (before ask_user)
                for (pending_tc, _, _) in &needs_confirm {
                    app.push_tool(&pending_tc.id, "[CANCELLED] ask_user must be the only tool call in a response. Re-call this tool in the next turn.");
                }
                // Cancel remaining tools after ask_user
                for remaining_tc in tool_calls.iter().skip(idx + 1) {
                    app.push_tool(&remaining_tc.id, "[CANCELLED] ask_user must be the only tool call in a response. Re-call this tool in the next turn.");
                }
                return;
            }
            if tc.function.name == "exec" && arg_parser::tool_action(&tc.function.arguments) == "execute" {
                if gates::explore_gate(app, "exec", &tc.id, &tc.function.arguments) { continue; }
                if app.exec_pending >= 2 {
                    app.push_tool(&tc.id, "[CANCELLED] Max 2 concurrent execs per turn. Wait for pending execs to complete, then retry.");
                    continue;
                }
                app.exec_pending += 1;
                if app.exec_started_at.is_none() {
                    app.exec_started_at = Some(Instant::now());
                }
                app.exec_buffer.clear();
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&tc.function.arguments) {
                    if let Some(cmd) = v.get("command").and_then(|c| c.as_str()) {
                        app.tool_code_path = cmd.to_string();
                        app.tool_code_action = "exec".into();
                        app.tool_code_status = None;
                        app.exec_scroll_offset = 0;
                        app.tool_code_content = format!("$ {}\n", cmd);
                    }
                }
                tools::spawn_exec_pty(&tc.id, &tc.function.arguments, tx.clone());
                continue;
            }
            if let Some(warn) = app.health.track_tool(&tc.function.name, &tc.function.arguments) {
                app.status_msg = format!("⚠ {}", warn);
                let interrupt = format!("[HEALTH] {} Forcing fallback to text response.", warn);
                app.push_tool(&tc.id, &interrupt);
                continue;
            }
            match pre_tool_gate(&tc.function.name, &tc.function.arguments, &app.monitor) {
                crate::health::monitor::PreToolResult::Block { reason } => {
                    app.push_tool(&tc.id, &format!("[ERROR] Blocked: {}", reason));
                    continue;
                }
                _ => {}
            }
            let raw = if gates::phase_check_tool(app, &tc.function.name, &tc.id)
                || gates::explore_gate(app, &tc.function.name, &tc.id, &tc.function.arguments) {
                continue;
            } else if let Some(reason) = gates::pre_tool_health_check(app, &tc.function.name) {
                app.push_tool(&tc.id, &reason);
                continue;
            } else {
                execute_tool(&tc.function.name, &tc.function.arguments)
            };
            post_tool_record(&tc.function.name, !raw.starts_with("[ERROR]") && !raw.starts_with("[FAIL]"), &mut app.monitor);
            if tc.function.name == "exec" && arg_parser::tool_action(&tc.function.arguments) == "explore" { app.has_explored = true; }
            if tc.function.name == "file" && arg_parser::tool_action(&tc.function.arguments) == "read" { app.turns_since_last_read = 0; }
            if raw.starts_with("[PROJECT_MAP]") { app.project_map = raw.clone(); }
            if raw.starts_with("[SUDO_REQUIRED]") {
                let sudo_cmd = raw[16..].trim().to_string();
                app.sudo_pending.push((tc.clone(), sudo_cmd));
                continue;
            }
            tracker::track_tool_code(app, &tc.function.name, &tc.function.arguments, &raw);
            let failed = raw.starts_with("[ERROR]") || raw.starts_with("[FAIL]");
            if failed {
                app.tool_failures += 1;
                app.health.record_error(&tc.function.name, &raw);
                app.health.consecutive_failures = app.health.consecutive_failures.saturating_add(1);
                app.health.tool_failures_this_turn += 1;
            } else {
                app.tool_failures = 0;
                app.health.consecutive_failures = 0;
                app.track_file_written(&tc.function.arguments);
            }
            gates::post_tool_health_record(app, &tc.function.name, !failed);
            let result = wrap_tool_result(&tc.function.name, &raw);
            app.tool_results.push((tc.function.name.clone(), result.clone()));
            app.push_tool(&tc.id, &result);
            if raw.starts_with("[OK]") && (tc.function.name == "file" && (arg_parser::tool_action(&tc.function.arguments) == "write" || arg_parser::tool_action(&tc.function.arguments) == "edit")) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&tc.function.arguments) {
                    if let Some(path) = v.get("path").and_then(|p| p.as_str()) {
                        if path.ends_with(".rs") && !app.auto_verify.contains(&"cargo check".to_string()) {
                            app.auto_verify.push("cargo check".to_string());
                        }
                    }
                }
            }
        } else if app.skip_all {
            match pre_tool_gate(&tc.function.name, &tc.function.arguments, &app.monitor) {
                crate::health::monitor::PreToolResult::Block { reason } => {
                    app.push_tool(&tc.id, &format!("[ERROR] Blocked: {}", reason));
                    continue;
                }
                _ => {}
            }
            let raw = if gates::phase_check_tool(app, &tc.function.name, &tc.id)
                || gates::explore_gate(app, &tc.function.name, &tc.id, &tc.function.arguments) {
                continue;
            } else if let Some(reason) = gates::pre_tool_health_check(app, &tc.function.name) {
                app.push_tool(&tc.id, &reason);
                continue;
            } else {
                execute_tool(&tc.function.name, &tc.function.arguments)
            };
            post_tool_record(&tc.function.name, !raw.starts_with("[ERROR]") && !raw.starts_with("[FAIL]"), &mut app.monitor);
            if tc.function.name == "exec" && arg_parser::tool_action(&tc.function.arguments) == "explore" { app.has_explored = true; }
            if tc.function.name == "file" && arg_parser::tool_action(&tc.function.arguments) == "read" { app.turns_since_last_read = 0; }
            tracker::track_tool_code(app, &tc.function.name, &tc.function.arguments, &raw);
            if raw.starts_with("[SUDO_REQUIRED]") {
                app.sudo_pending.push((tc.clone(), raw[16..].trim().to_string()));
            } else {
                let failed = raw.starts_with("[ERROR]") || raw.starts_with("[FAIL]");
                if failed {
                    app.health.record_error(&tc.function.name, &raw);
                    app.health.consecutive_failures = app.health.consecutive_failures.saturating_add(1);
                    app.health.tool_failures_this_turn += 1;
                } else {
                    app.health.consecutive_failures = 0;
                    tracker::track_file_written(app, &tc.function.arguments);
                }
                let result = wrap_tool_result(&tc.function.name, &raw);
                app.tool_results.push((tc.function.name.clone(), result.clone()));
                app.push_tool(&tc.id, &result);
            }
        } else {
            needs_confirm.push((tc.clone(), safety, prompt));
        }
    }

    // Health escalation
    if let Some(escalation) = app.health.should_escalate() {
        log::warn!("health escalation: {}", escalation);
        app.status_msg = format!("⚠ Health: {}", escalation);
        app.ctx.annotate("SYSTEM", &escalation);
        app.ctx.flush_annotations();
        session_persistence::maybe_save_session(app);
        agent_loop::handle_start_agent_loop(app, tx);
        return;
    }

    // 100-call safety gate
    if app.tool_calls_this_turn >= 100 {
        log::warn!("safety gate: 100 tool calls this turn, forcing text response");
        app.turn_scores.push(turn_scorer::score_current_turn(app));
        app.tool_failures = 0;
        app.tool_calls_this_turn = 0;
        app.status_msg = "Tool loop limit reached (100). Forcing text response.".into();
        let interrupt = "[System] Tool loop limit reached (100 calls in this turn). Respond with what you have, without calling more tools.";
        if app.exec_pending == 0 {
            app.ctx.annotate("System", interrupt);
            app.ctx.flush_annotations();
            session_persistence::maybe_save_session(app);
            agent_loop::handle_start_agent_loop(app, tx.clone());
        } else {
            app.gate_message = Some(interrupt.into());
            app.mode = Mode::Chat;
        }
        return;
    }

    // 3-consecutive-failure gate
    if app.tool_failures >= 3 {
        log::warn!("safety gate: 3 consecutive failures, forcing text response");
        app.turn_scores.push(turn_scorer::score_current_turn(app));
        app.tool_failures = 0;
        app.tool_calls_this_turn = 0;
        app.status_msg = "3 consecutive tool failures. Forcing text response.".into();
        let interrupt = "3 consecutive tool failures occurred. Respond with your analysis — do not call more tools.";
        if app.exec_pending == 0 {
            app.ctx.annotate("System", interrupt);
            app.ctx.flush_annotations();
            session_persistence::maybe_save_session(app);
            agent_loop::handle_start_agent_loop(app, tx.clone());
        } else {
            app.gate_message = Some(interrupt.into());
            app.mode = Mode::Chat;
        }
        return;
    }

    // Tool-only turn counting
    if needs_confirm.is_empty() {
        app.consecutive_tool_turns += 1;
    } else {
        app.consecutive_tool_turns = 0;
    }

    if app.consecutive_tool_turns >= 30 {
        app.turn_scores.push(turn_scorer::score_current_turn(app));
        app.consecutive_tool_turns = 0;
        app.status_msg = "30 consecutive tool-only turns. Breaking.".into();
        app.ctx.annotate("SYSTEM", "30 consecutive tool-only turns. Tools may be broken or stuck. Stop calling tools and respond with your analysis.");
        app.ctx.flush_annotations();
        session_persistence::maybe_save_session(app);
        agent_loop::handle_start_agent_loop(app, tx);
        return;
    }
    if app.consecutive_tool_turns == 5 {
        app.ctx.annotate("REMINDER", "5 consecutive tool-only turns. Are tools working correctly? Consider explaining what you're doing in text.");
        app.ctx.flush_annotations();
    }

    // Sudo prompt gate
    if !app.sudo_pending.is_empty() {
        app.mode = Mode::SudoPrompt;
        app.status_msg = format!("Sudo password required for {} command(s). [Enter] to submit, [Esc] to cancel.", app.sudo_pending.len());
        app.stream_content.clear();
        app.stream_reasoning.clear();
        return;
    }

    // Auto-verify queue
    if needs_confirm.is_empty() && !app.auto_verify.is_empty() {
        let pending: Vec<String> = app.auto_verify.drain(..).collect();
        for cmd in pending {
            let r = execute_tool("exec", &format!("{{\"command\":\"{}\"}}", cmd));
            log::info!("auto-verify: {} → {}", cmd, r.lines().next().unwrap_or("?"));
        }
    }

    // Agent loop continuation or confirmation prompt
    if needs_confirm.is_empty() {
        app.stream_content.clear();
        app.stream_reasoning.clear();
        agent_loop::handle_start_agent_loop(app, tx);
    } else {
        app.pending_tools = needs_confirm;
        let has_danger = app.pending_tools.iter().any(|(_, s, _)| *s == SafetyLevel::Danger);
        app.mode = if has_danger { Mode::DangerPrompt } else { Mode::ToolConfirm };
        app.status_msg = app.tt().tool_confirm_prompt.to_string();
    }
}
