use std::collections::BTreeMap;
use std::sync::Arc;

use qaqh_core_vocab::{ContentRef, ExecSessionId, InputId, InteractionId, SessionId, TurnId};
use qaqh_ringing_contract::{SessionEvent as OldEvent, state as old_state};
use qaqh_ringing_runtime::RingingHub;
use qaqh_ringing_v3::{self as v3, SessionEvent, ToolCallStatus};
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
    blocks: &mut BTreeMap<String, String>,
    arguments: &mut BTreeMap<String, String>,
    tools: &mut BTreeMap<String, v3::ToolCallView>,
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
                    .map(|value| TurnId::new(value.as_str()))
                    .transpose()
                    .map_err(|error| error.to_string())?,
            })
        }
        OldEvent::TurnStarted { turn_id, input_id } => {
            runtime.start_turn(None);
            some(SessionEvent::TurnStarted {
                session_id: id,
                turn_id: TurnId::new(turn_id.as_str()).map_err(|error| error.to_string())?,
                input_id: input_id
                    .map(|value| InputId::new(value.as_str()))
                    .transpose()
                    .map_err(|error| error.to_string())?,
                started_at_ms: now_ms(),
            })
        }
        OldEvent::TurnPhaseChanged { turn_id, phase } => some(SessionEvent::TurnPhaseChanged {
            session_id: id,
            turn_id: TurnId::new(turn_id.as_str()).map_err(|error| error.to_string())?,
            phase: map_phase(phase),
        }),
        OldEvent::TurnEnded { turn_id, outcome } => {
            if let Some(ordinal) = runtime.current_turn() {
                let terminal = session::turn_terminal(&outcome);
                runtime.finish_turn(ordinal, terminal);
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
                turn_id: TurnId::new(turn_id.as_str()).map_err(|error| error.to_string())?,
                terminal,
                usage: usage.map(map_usage).unwrap_or_default(),
            })
        }
        OldEvent::ProviderRetrying {
            turn_id: _,
            attempt,
            max_attempts,
            delay_secs,
            error,
        } => some(SessionEvent::ProviderRetrying {
            session_id: id,
            attempt,
            max_attempts,
            delay_ms: delay_secs.saturating_mul(1000),
            reason: error,
        }),
        OldEvent::MessageDelta {
            block_id,
            block_kind,
            text,
            ..
        } => {
            let key = block_id.as_str().to_string();
            let previous = blocks.insert(key.clone(), text.clone()).unwrap_or_default();
            let delta = text
                .strip_prefix(&previous)
                .ok_or_else(|| format!("message block changed non-monotonically: {key}"))?
                .to_string();
            let block_id = qaqh_core_vocab::BlockId::new(block_id.as_str())
                .map_err(|error| error.to_string())?;
            some(match block_kind {
                qaqh_ringing_contract::fact::AssistantBlockKind::Answer => {
                    SessionEvent::AnswerDelta {
                        session_id: id,
                        block_id,
                        text: delta,
                    }
                }
                qaqh_ringing_contract::fact::AssistantBlockKind::Reasoning => {
                    SessionEvent::ReasoningDelta {
                        session_id: id,
                        block_id,
                        text: delta,
                    }
                }
                qaqh_ringing_contract::fact::AssistantBlockKind::ToolCall => return Ok(None),
            })
        }
        OldEvent::ToolArgumentsDelta {
            call_id,
            tool_name,
            text,
            lines_added,
            lines_removed,
            target_files,
            args_complete,
            ..
        } => {
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
                tool_name,
                delta,
                lines_added,
                lines_removed,
                target_files,
                args_complete,
            })
        }
        OldEvent::ToolDeclared {
            call_id,
            turn_id,
            tool_name,
            args,
        } => {
            let key = call_id.as_str().to_string();
            arguments.remove(&key);
            let call = qaqh_core_vocab::ToolCallId::new(call_id.as_str())
                .map_err(|error| error.to_string())?;
            let turn = TurnId::new(turn_id.as_str()).map_err(|error| error.to_string())?;
            let args = match args {
                Some(value) => {
                    Some(content::json_value(hub, state, runtime.id.as_str(), &value).await?)
                }
                None => None,
            };
            let view = v3::ToolCallView {
                tool_call_id: call,
                turn_id: Some(turn),
                tool_name,
                status: ToolCallStatus::Pending,
                started_at_ms: now_ms(),
                finished_at_ms: None,
                args,
                output: None,
                output_truncated: false,
                diff: None,
                code_delta: None,
                metrics: None,
                exec_session_id: None,
                interaction_id: None,
                effects: Vec::new(),
            };
            tools.insert(key, view.clone());
            runtime.upsert_tool_call(view.clone());
            some(SessionEvent::ToolCallUpdated {
                session_id: id,
                call: view,
            })
        }
        OldEvent::ToolStateChanged {
            call_id,
            turn_id,
            state: call_state,
        } => {
            let key = call_id.as_str().to_string();
            let mut view = match tools.get(&key).cloned() {
                Some(existing) => existing,
                None => match views::stored_tool_view(
                    service,
                    hub,
                    state,
                    runtime.id.as_str(),
                    call_id.as_str(),
                )
                .await?
                {
                    Some(stored) => stored,
                    None => return Err(format!("tool call missing: {key}")),
                },
            };
            view.turn_id = Some(TurnId::new(turn_id.as_str()).map_err(|error| error.to_string())?);
            let (status, interaction_id) = match call_state {
                old_state::ToolCallState::Declared => (ToolCallStatus::Pending, None),
                old_state::ToolCallState::AwaitingInteraction { interaction_id } => (
                    ToolCallStatus::AwaitingInteraction,
                    Some(
                        InteractionId::new(interaction_id.as_str())
                            .map_err(|error| error.to_string())?,
                    ),
                ),
                old_state::ToolCallState::Running => (ToolCallStatus::Running, None),
                old_state::ToolCallState::Cancelling => (ToolCallStatus::Cancelling, None),
                old_state::ToolCallState::Finished { .. } => return Ok(None),
            };
            view.status = status;
            view.interaction_id = interaction_id;
            tools.insert(key, view.clone());
            runtime.upsert_tool_call(view.clone());
            some(SessionEvent::ToolCallUpdated {
                session_id: id,
                call: view,
            })
        }
        OldEvent::ToolFinished {
            call_id,
            outcome,
            exec_session_id,
            output,
            output_truncated,
            diff,
            metrics,
            code_delta,
            effects,
        } => {
            let key = call_id.as_str().to_string();
            arguments.remove(&key);
            let mut view = match tools.get(&key).cloned() {
                Some(existing) => existing,
                None => match views::stored_tool_view(
                    service,
                    hub,
                    state,
                    runtime.id.as_str(),
                    call_id.as_str(),
                )
                .await?
                {
                    Some(stored) => stored,
                    None => return Err(format!("tool call missing: {key}")),
                },
            };
            view.status = views::map_tool_outcome(&outcome);
            view.interaction_id = None;
            if let Some(metrics) = &metrics {
                view.started_at_ms = metrics.started_at_ms.max(0) as u64;
                view.finished_at_ms = Some(metrics.finished_at_ms.max(0) as u64);
                view.metrics = Some(views::map_metrics(metrics));
            }
            view.exec_session_id = exec_session_id
                .map(|value| ExecSessionId::new(value.as_str()).map_err(|error| error.to_string()))
                .transpose()?;
            view.output = match output {
                Some(value) => {
                    Some(content::content_ref(hub, state, runtime.id.as_str(), value).await?)
                }
                None => None,
            };
            view.output_truncated = output_truncated;
            view.diff = match diff {
                Some(value) => {
                    Some(content::content_ref(hub, state, runtime.id.as_str(), value).await?)
                }
                None => None,
            };
            view.code_delta = code_delta.as_ref().map(views::map_code_delta);
            view.effects = views::map_effects(&effects)?;
            tools.insert(key, view.clone());
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
        OldEvent::TimelineAppended { entry } => {
            if let old_state::TimelineEntry::AssistantBlock { block_id, .. } = &entry {
                blocks.remove(block_id.as_str());
            }
            some(SessionEvent::TimelineAppended {
                session_id: id,
                block: timeline::timeline_block(service, hub, state, runtime.id.as_str(), entry)
                    .await?,
            })
        }
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
            interaction_id: InteractionId::new(interaction_id.as_str())
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
            sealed_session_id,
            successor_session_id,
            turn_id,
            work_summary,
            at_ms,
            ..
        } => some(SessionEvent::HandoffSealed {
            session_id: id,
            sealed_session_id: SessionId::new(sealed_session_id)
                .map_err(|error| error.to_string())?,
            successor_session_id: SessionId::new(successor_session_id)
                .map_err(|error| error.to_string())?,
            turn_id: TurnId::new(turn_id.as_str()).map_err(|error| error.to_string())?,
            // Guaranteed Inline so UI handoff cards never need an extra blob fetch.
            summary: ContentRef::Inline { text: work_summary },
            at_ms,
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

pub(crate) fn map_usage(value: qaqh_types::UsageInfo) -> qaqh_core_vocab::UsageInfo {
    let cached_total = value
        .prompt_cache_hit_tokens
        .saturating_add(value.prompt_cache_miss_tokens);
    let input_tokens = if cached_total > 0 {
        cached_total as u64
    } else {
        value.prompt_tokens as u64
    };
    let cache_reported = value.cache_usage_reported == Some(true) || cached_total > 0;
    qaqh_core_vocab::UsageInfo {
        input_tokens,
        output_tokens: value.completion_tokens as u64,
        cached_input_tokens: cache_reported.then_some(value.prompt_cache_hit_tokens as u64),
        reasoning_tokens: (value.reasoning_tokens != 0).then_some(value.reasoning_tokens as u64),
    }
}
