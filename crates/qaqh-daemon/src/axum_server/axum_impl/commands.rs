//! RingingV2 common command ingress and synchronous mutation dispatch.
//!
//! `POST /ringing/v2/commands`（无 channel id）接收
//! [`qaqh_ringing::RingingV2CommandEnvelope`]：唯一命令目录 + 显式 target。
//! 业务执行直接进 `QaqhService` 的 typed application handlers
//! （`handlers/providers.rs` / `handlers/session.rs`），**不经过**
//! `handle(method, Value)` 字符串 decoder。回执复用 [`RingingV2CommandAck`]，
//! 同步完成的操作带 `result` / `operation` / `revision`。
//!
//! 授权基线（§8.6）：`provider.*` / `model.selection.set_default` 是全局配置
//! 管理 → Admin；`session.create` / `session.settings.patch` → Interact +
//! 会话归属（create 豁免——它是建立归属的入口）。

use axum::Extension;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::json;

use qaqh_ringing::{
    CommandTarget, RingingCommand, RingingCommandAckStatus, RingingV2CommandAck,
    RingingV2CommandEnvelope,
};

use super::authz::Identity;
use super::command::fail;
use super::v2::{
    api_error_response, caller_lease_bound_to_identity, existing_v2_receipt, json_response,
    resolved_interaction_existing,
};
use super::{AppState, Scope};

/// `fail` 的 Response 包装：回执置终态并按 HTTP 状态返回。
fn fail_response(
    state: &AppState,
    command_id: String,
    status: StatusCode,
    code: &str,
    message: impl Into<String>,
) -> Response {
    let (status, ack) = fail(state, command_id, status, code, message);
    json_response(status, &ack)
}

pub(crate) async fn handle_command(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let envelope: RingingV2CommandEnvelope = match serde_json::from_slice(&body) {
        Ok(envelope) => envelope,
        Err(error) => {
            return api_error_response(
                StatusCode::BAD_REQUEST,
                "invalid_body",
                &format!("invalid RingingV2 command envelope: {error}"),
            );
        }
    };
    if let Err(code) = envelope.validate() {
        return api_error_response(
            StatusCode::BAD_REQUEST,
            code,
            "invalid RingingV2 command envelope",
        );
    }
    if let RingingCommand::ConversationSendMessage(message) = &envelope.command
        && (message.as_system
            || message.inter_agent.is_some()
            || message.subagent_terminal.is_some())
    {
        return api_error_response(
            StatusCode::BAD_REQUEST,
            "trusted_invocation_required",
            "system and inter-agent metadata can only be supplied by trusted runtime callers",
        );
    }

    // 身份与租约：principal 来自 Bearer/设备认证；租约绑定调用方 client session。
    let Some(client_session) = caller_lease_bound_to_identity(&state, &headers, &identity) else {
        return json_response(
            StatusCode::UNAUTHORIZED,
            &super::command::reject_ack(
                envelope.command_id.clone(),
                "lease_required",
                "open a Ringing v2 client session first".into(),
            ),
        );
    };

    // scope + 资源授权：全局配置管理一律 Admin；会话操作需要归属（admin 豁免）。
    let required = required_scope(&envelope.command);
    if !identity.scope().at_least(required) {
        return json_response(
            StatusCode::FORBIDDEN,
            &super::command::reject_ack(
                envelope.command_id.clone(),
                "insufficient_scope",
                format!("command requires {required:?} scope"),
            ),
        );
    }
    if let CommandTarget::Session { session_id } = &envelope.target
        && !identity.is_admin()
        && !matches!(
            envelope.command,
            RingingCommand::SessionAttach {} | RingingCommand::SessionResume {}
        )
    {
        let owns = state
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owns_session(&client_session, session_id);
        if !owns {
            return json_response(
                StatusCode::FORBIDDEN,
                &super::command::reject_ack(
                    envelope.command_id.clone(),
                    "forbidden_not_owner",
                    "lease does not own the target session".into(),
                ),
            );
        }
    }

    // 幂等指纹：target + command + precondition 的规范化摘要（§8.5）。
    let fingerprint = super::command::command_fingerprint(&envelope);
    if let Some(existing) = existing_v2_receipt(&state, &headers, &envelope.command_id) {
        if existing.payload_fingerprint != fingerprint {
            return json_response(
                StatusCode::CONFLICT,
                &super::command::reject_ack(
                    envelope.command_id.clone(),
                    "duplicate_command_mismatch",
                    "command_id was already used with another payload".into(),
                ),
            );
        }
        let mut ack = super::command::accept_ack(
            envelope.command_id.clone(),
            Some("duplicate command_id (already recorded)".into()),
        );
        ack.existing = Some(existing.into_existing());
        return json_response(StatusCode::OK, &ack);
    }
    if let CommandTarget::Session { session_id } = &envelope.target
        && let Some(existing) =
            resolved_interaction_existing(&state, session_id, &envelope.command).await
    {
        return json_response(
            StatusCode::OK,
            &RingingV2CommandAck {
                command_id: envelope.command_id.clone(),
                status: RingingCommandAckStatus::Rejected,
                code: Some("interaction_already_resolved".into()),
                message: Some("interaction was already resolved".into()),
                retry_after_ms: None,
                existing: Some(existing),
                result: None,
                operation: None,
                revision: None,
            },
        );
    }

    if !matches!(
        &envelope.command,
        RingingCommand::SessionCreate { .. }
            | RingingCommand::SessionSettingsPatch { .. }
            | RingingCommand::ProviderList { .. }
            | RingingCommand::ProviderUpsert { .. }
            | RingingCommand::ProviderDelete { .. }
            | RingingCommand::ProviderModelUpsert { .. }
            | RingingCommand::ProviderModelDelete { .. }
            | RingingCommand::ModelSelectionSetDefault { .. }
            | RingingCommand::ConfigSave { .. }
            | RingingCommand::ConfigSetPermissionLevel { .. }
            | RingingCommand::ProfileCreate { .. }
            | RingingCommand::ProfileSetDefault { .. }
            | RingingCommand::ProfileApply { .. }
            | RingingCommand::ProfileSaveCurrent { .. }
            | RingingCommand::ProfileDelete { .. }
            | RingingCommand::WorkspaceSet { .. }
            | RingingCommand::WorkspaceCreate { .. }
            | RingingCommand::WorkspaceRename { .. }
            | RingingCommand::WorkspaceDelete {}
            | RingingCommand::WorkspaceMoveSession { .. }
            | RingingCommand::WorkspaceDetach {}
            | RingingCommand::SubagentSpawn { .. }
            | RingingCommand::TodoSet { .. }
            | RingingCommand::TodoCancel { .. }
            | RingingCommand::PlanAction { .. }
            | RingingCommand::DiagnosticsMemoryStart {}
            | RingingCommand::DiagnosticsMemoryStop {}
    ) {
        let (status, ack) = super::execute_command(&state, &headers, &identity, envelope).await;
        return json_response(status, &ack);
    }

    {
        let owner_key = state
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .instance_for_session(&client_session)
            .unwrap_or_else(|| client_session.clone());
        let mut pending = state.pending.lock().unwrap_or_else(|e| e.into_inner());
        match pending.record_fingerprint_for_session(&envelope.command_id, &fingerprint, &owner_key)
        {
            Ok(first) if first => {}
            Ok(_) => {
                return json_response(
                    StatusCode::OK,
                    &super::command::accept_ack(
                        envelope.command_id,
                        Some("duplicate command_id (already accepted)".into()),
                    ),
                );
            }
            Err(()) => {
                return json_response(
                    StatusCode::CONFLICT,
                    &super::command::reject_ack(
                        envelope.command_id,
                        "duplicate_command_mismatch",
                        "command_id was already used with another payload".into(),
                    ),
                );
            }
        }
    }

    dispatch(state, &client_session, &identity, envelope).await
}

fn required_scope(command: &RingingCommand) -> Scope {
    match command {
        RingingCommand::SessionAttach {} => Scope::View,
        RingingCommand::SessionCreate { .. } | RingingCommand::SessionSettingsPatch { .. } => {
            Scope::Interact
        }
        // 全局配置 / provider 目录 / 默认模型选择：§8.6 → Admin。
        RingingCommand::ProviderList { .. }
        | RingingCommand::ProviderUpsert { .. }
        | RingingCommand::ProviderDelete { .. }
        | RingingCommand::ProviderModelUpsert { .. }
        | RingingCommand::ProviderModelDelete { .. }
        | RingingCommand::ModelSelectionSetDefault { .. } => Scope::Admin,
        RingingCommand::DiagnosticsMemoryStart {} | RingingCommand::DiagnosticsMemoryStop {} => {
            Scope::Admin
        }
        _ => Scope::Interact,
    }
}

/// 提取 provider 目录操作期望的资源 revision：显式字段优先，其次信封
/// precondition 中的 config_revision。
fn expected_revision_from(envelope: &RingingV2CommandEnvelope) -> Option<u64> {
    envelope.precondition("config_revision")
}

async fn dispatch(
    state: AppState,
    client_session: &str,
    _identity: &Identity,
    envelope: RingingV2CommandEnvelope,
) -> Response {
    let service = &state.service;
    let command_id = envelope.command_id.clone();
    let operation = envelope.command.name();

    // 同步操作直接 completed（§8.4）：回执带 typed result 与资源 revision。
    let outcome: Result<(serde_json::Value, Option<u64>), String> = match &envelope.command {
        RingingCommand::SessionCreate {
            cwd,
            title,
            profile,
            effort,
            model_selection,
            tool_mode,
            custom_tools,
        } => {
            let created = match service.create_session_command(
                cwd.clone(),
                title.clone(),
                profile.clone(),
                effort.clone(),
                model_selection.clone(),
                tool_mode.clone(),
                custom_tools.clone(),
            ) {
                Ok(created) => created,
                Err(error) => {
                    return fail_response(
                        &state,
                        command_id,
                        StatusCode::BAD_GATEWAY,
                        "operation_failed",
                        error.to_string(),
                    );
                }
            };
            let attached = state
                .leases
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .attach_session(client_session, &created.session_id);
            if !attached {
                return fail_response(
                    &state,
                    command_id,
                    StatusCode::UNAUTHORIZED,
                    "lease_required",
                    "lease is not active",
                );
            }
            match serde_json::to_value(created) {
                Ok(result) => Ok((result, None)),
                Err(error) => Err(error.to_string()),
            }
        }
        RingingCommand::SessionSettingsPatch { patch } => {
            let CommandTarget::Session { session_id } = &envelope.target else {
                return fail_response(
                    &state,
                    command_id,
                    StatusCode::BAD_REQUEST,
                    "target_mismatch",
                    "session.settings.patch requires a session target",
                );
            };
            let revision = envelope
                .precondition("settings_revision")
                .expect("validated settings revision precondition");
            match service.patch_session_settings(session_id, patch.clone(), revision) {
                Ok(revision) => Ok((json!({ "settings_revision": revision }), Some(revision))),
                Err(qaqh_runtime::QaqhError::Conflict { message }) => {
                    return fail_response(
                        &state,
                        command_id,
                        StatusCode::CONFLICT,
                        "settings_revision_conflict",
                        message,
                    );
                }
                Err(error) => Err(error.to_string()),
            }
        }
        RingingCommand::ProviderList { .. } => service
            .provider_catalog()
            .map(|value| (value, None))
            .map_err(|error| error.to_string()),
        RingingCommand::ProviderUpsert { id, provider } => service
            .provider_upsert(id, provider, expected_revision_from(&envelope))
            .map(|value| {
                let revision = catalog_revision(&value);
                (value, revision)
            })
            .map_err(|error| error.to_string()),
        RingingCommand::ProviderDelete { id } => service
            .provider_delete(id, expected_revision_from(&envelope))
            .map(|value| {
                let revision = catalog_revision(&value);
                (value, revision)
            })
            .map_err(|error| error.to_string()),
        RingingCommand::ProviderModelUpsert {
            provider_id,
            model_id,
            model,
        } => service
            .provider_model_upsert(
                provider_id,
                model_id,
                expected_revision_from(&envelope),
                model.clone(),
            )
            .map(|value| {
                let revision = catalog_revision(&value);
                (value, revision)
            })
            .map_err(|error| error.to_string()),
        RingingCommand::ProviderModelDelete {
            provider_id,
            model_id,
        } => service
            .provider_model_delete(provider_id, model_id, expected_revision_from(&envelope))
            .map(|value| {
                let revision = catalog_revision(&value);
                (value, revision)
            })
            .map_err(|error| error.to_string()),
        RingingCommand::ModelSelectionSetDefault { selection } => service
            .set_default_model_selection(expected_revision_from(&envelope), selection.clone())
            .map(|value| {
                let revision = catalog_revision(&value);
                (value, revision)
            })
            .map_err(|error| error.to_string()),
        RingingCommand::ConfigSave { .. }
        | RingingCommand::ConfigSetPermissionLevel { .. }
        | RingingCommand::ProfileCreate { .. }
        | RingingCommand::ProfileSetDefault { .. }
        | RingingCommand::ProfileApply { .. }
        | RingingCommand::ProfileSaveCurrent { .. }
        | RingingCommand::ProfileDelete { .. }
        | RingingCommand::WorkspaceSet { .. }
        | RingingCommand::WorkspaceCreate { .. }
        | RingingCommand::WorkspaceRename { .. }
        | RingingCommand::WorkspaceDelete {}
        | RingingCommand::WorkspaceMoveSession { .. }
        | RingingCommand::WorkspaceDetach {}
        | RingingCommand::SubagentSpawn { .. }
        | RingingCommand::TodoSet { .. }
        | RingingCommand::TodoCancel { .. }
        | RingingCommand::PlanAction { .. }
        | RingingCommand::DiagnosticsMemoryStart {}
        | RingingCommand::DiagnosticsMemoryStop {} => service
            .execute_ringing_command(&envelope.target, &envelope.command)
            .map(|value| (value, None))
            .map_err(|error| error.to_string()),
        _ => unreachable!("command router selected a non-synchronous operation"),
    };

    match outcome {
        Ok((result, revision)) => {
            let mut ack = super::command::accept_ack(command_id, None);
            ack.result = Some(result);
            ack.operation = Some(operation.to_string());
            ack.revision = revision;
            state
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .mark_terminal(
                    &ack.command_id,
                    qaqh_ringing::RingingCommandState::Succeeded,
                    None,
                    None,
                );
            json_response(StatusCode::OK, &ack)
        }
        Err(error) => fail_response(
            &state,
            command_id,
            StatusCode::BAD_GATEWAY,
            "operation_failed",
            error,
        ),
    }
}

fn catalog_revision(result: &serde_json::Value) -> Option<u64> {
    result.get("revision").and_then(|v| v.as_u64())
}
