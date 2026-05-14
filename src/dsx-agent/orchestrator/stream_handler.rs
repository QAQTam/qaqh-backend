//! Stream event handler: processes real-time streaming events from the API.
//! finalize_stream_response lives in response_processor.rs.
//!
//! There are two entry points:
//!   - handle_stream_event: Agent-side handler operating on AgentState
//!   - handle_stream_event_legacy: TUI-side display handler (legacy compat)

use crate::api::StreamEvent;
use crate::types::{AgentPhase, UsageInfo};
use tokio::sync::mpsc;

/// Agent-side stream handler — processes events for health/phase tracking.
/// Tool call finalization is done by response_processor::finalize_stream_response.
pub fn handle_stream_event(
    state: &mut crate::agent::AgentState,
    event: StreamEvent,
    tx: mpsc::Sender<StreamEvent>,
) {
    match event {
        StreamEvent::ContentDelta(_text) => {
            // Content delta is handled by TUI via StreamBuffer.
            // Agent only tracks health.
        }
        StreamEvent::ReasoningDelta(ref text) => {
            state.update_phase(text);
        }
        StreamEvent::ToolCallProgress { .. } => {
            state.phase = AgentPhase::Executing;
        }
        StreamEvent::Done { raw_message, usage, ref stop_reason } => {
            if let Some(ref u) = usage {
                let total = u.prompt_cache_hit_tokens + u.prompt_cache_miss_tokens;
                if total > 0 {
                    state.cache_hit_pct = u.prompt_cache_hit_tokens as f64 / total as f64;
                }
                if let Some(ref dt) = u.completion_tokens_details {
                    state.reasoning_tokens = dt.reasoning_tokens;
                }
            }
            let _ = crate::orchestrator::response_processor::finalize_stream_response(
                state, raw_message, usage, tx);
        }
        StreamEvent::Error(msg) => {
            state.health.has_orphan_tool_uses = msg.contains("tool_use") && msg.contains("tool_result");
            state.health.record_api_error();
        }
        _ => {}
    }
}

// ── TUI-side legacy handler ──

use crate::tui::{AppState, StreamBuffer, Mode};

/// Legacy TUI-side stream event handler — updates display state only.
///
/// In the new IPC architecture, streaming events arrive via AgentFrame::StreamPayload
/// and are handled by process_agent_frame in the TUI event loop. This function is
/// a backward-compat bridge for the mpsc::channel<StreamEvent> legacy path.
///
/// Tool call processing is NOT done here — that is the Agent's responsibility
/// via the IPC stream.
pub fn handle_stream_event_legacy(
    app: &mut AppState,
    buf: &mut StreamBuffer,
    event: StreamEvent,
    _tx: mpsc::Sender<StreamEvent>,
) {
    match event {
        StreamEvent::ContentDelta(text) => {
            app.dirty = true;
            buf.content.push_str(&text);
        }
        StreamEvent::ReasoningDelta(text) => {
            app.dirty = true;
            buf.reasoning.push_str(&text);
        }
        StreamEvent::ToolCallProgress { name, args_so_far } => {
            app.dirty = true;
            if let Some(existing) = buf.tool_progress.iter_mut()
                .find(|(n, _)| n == &name)
            {
                existing.1 = args_so_far;
            } else {
                buf.tool_progress.push((name, args_so_far));
            }
        }
        StreamEvent::Done { raw_message, usage, stop_reason: _ } => {
            let has_content = raw_message.content.as_ref().map(|c| !c.is_empty()).unwrap_or(false);
            let has_tools = raw_message.tool_calls.as_ref().map(|t| !t.is_empty()).unwrap_or(false);
            let has_reasoning = raw_message.reasoning_content.as_ref().map(|r| r.len() > 50).unwrap_or(false);
            if !has_content && !has_tools && has_reasoning {
                log::warn!("EMPTY RESPONSE with reasoning_len={} — possible API truncation",
                    raw_message.reasoning_content.as_ref().map(|r| r.len()).unwrap_or(0));
            }
            // Push accumulated buffer content as a message
            if buf.is_active() {
                app.push_message(buf.to_message());
            }
            buf.clear();
            if let Some(u) = usage {
                app.api_usage = Some(u);
            }
            app.dirty = true;
        }
        StreamEvent::Error(msg) => {
            app.status_msg = format!("Error: {}", msg);
            app.mode = Mode::Chat;
            buf.content.clear();
            buf.reasoning.clear();
            app.dirty = true;
        }
        StreamEvent::BalanceResult(msg) => {
            app.balance = Some(msg.clone());
            if app.mode == Mode::ApiConfig {
                if msg == "ok" {
                    app.status_msg = "Ready. Start coding!".into();
                } else if msg == "fail_auth" {
                    app.status_msg = "Key rejected. Check your API key.".into();
                } else if msg == "netfail" {
                    app.status_msg = "Network unreachable. Check internet connection.".into();
                } else if msg.starts_with("fail_") {
                    let detail = &msg["fail_".len()..];
                    app.status_msg = format!("API error: {} ", detail);
                }
            }
            app.dirty = true;
        }
        StreamEvent::ExecProgress(chunk) => {
            app.tool_code_action = "exec".into();
            if !chunk.is_empty() {
                app.tool_code_content = chunk.to_string();
            }
            app.dirty = true;
        }
        StreamEvent::ModelListResult(_) => {}
        StreamEvent::SudoDone(_) => {}
        StreamEvent::ExecDone(_, _) => {}
        StreamEvent::ExecStarted(_, _) => {}
    }
}
