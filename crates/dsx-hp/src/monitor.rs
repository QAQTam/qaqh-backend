//! NOTE: Disconnected from dsx-agent — dsx-agent uses its own stubs in health.rs.
//! These types are available for the dsx-hp binary. Wire up via IPC later.
//!
//! Tool loop detection + failure threshold monitoring.
//!
//! Watches the agent's behavior in real-time: tool loops, failure thresholds,
//! content quality, and reasoning quality. Provides escalation signals that
//! the agent loop uses to break out of stuck states.

use std::collections::HashMap;

use crate::circuit_breaker::{check_circuit_breaker, record_tool_result};

/// Per-turn and cross-turn monitoring state.
#[derive(Debug, Clone)]
pub struct MonitorState {
    /// Tool calls issued in the current turn.
    pub tool_calls_this_turn: u32,
    /// Total tool failures in the current turn.
    pub tool_failures_this_turn: u32,

    // ── Cross-turn counters ──
    /// Consecutive tool failures across turns (resets on success).
    pub consecutive_failures: u32,
    /// Consecutive turns where the model only called tools (no text content).
    pub consecutive_tool_turns: u32,

    // ── Per-tool circuit breaker ──
    /// Consecutive failures per tool name.
    pub tool_fail_counts: HashMap<String, u32>,
    /// Tools temporarily disabled by circuit breaker.
    pub disabled_tools: Vec<String>,

    // ── Tool loop detection ──
    /// Rolling trail of (tool_name, args_hash) for identical-call detection.
    pub tool_trail: Vec<(String, String)>,
    /// How many times an identical tool call has been detected this session.
    pub tool_loop_count: u32,

    // ── Content / reasoning buffers ──
    pub reasoning_sample: Vec<String>,
    pub content_buffer: String,
}

/// Result of a pre-tool gate check.
#[derive(Debug, Clone)]
pub enum PreToolResult {
    /// Tool call is allowed — proceed.
    Pass,
    /// Tool call is blocked — do not execute.
    Block {
        reason: String,
    },
    /// Tool call is allowed but with a warning.
    Warn {
        reason: String,
    },
}

/// Result of a monitor check.
#[derive(Debug, Clone)]
pub enum MonitorResult {
    /// Everything is fine — proceed.
    Pass,
    /// Something is off — non-blocking warning.
    Warn {
        reason: String,
    },
    /// Action required — the caller should intervene (e.g., force text response).
    Escalate {
        reason: String,
        advice: String,
    },
}

impl MonitorState {
    /// Create a fresh monitor state with default capacities.
    pub fn new() -> Self {
        Self {
            tool_calls_this_turn: 0,
            tool_failures_this_turn: 0,
            consecutive_failures: 0,
            consecutive_tool_turns: 0,
            tool_fail_counts: HashMap::new(),
            disabled_tools: Vec::new(),
            tool_trail: Vec::with_capacity(12),
            tool_loop_count: 0,
            reasoning_sample: Vec::with_capacity(12),
            content_buffer: String::with_capacity(1200),
        }
    }
}

impl Default for MonitorState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Pre-tool gate ──

/// Pre-execution gate check for tool calls.
///
/// Examines circuit breaker, tool loop state, and failure thresholds
/// before a tool is allowed to execute.
///
/// Returns:
/// - `Pass` — all checks clear, tool may proceed.
/// - `Block` — circuit breaker open, loop detected, or too many failures.
/// - `Warn` — approaching a threshold (for informative feedback).
pub fn pre_tool_gate(tool_name: &str, args: &str, monitor: &MonitorState) -> PreToolResult {
    // 1. Circuit breaker: is this tool disabled?
    if let Some(reason) = check_circuit_breaker(monitor, tool_name) {
        return PreToolResult::Block { reason };
    }

    // 2. Tool loop: same tool + args 3+ times already → 4th call would loop
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    args.hash(&mut h);
    let args_hash = format!("{:x}", h.finish());

    if monitor.tool_trail.len() >= 3 {
        let start = monitor.tool_trail.len() - 3;
        let recent = &monitor.tool_trail[start..];
        let all_same = recent.iter().all(|(n, a)| n == tool_name && a == &args_hash);
        if all_same {
            return PreToolResult::Block {
                reason: format!(
                    "Tool loop: '{}' called 4+ times with identical args.",
                    tool_name
                ),
            };
        }
    }

    // 3. Failure threshold: too many failures this turn
    if monitor.tool_failures_this_turn >= 3 {
        return PreToolResult::Block {
            reason: format!(
                "{} tool failures this turn. Blocking further tool calls.",
                monitor.tool_failures_this_turn
            ),
        };
    }

    PreToolResult::Pass
}

// ── Post-tool recording ──

/// Post-execution recording for tool calls.
///
/// Updates per-turn and cross-turn counters, failure tracking,
/// circuit breaker state, and delegates to `record_tool_result`
/// for per-tool fail count management.
pub fn post_tool_record(tool_name: &str, success: bool, monitor: &mut MonitorState) {
    monitor.tool_calls_this_turn += 1;

    if success {
        monitor.consecutive_failures = 0;
    } else {
        monitor.consecutive_failures += 1;
    }

    // Delegates to record_tool_result which handles:
    // - tool_failures_this_turn increment (on failure)
    // - per-tool fail counts
    // - circuit breaker state (disabling tools at threshold)
    record_tool_result(monitor, tool_name, success);
}

/// Record a tool call invocation. Increments the per-turn counter.
pub fn record_tool_call(state: &mut MonitorState, tool_name: &str) {
    state.tool_calls_this_turn += 1;
    // Track tool loop detection via identical-args check is done separately
    // via track_tool_call(). This function only bumps the counter.
    let _ = tool_name;
}

/// Reset all per-turn counters.
///
/// Call at the start of each new turn (before the API request).
///
/// Ported from `DsAgentsHealthPlatform::reset_turn`.
pub fn reset_turn(state: &mut MonitorState) {
    state.reasoning_sample.clear();
    state.content_buffer.clear();
    state.tool_calls_this_turn = 0;
    state.tool_failures_this_turn = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── PreToolResult tests ──

    #[test]
    fn test_pre_tool_gate_passes_for_clean_state() {
        let state = MonitorState::new();
        assert!(matches!(
            pre_tool_gate("read_file", r#"{"path":"x"}"#, &state),
            PreToolResult::Pass
        ));
    }

    #[test]
    fn test_pre_tool_gate_blocks_disabled_tool() {
        let mut state = MonitorState::new();
        state.disabled_tools.push("exec".into());
        state.tool_fail_counts.insert("exec".into(), 5);
        let result = pre_tool_gate("exec", "ls", &state);
        assert!(matches!(result, PreToolResult::Block { .. }));
        if let PreToolResult::Block { reason } = &result {
            assert!(reason.contains("CIRCUIT BREAKER"));
        }
    }

    #[test]
    fn test_pre_tool_gate_blocks_after_3_identical_calls() {
        let mut state = MonitorState::new();
        // Simulate 3 prior identical calls already in the trail
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let args = r#"{"path":"src/main.rs"}"#;
        args.hash(&mut h);
        let hash = format!("{:x}", h.finish());
        for _ in 0..3 {
            state.tool_trail.push(("read_file".into(), hash.clone()));
        }

        let result = pre_tool_gate("read_file", args, &state);
        assert!(matches!(result, PreToolResult::Block { .. }));
        if let PreToolResult::Block { reason } = &result {
            assert!(reason.contains("Tool loop"));
        }
    }

    #[test]
    fn test_pre_tool_gate_blocks_at_3_failures_this_turn() {
        let mut state = MonitorState::new();
        state.tool_failures_this_turn = 3;
        let result = pre_tool_gate("read_file", r#"{"path":"x"}"#, &state);
        assert!(matches!(result, PreToolResult::Block { .. }));
    }

    #[test]
    fn test_pre_tool_gate_passes_below_failure_threshold() {
        let mut state = MonitorState::new();
        state.tool_failures_this_turn = 2;
        let result = pre_tool_gate("read_file", r#"{"path":"x"}"#, &state);
        assert!(matches!(result, PreToolResult::Pass));
    }

    // ── PostToolRecord tests ──

    #[test]
    fn test_post_tool_record_increments_calls() {
        let mut state = MonitorState::new();
        post_tool_record("read_file", true, &mut state);
        assert_eq!(state.tool_calls_this_turn, 1);
    }

    #[test]
    fn test_post_tool_record_resets_consecutive_failures_on_success() {
        let mut state = MonitorState::new();
        state.consecutive_failures = 3;
        post_tool_record("read_file", true, &mut state);
        assert_eq!(state.consecutive_failures, 0);
    }

    #[test]
    fn test_post_tool_record_increments_consecutive_failures_on_failure() {
        let mut state = MonitorState::new();
        post_tool_record("exec", false, &mut state);
        assert_eq!(state.consecutive_failures, 1);
    }

    #[test]
    fn test_post_tool_record_delegates_circuit_breaker() {
        let mut state = MonitorState::new();
        // 5 consecutive failures should trip the circuit breaker
        for _ in 0..5 {
            post_tool_record("exec", false, &mut state);
        }
        assert!(state.disabled_tools.contains(&"exec".to_string()));
    }

    #[test]
    fn test_reset_turn_clears_per_turn_counters() {
        let mut state = MonitorState::new();
        state.tool_calls_this_turn = 42;
        state.tool_failures_this_turn = 3;
        state.reasoning_sample.push("test".into());
        state.content_buffer = "xyz".into();

        reset_turn(&mut state);

        assert_eq!(state.tool_calls_this_turn, 0);
        assert_eq!(state.tool_failures_this_turn, 0);
        assert!(state.reasoning_sample.is_empty());
        assert!(state.content_buffer.is_empty());
    }
}
