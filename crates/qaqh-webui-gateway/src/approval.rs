//! Server-issued, one-shot browser approval challenges.
//!
//! The browser never receives the daemon's canonical interaction or tool-call
//! id. It receives an opaque challenge id plus bounded display details; the
//! gateway keeps the canonical id and seed binding server-side.

use std::time::{Duration, Instant};

use qaqh_domain::{AskAnswer, ControlCommand, ToolCommand};
use qaqh_ringing::RingingCommand;
use serde::Deserialize;
use serde_json::{Value, json};

pub const APPROVAL_TTL: Duration = Duration::from_secs(5 * 60);
pub const MAX_PENDING_APPROVALS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalKind {
    ToolPermission,
    Ask,
    Plan,
}

impl ApprovalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ToolPermission => "tool_permission",
            Self::Ask => "ask",
            Self::Plan => "plan",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApprovalChallenge {
    pub id: String,
    pub kind: ApprovalKind,
    pub source_id: String,
    pub session_id: String,
    pub details: Value,
    pub issued_at: Instant,
}

impl ApprovalChallenge {
    pub fn is_expired(&self) -> bool {
        self.issued_at.elapsed() >= APPROVAL_TTL
    }

    pub fn expires_in_secs(&self) -> u64 {
        APPROVAL_TTL
            .saturating_sub(self.issued_at.elapsed())
            .as_secs()
    }

    pub fn view(&self) -> Value {
        json!({
            "challenge_id": self.id,
            "kind": self.kind.as_str(),
            "expires_in": self.expires_in_secs(),
            "details": self.details,
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct ApprovalRequest {
    pub decision: String,
    #[serde(default)]
    pub payload: Value,
}

pub fn command_for(
    challenge: &ApprovalChallenge,
    request: &ApprovalRequest,
) -> Result<RingingCommand, &'static str> {
    match challenge.kind {
        ApprovalKind::ToolPermission => {
            let (approved, trust_folder) = match request.decision.as_str() {
                "approve" => (true, false),
                "reject" => (false, false),
                "trust" => (true, true),
                _ => return Err("invalid_decision"),
            };
            Ok(RingingCommand::Tool(ToolCommand::ToolPermissionRespond {
                tool_call_id: challenge.source_id.clone(),
                approved,
                trust_folder,
            }))
        }
        ApprovalKind::Ask => match request.decision.as_str() {
            "submit" => {
                let answers = request
                    .payload
                    .get("answers")
                    .cloned()
                    .ok_or("missing_answers")?;
                let answers: Vec<AskAnswer> =
                    serde_json::from_value(answers).map_err(|_| "invalid_answers")?;
                Ok(RingingCommand::Control(
                    ControlCommand::InteractionAskRespond {
                        interaction_id: challenge.source_id.clone(),
                        answers,
                    },
                ))
            }
            "dismiss" => Ok(RingingCommand::Control(
                ControlCommand::InteractionAskDismiss {
                    interaction_id: challenge.source_id.clone(),
                },
            )),
            _ => Err("invalid_decision"),
        },
        ApprovalKind::Plan => {
            let approved = match request.decision.as_str() {
                "approve" => true,
                "reject" => false,
                _ => return Err("invalid_decision"),
            };
            let message = request
                .payload
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let autonomous = request
                .payload
                .get("autonomous")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(RingingCommand::Control(ControlCommand::PlanReviewRespond {
                interaction_id: challenge.source_id.clone(),
                approved,
                message,
                autonomous,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn challenge(kind: ApprovalKind) -> ApprovalChallenge {
        ApprovalChallenge {
            id: "challenge".into(),
            kind,
            source_id: "canonical-id".into(),
            session_id: "0123abcd".into(),
            details: json!({}),
            issued_at: Instant::now(),
        }
    }

    fn request(decision: &str, payload: Value) -> ApprovalRequest {
        ApprovalRequest {
            decision: decision.into(),
            payload,
        }
    }

    #[test]
    fn tool_decision_maps_to_canonical_command() {
        let command = command_for(
            &challenge(ApprovalKind::ToolPermission),
            &request("trust", json!({})),
        )
        .unwrap();
        assert!(matches!(
            command,
            RingingCommand::Tool(ToolCommand::ToolPermissionRespond {
                tool_call_id,
                approved: true,
                trust_folder: true,
            }) if tool_call_id == "canonical-id"
        ));
    }

    #[test]
    fn ask_payload_is_typed_and_dismiss_is_explicit() {
        let command = command_for(
            &challenge(ApprovalKind::Ask),
            &request(
                "submit",
                json!({ "answers": [{ "question_id": "q1", "answer": "yes" }] }),
            ),
        )
        .unwrap();
        assert!(matches!(
            command,
            RingingCommand::Control(ControlCommand::InteractionAskRespond {
                interaction_id,
                answers,
            }) if interaction_id == "canonical-id"
                && answers == vec![AskAnswer {
                    question_id: "q1".into(),
                    answer: "yes".into(),
                }]
        ));

        assert!(matches!(
            command_for(
                &challenge(ApprovalKind::Ask),
                &request("dismiss", json!({}))
            )
            .unwrap(),
            RingingCommand::Control(ControlCommand::InteractionAskDismiss { interaction_id })
                if interaction_id == "canonical-id"
        ));
    }

    #[test]
    fn plan_payload_is_limited_to_known_fields() {
        let command = command_for(
            &challenge(ApprovalKind::Plan),
            &request(
                "approve",
                json!({ "message": "ok", "autonomous": true, "ignored": "x" }),
            ),
        )
        .unwrap();
        assert!(matches!(
            command,
            RingingCommand::Control(ControlCommand::PlanReviewRespond {
                interaction_id,
                approved: true,
                message: Some(message),
                autonomous: true,
            }) if interaction_id == "canonical-id" && message == "ok"
        ));
    }

    #[test]
    fn unknown_decisions_are_rejected() {
        for kind in [
            ApprovalKind::ToolPermission,
            ApprovalKind::Ask,
            ApprovalKind::Plan,
        ] {
            assert_eq!(
                command_for(&challenge(kind), &request("auto_approve", json!({}))).unwrap_err(),
                "invalid_decision"
            );
        }
    }
}
