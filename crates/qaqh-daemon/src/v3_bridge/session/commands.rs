use std::sync::Arc;

use qaqh_core_vocab::{
    ExecSessionId, InteractionId, ModelSelection, SessionId, SettingOverride, ToolMode,
};
use qaqh_ringing_v3::{
    CommandInteractionResolution, CommandResponse, RpcError, SessionResponse, SessionSettingsPatch,
    SessionSummary, ToolsSettingPatch,
};
use qaqh_runtime::QaqhService;

use super::super::{RealBackend, bad_request, internal};
use super::mappings::{model_selection_stored, runtime_mode, summary_entry};
use crate::v3_server::facade::CallContext;
use crate::v3_server::session_runtime::SessionRuntime;

pub(super) struct CreateSession {
    pub(super) title: Option<String>,
    pub(super) cwd: Option<String>,
    pub(super) tool_mode: Option<ToolMode>,
    pub(super) model_selection: Option<ModelSelection>,
    pub(super) custom_tools: Option<Vec<String>>,
    pub(super) handoff_from: Option<SessionId>,
}

pub(super) async fn create(
    backend: &RealBackend,
    context: &CallContext,
    request: CreateSession,
) -> Result<SessionResponse, RpcError> {
    let CreateSession {
        title,
        cwd,
        tool_mode,
        model_selection,
        custom_tools,
        handoff_from,
    } = request;
    if cwd.is_some() {
        context.identity.require_owner("session.create.cwd")?;
    }
    if let Some(session_id) = &handoff_from {
        backend.session_for(context, session_id).await?;
    }
    if tool_mode != Some(ToolMode::Custom) && custom_tools.is_some() {
        return Err(bad_request("custom_tools 仅在 Custom 工具模式下有效"));
    }
    let service = backend.service.clone();
    let created = RealBackend::blocking(move || {
        service
            .create_session_command(
                cwd,
                title,
                model_selection.map(model_selection_stored),
                tool_mode.map(runtime_mode),
                custom_tools,
                handoff_from.map(|id| id.as_str().to_string()),
            )
            .map_err(|error| error.to_string())
    })
    .await?;
    let id = SessionId::new(created.session_id)
        .map_err(|error| internal(format!("invalid created session id: {error}")))?;
    let runtime = backend.state.register_session(
        id.clone(),
        &format!("os:{}", backend.state.identity.os_user()),
    );
    runtime.allow_device(&context.identity.device);
    backend.ensure_projector_ready(&id).await?;
    summary(backend, &id).await.map(SessionResponse::Created)
}

pub(super) async fn list(
    backend: &RealBackend,
    context: &CallContext,
    include_archived: bool,
) -> Result<SessionResponse, RpcError> {
    let service = backend.service.clone();
    let sessions = RealBackend::blocking(move || {
        service
            .list_sessions(include_archived)
            .map_err(|error| error.to_string())
    })
    .await?;
    let mut visible = Vec::new();
    for entry in sessions {
        let id = SessionId::new(entry.meta.session_id.clone())
            .map_err(|error| internal(format!("invalid persisted session id: {error}")))?;
        let Some(runtime) = backend.ensure_session_runtime(&id).await? else {
            continue;
        };
        if runtime.can_access(&context.identity.device) {
            visible.push(summary_entry(entry)?);
        }
    }
    Ok(SessionResponse::Sessions(visible))
}

pub(super) async fn summary(
    backend: &RealBackend,
    id: &SessionId,
) -> Result<SessionSummary, RpcError> {
    let service = backend.service.clone();
    let raw = id.as_str().to_string();
    let sessions = RealBackend::blocking(move || {
        service
            .list_sessions(true)
            .map_err(|error| error.to_string())
    })
    .await?;
    let entry = sessions
        .into_iter()
        .find(|entry| entry.meta.session_id == raw)
        .ok_or_else(|| crate::v3_server::error::not_found(format!("会话 `{id}` 不存在")))?;
    summary_entry(entry)
}

pub(super) async fn patch_settings(
    backend: &RealBackend,
    runtime: &Arc<SessionRuntime>,
    patch: SessionSettingsPatch,
    expected_revision: u64,
) -> Result<(), RpcError> {
    let old = qaqh_ringing::SessionSettingsPatch {
        model_selection: patch.model_selection.map(|value| match value {
            SettingOverride::Use(value) => qaqh_ringing::SettingValue::Use {
                value: model_selection_stored(value),
            },
            SettingOverride::Inherit => qaqh_ringing::SettingValue::Inherit,
        }),
        tools: patch.tools.map(|tools| match tools {
            ToolsSettingPatch::Core => qaqh_ringing::ToolsSettingPatch::Preset {
                name: "minimal".into(),
            },
            ToolsSettingPatch::Standard => qaqh_ringing::ToolsSettingPatch::All,
            ToolsSettingPatch::Custom { tools } => {
                qaqh_ringing::ToolsSettingPatch::Custom { names: tools }
            }
        }),
        team_mode: patch.team_mode,
    };
    let (service, id) = (backend.service.clone(), runtime.id.as_str().to_string());
    RealBackend::blocking(move || {
        service
            .patch_session_settings(&id, old, expected_revision)
            .map_err(|error| error.to_string())
    })
    .await?;
    runtime.emit(qaqh_ringing_v3::SessionEvent::CatalogChanged {
        session_id: runtime.id.clone(),
    });
    Ok(())
}

pub(super) async fn respond_interaction(
    backend: &RealBackend,
    session_id: &SessionId,
    interaction_id: &InteractionId,
    resolution: CommandInteractionResolution,
) -> Result<CommandResponse, RpcError> {
    let service = backend.service.clone();
    let raw = session_id.as_str().to_string();
    let wanted = interaction_id.as_str().to_string();
    let interaction = RealBackend::blocking(move || {
        let interactions = service
            .owners()
            .db()
            .read_blocking(|conn| qaqh_session::db::store::pending_interactions(conn, &raw))
            .map_err(|error| error.to_string())?;
        interactions
            .into_iter()
            .find(|row| row.interaction_id == wanted)
            .ok_or_else(|| format!("interaction not found: {wanted}"))
    })
    .await?;
    let call_id = interaction.call_id.clone();
    let command = match (interaction.kind.as_str(), resolution) {
        ("permission", CommandInteractionResolution::Approve { scope }) => {
            qaqh_ringing::RingingCommand::ToolPermissionRespond {
                tool_call_id: call_id
                    .ok_or_else(|| internal("permission interaction 缺 call_id"))?,
                approved: true,
                trust_folder: scope == qaqh_core_vocab::PermissionGrantScope::Persistent,
                scope: Some(permission_scope(scope)),
            }
        }
        ("permission", CommandInteractionResolution::Reject { .. }) => {
            qaqh_ringing::RingingCommand::ToolPermissionRespond {
                tool_call_id: call_id
                    .ok_or_else(|| internal("permission interaction 缺 call_id"))?,
                approved: false,
                trust_folder: false,
                scope: None,
            }
        }
        ("ask", CommandInteractionResolution::Answer { answers }) => {
            qaqh_ringing::RingingCommand::InteractionAskRespond {
                interaction_id: interaction_id.as_str().to_string(),
                answers: answers
                    .into_iter()
                    .map(|answer| qaqh_ringing_contract::commands::AskAnswer {
                        question_id: answer.question_id,
                        answer: answer.answer,
                    })
                    .collect(),
            }
        }
        ("ask", CommandInteractionResolution::Dismiss) => {
            qaqh_ringing::RingingCommand::InteractionAskDismiss {
                interaction_id: interaction_id.as_str().to_string(),
            }
        }
        (kind, _) => return Err(bad_request(format!("交互种类 `{kind}` 不支持该处置"))),
    };
    send(&backend.service, session_id, command).await?;
    Ok(CommandResponse::InteractionAccepted)
}

pub(super) async fn stop_exec(
    backend: &RealBackend,
    context: &CallContext,
    id: &ExecSessionId,
    reason: qaqh_ringing_contract::state::ExecStopReason,
) -> Result<CommandResponse, RpcError> {
    let service = backend.service.clone();
    let raw = id.as_str().to_string();
    let owner = RealBackend::blocking(move || {
        service.exec_session_owner(&qaqh_types::ExecSessionId::new(raw))
    })
    .await?
    .ok_or_else(|| crate::v3_server::error::not_found(format!("执行会话 `{id}` 不存在")))?;
    let session_id = SessionId::new(owner).map_err(|error| internal(error.to_string()))?;
    backend.session_for(context, &session_id).await?;
    let (service, owner, id) = (
        backend.service.clone(),
        session_id.as_str().to_string(),
        qaqh_types::ExecSessionId::new(id.as_str()),
    );
    RealBackend::blocking(move || service.stop_exec_session(&owner, &id, reason).map(|_| ()))
        .await?;
    Ok(CommandResponse::CancelAccepted)
}

pub(super) async fn send(
    service: &QaqhService,
    session_id: &SessionId,
    command: qaqh_ringing::RingingCommand,
) -> Result<(), RpcError> {
    let service = service.clone();
    let id = session_id.as_str().to_string();
    let envelope = qaqh_ringing::RingingWorkerCommandEnvelope::new(
        id.clone(),
        format!("v3-{}", qaqh_ringing_contract::ulid::generate_ulid()),
        command,
    );
    RealBackend::blocking(move || {
        service
            .send_ringing_command(&id, &envelope)
            .map_err(|error| error.to_string())
    })
    .await
}

pub(super) async fn command_value(
    service: &QaqhService,
    session_id: &SessionId,
    command: qaqh_ringing::RingingCommand,
) -> Result<(), RpcError> {
    let service = service.clone();
    let id = session_id.as_str().to_string();
    RealBackend::blocking(move || {
        service
            .execute_ringing_command(
                &qaqh_ringing::CommandTarget::Session { session_id: id },
                &command,
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
    })
    .await
}

fn permission_scope(
    scope: qaqh_core_vocab::PermissionGrantScope,
) -> qaqh_ringing_contract::fact::PermissionGrantScope {
    match scope {
        qaqh_core_vocab::PermissionGrantScope::Once => {
            qaqh_ringing_contract::fact::PermissionGrantScope::Once
        }
        qaqh_core_vocab::PermissionGrantScope::Session => {
            qaqh_ringing_contract::fact::PermissionGrantScope::Session
        }
        qaqh_core_vocab::PermissionGrantScope::Persistent => {
            qaqh_ringing_contract::fact::PermissionGrantScope::Persistent
        }
    }
}
