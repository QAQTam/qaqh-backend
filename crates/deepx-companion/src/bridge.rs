use deepx_proto::{
    Agent2Ui, CompanionInteraction, CompanionInteractionKey, CompanionInteractionKind,
    CompanionInteractionPayload, CompanionInteractionResponse, Ui2Agent,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseFrameError {
    KindMismatch,
}

pub fn interaction_from_agent_event(
    seed: &str,
    generation: u64,
    event: &Agent2Ui,
) -> Option<CompanionInteraction> {
    let (kind, request_id, payload) = match event {
        Agent2Ui::PermissionRequest {
            tool_call_id,
            tool_name,
            reason,
            paths,
            category,
            level,
            risk,
            consequence,
        } => (
            CompanionInteractionKind::Permission,
            tool_call_id.clone(),
            CompanionInteractionPayload::Permission {
                tool_name: tool_name.clone(),
                reason: reason.clone(),
                paths: paths.clone(),
                category: category.clone(),
                level: *level,
                risk: serde_json::to_value(risk)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_else(|| "unknown".into()),
                consequence: consequence.clone(),
            },
        ),
        Agent2Ui::AskUser {
            turn_id,
            round_num,
            ask_id,
            mode,
            questions,
        } => (
            CompanionInteractionKind::AskUser,
            ask_id.clone(),
            CompanionInteractionPayload::AskUser {
                turn_id: turn_id.clone(),
                round_num: *round_num,
                mode: *mode,
                questions: questions.clone(),
            },
        ),
        Agent2Ui::PlanSubmitted {
            call_id,
            plan_content,
            ..
        } => (
            CompanionInteractionKind::PlanReview,
            call_id.clone(),
            CompanionInteractionPayload::PlanReview {
                plan_content: plan_content.clone(),
            },
        ),
        _ => return None,
    };
    Some(CompanionInteraction {
        key: CompanionInteractionKey {
            seed: seed.to_string(),
            generation,
            kind,
            request_id,
        },
        payload,
    })
}

pub fn response_to_agent_frame(
    key: &CompanionInteractionKey,
    response: CompanionInteractionResponse,
) -> Result<Ui2Agent, ResponseFrameError> {
    match (key.kind, response) {
        (
            CompanionInteractionKind::Permission,
            CompanionInteractionResponse::Permission {
                approved,
                trust_folder,
            },
        ) => Ok(Ui2Agent::PermissionResponse {
            tool_call_id: key.request_id.clone(),
            approved,
            trust_folder,
        }),
        (
            CompanionInteractionKind::AskUser,
            CompanionInteractionResponse::AskUser {
                answers,
                dismissed: false,
            },
        ) => Ok(Ui2Agent::AskResponse {
            ask_id: key.request_id.clone(),
            answers,
        }),
        (
            CompanionInteractionKind::AskUser,
            CompanionInteractionResponse::AskUser {
                dismissed: true, ..
            },
        ) => Ok(Ui2Agent::AskDismiss {
            ask_id: key.request_id.clone(),
        }),
        (
            CompanionInteractionKind::PlanReview,
            CompanionInteractionResponse::PlanReview {
                approved,
                message,
                autonomous,
            },
        ) => Ok(Ui2Agent::PlanReview {
            call_id: key.request_id.clone(),
            approved,
            message,
            autonomous,
        }),
        _ => Err(ResponseFrameError::KindMismatch),
    }
}

#[cfg(test)]
mod tests {
    use deepx_proto::{
        Agent2Ui, AskAnswer, AskMode, AskQuestion, CompanionInteractionKind,
        CompanionInteractionPayload, CompanionInteractionResponse, PermissionRisk, Ui2Agent,
    };

    use super::{ResponseFrameError, interaction_from_agent_event, response_to_agent_frame};

    #[test]
    fn converts_all_agent_interaction_requests() {
        let permission = Agent2Ui::PermissionRequest {
            tool_call_id: "tool-1".into(),
            tool_name: "shell_command".into(),
            reason: "Run tests".into(),
            paths: vec!["F:\\DeepX-Fork".into()],
            category: "exec".into(),
            level: 2,
            risk: PermissionRisk::Medium,
            consequence: "Runs a command".into(),
        };
        let ask = Agent2Ui::AskUser {
            turn_id: "turn-1".into(),
            round_num: 2,
            ask_id: "ask-1".into(),
            mode: AskMode::Single,
            questions: vec![AskQuestion {
                id: "q1".into(),
                question: "Continue?".into(),
                options: vec!["Yes".into()],
                allow_custom: true,
            }],
        };
        let plan = Agent2Ui::PlanSubmitted {
            call_id: "plan-1".into(),
            plan_content: "# Plan".into(),
        };

        let permission = interaction_from_agent_event("deadbeef", 3, &permission).unwrap();
        assert_eq!(permission.key.kind, CompanionInteractionKind::Permission);
        assert!(matches!(
            permission.payload,
            CompanionInteractionPayload::Permission { .. }
        ));
        let ask = interaction_from_agent_event("deadbeef", 3, &ask).unwrap();
        assert_eq!(ask.key.request_id, "ask-1");
        assert!(matches!(
            ask.payload,
            CompanionInteractionPayload::AskUser { .. }
        ));
        let plan = interaction_from_agent_event("deadbeef", 3, &plan).unwrap();
        assert_eq!(plan.key.request_id, "plan-1");
        assert!(matches!(
            plan.payload,
            CompanionInteractionPayload::PlanReview { .. }
        ));
    }

    #[test]
    fn converts_matching_responses_to_agent_frames() {
        let permission = interaction_from_agent_event(
            "deadbeef",
            3,
            &Agent2Ui::PermissionRequest {
                tool_call_id: "tool-1".into(),
                tool_name: "shell_command".into(),
                reason: "Run tests".into(),
                paths: vec![],
                category: "exec".into(),
                level: 2,
                risk: PermissionRisk::Medium,
                consequence: "Runs a command".into(),
            },
        )
        .unwrap();
        assert!(matches!(
            response_to_agent_frame(
                &permission.key,
                CompanionInteractionResponse::Permission {
                    approved: true,
                    trust_folder: false,
                }
            ),
            Ok(Ui2Agent::PermissionResponse { approved: true, .. })
        ));

        let mut ask_key = permission.key.clone();
        ask_key.kind = CompanionInteractionKind::AskUser;
        ask_key.request_id = "ask-1".into();
        assert!(matches!(
            response_to_agent_frame(
                &ask_key,
                CompanionInteractionResponse::AskUser {
                    answers: vec![AskAnswer {
                        question_id: "q1".into(),
                        answer: "Yes".into()
                    }],
                    dismissed: false,
                }
            ),
            Ok(Ui2Agent::AskResponse { .. })
        ));

        let mut plan_key = ask_key;
        plan_key.kind = CompanionInteractionKind::PlanReview;
        plan_key.request_id = "plan-1".into();
        assert!(matches!(
            response_to_agent_frame(
                &plan_key,
                CompanionInteractionResponse::PlanReview {
                    approved: true,
                    message: "revise".into(),
                    autonomous: true,
                }
            ),
            Ok(Ui2Agent::PlanReview {
                approved: true,
                autonomous: true,
                ..
            })
        ));
    }

    #[test]
    fn rejects_response_kind_mismatch() {
        let key = deepx_proto::CompanionInteractionKey {
            seed: "deadbeef".into(),
            generation: 1,
            kind: CompanionInteractionKind::Permission,
            request_id: "tool-1".into(),
        };
        assert!(matches!(
            response_to_agent_frame(
                &key,
                CompanionInteractionResponse::PlanReview {
                    approved: true,
                    message: String::new(),
                    autonomous: false,
                }
            ),
            Err(ResponseFrameError::KindMismatch)
        ));
    }
}
