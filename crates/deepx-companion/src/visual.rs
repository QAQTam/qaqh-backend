use deepx_proto::CompanionVisualState;

pub fn notification_for_agent_event(
    event_type: &str,
    payload: &serde_json::Value,
) -> Option<(String, String)> {
    match event_type {
        "turn_end" => Some(("info".into(), "DeepX task completed".into())),
        "error" | "ask_rejected" => {
            let message = payload
                .get("message")
                .and_then(serde_json::Value::as_str)
                .filter(|message| !message.trim().is_empty())
                .unwrap_or("DeepX task failed");
            Some(("error".into(), message.to_string()))
        }
        _ => None,
    }
}

pub fn visual_state_for_agent_event(
    event_type: &str,
    payload: &serde_json::Value,
) -> Option<CompanionVisualState> {
    match event_type {
        "ready" | "done" | "cancelled" => Some(CompanionVisualState::Idle),
        "turn_start" => Some(CompanionVisualState::Thinking),
        "round_delta" => match payload.get("kind").and_then(serde_json::Value::as_str) {
            Some("tool") | Some("tool_calling") => Some(CompanionVisualState::Working),
            Some("thinking") | Some("answering") => Some(CompanionVisualState::Thinking),
            _ => None,
        },
        "round_complete" | "tool_results" | "tool_exec_delta" | "exec_progress"
        | "tool_call_preview" | "code_delta" | "ask_resolved" | "plan_resolved" => {
            Some(CompanionVisualState::Working)
        }
        "compact_start" | "compact_delta" => Some(CompanionVisualState::Sweeping),
        "compact_end" => Some(CompanionVisualState::Working),
        "permission_request" | "ask_user" | "plan_submitted" => {
            Some(CompanionVisualState::WaitingUser)
        }
        "turn_end" => Some(CompanionVisualState::Completed),
        "error" | "ask_rejected" => Some(CompanionVisualState::Error),
        "shutdown_ack" => Some(CompanionVisualState::Disconnected),
        _ => None,
    }
}

pub fn next_visual_state_for_agent_event(
    event_type: &str,
    payload: &serde_json::Value,
    previous: Option<CompanionVisualState>,
    default: CompanionVisualState,
) -> CompanionVisualState {
    visual_state_for_agent_event(event_type, payload)
        .or(previous)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use deepx_proto::CompanionVisualState;

    use super::{
        next_visual_state_for_agent_event, notification_for_agent_event,
        visual_state_for_agent_event,
    };

    #[test]
    fn maps_agent_events_to_recoverable_pet_visual_states() {
        let cases = [
            ("ready", serde_json::json!({}), CompanionVisualState::Idle),
            (
                "turn_start",
                serde_json::json!({}),
                CompanionVisualState::Thinking,
            ),
            (
                "round_delta",
                serde_json::json!({"kind":"thinking"}),
                CompanionVisualState::Thinking,
            ),
            (
                "round_delta",
                serde_json::json!({"kind":"tool"}),
                CompanionVisualState::Working,
            ),
            (
                "tool_results",
                serde_json::json!({}),
                CompanionVisualState::Working,
            ),
            (
                "compact_start",
                serde_json::json!({}),
                CompanionVisualState::Sweeping,
            ),
            (
                "permission_request",
                serde_json::json!({}),
                CompanionVisualState::WaitingUser,
            ),
            (
                "ask_user",
                serde_json::json!({}),
                CompanionVisualState::WaitingUser,
            ),
            (
                "plan_submitted",
                serde_json::json!({}),
                CompanionVisualState::WaitingUser,
            ),
            (
                "turn_end",
                serde_json::json!({}),
                CompanionVisualState::Completed,
            ),
            ("error", serde_json::json!({}), CompanionVisualState::Error),
            (
                "shutdown_ack",
                serde_json::json!({}),
                CompanionVisualState::Disconnected,
            ),
        ];
        for (event_type, payload, expected) in cases {
            assert_eq!(
                visual_state_for_agent_event(event_type, &payload),
                Some(expected)
            );
        }
    }

    #[test]
    fn ignores_events_that_do_not_change_pet_visual_state() {
        assert_eq!(
            visual_state_for_agent_event("dashboard", &serde_json::json!({})),
            None
        );
    }

    #[test]
    fn non_visual_events_preserve_completed_and_error_states() {
        for previous in [CompanionVisualState::Completed, CompanionVisualState::Error] {
            assert_eq!(
                next_visual_state_for_agent_event(
                    "dashboard",
                    &serde_json::json!({}),
                    Some(previous),
                    CompanionVisualState::Idle,
                ),
                previous
            );
        }
    }

    #[test]
    fn creates_user_notifications_only_for_completion_and_errors() {
        assert_eq!(
            notification_for_agent_event("turn_end", &serde_json::json!({})),
            Some(("info".into(), "DeepX task completed".into()))
        );
        assert_eq!(
            notification_for_agent_event(
                "error",
                &serde_json::json!({"message":"Agent disconnected"})
            ),
            Some(("error".into(), "Agent disconnected".into()))
        );
        assert_eq!(
            notification_for_agent_event("round_delta", &serde_json::json!({})),
            None
        );
    }
}
