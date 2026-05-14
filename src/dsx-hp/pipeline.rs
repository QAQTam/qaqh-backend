//! Health judgment pipeline — `judge()` priority chain.
//!
//! The pipeline combines signals from all HP subsystems into a single
//! `Vec<Verdict>`. Priority order (highest first):
//!
//! 1. **Liveness** — dead processes are reported immediately; short-circuits.
//! 2. **Circuit Breaker** — disabled tools, tool loops, failure thresholds.
//! 3. **Monitor** — per-turn behavior warnings.
//! 4. **Sentinel** — API error rate warnings.
//! 5. **Healthy** — nothing wrong → `Verdict::Healthy`.
//!
//! This is the **only** place that aggregates across subsystems. Each subsystem
//! remains independently testable.

use super::circuit_breaker::{check_failures, check_loop};
use super::emotion::AgentEmotion;
use super::liveness::LivenessResult;
use super::monitor::MonitorState;
use super::registry::ProcessRegistry;
use super::types::{AlertSeverity, AlertSource, HealthLevel, Verdict};

/// Run the full health judgment pipeline.
///
/// Reads from the process registry and monitor state (read-only), returns
/// zero or more `Verdict`s sorted by priority.
pub fn judge(registry: &ProcessRegistry, monitor: &MonitorState) -> Vec<Verdict> {
    let mut verdicts: Vec<Verdict> = Vec::new();

    // ── Stage 1: Liveness (highest priority) ──
    //
    // Dead processes short-circuit the pipeline: if any process is dead,
    // return immediately without checking lower-priority stages.
    let mut has_dead = false;
    for (pid, result) in registry.check_all() {
        if let LivenessResult::Dead { reason } = &result {
            has_dead = true;
            if let Some(reg) = registry.query(pid) {
                let since = reg.liveness.last_activity.elapsed().as_secs();
                verdicts.push(Verdict::Dead {
                    pid,
                    name: reg.name.clone(),
                    reason: reason.clone(),
                    since_secs: since,
                });
            }
        }
    }
    if has_dead {
        return verdicts;
    }

    // ── Stage 2: Circuit Breaker ──
    //
    // Tool loops and failure thresholds. These indicate systemic problems
    // (stuck agent, broken tool) and generate Alert verdicts.
    match check_loop(monitor) {
        super::monitor::MonitorResult::Escalate { reason, advice: _ } => {
            verdicts.push(Verdict::Alert {
                severity: AlertSeverity::Critical,
                source: AlertSource::CircuitBreaker,
                message: reason,
            });
        }
        super::monitor::MonitorResult::Warn { reason } => {
            verdicts.push(Verdict::Alert {
                severity: AlertSeverity::Warning,
                source: AlertSource::CircuitBreaker,
                message: reason,
            });
        }
        super::monitor::MonitorResult::Pass => {}
    }

    match check_failures(monitor) {
        super::monitor::MonitorResult::Escalate { reason, advice: _ } => {
            verdicts.push(Verdict::Alert {
                severity: AlertSeverity::Critical,
                source: AlertSource::CircuitBreaker,
                message: reason,
            });
        }
        super::monitor::MonitorResult::Warn { reason } => {
            verdicts.push(Verdict::Alert {
                severity: AlertSeverity::Warning,
                source: AlertSource::CircuitBreaker,
                message: reason,
            });
        }
        super::monitor::MonitorResult::Pass => {}
    }

    // ── Stage 3: Monitor (degradation signals) ──
    //
    // Non-critical warnings from the monitor: approaching thresholds,
    // content quality issues, etc.
    // (Currently a passthrough — monitor's pre_tool_gate is checked
    //  per-call by the Agent, not from the pipeline.)

    // ── Stage 4: Sentinel (API errors — lowest priority) ──
    //
    // Reserved for HP-integrated API error tracking.
    // (Not yet wired — sentinel is a dependency placeholder.)

    // ── Aggregate: if nothing is wrong, report healthy ──
    if verdicts.is_empty() {
        let emotion = infer_emotion(monitor);
        let level = compute_level(registry, monitor);
        verdicts.push(Verdict::Healthy {
            level,
            emotion: Some(emotion),
        });
    }

    verdicts
}

/// Infer the agent's emotional state from monitor signals.
///
/// Lightweight version of the original `infer_emotion()` on
/// `DsAgentsHealthPlatform`. Uses only `MonitorState` fields
/// (consecutive_failures, tool_loop_count, etc.) rather than
/// the full 35-field god object.
fn infer_emotion(monitor: &MonitorState) -> AgentEmotion {
    if monitor.consecutive_failures >= 5 || monitor.tool_loop_count >= 4 {
        return AgentEmotion::Panic;
    }
    if monitor.consecutive_failures >= 3 || monitor.tool_loop_count >= 3 {
        return AgentEmotion::Frustrated;
    }
    if monitor.consecutive_failures >= 2 && monitor.tool_calls_this_turn > 4 {
        return AgentEmotion::Confused;
    }
    if monitor.tool_failures_this_turn > 0 || monitor.consecutive_tool_turns >= 5 {
        return AgentEmotion::Anxious;
    }
    AgentEmotion::Calm
}

/// Compute aggregate health level from registry and monitor state.
fn compute_level(registry: &ProcessRegistry, monitor: &MonitorState) -> HealthLevel {
    // Red: processes missing or critical monitor thresholds
    if registry.len() == 0 {
        return HealthLevel::Red;
    }

    let has_orphans = registry
        .check_all()
        .iter()
        .any(|(_, r)| matches!(r, LivenessResult::Dead { .. }));

    if has_orphans || monitor.consecutive_failures >= 5 || monitor.tool_loop_count >= 4 {
        return HealthLevel::Red;
    }

    // Yellow: degradation signals
    if monitor.consecutive_failures >= 2
        || monitor.tool_loop_count > 0
        || monitor.consecutive_tool_turns >= 5
    {
        return HealthLevel::Yellow;
    }

    HealthLevel::Green
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsx_hp::monitor::MonitorState;
    use crate::dsx_hp::registry::ProcessRegistry;
    use crate::dsx_hp::types::ProcessKind;

    fn empty_registry() -> ProcessRegistry {
        ProcessRegistry::new(30)
    }

    #[test]
    fn test_judge_healthy_with_empty_registry() {
        let registry = empty_registry();
        let monitor = MonitorState::new();
        let verdicts = judge(&registry, &monitor);

        // Empty registry is Degraded (Red), not Healthy
        assert!(!verdicts.is_empty());
        assert!(matches!(verdicts[0], Verdict::Healthy { .. }));
    }

    #[test]
    fn test_judge_healthy_with_registered_processes() {
        let mut registry = empty_registry();
        registry.register(ProcessKind::Agent, "test", 42).unwrap();
        let monitor = MonitorState::new();
        let verdicts = judge(&registry, &monitor);

        assert_eq!(verdicts.len(), 1);
        assert!(matches!(verdicts[0], Verdict::Healthy { .. }));
    }

    #[test]
    fn test_judge_alerts_on_circuit_breaker() {
        let registry = empty_registry();
        let mut monitor = MonitorState::new();
        monitor.consecutive_failures = 3;

        let verdicts = judge(&registry, &monitor);
        assert!(verdicts.iter().any(|v| matches!(v, Verdict::Alert { source: AlertSource::CircuitBreaker, .. })));
    }

    #[test]
    fn test_infer_emotion_panic() {
        let mut monitor = MonitorState::new();
        monitor.consecutive_failures = 5;
        assert_eq!(infer_emotion(&monitor), AgentEmotion::Panic);
    }

    #[test]
    fn test_infer_emotion_calm() {
        let monitor = MonitorState::new();
        assert_eq!(infer_emotion(&monitor), AgentEmotion::Calm);
    }
}
