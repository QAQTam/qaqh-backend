use serde::{Deserialize, Serialize};

use crate::{AskAnswer, AskMode, AskQuestion, SessionActivityState};

pub const COMPANION_PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompanionClientMessage {
    ClientHello {
        protocol_version: u16,
        client_version: String,
        #[serde(default)]
        capabilities: Vec<String>,
    },
    InteractionResponse {
        command_id: String,
        key: CompanionInteractionKey,
        response: CompanionInteractionResponse,
    },
    FocusSession {
        command_id: String,
        seed: String,
    },
    Heartbeat {
        nonce: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompanionServerMessage {
    ServerHello {
        protocol_version: u16,
        server_epoch: String,
        heartbeat_interval_ms: u64,
        max_frame_bytes: usize,
    },
    Snapshot {
        server_epoch: String,
        seq: u64,
        snapshot: CompanionSnapshot,
    },
    Event {
        server_epoch: String,
        seq: u64,
        event: CompanionEvent,
    },
    CommandResult {
        server_epoch: String,
        seq: u64,
        command_id: String,
        status: CompanionCommandStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Heartbeat {
        server_epoch: String,
        seq: u64,
        nonce: u64,
    },
    Shutdown {
        server_epoch: String,
        seq: u64,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompanionSnapshot {
    pub snapshot_seq: u64,
    pub sessions: Vec<CompanionSession>,
    pub pending_interactions: Vec<CompanionInteraction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompanionSession {
    pub seed: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub state: SessionActivityState,
    pub visual_state: CompanionVisualState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub session_seq: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompanionVisualState {
    Starting,
    Idle,
    Thinking,
    Working,
    Sweeping,
    WaitingUser,
    Completed,
    Error,
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompanionInteractionKind {
    Permission,
    AskUser,
    PlanReview,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CompanionInteractionKey {
    pub seed: String,
    pub generation: u64,
    pub kind: CompanionInteractionKind,
    pub request_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompanionInteraction {
    pub key: CompanionInteractionKey,
    pub payload: CompanionInteractionPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompanionInteractionPayload {
    Permission {
        tool_name: String,
        reason: String,
        paths: Vec<String>,
        category: String,
        level: u8,
        risk: String,
        consequence: String,
    },
    AskUser {
        turn_id: String,
        round_num: u32,
        mode: AskMode,
        questions: Vec<AskQuestion>,
    },
    PlanReview {
        plan_content: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompanionInteractionResponse {
    Permission {
        approved: bool,
        #[serde(default)]
        trust_folder: bool,
    },
    AskUser {
        #[serde(default)]
        answers: Vec<AskAnswer>,
        #[serde(default)]
        dismissed: bool,
    },
    PlanReview {
        approved: bool,
        #[serde(default)]
        message: String,
        #[serde(default)]
        autonomous: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompanionEvent {
    SessionActivity {
        session: CompanionSession,
    },
    InteractionRequested {
        interaction: CompanionInteraction,
    },
    InteractionResolved {
        key: CompanionInteractionKey,
        resolution: String,
    },
    Notification {
        seed: Option<String>,
        level: String,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompanionCommandStatus {
    Accepted,
    AlreadyResolved,
    StaleGeneration,
    Rejected,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AskAnswer, AskMode, AskQuestion, SessionActivityState};

    fn interaction_key(kind: CompanionInteractionKind) -> CompanionInteractionKey {
        CompanionInteractionKey {
            seed: "deadbeef".into(),
            generation: 7,
            kind,
            request_id: "request-1".into(),
        }
    }

    #[test]
    fn client_hello_round_trip_uses_versioned_tagged_shape() {
        let message = CompanionClientMessage::ClientHello {
            protocol_version: COMPANION_PROTOCOL_VERSION,
            client_version: "0.1.0".into(),
            capabilities: vec!["interactions".into()],
        };
        let json = serde_json::to_value(&message).expect("serialize client hello");
        assert_eq!(json["type"], "client_hello");
        assert_eq!(json["protocol_version"], 1);
        assert_eq!(
            serde_json::from_value::<CompanionClientMessage>(json).expect("deserialize"),
            message
        );
    }

    #[test]
    fn snapshot_preserves_session_sequence_and_pending_interactions() {
        let snapshot = CompanionSnapshot {
            snapshot_seq: 12,
            sessions: vec![CompanionSession {
                seed: "deadbeef".into(),
                title: Some("Companion work".into()),
                workspace: Some("F:\\DeepX-Fork".into()),
                state: SessionActivityState::WaitingUser,
                visual_state: CompanionVisualState::WaitingUser,
                turn_id: Some("turn-1".into()),
                session_seq: 4,
                updated_at: 123,
            }],
            pending_interactions: vec![CompanionInteraction {
                key: interaction_key(CompanionInteractionKind::Permission),
                payload: CompanionInteractionPayload::Permission {
                    tool_name: "shell_command".into(),
                    reason: "Run tests".into(),
                    paths: vec!["F:\\DeepX-Fork".into()],
                    category: "exec".into(),
                    level: 2,
                    risk: "medium".into(),
                    consequence: "Runs the selected command".into(),
                },
            }],
        };
        let message = CompanionServerMessage::Snapshot {
            server_epoch: "epoch-1".into(),
            seq: 12,
            snapshot,
        };
        let json = serde_json::to_value(&message).expect("serialize snapshot");
        assert_eq!(json["type"], "snapshot");
        assert_eq!(json["snapshot"]["sessions"][0]["session_seq"], 4);
        assert_eq!(
            json["snapshot"]["pending_interactions"][0]["payload"]["kind"],
            "permission"
        );
        assert_eq!(
            serde_json::from_value::<CompanionServerMessage>(json).expect("deserialize"),
            message
        );
    }

    #[test]
    fn permission_ask_and_plan_responses_have_stable_wire_shapes() {
        let messages = [
            CompanionClientMessage::InteractionResponse {
                command_id: "command-permission".into(),
                key: interaction_key(CompanionInteractionKind::Permission),
                response: CompanionInteractionResponse::Permission {
                    approved: true,
                    trust_folder: false,
                },
            },
            CompanionClientMessage::InteractionResponse {
                command_id: "command-ask".into(),
                key: interaction_key(CompanionInteractionKind::AskUser),
                response: CompanionInteractionResponse::AskUser {
                    answers: vec![AskAnswer {
                        question_id: "q1".into(),
                        answer: "A".into(),
                    }],
                    dismissed: false,
                },
            },
            CompanionClientMessage::InteractionResponse {
                command_id: "command-plan".into(),
                key: interaction_key(CompanionInteractionKind::PlanReview),
                response: CompanionInteractionResponse::PlanReview {
                    approved: false,
                    message: "Revise the tests".into(),
                    autonomous: false,
                },
            },
        ];

        let expected = ["permission", "ask_user", "plan_review"];
        for (message, expected_kind) in messages.into_iter().zip(expected) {
            let json = serde_json::to_value(&message).expect("serialize response");
            assert_eq!(json["type"], "interaction_response");
            assert_eq!(json["response"]["kind"], expected_kind);
            assert_eq!(
                serde_json::from_value::<CompanionClientMessage>(json).expect("deserialize"),
                message
            );
        }
    }

    #[test]
    fn ask_payload_reuses_agent_protocol_questions() {
        let interaction = CompanionInteraction {
            key: interaction_key(CompanionInteractionKind::AskUser),
            payload: CompanionInteractionPayload::AskUser {
                turn_id: "turn-1".into(),
                round_num: 2,
                mode: AskMode::Single,
                questions: vec![AskQuestion {
                    id: "q1".into(),
                    question: "Continue?".into(),
                    options: vec![],
                    allow_custom: false,
                }],
            },
        };
        let json = serde_json::to_value(&interaction).expect("serialize ask");
        assert_eq!(json["payload"]["kind"], "ask_user");
    }

    #[test]
    fn command_result_reports_idempotent_and_stale_outcomes() {
        for status in [
            CompanionCommandStatus::Accepted,
            CompanionCommandStatus::AlreadyResolved,
            CompanionCommandStatus::StaleGeneration,
            CompanionCommandStatus::Rejected,
        ] {
            let message = CompanionServerMessage::CommandResult {
                server_epoch: "epoch-1".into(),
                seq: 20,
                command_id: "command-1".into(),
                status,
                message: None,
            };
            let json = serde_json::to_string(&message).expect("serialize command result");
            assert_eq!(
                serde_json::from_str::<CompanionServerMessage>(&json).expect("deserialize"),
                message
            );
        }
    }
}
