//! 统一命令面 dispatch（统一切换设计 §8.1）。
//!
//! `POST /ringing/v2/commands`（无 channel id）接收
//! [`qaqh_ringing::UnifiedCommandEnvelope`]：强类型封闭命令目录 + 显式 target。
//! 业务执行直接进 `QaqhService` 的 typed application handlers
//! （`handlers/providers.rs` / `handlers/session.rs`），**不经过**
//! `handle(method, Value)` 字符串 decoder。回执复用 [`RingingV2CommandAck`]，
//! 同步完成的操作带 `result` / `operation` / `revision`。
//!
//! 授权基线（§8.6）：`provider.*` / `model.selection.set_default` 是全局配置
//! 管理 → Admin；`session.create` / `session.settings.patch` → Interact +
//! 会话归属（create 豁免——它是建立归属的入口）。

use axum::response::Response;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Extension;
use serde_json::json;

use qaqh_ringing::{
    UnifiedCommand, UnifiedCommandEnvelope, UnifiedPrecondition,
    UnifiedTarget,
};

use super::authz::Identity;
use super::command::fail;
use super::v2::{api_error_response, caller_lease_bound_to_identity, json_response};
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

pub(crate) async fn handle_unified_command(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let envelope: UnifiedCommandEnvelope = match serde_json::from_slice(&body) {
        Ok(envelope) => envelope,
        Err(error) => {
            return api_error_response(
                StatusCode::BAD_REQUEST,
                "invalid_body",
                &format!("invalid unified command envelope: {error}"),
            );
        }
    };
    if let Err(code) = envelope.validate() {
        return api_error_response(StatusCode::BAD_REQUEST, code, "invalid unified command envelope");
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
    if let UnifiedTarget::Session { session_id } = &envelope.target
        && !identity.is_admin()
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
    let fingerprint = unified_fingerprint(&envelope);
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

    dispatch(state, &identity, envelope).await
}

fn required_scope(command: &UnifiedCommand) -> Scope {
    match command {
        UnifiedCommand::SessionCreate { .. } | UnifiedCommand::SessionSettingsPatch { .. } => {
            Scope::Interact
        }
        // 全局配置 / provider 目录 / 默认模型选择：§8.6 → Admin。
        UnifiedCommand::ProviderList { .. }
        | UnifiedCommand::ProviderUpsert { .. }
        | UnifiedCommand::ProviderDelete { .. }
        | UnifiedCommand::ProviderModelUpsert { .. }
        | UnifiedCommand::ProviderModelDelete { .. }
        | UnifiedCommand::ProviderModelsDiscover { .. }
        | UnifiedCommand::ModelSelectionSetDefault { .. } => Scope::Admin,
    }
}

fn unified_fingerprint(envelope: &UnifiedCommandEnvelope) -> String {
    let payload = serde_json::to_string(&json!({
        "contract_revision": envelope.contract_revision,
        "target": envelope.target,
        "command": envelope.command,
        "precondition": envelope.precondition,
    }))
    .unwrap_or_default();
    qaqh_types::sha256_hex(payload.as_bytes())
}

/// 提取 provider 目录操作期望的资源 revision：显式字段优先，其次信封
/// precondition（expected_revision / config_revision）。
fn expected_revision_from(
    explicit: Option<u64>,
    precondition: Option<UnifiedPrecondition>,
) -> Option<u64> {
    explicit.or_else(|| {
        precondition.and_then(|precondition| match precondition {
            UnifiedPrecondition::ExpectedRevision { revision }
            | UnifiedPrecondition::ConfigRevision { revision } => Some(revision),
            _ => None,
        })
    })
}

async fn dispatch(
    state: AppState,
    _identity: &Identity,
    envelope: UnifiedCommandEnvelope,
) -> Response {
    let service = &state.service;
    let command_id = envelope.command_id.clone();
    let operation = operation_name(&envelope.command);

    // 同步操作直接 completed（§8.4）：回执带 typed result 与资源 revision。
    let outcome: Result<(serde_json::Value, Option<u64>), String> = match &envelope.command {
        UnifiedCommand::SessionCreate {
            cwd,
            title,
            model_selection,
            tool_mode,
            custom_tools,
        } => service
            .create_session_unified(
                cwd.clone(),
                title.clone(),
                model_selection.clone(),
                tool_mode.clone(),
                custom_tools.clone(),
            )
            .map(|value| (value, None))
            .map_err(|error| error.to_string()),
        UnifiedCommand::SessionSettingsPatch { patch } => {
            let UnifiedTarget::Session { session_id } = &envelope.target else {
                return fail_response(
                    &state,
                    command_id,
                    StatusCode::BAD_REQUEST,
                    "target_mismatch",
                    "session.settings.patch requires a session target",
                );
            };
            service
                .patch_session_settings(session_id, patch.clone())
                .map(|value| (value, None))
                .map_err(|error| error.to_string())
        }
        UnifiedCommand::ProviderList { .. } => service
            .provider_catalog()
            .map(|value| (value, None))
            .map_err(|error| error.to_string()),
        UnifiedCommand::ProviderUpsert { id, provider } => service
            .provider_upsert(id, provider)
            .map(|value| {
                let revision = catalog_revision(&value);
                (value, revision)
            })
            .map_err(|error| error.to_string()),
        UnifiedCommand::ProviderDelete { id } => service
            .provider_delete(id, expected_revision_from(None, envelope.precondition))
            .map(|value| {
                let revision = catalog_revision(&value);
                (value, revision)
            })
            .map_err(|error| error.to_string()),
        UnifiedCommand::ProviderModelUpsert {
            provider_id,
            model_id,
            expected_revision,
            model,
        } => service
            .provider_model_upsert(
                provider_id,
                model_id,
                expected_revision_from(Some(*expected_revision).filter(|r| *r != 0), envelope.precondition),
                model.clone(),
            )
            .map(|value| {
                let revision = catalog_revision(&value);
                (value, revision)
            })
            .map_err(|error| error.to_string()),
        UnifiedCommand::ProviderModelDelete {
            provider_id,
            model_id,
        } => service
            .provider_model_delete(
                provider_id,
                model_id,
                expected_revision_from(None, envelope.precondition),
            )
            .map(|value| {
                let revision = catalog_revision(&value);
                (value, revision)
            })
            .map_err(|error| error.to_string()),
        UnifiedCommand::ModelSelectionSetDefault {
            expected_revision,
            selection,
        } => service
            .set_default_model_selection(
                expected_revision_from(Some(*expected_revision).filter(|r| *r != 0), envelope.precondition),
                selection.clone(),
            )
            .map(|value| {
                let revision = catalog_revision(&value);
                (value, revision)
            })
            .map_err(|error| error.to_string()),
        // §6.3：实时上游发现属于后续扩展，本次不承诺、不静默折叠。
        UnifiedCommand::ProviderModelsDiscover { .. } => {
            return fail_response(
                &state,
                command_id,
                StatusCode::UNPROCESSABLE_ENTITY,
                "unsupported_operation",
                "provider.models.discover is not part of this contract revision (deferred)",
            );
        }
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

fn operation_name(command: &UnifiedCommand) -> &'static str {
    match command {
        UnifiedCommand::SessionCreate { .. } => "session.create",
        UnifiedCommand::SessionSettingsPatch { .. } => "session.settings.patch",
        UnifiedCommand::ProviderList { .. } => "provider.list",
        UnifiedCommand::ProviderUpsert { .. } => "provider.upsert",
        UnifiedCommand::ProviderDelete { .. } => "provider.delete",
        UnifiedCommand::ProviderModelUpsert { .. } => "provider.model.upsert",
        UnifiedCommand::ProviderModelDelete { .. } => "provider.model.delete",
        UnifiedCommand::ProviderModelsDiscover { .. } => "provider.models.discover",
        UnifiedCommand::ModelSelectionSetDefault { .. } => "model.selection.set_default",
    }
}

fn catalog_revision(result: &serde_json::Value) -> Option<u64> {
    result.get("revision").and_then(|v| v.as_u64())
}
