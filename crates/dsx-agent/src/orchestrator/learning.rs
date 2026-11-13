//! Self-learning capture: health assessment from completed turns.
//! (Memory extraction removed — will be redesigned later)

use crate::agent::AgentState;
use dsx_types::Message;
use crate::health::HealthLevel;

// ── Post-turn processing ──

pub fn auto_extract_memory(state: &mut AgentState, _final_msg: &Message) {
    // 1. Drain tool_results and track errors
    let mut all_tools: Vec<String> = Vec::new();
    let mut had_errors = false;
    for (tool_name, result) in state.tool_results.drain(..) {
        all_tools.push(tool_name.clone());
        if result.starts_with("[ERROR]") || result.starts_with("[FAIL]") {
            state.health.record_error(&tool_name, &result);
            had_errors = true;
        }
    }

    // 1b. Update context health stats + sync state-level counters
    state.health.context_tokens = state.tokens_used();
    state.health.context_limit = state.config.context_limit;
    if had_errors {
        state.health.consecutive_tool_only_turns = state.consecutive_tool_turns;
    } else {
        state.health.consecutive_tool_only_turns = 0;
    }

    // Idle detection: if in coding mode with no tools, increment idle counter
    if all_tools.is_empty() && state.current_task_phase == dsx_types::TaskPhase::Coding {
        state.health.idle_chat_turns = state.health.idle_chat_turns.saturating_add(1);
    } else {
        state.health.idle_chat_turns = 0;
    }

    // Explore-before-read: track turns since last read, warn at 3+
    state.turns_since_last_read = state.turns_since_last_read.saturating_add(1);
    if state.turns_since_last_read == 3 && state.has_explored {
        let note = format!(
            "{} turns without reading files. Context may be missing. Consider read_file().",
            state.turns_since_last_read);
        state.pending_notes.push(note);
    }

    // 1c. Record turn + assess health
    state.health.record_turn(had_errors);
    let assessment = state.health.assess();
    let report = state.health.render_health();
    crate::health::update_health_report(report);

    // Health status — store for TUI display, inject as system note for AI awareness
    let level_tag = if assessment.level == HealthLevel::Red { "RED" }
        else if assessment.level == HealthLevel::Yellow { "YLW" }
        else { "OK" };
    let turn = state.health.turn;
    state.health_status_line = format!(
        "[{} {} {} | tier:{:?} | {}% | t{}]",
        level_tag,
        assessment.emotion.emoji(),
        assessment.emotion.label(),
        state.health.context_tier,
        (assessment.success_rate * 100.0) as u32,
        turn,
    );
    state.turn_annotations.push(format!("[HEALTH] {} {} {}", level_tag, assessment.emotion.emoji(), assessment.emotion.label()));
    state.flush_notes();

    if let Some(interrupt) = assessment.interrupt {
        state.turn_annotations.push(format!("[HEALTH] {}", interrupt));
        state.flush_notes();
    }

    // (Memory extraction removed — will be redesigned later)
    // (Pitfall extraction removed — will be redesigned later)
    // (Learning capture removed — will be redesigned later)
}
