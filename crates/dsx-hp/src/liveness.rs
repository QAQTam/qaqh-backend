//! Liveness — agent heartbeat and crash detection.
//!
//! Watches agent activity, detects hangs and crashes.
//! Stub for a future healthd daemon.

use std::time::Instant;

/// Liveness state for an agent.
#[derive(Debug, Clone)]
pub struct LivenessState {
    /// When the agent last reported activity.
    pub last_activity: Instant,
    /// Expected interval between heartbeats in seconds.
    pub heartbeat_interval_secs: u64,
    /// Consecutive missed heartbeats.
    pub missed_heartbeats: u32,
}

impl Default for LivenessState {
    fn default() -> Self {
        Self {
            last_activity: Instant::now(),
            heartbeat_interval_secs: 30,
            missed_heartbeats: 0,
        }
    }
}

impl LivenessState {
    /// Create a new liveness state with a custom heartbeat interval.
    pub fn new(heartbeat_interval_secs: u64) -> Self {
        Self {
            last_activity: Instant::now(),
            heartbeat_interval_secs,
            missed_heartbeats: 0,
        }
    }
}

/// Result of a liveness check.
#[derive(Debug, Clone, PartialEq)]
pub enum LivenessResult {
    /// Agent is active and responsive.
    Alive,
    /// Agent has missed some heartbeats but may recover.
    Unresponsive {
        /// Seconds since last activity.
        since_secs: u64,
    },
    /// Agent is presumed dead.
    Dead {
        reason: String,
    },
}

/// Check whether the agent is still alive based on its last activity.
///
/// Returns:
/// - `Alive` if activity is within the heartbeat interval.
/// - `Unresponsive` if within 3x the interval.
/// - `Dead` if beyond 3x the interval.
pub fn check_liveness(state: &LivenessState) -> LivenessResult {
    let elapsed = state.last_activity.elapsed().as_secs();

    if elapsed < state.heartbeat_interval_secs {
        LivenessResult::Alive
    } else if elapsed < state.heartbeat_interval_secs * 3 {
        LivenessResult::Unresponsive {
            since_secs: elapsed,
        }
    } else {
        LivenessResult::Dead {
            reason: format!(
                "No activity for {}s (threshold: {}s)",
                elapsed,
                state.heartbeat_interval_secs * 3
            ),
        }
    }
}

/// Record a heartbeat — resets the activity timer and missed count.
pub fn heartbeat(state: &mut LivenessState) {
    state.last_activity = Instant::now();
    state.missed_heartbeats = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initial_alive() {
        let state = LivenessState::default();
        assert_eq!(check_liveness(&state), LivenessResult::Alive);
    }

    #[test]
    fn test_heartbeat_resets() {
        let mut state = LivenessState::new(10);
        state.last_activity = Instant::now() - std::time::Duration::from_secs(5);
        assert_eq!(check_liveness(&state), LivenessResult::Alive);

        heartbeat(&mut state);
        // After heartbeat, elapsed should be near-zero
        assert_eq!(check_liveness(&state), LivenessResult::Alive);
    }

    #[test]
    fn test_unresponsive() {
        let state = LivenessState {
            last_activity: Instant::now() - std::time::Duration::from_secs(40),
            heartbeat_interval_secs: 30,
            missed_heartbeats: 0,
        };
        assert!(matches!(check_liveness(&state), LivenessResult::Unresponsive { since_secs: 40 }));
    }

    #[test]
    fn test_dead() {
        let state = LivenessState {
            last_activity: Instant::now() - std::time::Duration::from_secs(120),
            heartbeat_interval_secs: 30,
            missed_heartbeats: 0,
        };
        assert!(matches!(check_liveness(&state), LivenessResult::Dead { .. }));
    }
}
