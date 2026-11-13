//! NOTE: Disconnected from dsx-agent — dsx-agent uses its own stubs in health.rs.
//! This module is used by the dsx-hp binary internally. Wire up via IPC later.
//!
//! Circuit breaker: tool failure thresholds and auto-intervention.

use crate::monitor::MonitorState;

// ── Tool loop constants ──

const TOOL_TRAIL_SIZE: usize = 12;
const TOOL_LOOP_THRESHOLD: u32 = 4;

// ── Failure threshold constants ──

const CIRCUIT_BREAKER_THRESHOLD: u32 = 5;

/// Check whether a specific tool is disabled by the circuit breaker.
pub fn check_circuit_breaker(state: &MonitorState, tool_name: &str) -> Option<String> {
    if state.disabled_tools.iter().any(|t| t == tool_name) {
        Some(format!(
            "[CIRCUIT BREAKER] '{}' temporarily disabled ({} consecutive failures). \
             Try a different approach or ask the user to investigate.",
            tool_name,
            state.tool_fail_counts.get(tool_name).copied().unwrap_or(0)
        ))
    } else {
        None
    }
}

// ── Tool trail tracking (identical-call loop detection) ──

/// Record a tool call in the trail and check for identical-call loops.
///
/// Returns a warning string if the same tool with the same arguments
/// has been called 4+ times consecutively.
///
/// Ported from `DsAgentsHealthPlatform::track_tool`.
pub fn track_tool_call(state: &mut MonitorState, tool_name: &str, args: &str) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    args.hash(&mut h);
    let hash = format!("{:x}", h.finish());

    state.tool_trail.push((tool_name.to_string(), hash.clone()));
    if state.tool_trail.len() > TOOL_TRAIL_SIZE {
        state.tool_trail.remove(0);
    }

    if state.tool_trail.len() >= 4 {
        let recent = &state.tool_trail[state.tool_trail.len().saturating_sub(4)..];
        let all_same = recent.iter().all(|(n, a)| n == tool_name && a == &hash);
        if all_same {
            state.tool_loop_count += 1;
            if state.tool_loop_count >= TOOL_LOOP_THRESHOLD {
                return Some(format!(
                    "Tool loop: '{}' called {}x with identical args. Breaking loop.",
                    tool_name, state.tool_loop_count
                ));
            }
            return Some(format!(
                "Tool loop: '{}' repeated with same args ({}x)",
                tool_name, state.tool_loop_count
            ));
        }
    }

    None
}

/// Record a tool result. Updates failure counters and circuit breaker state.
///
/// - `tool_name`: the tool that was executed
/// - `success`: whether the tool completed without error
///
/// On success: resets the per-tool failure count.
/// On failure: increments consecutive failures and per-tool count.
/// At 5 consecutive failures for a tool: disables it via circuit breaker.
pub fn record_tool_result(state: &mut MonitorState, tool_name: &str, success: bool) {
    if success {
        state.tool_fail_counts.remove(tool_name);
    } else {
        state.tool_failures_this_turn += 1;
        let count = state
            .tool_fail_counts
            .entry(tool_name.to_string())
            .or_insert(0);
        *count += 1;

        if *count >= CIRCUIT_BREAKER_THRESHOLD
            && !state.disabled_tools.contains(&tool_name.to_string())
        {
            state.disabled_tools.push(tool_name.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::MonitorState;

    #[test]
    fn test_record_tool_result_success_resets_fail_count() {
        let mut state = MonitorState::new();
        record_tool_result(&mut state, "read_file", false);
        assert_eq!(state.tool_failures_this_turn, 1);
        assert_eq!(*state.tool_fail_counts.get("read_file").unwrap(), 1);

        record_tool_result(&mut state, "read_file", true);
        assert!(state.tool_fail_counts.get("read_file").is_none());
    }

    #[test]
    fn test_circuit_breaker_disables_tool_at_5_failures() {
        let mut state = MonitorState::new();
        for _ in 0..5 {
            record_tool_result(&mut state, "exec", false);
        }
        assert!(state.disabled_tools.contains(&"exec".to_string()));
        assert!(check_circuit_breaker(&state, "exec").is_some());
    }

    #[test]
    fn test_track_tool_call_detects_loop() {
        let mut state = MonitorState::new();
        // 4 identical calls should trigger loop detection
        for _ in 0..4 {
            let warn = track_tool_call(&mut state, "read_file", r#"{"path":"src/main.rs"}"#);
            if let Some(msg) = warn {
                assert!(msg.contains("Tool loop"));
                return;
            }
        }
        panic!("expected tool loop warning after 4 identical calls");
    }
}
