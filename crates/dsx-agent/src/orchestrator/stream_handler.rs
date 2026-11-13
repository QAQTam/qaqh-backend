//! Stream event handler: processes real-time streaming events from the API.
//! finalize_stream_response lives in response_processor.rs.
//!
//! Entry point: handle_stream_event — Agent-side handler operating on AgentState.

use crate::api::StreamEvent;
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
        StreamEvent::Done { raw_message, usage, stop_reason: _ } => {
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
        StreamEvent::ExecDone(id, result) => {
            state.decrement_exec_pending();
            // Push result to context to prevent orphan tool_use.
            // Use push_tool_result_for to search all turns, since async exec
            // results may arrive after the context has advanced (though
            // exec_pending gates prevent new API requests).
            let wrapped = crate::tools::wrap_tool_result("exec", &result);
            match state.ctx.push_tool_result(&id, &wrapped) {
                Ok(()) => {}
                Err(_) => {
                    // If current step doesn't have this tool_call_id (e.g. turn
                    // advanced due to edge case), search all turns.
                    let _ = state.ctx.push_tool_result_for(&id, &wrapped);
                }
            }
            if state.exec_pending == 0 {
                state.stream_content.clear();
                state.stream_reasoning.clear();
                crate::orchestrator::agent_loop::handle_start_agent_loop(state, tx);
                return;
            }
        }
        StreamEvent::ExecStarted(_id, pid) => {
            if !state.exec_child_pids.contains(&pid) {
                state.exec_child_pids.push(pid);
            }
        }
        _ => {}
    }
}
