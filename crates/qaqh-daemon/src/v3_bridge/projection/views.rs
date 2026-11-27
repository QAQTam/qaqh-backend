use qaqh_core_vocab::{ExecSessionId, InteractionId, SessionId, ToolCallId, TurnId};
use qaqh_ringing_contract::fact::{
    ContentValue, InteractionDecision, InteractionKind, ToolCallOutcome,
};
use qaqh_ringing_contract::state as old_state;
use qaqh_ringing_runtime::RingingHub;
use qaqh_ringing_v3::{
    self as v3, ChoiceTone, CodeDeltaView, ExecCancelReasonView, ExecInterruptReasonView,
    ExecOutcomeView, ExecSessionStateView, ExecStopReasonView, InteractionChoiceView,
    InteractionDecisionKind, PermissionRequestView, ToolCallStatus, ToolEffectView,
    ToolMetricsView,
};
use qaqh_runtime::QaqhService;

use crate::v3_server::state::ServerState;

use super::content;

pub(crate) async fn interaction_view(
    _hub: &RingingHub,
    _state: &ServerState,
    _session_id: &str,
    interaction: &old_state::InteractionView,
) -> Result<v3::InteractionView, String> {
    let text = match &interaction.request {
        ContentValue::Inline { text } => text.clone(),
        _ => return Err("interaction request must be inline JSON".into()),
    };
    let turn_id = interaction
        .turn_id
        .as_ref()
        .map(|id| TurnId::new(id.as_str()).map_err(|error| error.to_string()))
        .transpose()?;
    let choices = interaction
        .choices
        .iter()
        .map(|choice| InteractionChoiceView {
            decision: match choice.decision {
                InteractionDecision::Approved => InteractionDecisionKind::Approved,
                InteractionDecision::ApprovedForSession => {
                    InteractionDecisionKind::ApprovedForSession
                }
                InteractionDecision::Rejected => InteractionDecisionKind::Rejected,
                InteractionDecision::Answered => InteractionDecisionKind::Answered,
                InteractionDecision::Dismissed => InteractionDecisionKind::Dismissed,
            },
            label: choice.label.clone(),
            description: choice.description.clone(),
            tone: match choice.tone {
                qaqh_ringing_contract::fact::InteractionChoiceTone::Primary => ChoiceTone::Primary,
                qaqh_ringing_contract::fact::InteractionChoiceTone::Secondary => {
                    ChoiceTone::Secondary
                }
                qaqh_ringing_contract::fact::InteractionChoiceTone::Destructive => {
                    ChoiceTone::Destructive
                }
            },
        })
        .collect();
    let (kind, title, body_text, questions, permission) = match interaction.kind {
        InteractionKind::Permission => {
            let details: old_state::PermissionRequestDetails = serde_json::from_str(&text)
                .map_err(|error| format!("permission interaction details: {error}"))?;
            if !details.is_valid() {
                return Err("permission interaction details failed validation".into());
            }
            let title = details
                .action_summary
                .clone()
                .filter(|summary| !summary.trim().is_empty())
                .unwrap_or_else(|| format!("确认执行 {}", details.tool_name));
            let body = details.reason.clone();
            let permission = PermissionRequestView {
                tool_name: details.tool_name,
                action_summary: details.action_summary,
                justification: details.justification,
                reason: details.reason,
                paths: details.paths,
                category: details.category,
                level: details.level,
                risk: details.risk,
                consequence: details.consequence,
            };
            (
                v3::InteractionKind::Permission,
                title,
                body,
                Vec::new(),
                Some(permission),
            )
        }
        InteractionKind::Ask => {
            let ask: qaqh_workspace::ask_user::NormalizedAsk =
                serde_json::from_str(&text).map_err(|error| format!("ask interaction: {error}"))?;
            let questions: Vec<v3::AskQuestionView> = ask
                .questions
                .into_iter()
                .map(|question| v3::AskQuestionView {
                    id: question.id,
                    prompt: question.question,
                    options: question.options,
                    allow_free_text: question.allow_custom,
                })
                .collect();
            let body = questions
                .first()
                .map(|question| question.prompt.clone())
                .unwrap_or_default();
            (
                v3::InteractionKind::Ask,
                "Question".to_string(),
                body,
                questions,
                None,
            )
        }
    };
    Ok(v3::InteractionView {
        interaction_id: InteractionId::new(interaction.interaction_id.as_str())
            .map_err(|error| error.to_string())?,
        turn_id,
        kind,
        tool_call_id: interaction
            .call_id
            .as_ref()
            .map(|id| ToolCallId::new(id.as_str()).map_err(|error| error.to_string()))
            .transpose()?,
        title,
        body: body_text,
        options: interaction
            .choices
            .iter()
            .map(|choice| choice.label.clone())
            .collect(),
        choices,
        questions,
        permission,
        expires_at_ms: interaction.expires_at_ms.map(|value| value.max(0) as u64),
    })
}

pub(crate) async fn exec_view(
    hub: &RingingHub,
    state: &ServerState,
    exec: &old_state::ExecSessionView,
) -> Result<v3::ExecSessionSnapshot, String> {
    let output = match &exec.output {
        Some(output) => Some(
            content::content_ref(hub, state, exec.owner_session_id.as_str(), output.clone())
                .await?,
        ),
        None => None,
    };
    let origin_turn_id = (!exec.origin_turn_id.as_str().is_empty())
        .then(|| TurnId::new(exec.origin_turn_id.as_str()).map_err(|error| error.to_string()))
        .transpose()?;
    let origin_call_id = (!exec.origin_call_id.as_str().is_empty())
        .then(|| ToolCallId::new(exec.origin_call_id.as_str()).map_err(|error| error.to_string()))
        .transpose()?;
    Ok(v3::ExecSessionSnapshot {
        exec_session_id: ExecSessionId::new(exec.id.as_str()).map_err(|error| error.to_string())?,
        command: exec.command.clone(),
        tty: exec.tty,
        finished: matches!(&exec.state, old_state::ExecSessionState::Finished { .. }),
        exit_code: match &exec.state {
            old_state::ExecSessionState::Finished {
                outcome: old_state::ExecSessionOutcome::Exited { exit_code },
            } => Some(*exit_code),
            _ => None,
        },
        next_offset: exec.next_offset,
        output,
        state: map_exec_state(&exec.state),
        owner_session_id: SessionId::new(exec.owner_session_id.as_str())
            .map_err(|error| error.to_string())?,
        origin_turn_id,
        origin_call_id,
        cwd: exec.cwd.clone(),
        started_at_ms: exec.started_at_ms.max(0) as u64,
        finished_at_ms: exec.finished_at_ms.map(|value| value.max(0) as u64),
        output_preview: exec.output_preview.clone(),
        preview_start_offset: exec.preview_start_offset,
        output_complete: exec.output_complete,
        capture_omitted_bytes: exec.capture_omitted_bytes,
        stream_dropped_bytes: exec.stream_dropped_bytes,
    })
}

fn map_exec_stop_reason(reason: old_state::ExecStopReason) -> ExecStopReasonView {
    match reason {
        old_state::ExecStopReason::Interrupt => ExecStopReasonView::Interrupt,
        old_state::ExecStopReason::Terminate => ExecStopReasonView::Terminate,
        old_state::ExecStopReason::TurnCancel => ExecStopReasonView::TurnCancel,
        old_state::ExecStopReason::Timeout => ExecStopReasonView::Timeout,
        old_state::ExecStopReason::Shutdown => ExecStopReasonView::Shutdown,
    }
}

fn map_exec_state(state: &old_state::ExecSessionState) -> ExecSessionStateView {
    match state {
        old_state::ExecSessionState::Running => ExecSessionStateView::Running,
        old_state::ExecSessionState::Stopping { reason } => ExecSessionStateView::Stopping {
            reason: map_exec_stop_reason(*reason),
        },
        old_state::ExecSessionState::Finished { outcome } => ExecSessionStateView::Finished {
            outcome: match outcome {
                old_state::ExecSessionOutcome::Exited { exit_code } => ExecOutcomeView::Exited {
                    exit_code: *exit_code,
                },
                old_state::ExecSessionOutcome::Cancelled { reason, exit_code } => {
                    ExecOutcomeView::Cancelled {
                        reason: match reason {
                            old_state::ExecCancelReason::Interrupt => {
                                ExecCancelReasonView::Interrupt
                            }
                            old_state::ExecCancelReason::Terminate => {
                                ExecCancelReasonView::Terminate
                            }
                            old_state::ExecCancelReason::TurnCancel => {
                                ExecCancelReasonView::TurnCancel
                            }
                            old_state::ExecCancelReason::Shutdown => ExecCancelReasonView::Shutdown,
                        },
                        exit_code: *exit_code,
                    }
                }
                old_state::ExecSessionOutcome::TimedOut { exit_code } => {
                    ExecOutcomeView::TimedOut {
                        exit_code: *exit_code,
                    }
                }
                old_state::ExecSessionOutcome::Interrupted { reason } => {
                    ExecOutcomeView::Interrupted {
                        reason: match reason {
                            old_state::ExecInterruptReason::DaemonRestart => {
                                ExecInterruptReasonView::DaemonRestart
                            }
                            old_state::ExecInterruptReason::LostExecution => {
                                ExecInterruptReasonView::LostExecution
                            }
                            old_state::ExecInterruptReason::UpgradeStateUnknown => {
                                ExecInterruptReasonView::UpgradeStateUnknown
                            }
                        },
                    }
                }
            },
        },
    }
}

pub(crate) async fn tool_call_view(
    hub: &RingingHub,
    state: &ServerState,
    session_id: &str,
    tool: &old_state::ToolCallView,
) -> Result<v3::ToolCallView, String> {
    let (status, interaction_id, finished) = match &tool.state {
        old_state::ToolCallState::Declared => (ToolCallStatus::Pending, None, None),
        old_state::ToolCallState::AwaitingInteraction { interaction_id } => (
            ToolCallStatus::AwaitingInteraction,
            Some(InteractionId::new(interaction_id.as_str()).map_err(|error| error.to_string())?),
            None,
        ),
        old_state::ToolCallState::Running => (ToolCallStatus::Running, None, None),
        old_state::ToolCallState::Cancelling => (ToolCallStatus::Cancelling, None, None),
        old_state::ToolCallState::Finished { outcome } => (
            map_tool_outcome(outcome),
            None,
            tool.metrics
                .as_ref()
                .map(|metrics| metrics.finished_at_ms.max(0) as u64),
        ),
    };
    let args = match &tool.args {
        Some(value) => Some(content::json_value(hub, state, session_id, value).await?),
        None => None,
    };
    let output = match &tool.output {
        Some(value) => Some(content::content_ref(hub, state, session_id, value.clone()).await?),
        None => None,
    };
    let diff = match &tool.diff {
        Some(value) => Some(content::content_ref(hub, state, session_id, value.clone()).await?),
        None => None,
    };
    let effects = map_effects(&tool.effects)?;
    Ok(v3::ToolCallView {
        tool_call_id: ToolCallId::new(tool.call_id.as_str()).map_err(|error| error.to_string())?,
        turn_id: tool
            .turn_id
            .as_ref()
            .map(|id| TurnId::new(id.as_str()).map_err(|error| error.to_string()))
            .transpose()?,
        tool_name: tool.tool_name.clone().unwrap_or_default(),
        status,
        started_at_ms: tool
            .metrics
            .as_ref()
            .map(|metrics| metrics.started_at_ms.max(0) as u64)
            .unwrap_or_default(),
        finished_at_ms: finished,
        args,
        output,
        output_truncated: tool.output_truncated,
        diff,
        code_delta: tool.code_delta.as_ref().map(map_code_delta),
        metrics: tool.metrics.as_ref().map(map_metrics),
        exec_session_id: tool
            .exec_session_id
            .as_ref()
            .map(|id| ExecSessionId::new(id.as_str()).map_err(|error| error.to_string()))
            .transpose()?,
        interaction_id,
        effects,
    })
}

pub(crate) fn map_tool_outcome(outcome: &ToolCallOutcome) -> ToolCallStatus {
    match outcome {
        ToolCallOutcome::Completed | ToolCallOutcome::Yielded { .. } => ToolCallStatus::Succeeded,
        ToolCallOutcome::Failed { .. } => ToolCallStatus::Failed,
        ToolCallOutcome::Cancelled { .. } => ToolCallStatus::Cancelled,
        ToolCallOutcome::Interrupted { .. } => ToolCallStatus::Indeterminate,
    }
}

pub(crate) fn map_code_delta(delta: &old_state::CodeDelta) -> CodeDeltaView {
    CodeDeltaView {
        lines_added: delta.lines_added as u32,
        lines_removed: delta.lines_removed as u32,
        files_created: delta.files_created as u32,
        files_deleted: delta.files_deleted as u32,
        file: delta.file.clone(),
    }
}

pub(crate) fn map_metrics(metrics: &qaqh_ringing_contract::fact::ToolMetrics) -> ToolMetricsView {
    ToolMetricsView {
        started_at_ms: metrics.started_at_ms.max(0) as u64,
        finished_at_ms: metrics.finished_at_ms.max(0) as u64,
        retry_count: metrics.retry_count,
        output_bytes: metrics.output_bytes,
        progress_bytes_total: metrics.progress_bytes_total,
    }
}

pub(crate) fn map_effects(
    effects: &[old_state::ToolEffectInfo],
) -> Result<Vec<ToolEffectView>, String> {
    effects
        .iter()
        .map(|effect| match effect {
            old_state::ToolEffectInfo::SubagentSpawned {
                child_session_id,
                name,
                task_text,
            } => Ok(ToolEffectView::SubagentSpawned {
                child_session_id: SessionId::new(child_session_id.as_str())
                    .map_err(|error| error.to_string())?,
                name: name.clone(),
                task_text: task_text.clone(),
            }),
        })
        .collect()
}

pub(crate) async fn stored_tool_view(
    service: &QaqhService,
    hub: &RingingHub,
    state: &ServerState,
    session_id: &str,
    call_id: &str,
) -> Result<Option<v3::ToolCallView>, String> {
    let (raw, call) = (session_id.to_string(), call_id.to_string());
    let db = service.owners().db().clone();
    let row = tokio::task::spawn_blocking(move || {
        let rows = db
            .read_blocking(|conn| qaqh_session::db::store::recent_tool_calls(conn, &raw, 500))
            .map_err(|error| error.to_string())?;
        Ok::<_, String>(rows.into_iter().find(|row| row.call_id == call))
    })
    .await
    .map_err(|error| error.to_string())??;
    let Some(row) = row else {
        return Ok(None);
    };
    let parsed_state: old_state::ToolCallState =
        serde_json::from_str(&row.state_json).map_err(|error| error.to_string())?;
    let args = row
        .args_json
        .as_deref()
        .map(serde_json::from_str::<serde_json::Value>)
        .transpose()
        .map_err(|error| error.to_string())?;
    let metrics = row
        .meta_json
        .as_deref()
        .map(serde_json::from_str::<qaqh_ringing_contract::fact::ToolMetrics>)
        .transpose()
        .map_err(|error| error.to_string())?;
    let code_delta = row
        .code_delta_json
        .as_deref()
        .map(serde_json::from_str::<old_state::CodeDelta>)
        .transpose()
        .map_err(|error| error.to_string())?;
    let effects = row
        .effects_json
        .as_deref()
        .map(serde_json::from_str::<Vec<old_state::ToolEffectInfo>>)
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    let output = row
        .output_text
        .map(|text| ContentValue::Inline { text })
        .or_else(|| {
            row.output_ref.map(|content_id| ContentValue::Ref {
                content_ref: qaqh_ringing_contract::fact::ContentRef::new(
                    qaqh_ringing_contract::fact::ContentHash::new(
                        qaqh_session::db::store::canonical_content_ref(&content_id),
                    ),
                ),
            })
        });
    let diff = row
        .diff_text
        .map(|text| ContentValue::Inline { text })
        .or_else(|| {
            row.diff_ref.map(|content_id| ContentValue::Ref {
                content_ref: qaqh_ringing_contract::fact::ContentRef::new(
                    qaqh_ringing_contract::fact::ContentHash::new(
                        qaqh_session::db::store::canonical_content_ref(&content_id),
                    ),
                ),
            })
        });
    let old = old_state::ToolCallView {
        call_id: qaqh_ringing_contract::fact::ToolCallId::new(row.call_id),
        turn_id: row.turn_id.map(qaqh_ringing_contract::fact::TurnId::new),
        tool_name: Some(row.tool_name),
        args,
        state: parsed_state,
        exec_session_id: row
            .exec_session_id
            .map(qaqh_ringing_contract::fact::ExecSessionId::new),
        output,
        output_truncated: row.output_truncated,
        diff,
        metrics,
        code_delta,
        effects,
    };
    tool_call_view(hub, state, session_id, &old)
        .await
        .map(Some)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use qaqh_ringing_contract::fact::{
        ContentValue, InteractionChoice, InteractionChoiceTone, InteractionDecision, InteractionId,
        InteractionKind, TurnId,
    };
    use qaqh_workspace::ask_user::{NormalizedAsk, NormalizedAskMode, NormalizedAskQuestion};

    use super::*;
    use crate::v3_server::state::ServerConfig;

    #[tokio::test]
    async fn ask_interaction_projects_normalized_question_and_allow_custom() {
        let temp = tempfile::tempdir().unwrap();
        let db = std::sync::Arc::new(qaqh_session::db::Db::open(temp.path().join("q.db")).unwrap());
        let hub = RingingHub::with_database("e", db);
        let state = ServerState::new(ServerConfig::default(), "user");
        let ask = NormalizedAsk {
            mode: NormalizedAskMode::Single,
            questions: vec![NormalizedAskQuestion {
                id: "q1".into(),
                question: "Pick one".into(),
                options: vec!["A".into(), "B".into()],
                allow_custom: false,
            }],
        };
        let raw = old_state::InteractionView {
            interaction_id: InteractionId::new("ia-1"),
            call_id: None,
            turn_id: Some(TurnId::new("t1")),
            kind: InteractionKind::Ask,
            request: ContentValue::Inline {
                text: serde_json::to_string(&ask).unwrap(),
            },
            choices: Vec::new(),
            expires_at_ms: None,
            resolution: None,
        };
        let projected = interaction_view(&hub, &state, "s1", &raw).await.unwrap();
        assert_eq!(projected.kind, v3::InteractionKind::Ask);
        assert_eq!(
            projected.turn_id,
            Some(qaqh_core_vocab::TurnId::new("t1").unwrap())
        );
        assert_eq!(projected.body, "Pick one");
        assert!(projected.permission.is_none());
        assert_eq!(projected.questions.len(), 1);
        assert_eq!(projected.questions[0].prompt, "Pick one");
        assert!(!projected.questions[0].allow_free_text);
    }

    #[tokio::test]
    async fn permission_interaction_projects_structured_fields_without_json_in_body() {
        let temp = tempfile::tempdir().unwrap();
        let db = std::sync::Arc::new(qaqh_session::db::Db::open(temp.path().join("q.db")).unwrap());
        let hub = RingingHub::with_database("e", db);
        let state = ServerState::new(ServerConfig::default(), "user");
        let details = old_state::PermissionRequestDetails {
            tool_name: "write".into(),
            action_summary: Some("Write src/lib.rs".into()),
            justification: Some("Fix bug".into()),
            reason: "Needs workspace write".into(),
            paths: vec!["src/lib.rs".into()],
            category: "write".into(),
            level: 2,
            risk: "medium".into(),
            consequence: "Overwrites src/lib.rs".into(),
        };
        let raw = old_state::InteractionView {
            interaction_id: InteractionId::new("perm-1"),
            call_id: Some(qaqh_ringing_contract::fact::ToolCallId::new("c1")),
            turn_id: Some(TurnId::new("t1")),
            kind: InteractionKind::Permission,
            request: ContentValue::Inline {
                text: serde_json::to_string(&details).unwrap(),
            },
            choices: vec![InteractionChoice {
                decision: InteractionDecision::Approved,
                label: "允许".into(),
                description: Some("仅此次".into()),
                tone: InteractionChoiceTone::Primary,
            }],
            expires_at_ms: Some(999),
            resolution: None,
        };
        let projected = interaction_view(&hub, &state, "s1", &raw).await.unwrap();
        assert_eq!(projected.kind, v3::InteractionKind::Permission);
        assert_eq!(projected.title, "Write src/lib.rs");
        assert_eq!(projected.body, "Needs workspace write");
        assert!(
            !projected.body.starts_with('{'),
            "Permission body must never carry JSON string"
        );
        let perm = projected.permission.expect("structured permission");
        assert_eq!(perm.tool_name, "write");
        assert_eq!(perm.category, "write");
        assert_eq!(perm.level, 2);
        assert_eq!(perm.paths, vec!["src/lib.rs"]);
        assert_eq!(projected.choices.len(), 1);
        assert_eq!(projected.choices[0].decision, InteractionDecisionKind::Approved);
        assert_eq!(projected.choices[0].tone, ChoiceTone::Primary);
    }
}
