//! Tool dispatch: confirm, reject, cancel streaming, cancel exec.
//! Delegates to App for state mutations and orchestrator modules for control flow.

use crate::api::StreamEvent;
use crate::health::monitor::{post_tool_record, pre_tool_gate};
use crate::tools::{self, execute_tool, wrap_tool_result};
use crate::tui::app::App;
use crate::tui::Mode;
use std::time::Instant;
use tokio::sync::mpsc;

pub fn handle_confirm_tools(app: &mut App, tx: mpsc::Sender<StreamEvent>) {
    let tools = std::mem::take(&mut app.pending_tools);
    let mut has_long_exec = false;
    for (tc, _, _) in &tools {
        // Long-running exec: spawn async, don't block
        if tc.function.name == "exec" {
            // Apply exec gates BEFORE spawning
            if app.explore_gate("exec", &tc.id, &tc.function.arguments) {
                continue; // blocked by gate
            }
            // Limit concurrent execs
            if app.exec_pending >= 2 {
                app.push_tool(&tc.id,
                    "[CANCELLED] Max 2 concurrent execs per turn. Wait for pending execs to complete, then retry.");
                continue;
            }
            has_long_exec = true;
            app.exec_pending += 1;
            if app.exec_started_at.is_none() {
                app.exec_started_at = Some(Instant::now());
            }
            app.exec_buffer.clear();
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&tc.function.arguments) {
                if let Some(cmd) = v.get("command").and_then(|c| c.as_str()) {
                    app.tool_code_path = cmd.to_string();
                    app.tool_code_action = "exec".into();
                    app.tool_code_content = format!("$ {}\n", cmd);
                }
            }
            tools::spawn_exec_async(&tc.id, &tc.function.arguments, tx.clone());
            continue;
        }
        match pre_tool_gate(&tc.function.name, &tc.function.arguments, &app.monitor) {
            crate::health::monitor::PreToolResult::Block { reason } => {
                app.push_tool(&tc.id, &format!("[ERROR] Blocked: {}", reason));
                continue;
            }
            _ => {}
        }
        let raw = if app.phase_check_tool(&tc.function.name, &tc.id)
            || app.explore_gate(&tc.function.name, &tc.id, &tc.function.arguments) {
            continue;
        } else {
            execute_tool(&tc.function.name, &tc.function.arguments)
        };
        let success = !raw.starts_with("[ERROR]") && !raw.starts_with("[FAIL]");
        post_tool_record(&tc.function.name, success, &mut app.monitor);
        if tc.function.name == "exec" && App::tool_action(&tc.function.arguments) == "explore" { app.has_explored = true; }
        if tc.function.name == "file" && App::tool_action(&tc.function.arguments) == "read" { app.turns_since_last_read = 0; }
        app.track_tool_code(&tc.function.name, &tc.function.arguments, &raw);
        if raw.starts_with("[SUDO_REQUIRED]") {
            let sudo_cmd = raw[16..].trim().to_string();
            app.sudo_pending.push((tc.clone(), sudo_cmd));
            continue;
        }
        let failed = raw.starts_with("[ERROR]") || raw.starts_with("[FAIL]");
        if failed {
            app.health.record_error(&tc.function.name, &raw);
            app.health.consecutive_failures = app.health.consecutive_failures.saturating_add(1);
            app.health.tool_failures_this_turn += 1;
        } else {
            app.health.consecutive_failures = 0;
            app.track_file_written(&tc.function.arguments);
        }
        let result = wrap_tool_result(&tc.function.name, &raw);
        app.tool_results.push((tc.function.name.clone(), result.clone()));
        app.push_tool(&tc.id, &result);

        // [MEMORY FROZEN] knowledge extraction disabled

        if raw.starts_with("[OK]") && (tc.function.name == "write_file" || tc.function.name == "edit_file") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&tc.function.arguments) {
                if let Some(path) = v.get("path").and_then(|p| p.as_str()) {
                    if path.ends_with(".rs") && !app.auto_verify.contains(&"cargo check".to_string()) {
                        app.auto_verify.push("cargo check".to_string());
                    }
                }
            }
        }
    }
    if !app.sudo_pending.is_empty() {
        app.mode = Mode::SudoPrompt;
        app.status_msg = "Sudo password required. [Enter] to submit.".into();
        return;
    }
    if has_long_exec {
        app.mode = Mode::Chat;
        app.status_msg = "Waiting for exec to complete...".into();
        app.stream_content.clear();
        app.stream_reasoning.clear();
        return;
    }
    app.stream_content.clear();
    app.stream_reasoning.clear();
    app.mode = Mode::Chat;
    app.status_msg = format!("{} tool(s) executed. Continuing...", tools.len());
    app.start_agent_loop(tx);
}

pub fn handle_reject_tools(app: &mut App) {
    app.pending_tools.clear();
    // Remove the last step (the unconfirmed assistant) from the Assembler
    app.ctx.remove_last_step();
    app.sync_messages();
    app.tool_results.clear();
    app.stream_content.clear();
    app.stream_reasoning.clear();
    app.mode = Mode::Chat;
    app.status_msg = "Tools rejected. Conversation rolled back.".into();
}

pub fn handle_cancel_stream(app: &mut App) {
    crate::tools::CANCEL.store(true, std::sync::atomic::Ordering::Relaxed);
    app.stream_cancelled = true;
    if app.exec_pending > 0 {
        handle_cancel_exec(app);
    }

    let had_text = !app.stream_content.is_empty();
    let had_tools = !app.stream_tool_progress.is_empty();

    if had_text || had_tools {
        // Remove the incomplete step (streaming was in progress, no assistant pushed yet)
        app.ctx.remove_last_step_if_incomplete();
        // Push partial assistant
        let mut partial = crate::types::Message::assistant_empty();
        if had_text {
            partial.content = Some(format!("{}\n\n*(interrupted by user)*", app.stream_content));
        }
        if !app.stream_reasoning.is_empty() {
            partial.reasoning_content = Some(app.stream_reasoning.clone());
        }
        let _ = app.ctx.push_assistant_unchecked(partial);
        app.ctx.annotate("health", "User interrupted the AI during output");
    } else if !app.stream_reasoning.is_empty() {
        app.ctx.annotate("health", "User interrupted the AI during thinking");
    }

    app.sync_messages();
    app.mode = Mode::Chat;
    app.stream_content.clear();
    app.stream_reasoning.clear();
    app.stream_tool_progress.clear();
    app.status_msg = "Stream cancelled.".into();
}

pub fn handle_cancel_exec(app: &mut App) {
    for &pid in &app.exec_child_pids {
        let _ = std::process::Command::new("kill").args([&pid.to_string()]).output();
    }
    // SIGKILL after a brief window
    std::thread::sleep(std::time::Duration::from_millis(100));
    for &pid in &app.exec_child_pids {
        let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).output();
    }
    app.exec_child_pids.clear();
    app.exec_pending = 0;
    app.exec_started_at = None;
    app.status_msg = "Exec cancelled by user.".into();
}
