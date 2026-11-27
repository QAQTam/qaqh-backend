use qaqh_core_vocab::{ActivityState, GoalStatus, ModelSelection, ToolMode, WorkspaceId};
use qaqh_ringing_v3::{GoalView, RpcError, SessionSummary};

use super::super::internal;
use qaqh_core_vocab::SessionId;

pub(super) fn summary_entry(
    entry: qaqh_types::SessionListEntry,
) -> Result<SessionSummary, RpcError> {
    let meta = entry.meta;
    Ok(SessionSummary {
        session_id: SessionId::new(meta.session_id).map_err(|error| internal(error.to_string()))?,
        title: meta.title,
        workspace_id: entry
            .workspace_id
            .map(|id| WorkspaceId::new(id).map_err(|error| internal(error.to_string())))
            .transpose()?,
        cwd: meta.cwd,
        archived: meta.archived,
        model_selection: meta.model_selection.map(model_selection),
        tool_mode: ToolMode::from_runtime_mode(Some(meta.tool_mode.as_str()))
            .map_err(|error| internal(error.to_string()))?,
        settings_revision: meta.settings_revision,
        handoff_from: meta
            .handoff_from_session_id
            .map(SessionId::new)
            .transpose()
            .map_err(|error| internal(error.to_string()))?,
        activity: match entry.status {
            qaqh_types::SessionRunStatus::Running => ActivityState::Running,
            qaqh_types::SessionRunStatus::WaitingUser => ActivityState::WaitingUser,
            qaqh_types::SessionRunStatus::Cancelling => ActivityState::Cancelling,
            _ => ActivityState::Idle,
        },
        updated_at_ms: meta.updated_at,
    })
}

pub(crate) fn goal_view(goal: &qaqh_ringing_contract::state::GoalView) -> GoalView {
    GoalView {
        goal_id: goal.goal_id.clone(),
        session_id: SessionId::new(goal.session_id.clone()).expect("validated goal session"),
        objective: goal.objective.clone(),
        status: goal_status_from_stored(goal.status),
        token_budget: goal.token_budget,
        tokens_used: goal.tokens_used,
        time_used_ms: goal.time_used_ms,
        created_at_ms: goal.created_at_ms,
        updated_at_ms: goal.updated_at_ms,
    }
}

pub(crate) fn turn_terminal(
    outcome: &qaqh_ringing_contract::state::TurnOutcome,
) -> qaqh_core_vocab::TurnTerminal {
    match outcome {
        qaqh_ringing_contract::state::TurnOutcome::Finished {
            terminal, error, ..
        } => match terminal {
            qaqh_ringing_contract::fact::TurnTerminal::Completed => {
                qaqh_core_vocab::TurnTerminal::Completed
            }
            qaqh_ringing_contract::fact::TurnTerminal::Failed => {
                qaqh_core_vocab::TurnTerminal::Failed {
                    code: error
                        .as_ref()
                        .map(|error| error.code.clone())
                        .unwrap_or_else(|| "failed".into()),
                    message: error
                        .as_ref()
                        .map(|error| error.message.clone())
                        .unwrap_or_default(),
                }
            }
            qaqh_ringing_contract::fact::TurnTerminal::Cancelled => {
                qaqh_core_vocab::TurnTerminal::Cancelled {
                    code: "cancelled".into(),
                }
            }
        },
        qaqh_ringing_contract::state::TurnOutcome::Interrupted { reason } => {
            qaqh_core_vocab::TurnTerminal::Interrupted {
                code: format!("{reason:?}").to_lowercase(),
            }
        }
    }
}

pub(crate) fn activity(value: qaqh_ringing_contract::fact::ActivityState) -> ActivityState {
    match value {
        qaqh_ringing_contract::fact::ActivityState::Idle => ActivityState::Idle,
        qaqh_ringing_contract::fact::ActivityState::Running => ActivityState::Running,
        qaqh_ringing_contract::fact::ActivityState::WaitingUser => ActivityState::WaitingUser,
        qaqh_ringing_contract::fact::ActivityState::Cancelling => ActivityState::Cancelling,
    }
}

pub(super) fn runtime_mode(mode: ToolMode) -> String {
    match mode {
        ToolMode::Core => "minimal".into(),
        ToolMode::Standard => "standard".into(),
        ToolMode::Custom => "custom".into(),
    }
}

pub(super) fn goal_status_to_stored(
    status: GoalStatus,
) -> qaqh_ringing_contract::state::GoalStatus {
    match status {
        GoalStatus::Active => qaqh_ringing_contract::state::GoalStatus::Active,
        GoalStatus::Paused => qaqh_ringing_contract::state::GoalStatus::Paused,
        GoalStatus::Blocked => qaqh_ringing_contract::state::GoalStatus::Blocked,
        GoalStatus::UsageLimited => qaqh_ringing_contract::state::GoalStatus::UsageLimited,
        GoalStatus::BudgetLimited => qaqh_ringing_contract::state::GoalStatus::BudgetLimited,
        GoalStatus::Complete => qaqh_ringing_contract::state::GoalStatus::Complete,
    }
}

fn goal_status_from_stored(status: qaqh_ringing_contract::state::GoalStatus) -> GoalStatus {
    match status {
        qaqh_ringing_contract::state::GoalStatus::Active => GoalStatus::Active,
        qaqh_ringing_contract::state::GoalStatus::Paused => GoalStatus::Paused,
        qaqh_ringing_contract::state::GoalStatus::Blocked => GoalStatus::Blocked,
        qaqh_ringing_contract::state::GoalStatus::UsageLimited => GoalStatus::UsageLimited,
        qaqh_ringing_contract::state::GoalStatus::BudgetLimited => GoalStatus::BudgetLimited,
        qaqh_ringing_contract::state::GoalStatus::Complete => GoalStatus::Complete,
    }
}

fn model_selection(value: qaqh_types::ModelSelection) -> ModelSelection {
    ModelSelection {
        provider_id: value.provider_id,
        model_id: value.model_id,
        options: qaqh_core_vocab::ModelSelectionOptions {
            max_output_tokens: value.options.max_output_tokens,
            reasoning_effort: value.options.reasoning_effort,
            native_web_search: value.options.native_web_search,
        },
    }
}

pub(crate) fn model_selection_stored(value: ModelSelection) -> qaqh_types::ModelSelection {
    qaqh_types::ModelSelection {
        provider_id: value.provider_id,
        model_id: value.model_id,
        options: qaqh_types::ModelSelectionOptions {
            max_output_tokens: value.options.max_output_tokens,
            reasoning_effort: value.options.reasoning_effort,
            native_web_search: value.options.native_web_search,
        },
    }
}
