use std::collections::BTreeMap;
use std::sync::Arc;

use qaqh_core_vocab::{ExecSessionId, SessionId};
use qaqh_ringing_contract::{SessionEvent as OldEvent, state as old_state};
use qaqh_ringing_runtime::RingingHub;
use qaqh_ringing_v3::{self as v3, SessionEvent};
use qaqh_runtime::QaqhService;

use crate::v3_server::now_ms;
use crate::v3_server::session_runtime::SessionRuntime;
use crate::v3_server::state::ServerState;

use super::super::session;
use super::{content, timeline, views};

pub(super) async fn map_event(
    service: &QaqhService,
    state: &ServerState,
    hub: &RingingHub,
    runtime: &Arc<SessionRuntime>,
    event: OldEvent,
    arguments: &mut BTreeMap<String, String>,
) -> Result<Option<SessionEvent>, String> {
    let id = runtime.id.clone();
    let some = |event| -> Result<Option<SessionEvent>, String> { Ok(Some(event)) };
    match event {
        OldEvent::ActivityChanged {
            activity,
            current_turn_id,
        } => {
            runtime.set_activity(session::activity(activity));
            some(SessionEvent::ActivityChanged {
                session_id: id,
                activity: session::activity(activity),
                current_turn_id: current_turn_id
                    .map(|value| qaqh_core_vocab::TurnId::new(value.as_str()))
                    .transpose()
                    .map_err(|error| error.to_string())?,
            })
        }
        OldEvent::TurnStarted { turn_id, .. } => {
            runtime.start_turn(None);
            some(SessionEvent::TurnStarted {
                session_id: id,
                turn_id: qaqh_core_vocab::TurnId::new(turn_id.as_str())
                    .map_err(|error| error.to_string())?,
                started_at_ms: now_ms(),
            })
        }
        OldEvent::TurnPhaseChanged { turn_id, phase } => some(SessionEvent::TurnPhaseChanged {
            session_id: id,
            turn_id: qaqh_core_vocab::TurnId::new(turn_id.as_str())
                .map_err(|error| error.to_string())?,
            phase: map_phase(phase),
        }),
        OldEvent::TurnEnded { turn_id, outcome } => {
            if let Some(ordinal) = runtime.current_turn() {
                let terminal = session::turn_terminal(&outcome);
                runtime.finish_turn(ordinal, terminal.clone());
            }
            let (terminal, usage) = match outcome {
                old_state::TurnOutcome::Finished {
                    terminal,
                    usage,
                    error,
                } => (
                    match terminal {
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
                    usage,
                ),
                old_state::TurnOutcome::Interrupted { reason } => (
                    qaqh_core_vocab::TurnTerminal::Interrupted {
                        code: format!("{reason:?}").to_lowercase(),
                    },
                    None,
                ),
            };
            some(SessionEvent::TurnFinished {
                session_id: id,
                turn_id: qaqh_core_vocab::TurnId::new(turn_id.as_str())
                    .map_err(|error| error.to_string())?,
                terminal,
                usage: usage.map(map_usage).unwrap_or_default(),
            })
        }
        OldEvent::ProviderRetrying {
            turn_id: _,
            attempt,
            delay_secs,
            error,
            ..
        } => some(SessionEvent::ProviderRetrying {
            session_id: id,
            attempt,
            delay_ms: delay_secs.saturating_mul(1000),
            reason: error,
        }),
        OldEvent::MessageDelta {
            block_id,
            block_kind,
            text,
            ..
        } => some(match block_kind {
            qaqh_ringing_contract::fact::AssistantBlockKind::Answer => SessionEvent::AnswerDelta {
                session_id: id,
                block_id: qaqh_core_vocab::BlockId::new(block_id.as_str())
                    .map_err(|error| error.to_string())?,
                text,
            },
            qaqh_ringing_contract::fact::AssistantBlockKind::Reasoning => {
                SessionEvent::ReasoningDelta {
                    session_id: id,
                    block_id: qaqh_core_vocab::BlockId::new(block_id.as_str())
                        .map_err(|error| error.to_string())?,
                    text,
                }
            }
            qaqh_ringing_contract::fact::AssistantBlockKind::ToolCall => return Ok(None),
        }),
        OldEvent::ToolArgumentsDelta { call_id, text, .. } => {
            let key = call_id.as_str().to_string();
            let previous = arguments
                .insert(key.clone(), text.clone())
                .unwrap_or_default();
            let delta = text
                .strip_prefix(&previous)
                .ok_or_else(|| format!("tool arguments changed non-monotonically: {key}"))?
                .to_string();
            some(SessionEvent::ToolArgumentsDelta {
                session_id: id,
                tool_call_id: qaqh_core_vocab::ToolCallId::new(call_id.as_str())
                    .map_err(|error| error.to_string())?,
                delta,
            })
        }
        OldEvent::ToolDeclared {
            call_id, tool_name, ..
        } => {
            let call = qaqh_core_vocab::ToolCallId::new(call_id.as_str())
                .map_err(|error| error.to_string())?;
            let view = v3::ToolCallView {
                tool_call_id: call,
                tool_name,
                status: v3::ToolCallStatus::Pending,
                started_at_ms: now_ms(),
                finished_at_ms: None,
                summary: None,
            };
            runtime.upsert_tool_call(view.clone());
            some(SessionEvent::ToolCallUpdated {
                session_id: id,
                call: view,
            })
        }
        OldEvent::ToolStateChanged { call_id, state, .. } => {
            let call = qaqh_core_vocab::ToolCallId::new(call_id.as_str())
                .map_err(|error| error.to_string())?;
            let tool_name =
                views::tool_name(service, runtime.id.as_str(), call_id.as_str()).await?;
            let status = match state {
                old_state::ToolCallState::Declared => v3::ToolCallStatus::Pending,
                old_state::ToolCallState::AwaitingInteraction { .. } => v3::ToolCallStatus::Pending,
                old_state::ToolCallState::Running => v3::ToolCallStatus::Running,
                old_state::ToolCallState::Cancelling => v3::ToolCallStatus::Cancelled,
                old_state::ToolCallState::Finished { .. } => return Ok(None),
            };
            let view = v3::ToolCallView {
                tool_call_id: call,
                tool_name,
                status,
                started_at_ms: now_ms(),
                finished_at_ms: None,
                summary: None,
            };
            runtime.upsert_tool_call(view.clone());
            some(SessionEvent::ToolCallUpdated {
                session_id: id,
                call: view,
            })
        }
        OldEvent::ToolFinished {
            call_id,
            output,
            metrics,
            outcome,
            ..
        } => {
            let call = qaqh_core_vocab::ToolCallId::new(call_id.as_str())
                .map_err(|error| error.to_string())?;
            let tool_name =
                views::tool_name(service, runtime.id.as_str(), call_id.as_str()).await?;
            let status = match outcome {
                qaqh_ringing_contract::fact::ToolCallOutcome::Completed
                | qaqh_ringing_contract::fact::ToolCallOutcome::Yielded { .. } => {
                    v3::ToolCallStatus::Succeeded
                }
                qaqh_ringing_contract::fact::ToolCallOutcome::Failed { .. } => {
                    v3::ToolCallStatus::Failed
                }
                qaqh_ringing_contract::fact::ToolCallOutcome::Cancelled { .. } => {
                    v3::ToolCallStatus::Cancelled
                }
                qaqh_ringing_contract::fact::ToolCallOutcome::Interrupted { .. } => {
                    v3::ToolCallStatus::Indeterminate
                }
            };
            let summary = if let Some(value) = output {
                Some(content::content_ref(hub, state, runtime.id.as_str(), value).await?)
            } else {
                None
            };
            let view = v3::ToolCallView {
                tool_call_id: call,
                tool_name,
                status,
                started_at_ms: metrics
                    .as_ref()
                    .map(|value| value.started_at_ms.max(0) as u64)
                    .unwrap_or_default(),
                finished_at_ms: metrics
                    .as_ref()
                    .map(|value| value.finished_at_ms.max(0) as u64),
                summary,
            };
            runtime.upsert_tool_call(view.clone());
            some(SessionEvent::ToolCallUpdated {
                session_id: id,
                call: view,
            })
        }
        OldEvent::ToolOutputDelta { call_id, text, .. } => some(SessionEvent::ToolOutputDelta {
            session_id: id,
            tool_call_id: qaqh_core_vocab::ToolCallId::new(call_id.as_str())
                .map_err(|error| error.to_string())?,
            text,
        }),
        OldEvent::ExecSessionStarted { session }
        | OldEvent::ExecSessionStateChanged { session }
        | OldEvent::ExecSessionFinished { session } => {
            let exec = views::exec_view(hub, state, &session).await?;
            runtime.upsert_exec_session(exec.clone());
            some(SessionEvent::ExecSessionUpdated {
                session_id: id,
                exec,
            })
        }
        OldEvent::ExecSessionOutputDelta {
            exec_session_id,
            start_offset,
            end_offset,
            text,
            ..
        } => some(SessionEvent::ExecOutputDelta {
            session_id: id,
            exec_session_id: ExecSessionId::new(exec_session_id.as_str())
                .map_err(|error| error.to_string())?,
            start_offset,
            end_offset,
            text,
            spilled: false,
        }),
        OldEvent::TimelineAppended { entry } => some(SessionEvent::TimelineAppended {
            session_id: id,
            block: timeline::timeline_block(service, hub, state, runtime.id.as_str(), entry)
                .await?,
        }),
        OldEvent::InteractionRequested { interaction } => {
            let interaction =
                views::interaction_view(hub, state, runtime.id.as_str(), &interaction).await?;
            some(SessionEvent::InteractionRequested {
                session_id: id,
                interaction,
            })
        }
        OldEvent::InteractionResolved {
            interaction_id,
            decision,
            status,
        } => some(SessionEvent::InteractionResolved {
            session_id: id,
            interaction_id: qaqh_core_vocab::InteractionId::new(interaction_id.as_str())
                .map_err(|error| error.to_string())?,
            resolution: match decision {
                Some(qaqh_ringing_contract::fact::InteractionDecision::Approved) => {
                    v3::InteractionResolution::Approved {
                        scope: qaqh_core_vocab::PermissionGrantScope::Once,
                    }
                }
                Some(qaqh_ringing_contract::fact::InteractionDecision::ApprovedForSession) => {
                    v3::InteractionResolution::Approved {
                        scope: qaqh_core_vocab::PermissionGrantScope::Session,
                    }
                }
                Some(qaqh_ringing_contract::fact::InteractionDecision::Rejected) => {
                    v3::InteractionResolution::Denied {
                        reason: "rejected".into(),
                    }
                }
                Some(qaqh_ringing_contract::fact::InteractionDecision::Answered) => {
                    v3::InteractionResolution::Answered {
                        answers: Vec::new(),
                    }
                }
                Some(qaqh_ringing_contract::fact::InteractionDecision::Dismissed) => {
                    v3::InteractionResolution::Dismissed
                }
                None => match status {
                    qaqh_ringing_contract::events::InteractionEndStatus::Expired
                    | qaqh_ringing_contract::events::InteractionEndStatus::Cancelled => {
                        v3::InteractionResolution::Abandoned
                    }
                    _ => v3::InteractionResolution::Dismissed,
                },
            },
        }),
        OldEvent::SubagentUpdated { subagent } => {
            let child = subagent.child_session_id.as_str().to_string();
            let service = service.clone();
            let loaded = tokio::task::spawn_blocking(move || {
                service
                    .agent_residency_loaded(&child)
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| error.to_string())??;
            some(SessionEvent::SubagentUpdated {
                session_id: id,
                subagent: v3::SubagentSummary {
                    session_id: SessionId::new(subagent.child_session_id.as_str())
                        .map_err(|error| error.to_string())?,
                    nickname: subagent.role,
                    status: match subagent.status {
                        Some(qaqh_ringing_contract::fact::SubagentTerminalStatus::Completed) => {
                            v3::TeamAgentStatus::Completed
                        }
                        Some(qaqh_ringing_contract::fact::SubagentTerminalStatus::Failed) => {
                            v3::TeamAgentStatus::Errored
                        }
                        Some(_) => v3::TeamAgentStatus::Interrupted,
                        None => v3::TeamAgentStatus::Running,
                    },
                    residency: if loaded {
                        v3::TeamAgentResidency::Loaded
                    } else {
                        v3::TeamAgentResidency::Unloaded
                    },
                },
            })
        }
        OldEvent::SessionMetaChanged { title } => {
            if let Some(title) = &title {
                runtime.rename(title.clone());
            }
            some(SessionEvent::SessionMetaChanged {
                session_id: id,
                title,
            })
        }
        OldEvent::GoalUpdated { goal } => some(SessionEvent::GoalUpdated {
            session_id: id,
            goal: goal.as_ref().map(session::goal_view),
        }),
        OldEvent::HandoffSealed {
            successor_session_id,
            work_summary,
            ..
        } => some(SessionEvent::HandoffSealed {
            session_id: id,
            successor_session_id: SessionId::new(successor_session_id)
                .map_err(|error| error.to_string())?,
            summary: content::content_ref(
                hub,
                state,
                runtime.id.as_str(),
                qaqh_ringing_contract::fact::ContentValue::Inline { text: work_summary },
            )
            .await?,
        }),
        OldEvent::PromptBlocked { reason } => some(SessionEvent::PromptBlocked {
            session_id: id,
            reason,
        }),
        OldEvent::HookStarted { .. } | OldEvent::HookFinished { .. } => Ok(None),
        OldEvent::TimelineTruncated { .. } => unreachable!(),
    }
}

fn map_phase(value: qaqh_ringing_contract::TurnPhase) -> qaqh_core_vocab::TurnPhase {
    match value {
        qaqh_ringing_contract::TurnPhase::Running => qaqh_core_vocab::TurnPhase::Running,
        qaqh_ringing_contract::TurnPhase::WaitingUser => qaqh_core_vocab::TurnPhase::WaitingUser,
        qaqh_ringing_contract::TurnPhase::Cancelling => qaqh_core_vocab::TurnPhase::Cancelling,
    }
}
fn map_usage(value: qaqh_types::UsageInfo) -> qaqh_core_vocab::UsageInfo {
    qaqh_core_vocab::UsageInfo {
        input_tokens: value.prompt_tokens as u64,
        output_tokens: value.completion_tokens as u64,
        cached_input_tokens: (value.cache_usage_reported == Some(true))
            .then_some(value.prompt_cache_hit_tokens as u64),
        reasoning_tokens: (value.reasoning_tokens != 0).then_some(value.reasoning_tokens as u64),
    }
}
