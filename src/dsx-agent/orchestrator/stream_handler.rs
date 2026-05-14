//! Stream event handler: processes real-time streaming events from the API.
//! finalize_stream_response lives in response_processor.rs.

use crate::api::StreamEvent;
use crate::tui::app::App;
use crate::tui::utils::{char_boundary, strip_ansi};
use crate::tui::Mode;
use crate::orchestrator::response_processor;
use crate::types::{AgentPhase, UsageInfo};
use tokio::sync::mpsc;

pub fn handle_stream_event(app: &mut App, event: StreamEvent, tx: mpsc::Sender<StreamEvent>) {
    log::trace!("stream: {:?}", std::mem::discriminant(&event));
    log::trace!("stream: {:?}", std::mem::discriminant(&event));
    match event {
        StreamEvent::ContentDelta(text) => {
            app.dirty = true;
            app.stream_content.push_str(&text);
            if let Some(warn) = app.health.check_content(&text) {
                app.status_msg = format!("⚠ {}", warn);
            }
        }
        StreamEvent::ReasoningDelta(text) => {
            app.dirty = true;
            app.stream_reasoning.push_str(&text);
            if let Some(warn) = app.health.check_reasoning(&text) {
                app.status_msg = format!("⚠ {}", warn);
            }
            let r = app.stream_reasoning.clone();
            app.update_phase(&r);
        }
        StreamEvent::ToolCallProgress { name, args_so_far } => {
            app.dirty = true;
            app.phase = AgentPhase::Executing;
            if let Some(existing) = app.stream_tool_progress.iter_mut()
                .find(|(n, _)| n == &name)
            {
                existing.1 = args_so_far;
            } else {
                app.stream_tool_progress.push((name, args_so_far));
            }
        }
        StreamEvent::Done { ref raw_message, ref usage, ref stop_reason } => {
            log::info!("stream done: content={} reasoning={} tools={} stop_reason={:?}",
                raw_message.content.as_ref().map(|c| c.len()).unwrap_or(0),
                raw_message.reasoning_content.as_ref().map(|r| r.len()).unwrap_or(0),
                raw_message.tool_calls.as_ref().map(|t| t.len()).unwrap_or(0),
                stop_reason);
            let has_content = raw_message.content.as_ref().map(|c| !c.is_empty()).unwrap_or(false);
            let has_tools = raw_message.tool_calls.as_ref().map(|t| !t.is_empty()).unwrap_or(false);
            let has_reasoning = raw_message.reasoning_content.as_ref().map(|r| r.len() > 50).unwrap_or(false);
            if !has_content && !has_tools && has_reasoning {
                log::warn!("EMPTY RESPONSE with reasoning_len={} stop_reason={:?} — possible API truncation or connection drop",
                    raw_message.reasoning_content.as_ref().map(|r| r.len()).unwrap_or(0),
                    stop_reason);
            }
            let raw_message = raw_message.clone();
            let usage = usage.clone();
            app.dirty = true;
            app.auto_scroll = true;
            if let Some(ref u) = usage {
                let total = u.prompt_cache_hit_tokens + u.prompt_cache_miss_tokens;
                if total > 0 {
                    app.cache_hit_pct = u.prompt_cache_hit_tokens as f64 / total as f64;
                }
                if let Some(ref dt) = u.completion_tokens_details {
                    app.reasoning_tokens = dt.reasoning_tokens;
                }
            }
            response_processor::finalize_stream_response(app, raw_message, usage, tx);
        }
        StreamEvent::Error(msg) => {
            log::error!("stream error: {}", msg);
            app.health.has_orphan_tool_uses = msg.contains("tool_use") && msg.contains("tool_result");
            if app.health.has_orphan_tool_uses {
                app.status_msg = format!("⚠ 400: orphan tool_use — type /fix to repair");
            } else {
                app.status_msg = format!("Error: {}", msg);
            }
            app.mode = Mode::Chat;
            app.stream_content.clear();
            app.stream_reasoning.clear();
            app.health.record_api_error();
        }
        StreamEvent::BalanceResult(msg) => {
            app.balance = Some(msg.clone());
            if app.mode == Mode::ApiConfig {
                if msg == "ok" {
                    app.config.save();
                    app.mode = Mode::Chat;
                    app.status_msg = "Ready. Start coding!".into();
                } else if msg == "fail_auth" {
                    app.input.clear(); app.input_cursor = 0;
                    app.status_msg = "Key rejected. Check your API key.".into();
                } else if msg == "netfail" {
                    app.status_msg = "Network unreachable. Check internet connection.".into();
                } else if msg.starts_with("fail_") {
                    let detail = &msg["fail_".len()..];
                    app.status_msg = format!("API error: {} ", detail);
                }
            }
        }
        StreamEvent::ModelListResult(_) => {}
        StreamEvent::SudoDone(_) => {}
        StreamEvent::ExecProgress(chunk) => {
            let chunk = strip_ansi(&chunk);
            if chunk.ends_with('\r') {
                let clean = chunk.trim_end_matches('\r');
                app.exec_buffer.push_str(clean);
                if app.tool_code_action == "exec" {
                    if let Some(nl) = app.tool_code_content.rfind('\n') {
                        app.tool_code_content.truncate(nl + 1);
                    } else {
                        app.tool_code_content.clear();
                    }
                    app.tool_code_content.push_str(clean);
                }
            } else {
                let has_cr = chunk.contains('\r');
                if has_cr {
                    let last = chunk.rsplit('\r').next().unwrap_or(&chunk);
                    app.exec_buffer.push_str(last);
                    if app.tool_code_action == "exec" {
                        if let Some(nl) = app.tool_code_content.rfind('\n') {
                            app.tool_code_content.truncate(nl + 1);
                        } else {
                            app.tool_code_content.clear();
                        }
                        app.tool_code_content.push_str(last);
                    }
                } else {
                    app.exec_buffer.push_str(&chunk);
                    app.exec_buffer.push('\n');
                    if app.tool_code_action == "exec" {
                        app.tool_code_content.push_str(&chunk);
                        app.tool_code_content.push('\n');
                    }
                }
            }
            if app.exec_buffer.len() > 50000 {
                let start = app.exec_buffer.len().saturating_sub(30000);
                let cut = char_boundary(&app.exec_buffer, start);
                app.exec_buffer = app.exec_buffer[cut..].to_string();
            }
            if app.tool_code_action == "exec" && app.tool_code_content.len() > 50000 {
                let start = app.tool_code_content.len().saturating_sub(30000);
                let cut = char_boundary(&app.tool_code_content, start);
                app.tool_code_content = app.tool_code_content[cut..].to_string();
            }
        }
        StreamEvent::ExecDone(_id, _result) => {}
        StreamEvent::ExecStarted(_, _) => {}
    }
}

