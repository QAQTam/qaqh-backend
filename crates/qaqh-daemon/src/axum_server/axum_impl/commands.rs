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
use super::test_hooks::InteractionFault;
use super::v2::{
    ResolvedInteraction, api_error_response, caller_lease_bound_to_identity, existing_v2_receipt,
    json_response, resolved_interaction_existing,
};
use super::{AppState, Scope};

/// P1-0a：同步分派臂统一迁阻塞池。这些调用点持 `registry` / `config_io_lock` /
/// `secrets.toml.lock` / `TODO_LOCK` 跨同步写盘或同步 IPC 等待（`L-01` 复合停摆的
/// worker 暴露面）；join error 如实上报 `operation_failed`，语义对齐
/// `service_api.rs` 的 PR #28 先例。
async fn block_on_command<T, F>(what: &str, f: F) -> Result<T, qaqh_runtime::QaqhError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, qaqh_runtime::QaqhError> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|join_error| {
            Err(qaqh_runtime::QaqhError::message(format!(
                "{what} worker failed: {join_error}"
            )))
        })
}

/// 回执预留 admission 的结果：persist 边界进阻塞池后 early-return 无法在闭包内
/// 携带，用枚举把判定结果带回 async 侧再出回执。
enum Admission {
    First,
    Duplicate,
    Mismatch,
}

/// `fail` 的 Response 包装：回执置终态并按 HTTP 状态返回。
///
/// P1-0a：`fail` 内的回执 rollback 走 persist（全量写盘），本包装随之转 async。
async fn fail_response(
    state: &AppState,
    command_id: String,
    status: StatusCode,
    code: &str,
    message: impl Into<String>,
) -> Response {
    let (status, ack) = fail(state, command_id, status, code, message).await;
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

    execute_command(&state, &headers, &identity, envelope).await
}

/// Shared ingress for HTTP commands and daemon-generated approval responses.
pub(crate) async fn execute_command(
    state: &AppState,
    headers: &HeaderMap,
    identity: &Identity,
    mut envelope: RingingV2CommandEnvelope,
) -> Response {
    if let Err(code) = envelope.validate() {
        return api_error_response(
            StatusCode::BAD_REQUEST,
            code,
            "invalid RingingV2 command envelope",
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
    if let CommandTarget::ExecSession { id } = &envelope.target {
        // P1-0a：owner 反查同步取 registry 锁（进程句柄读），迁阻塞池。
        let service = state.service.clone();
        let exec_id = id.clone();
        let owner_lookup = tokio::task::spawn_blocking(move || service.exec_session_owner(&exec_id))
            .await
            .expect("exec owner lookup task must not panic");
        let owner_session_id = match owner_lookup {
            Ok(Some(owner_session_id)) => owner_session_id,
            Ok(None) => {
                return json_response(
                    StatusCode::NOT_FOUND,
                    &super::command::reject_ack(
                        envelope.command_id.clone(),
                        "exec_session_not_found",
                        "execution session was not found".into(),
                    ),
                );
            }
            Err(error) => {
                return fail_response(
                    &state,
                    envelope.command_id.clone(),
                    StatusCode::BAD_GATEWAY,
                    "operation_failed",
                    error,
                )
                .await;
            }
        };
        if !identity.is_admin()
            && !state
                .leases
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .owns_session(&client_session, &owner_session_id)
        {
            return json_response(
                StatusCode::FORBIDDEN,
                &super::command::reject_ack(
                    envelope.command_id.clone(),
                    "forbidden_not_owner",
                    "lease does not own the execution session's conversation".into(),
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
    if let CommandTarget::Session { session_id } = &envelope.target {
        match resolved_interaction_existing(&state, session_id, &envelope.command).await {
            ResolvedInteraction::Verdict(existing) => {
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
            ResolvedInteraction::None => {}
            // 投影读不出来时**不能走 fail()**：那会写一条终态回执，等于宣称我们知道结果。
            // 错误码复用 `state_unavailable`（与 handle_pending_approvals_v2 里同一条件的
            // 用法同义：canonical projection 不可读），不新造词表；且不落回执，客户端按
            // outcome_unknown 用原 command_id 复查/重试
            // （coverage.json 的 receipt_policy.semantics.outcome_unknown）。
            ResolvedInteraction::Unavailable(error) => {
                return api_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "state_unavailable",
                    &format!("canonical control projection for {session_id}: {error}"),
                );
            }
        }
    }

    {
        let owner_key = state
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .instance_for_session(&client_session)
            .unwrap_or_else(|| client_session.clone());
        // P1-0a：`record_fingerprint_for_session` 触发回执 persist（
        // `ringing-command-receipts.json` 全量覆写，L-06），预留闸门随终态
        // 持久化一并迁阻塞池，不在 Tokio worker 上持 `pending` 锁写盘。
        let pending = state.pending.clone();
        let admission_command_id = envelope.command_id.clone();
        let admission_fingerprint = fingerprint.clone();
        let admission = tokio::task::spawn_blocking(move || {
            match pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .record_fingerprint_for_session(
                    &admission_command_id,
                    &admission_fingerprint,
                    &owner_key,
                )
            {
                Ok(first) if first => Admission::First,
                Ok(_) => Admission::Duplicate,
                Err(()) => Admission::Mismatch,
            }
        })
        .await
        .expect("receipt admission task must not panic");
        match admission {
            Admission::First => {}
            Admission::Duplicate => {
                return json_response(
                    StatusCode::OK,
                    &super::command::accept_ack(
                        envelope.command_id,
                        Some("duplicate command_id (already accepted)".into()),
                    ),
                );
            }
            Admission::Mismatch => {
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

    state
        .test_hooks
        .apply_command_ack_fault(&envelope.command)
        .await;
    if let Some(fault) = state.test_hooks.take_interaction_fault(&envelope.command) {
        match fault {
            InteractionFault::PermissionDeny => {
                if let qaqh_ringing::RingingCommand::ToolPermissionRespond {
                    approved,
                    trust_folder,
                    ..
                } = &mut envelope.command
                {
                    *approved = false;
                    *trust_folder = false;
                }
            }
            InteractionFault::PermissionHang => {
                if matches!(
                    &envelope.command,
                    qaqh_ringing::RingingCommand::ToolPermissionRespond { .. }
                ) {
                    std::future::pending::<()>().await;
                }
            }
            InteractionFault::AskDismiss => {
                if let qaqh_ringing::RingingCommand::InteractionAskRespond {
                    interaction_id, ..
                } = &envelope.command
                {
                    envelope.command = qaqh_ringing::RingingCommand::InteractionAskDismiss {
                        interaction_id: interaction_id.clone(),
                    };
                }
            }
            InteractionFault::AskHang => {
                if matches!(
                    &envelope.command,
                    qaqh_ringing::RingingCommand::InteractionAskRespond { .. }
                ) {
                    std::future::pending::<()>().await;
                }
            }
        }
    }
    dispatch(state, &client_session, identity, envelope).await
}

fn required_scope(command: &RingingCommand) -> Scope {
    match command {
        RingingCommand::SessionAttach {} => Scope::View,
        RingingCommand::SessionCreate { .. } | RingingCommand::SessionSettingsPatch { .. } => {
            Scope::Interact
        }
        // 全局配置 / provider 目录 / 默认模型选择：§8.6 → Admin。
        RingingCommand::ProviderList { .. }
        | RingingCommand::ProviderPresets { .. }
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
    state: &AppState,
    client_session: &str,
    identity: &Identity,
    envelope: RingingV2CommandEnvelope,
) -> Response {
    let service = &state.service;
    let command_id = envelope.command_id.clone();
    let operation = envelope.command.name();

    // 同步操作直接 completed（§8.4）：回执带 typed result 与资源 revision。
    // 错误保留 QaqhError 原型——Conflict（目录/会话修订 CAS）要在收口处映射
    // 409，不能在这里 to_string 掉变体、把并发修订伪装成 502 operation_failed。
    let outcome: Result<(serde_json::Value, Option<u64>), qaqh_runtime::QaqhError> = match &envelope
        .command
    {
        RingingCommand::SessionCreate {
            cwd,
            title,
            model_selection,
            tool_mode,
            custom_tools,
            handoff_from,
        } => {
            // P1-0a：同步分派臂（含回执持久化边界）统一迁阻塞池，见
            // `block_on_command`；lease attach 是纯内存判定，留在原路径。
            let service = service.clone();
            let cwd = cwd.clone();
            let title = title.clone();
            let model_selection = model_selection.clone();
            let tool_mode = tool_mode.clone();
            let custom_tools = custom_tools.clone();
            let handoff_from = handoff_from.clone();
            let created = match block_on_command("session.create", move || {
                service.create_session_command(
                    cwd,
                    title,
                    model_selection,
                    tool_mode,
                    custom_tools,
                    handoff_from,
                )
            })
            .await
            {
                Ok(created) => created,
                Err(error) => {
                    return fail_response(
                        &state,
                        command_id,
                        StatusCode::BAD_GATEWAY,
                        "operation_failed",
                        error.to_string(),
                    )
                    .await;
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
                )
                .await;
            }
            match serde_json::to_value(created) {
                Ok(result) => Ok((result, None)),
                Err(error) => Err(qaqh_runtime::QaqhError::message(format!(
                    "serialize session.create result: {error}"
                ))),
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
                )
                .await;
            };
            let revision = envelope
                .precondition("settings_revision")
                .expect("validated settings revision precondition");
            let service = service.clone();
            let session_id = session_id.clone();
            let patch = patch.clone();
            match block_on_command("session.settings.patch", move || {
                service.patch_session_settings(&session_id, patch, revision)
            })
            .await
            {
                Ok(revision) => Ok((json!({ "settings_revision": revision }), Some(revision))),
                Err(qaqh_runtime::QaqhError::Conflict { message }) => {
                    return fail_response(
                        &state,
                        command_id,
                        StatusCode::CONFLICT,
                        "settings_revision_conflict",
                        message,
                    )
                    .await;
                }
                Err(error) => Err(error),
            }
        }
        RingingCommand::ProviderList { .. } => {
            let service = service.clone();
            block_on_command("provider.list", move || {
                service.provider_catalog().map(|value| (value, None))
            })
            .await
        }
        RingingCommand::ProviderPresets { .. } => {
            let service = service.clone();
            block_on_command("provider.presets", move || {
                service.provider_presets().map(|value| (value, None))
            })
            .await
        }
        RingingCommand::ExecSessionInterrupt {} | RingingCommand::ExecSessionTerminate {} => {
            let CommandTarget::ExecSession { id } = &envelope.target else {
                return fail_response(
                    &state,
                    command_id,
                    StatusCode::BAD_REQUEST,
                    "target_mismatch",
                    "exec session stop commands require an execution-session target",
                )
                .await;
            };
            // P1-0a：owner 反查与 stop 都同步取 registry 锁并操作进程句柄，迁阻塞池。
            let lookup_service = service.clone();
            let exec_id = id.clone();
            let owner_session_id =
                match block_on_command("exec session owner lookup", move || {
                    lookup_service
                        .exec_session_owner(&exec_id)
                        .map_err(qaqh_runtime::QaqhError::message)
                })
                .await
            {
                Ok(Some(owner_session_id)) => owner_session_id,
                Ok(None) => {
                    return fail_response(
                        &state,
                        command_id,
                        StatusCode::NOT_FOUND,
                        "exec_session_not_found",
                        "execution session was not found",
                    )
                    .await;
                }
                Err(error) => {
                    return fail_response(
                        &state,
                        command_id,
                        StatusCode::BAD_GATEWAY,
                        "operation_failed",
                        error.to_string(),
                    )
                    .await;
                }
            };
            let reason = match &envelope.command {
                RingingCommand::ExecSessionInterrupt {} => {
                    qaqh_ringing_contract::state::ExecStopReason::Interrupt
                }
                RingingCommand::ExecSessionTerminate {} => {
                    qaqh_ringing_contract::state::ExecStopReason::Terminate
                }
                _ => unreachable!(),
            };
            let stop_service = service.clone();
            let exec_id = id.clone();
            block_on_command("exec session stop", move || {
                stop_service
                    .stop_exec_session(&owner_session_id, &exec_id, reason)
                    .map(|status| (json!({ "status": status }), None))
                    .map_err(qaqh_runtime::QaqhError::message)
            })
            .await
        }
        RingingCommand::ProviderUpsert { id, provider } => {
            let service = service.clone();
            let id = id.clone();
            let provider = provider.clone();
            let expected_revision = expected_revision_from(&envelope);
            block_on_command("provider.upsert", move || {
                service.provider_upsert(&id, &provider, expected_revision)
            })
            .await
            .map(|value| {
                let revision = catalog_revision(&value);
                (value, revision)
            })
        }
        RingingCommand::ProviderDelete { id } => {
            let service = service.clone();
            let id = id.clone();
            let expected_revision = expected_revision_from(&envelope);
            block_on_command("provider.delete", move || {
                service.provider_delete(&id, expected_revision)
            })
            .await
            .map(|value| {
                let revision = catalog_revision(&value);
                (value, revision)
            })
        }
        RingingCommand::ProviderModelUpsert {
            provider_id,
            model_id,
            model,
        } => {
            let service = service.clone();
            let provider_id = provider_id.clone();
            let model_id = model_id.clone();
            let model = model.clone();
            let expected_revision = expected_revision_from(&envelope);
            block_on_command("provider.model.upsert", move || {
                service.provider_model_upsert(&provider_id, &model_id, expected_revision, model)
            })
            .await
            .map(|value| {
                let revision = catalog_revision(&value);
                (value, revision)
            })
        }
        RingingCommand::ProviderModelDelete {
            provider_id,
            model_id,
        } => {
            let service = service.clone();
            let provider_id = provider_id.clone();
            let model_id = model_id.clone();
            let expected_revision = expected_revision_from(&envelope);
            block_on_command("provider.model.delete", move || {
                service.provider_model_delete(&provider_id, &model_id, expected_revision)
            })
            .await
            .map(|value| {
                let revision = catalog_revision(&value);
                (value, revision)
            })
        }
        RingingCommand::ModelSelectionSetDefault { selection } => {
            let service = service.clone();
            let selection = selection.clone();
            let expected_revision = expected_revision_from(&envelope);
            block_on_command("model.selection.set_default", move || {
                service.set_default_model_selection(expected_revision, selection)
            })
            .await
            .map(|value| {
                let revision = catalog_revision(&value);
                (value, revision)
            })
        }
        RingingCommand::ConfigSave { .. }
        | RingingCommand::ConfigSetPermissionLevel { .. }
        | RingingCommand::WorkspaceSet { .. }
        | RingingCommand::WorkspaceCreate { .. }
        | RingingCommand::WorkspaceRename { .. }
        | RingingCommand::WorkspaceDelete {}
        | RingingCommand::WorkspaceMoveSession { .. }
        | RingingCommand::WorkspaceDetach {}
        | RingingCommand::GoalSet { .. }
        | RingingCommand::GoalGet {}
        | RingingCommand::GoalClear {}
        | RingingCommand::TodoSet { .. }
        | RingingCommand::TodoCancel { .. }
        | RingingCommand::PlanAction { .. }
        | RingingCommand::DiagnosticsMemoryStart {}
        | RingingCommand::DiagnosticsMemoryStop {} => {
            // P1-0a：config/workspace/goal/todo 组合同步走 `config_io_lock` /
            // `TODO_LOCK` / secrets 写盘，迁阻塞池。
            let service = service.clone();
            let target = envelope.target.clone();
            let command = envelope.command.clone();
            block_on_command("ringing mutation", move || {
                service.execute_ringing_command(&target, &command)
            })
            .await
            .map(|value| (value, None))
        }
        _ => {
            let (status, ack) =
                super::command::dispatch_session(state, client_session, identity, envelope).await;
            return json_response(status, &ack);
        }
    };

    match outcome {
        Ok((result, revision)) => {
            let mut ack = super::command::accept_ack(command_id.clone(), None);
            ack.result = Some(result.clone());
            ack.operation = Some(operation.to_string());
            ack.revision = revision;
            // B05:同步结果原样落 receipt——ACK 丢失后重放原 command_id 或
            // 轮询 status 都能恢复首次执行承诺的 result/operation/revision。
            // P1-0a：`mark_terminal_sync` 走回执 persist（全量写盘），持久化
            // 边界迁阻塞池。
            let pending = state.pending.clone();
            tokio::task::spawn_blocking(move || {
                pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .mark_terminal_sync(
                        &command_id,
                        qaqh_ringing::RingingV2SyncResult {
                            operation: operation.to_string(),
                            result,
                            revision,
                        },
                    );
            })
            .await
            .expect("receipt terminal-sync task must not panic");
            json_response(StatusCode::OK, &ack)
        }
        Err(qaqh_runtime::QaqhError::Conflict { message }) => {
            // D3:目录/修订 CAS 冲突是并发修订事实(409),客户端据此重读后
            // 重试;不得伪装成 502 operation_failed(参数错误档)。
            fail_response(
                &state,
                command_id,
                StatusCode::CONFLICT,
                "revision_conflict",
                message,
            )
            .await
        }
        Err(error) => fail_response(
            &state,
            command_id,
            StatusCode::BAD_GATEWAY,
            "operation_failed",
            error.to_string(),
        )
        .await,
    }
}

fn catalog_revision(result: &serde_json::Value) -> Option<u64> {
    result.get("revision").and_then(|v| v.as_u64())
}
