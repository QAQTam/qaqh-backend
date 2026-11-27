//! Production Session, Command and ToolCatalog facade over the real service.

mod commands;
mod mappings;
mod team;
mod views;

use qaqh_core_vocab::ContentRef;
use qaqh_ringing_contract::fact::ContentRef as StoredContentRef;
use qaqh_ringing_v3::{
    self as v3, CommandRequest, CommandResponse, ErrorCode, RpcError, SessionRequest,
    SessionResponse, SkillsCatalogView, TodoItemView, TodoListSnapshot, ToolCatalogRequest,
    ToolCatalogResponse,
};

use super::{RealBackend, bad_request, internal, projection};
use crate::v3_server::facade::CallContext;
use crate::v3_server::identity::Scope;

use commands::{
    CreateSession, command_value, create, list, patch_settings, respond_interaction, send,
    stop_exec,
};
use mappings::{goal_status_to_stored, runtime_mode};
use team::team;
use views::{page, snapshot};

pub(crate) use mappings::{activity, goal_view, model_selection_stored, turn_terminal};
pub(crate) use views::block_view;

pub(crate) async fn handle_session(
    backend: &RealBackend,
    context: &CallContext,
    request: SessionRequest,
) -> Result<SessionResponse, RpcError> {
    match request {
        SessionRequest::Create {
            title,
            cwd,
            tool_mode,
            model_selection,
            custom_tools,
            handoff_from,
        } => {
            create(
                backend,
                context,
                CreateSession {
                    title,
                    cwd,
                    tool_mode,
                    model_selection,
                    custom_tools,
                    handoff_from,
                },
            )
            .await
        }
        SessionRequest::List { include_archived } => list(backend, context, include_archived).await,
        SessionRequest::GetSessionSnapshot {
            session_id,
            tail_blocks,
            tail_turns,
        } => {
            let runtime = backend.session_for(context, &session_id).await?;
            snapshot(backend, &runtime, tail_blocks, tail_turns)
                .await
                .map(|value| SessionResponse::Snapshot(Box::new(value)))
        }
        SessionRequest::TranscriptPage {
            session_id,
            before_ord,
            limit,
            timeline_epoch,
        } => {
            let runtime = backend.session_for(context, &session_id).await?;
            page(backend, &runtime, before_ord, limit, timeline_epoch)
                .await
                .map(|value| SessionResponse::Page(Box::new(value)))
        }
        SessionRequest::Rename { session_id, title } => {
            let runtime = backend.session_for(context, &session_id).await?;
            let (service, id, next) = (
                backend.service.clone(),
                session_id.as_str().to_string(),
                title.clone(),
            );
            RealBackend::blocking(move || service.rename_session(&id, &next)).await?;
            runtime.rename(title);
            Ok(SessionResponse::Ack)
        }
        SessionRequest::Archive { session_id } => {
            backend.session_for(context, &session_id).await?;
            let (service, id) = (backend.service.clone(), session_id.as_str().to_string());
            RealBackend::blocking(move || service.archive_session(&id, None)).await?;
            Ok(SessionResponse::Ack)
        }
        SessionRequest::Unarchive { session_id } => {
            backend.session_for(context, &session_id).await?;
            let (service, id) = (backend.service.clone(), session_id.as_str().to_string());
            RealBackend::blocking(move || service.unarchive_session(&id)).await?;
            Ok(SessionResponse::Ack)
        }
        SessionRequest::Delete { session_id } => {
            let runtime = backend.session_for(context, &session_id).await?;
            backend
                .service
                .delete_session(session_id.as_str())
                .await
                .map_err(internal)?;
            runtime.emit(v3::SessionEvent::SessionRemoved {
                session_id: session_id.clone(),
            });
            projection::notify_removed(&session_id);
            backend.state.remove_session(&session_id);
            Ok(SessionResponse::Ack)
        }
        SessionRequest::PatchSettings {
            session_id,
            patch,
            expected_revision,
        } => {
            let runtime = backend.session_for(context, &session_id).await?;
            patch_settings(backend, &runtime, patch, expected_revision).await?;
            Ok(SessionResponse::Ack)
        }
        SessionRequest::Truncate {
            session_id,
            turn_id,
        } => {
            backend.session_for(context, &session_id).await?;
            send(
                &backend.service,
                &session_id,
                qaqh_ringing::RingingCommand::ConversationTruncate {
                    turn_id: turn_id.as_str().to_string(),
                },
            )
            .await?;
            Ok(SessionResponse::Ack)
        }
        SessionRequest::SetGoal {
            session_id,
            objective,
            status,
            token_budget,
        } => {
            backend.session_for(context, &session_id).await?;
            command_value(
                &backend.service,
                &session_id,
                qaqh_ringing::RingingCommand::GoalSet {
                    objective: Some(objective),
                    status: status.map(goal_status_to_stored),
                    token_budget: Some(token_budget),
                },
            )
            .await?;
            Ok(SessionResponse::Ack)
        }
        SessionRequest::ClearGoal { session_id } => {
            backend.session_for(context, &session_id).await?;
            command_value(
                &backend.service,
                &session_id,
                qaqh_ringing::RingingCommand::GoalClear {},
            )
            .await?;
            Ok(SessionResponse::Ack)
        }
        SessionRequest::GetTodos { session_id } => {
            backend.session_for(context, &session_id).await?;
            let value = backend
                .json_call(
                    "todo.list",
                    serde_json::json!({"session_id": session_id.as_str()}),
                )
                .await?;
            let todos =
                serde_json::from_value::<qaqh_ringing_contract::todo::TodoListSnapshot>(value)
                    .map_err(|error| internal(format!("decode todo snapshot: {error}")))?;
            Ok(SessionResponse::Todos(TodoListSnapshot {
                items: todos
                    .items
                    .into_iter()
                    .map(|item| TodoItemView {
                        id: item.id,
                        title: item.title,
                        description: item.description,
                        status: item.status,
                        evidence: item.evidence,
                    })
                    .collect(),
                current_id: todos.current_id,
                counts: v3::TodoCountsView {
                    pending: todos.counts.pending as u64,
                    in_progress: todos.counts.in_progress as u64,
                    completed: todos.counts.completed as u64,
                    cancelled: todos.counts.cancelled as u64,
                    total: todos.counts.total as u64,
                },
            }))
        }
        SessionRequest::GetTeam { session_id } => {
            backend.session_for(context, &session_id).await?;
            team(backend, session_id)
                .await
                .map(|value| SessionResponse::Team(Box::new(value)))
        }
        SessionRequest::GetSkillsCatalog { cwd } => skills_catalog(backend, context, cwd).await,
        SessionRequest::SetToolMode { session_id, mode } => {
            let runtime = backend.session_for(context, &session_id).await?;
            let (service, id, name) = (
                backend.service.clone(),
                session_id.as_str().to_string(),
                runtime_mode(mode).to_string(),
            );
            RealBackend::blocking(move || service.set_tool_mode(&id, &name)).await?;
            runtime.set_tool_mode(mode);
            runtime.emit(v3::SessionEvent::CatalogChanged { session_id });
            Ok(SessionResponse::Ack)
        }
        SessionRequest::Attach {
            session_id,
            since_seq,
        } => {
            let runtime = backend.session_for(context, &session_id).await?;
            backend.ensure_projector_ready(&session_id).await?;
            if since_seq > runtime.bus.last_seq() {
                return Err(rpc(ErrorCode::StaleCursor, "since_seq 超过服务端当前水位"));
            }
            runtime.attach(&context.identity);
            context
                .attached_sessions
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(session_id);
            if runtime.driver().is_none() {
                runtime.claim_driver_if_vacant_and_emit(context.identity.driver_key());
            }
            Ok(SessionResponse::Ack)
        }
        SessionRequest::SwitchDriver {
            session_id,
            target_client_instance_id,
        } => {
            let runtime = backend.session_for(context, &session_id).await?;
            runtime
                .switch_driver_and_emit(&context.identity, target_client_instance_id.as_deref())?;
            Ok(SessionResponse::Ack)
        }
    }
}

pub(crate) async fn handle_command(
    backend: &RealBackend,
    context: &CallContext,
    request: CommandRequest,
) -> Result<CommandResponse, RpcError> {
    match request {
        CommandRequest::SendPrompt {
            session_id,
            input_id,
            text,
            attachments,
        } => {
            let runtime = backend.session_for(context, &session_id).await?;
            if !runtime.is_driver(&context.identity.driver_key()) {
                return Err(crate::v3_server::error::denied(
                    "只有当前驱动端可以投递用户输入",
                ));
            }
            if text.trim().is_empty() {
                return Err(bad_request("用户输入不能为空"));
            }
            let mut stored = Vec::with_capacity(attachments.len());
            for attachment in attachments {
                let ContentRef::Blob { hash, .. } = attachment else {
                    return Err(bad_request("附件必须是已上传的 Blob 引用"));
                };
                stored.push(StoredContentRef::new(
                    qaqh_ringing_contract::fact::ContentHash::new(format!(
                        "sha256:{}",
                        hash.to_hex()
                    )),
                ));
            }
            backend.ensure_projector_ready(&session_id).await?;
            send(
                &backend.service,
                &session_id,
                qaqh_ringing::RingingCommand::ConversationSendMessage(
                    qaqh_ringing::ConversationSendMessage {
                        text,
                        images: Vec::new(),
                        attachments: Some(stored),
                        message_id: Some(input_id.as_str().to_string()),
                        input_purpose: qaqh_ringing_contract::fact::InputPurpose::TriggerTurn,
                        as_system: false,
                        inter_agent: None,
                        subagent_terminal: None,
                        widget_context: None,
                    },
                ),
            )
            .await?;
            Ok(CommandResponse::Accepted { input_id })
        }
        CommandRequest::CancelTurn { session_id } => {
            backend.session_for(context, &session_id).await?;
            send(
                &backend.service,
                &session_id,
                qaqh_ringing::RingingCommand::ConversationCancel { turn_id: None },
            )
            .await?;
            Ok(CommandResponse::CancelAccepted)
        }
        CommandRequest::RespondInteraction {
            session_id,
            interaction_id,
            resolution,
        } => {
            backend.session_for(context, &session_id).await?;
            respond_interaction(backend, &session_id, &interaction_id, resolution).await
        }
        CommandRequest::ExecInterrupt { exec_session_id } => {
            stop_exec(
                backend,
                context,
                &exec_session_id,
                qaqh_ringing_contract::state::ExecStopReason::Interrupt,
            )
            .await
        }
        CommandRequest::ExecTerminate { exec_session_id } => {
            stop_exec(
                backend,
                context,
                &exec_session_id,
                qaqh_ringing_contract::state::ExecStopReason::Terminate,
            )
            .await
        }
    }
}

pub(crate) async fn handle_catalog(
    backend: &RealBackend,
    context: &CallContext,
    request: ToolCatalogRequest,
) -> Result<ToolCatalogResponse, RpcError> {
    let ToolCatalogRequest::Get { session_id } = request;
    backend.session_for(context, &session_id).await?;
    let service = backend.service.clone();
    let id = session_id.as_str().to_string();
    let catalog = RealBackend::blocking(move || {
        service
            .session_tool_catalog(&id)
            .map_err(|error| error.to_string())
    })
    .await?;
    Ok(ToolCatalogResponse::Catalog(Box::new(catalog)))
}

async fn skills_catalog(
    backend: &RealBackend,
    context: &CallContext,
    cwd: Option<String>,
) -> Result<SessionResponse, RpcError> {
    if context.identity.scope != Scope::Owner {
        let Some(cwd) = cwd.as_deref() else {
            return Err(crate::v3_server::error::denied(
                "paired devices must request a skills cwd",
            ));
        };
        let requested = std::path::PathBuf::from(cwd.to_string());
        let requested = RealBackend::blocking(move || {
            std::fs::canonicalize(requested)
                .map_err(|error| bad_request(format!("skills cwd is not accessible: {error}")))
        })
        .await?;
        let mut allowed = false;
        for session in backend.state.sessions() {
            if !session.can_access(&context.identity.device) {
                continue;
            }
            let service = backend.service.clone();
            let id = session.id.as_str().to_string();
            let root = RealBackend::blocking(move || {
                super::workspace::effective_session_workspace(&service, &id).map_err(internal)
            })
            .await?;
            let root = RealBackend::blocking(move || {
                std::fs::canonicalize(root)
                    .map_err(|error| internal(format!("canonicalize session workspace: {error}")))
            })
            .await?;
            if super::workspace::path_is_within(&requested, &root) {
                allowed = true;
                break;
            }
        }
        if !allowed {
            return Err(crate::v3_server::error::denied(
                "skills cwd is outside the device's accessible session workspaces",
            ));
        }
    }
    let params = match cwd {
        Some(cwd) => serde_json::json!({"cwd": cwd}),
        None => serde_json::json!({}),
    };
    let value = backend.json_call("skills.catalog", params).await?;
    let skills = serde_json::from_value::<
        qaqh_runtime::ringing::service_methods::SkillsCatalogResult,
    >(value)
    .map_err(|error| internal(format!("decode skills catalog: {error}")))?;
    Ok(SessionResponse::Skills(SkillsCatalogView {
        skills: skills
            .skills
            .into_iter()
            .map(|skill| v3::SkillCatalogEntry {
                name: skill.name,
                description: skill.description,
                scope: skill.scope,
                origin: skill.origin,
                path: skill.path,
            })
            .collect(),
        diagnostics: skills
            .diagnostics
            .into_iter()
            .map(|diagnostic| v3::SkillCatalogDiagnostic {
                severity: diagnostic.severity,
                path: diagnostic.path,
                message: diagnostic.message,
            })
            .collect(),
    }))
}

fn rpc(code: ErrorCode, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
        retryable: false,
    }
}
