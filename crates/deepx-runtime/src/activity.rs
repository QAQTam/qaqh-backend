use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use deepx_proto::{SessionActivity, SessionActivityState};

#[derive(Clone, Default)]
pub struct SessionActivityTracker {
    inner: Arc<Mutex<HashMap<String, TrackedActivity>>>,
}

struct TrackedActivity {
    generation: u64,
    activity: SessionActivity,
}

impl SessionActivityTracker {
    pub fn begin(&self, seed: &str) -> (u64, SessionActivity) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let previous = inner.get(seed);
        let generation = previous.map_or(1, |value| value.generation.saturating_add(1));
        let seq = previous.map_or(1, |value| value.activity.seq.saturating_add(1));
        let activity = SessionActivity {
            seed: seed.to_string(),
            state: SessionActivityState::Starting,
            turn_id: None,
            seq,
            updated_at: now_millis(),
        };
        inner.insert(
            seed.to_string(),
            TrackedActivity {
                generation,
                activity: activity.clone(),
            },
        );
        (generation, activity)
    }

    pub fn observe(
        &self,
        seed: &str,
        generation: u64,
        event: &serde_json::Value,
    ) -> Option<SessionActivity> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let tracked = inner.get_mut(seed)?;
        if tracked.generation != generation {
            return None;
        }
        let event_type = event.get("type")?.as_str()?;
        let current_turn = tracked.activity.turn_id.clone();
        let (state, turn_id) = match event_type {
            "ready" | "done" | "turn_end" | "cancelled" => (SessionActivityState::Idle, None),
            "shutdown_ack" => (SessionActivityState::Disconnected, None),
            "turn_start" => (
                SessionActivityState::Working,
                event
                    .get("turn_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            ),
            "permission_request" | "ask_user" | "plan_submitted" => {
                (SessionActivityState::WaitingUser, current_turn)
            }
            "ask_resolved" | "plan_resolved" | "round_delta" | "round_complete"
            | "tool_results" | "tool_exec_delta" | "exec_progress" | "tool_call_preview"
            | "code_delta" | "compact_start" | "compact_delta" => {
                (SessionActivityState::Working, current_turn)
            }
            "compact_end" if current_turn.is_none() => (SessionActivityState::Idle, None),
            "compact_end" => (SessionActivityState::Working, current_turn),
            _ => return None,
        };
        if tracked.activity.state == state && tracked.activity.turn_id == turn_id {
            return None;
        }
        tracked.activity.state = state;
        tracked.activity.turn_id = turn_id;
        tracked.activity.seq = tracked.activity.seq.saturating_add(1);
        tracked.activity.updated_at = now_millis();
        Some(tracked.activity.clone())
    }

    pub fn disconnect(&self, seed: &str, generation: u64) -> Option<SessionActivity> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let tracked = inner.get_mut(seed)?;
        if tracked.generation != generation
            || tracked.activity.state == SessionActivityState::Disconnected
        {
            return None;
        }
        tracked.activity.state = SessionActivityState::Disconnected;
        tracked.activity.turn_id = None;
        tracked.activity.seq = tracked.activity.seq.saturating_add(1);
        tracked.activity.updated_at = now_millis();
        Some(tracked.activity.clone())
    }

    pub fn snapshot(&self) -> Vec<SessionActivity> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut values: Vec<_> = inner.values().map(|value| value.activity.clone()).collect();
        values.sort_by(|a, b| a.seed.cmp(&b.seed));
        values
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
