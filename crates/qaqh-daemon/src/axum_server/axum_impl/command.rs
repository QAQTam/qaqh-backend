//! axum_impl::command — 命令面（open/renew/command/command_status + Ack builder）。

use super::test_hooks::InteractionFault;
use super::*;

/// Rejected 命令回执构造器（22 处 `RingingCommandAck` 字面量的单一构造点）。
pub(crate) fn reject_ack(command_id: String, code: &str, message: String) -> RingingCommandAck {
    RingingCommandAck {
        command_id,
        status: RingingCommandAckStatus::Rejected,
        code: Some(code.to_string()),
        message: Some(message),
        retry_after_ms: None,
    }
}

/// Accepted 命令回执构造器。
pub(crate) fn accept_ack(command_id: String, message: Option<String>) -> RingingCommandAck {
    RingingCommandAck {
        command_id,
        status: RingingCommandAckStatus::Accepted,
        code: None,
        message,
        retry_after_ms: None,
    }
}

/// 命令幂等指纹。v1/v2 handler 必须共用，否则重放判定会在两个协议面漂移。
pub(crate) fn command_fingerprint(
    channel: qaqh_domain::RingingChannel,
    seed: Option<&str>,
    expected_revision: Option<u64>,
    command: &qaqh_ringing::RingingCommand,
) -> String {
    let payload = serde_json::to_string(&serde_json::json!({
        "channel": channel,
        "seed": seed,
        "expected_revision": expected_revision,
        "command": command,
    }))
    .unwrap_or_default();
    qaqh_types::sha256_hex(payload.as_bytes())
}

/// Ack JSON 信封响应（`Content-Type: application/json` 固定）。
pub(crate) fn ack_response(status: StatusCode, ack: RingingCommandAck) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&ack).unwrap_or_default(),
    )
        .into_response()
}

pub(crate) async fn handle_command(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(channel): Path<String>,
    body: Bytes,
) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
    }
    let Some(session_id) = get_session_id(&headers) else {
        return lease_required_json();
    };
    let session_active = state
        .leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_active_session(&session_id);
    if !session_active {
        return lease_required_json();
    }
    let Some(expected) = parse_channel(&channel) else {
        return (StatusCode::NOT_FOUND, "unknown channel").into_response();
    };
    let mut env: RingingCommandEnvelope = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return ack_response(
                StatusCode::BAD_REQUEST,
                reject_ack(String::new(), "invalid_body", e.to_string()),
            );
        }
    };
    if let Err(code) = env.validate() {
        let status = if code == "lease_required" {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::BAD_REQUEST
        };
        return ack_response(
            status,
            reject_ack(
                env.command_id.clone(),
                code,
                "invalid Ringing v1 command envelope".into(),
            ),
        );
    }
    if env.channel != expected {
        return ack_response(
            StatusCode::BAD_REQUEST,
            reject_ack(
                env.command_id.clone(),
                "channel_mismatch",
                format!(
                    "path channel {channel} != envelope channel {:?}",
                    env.channel
                ),
            ),
        );
    }
    state
        .test_hooks
        .apply_command_ack_fault(expected, &env.command)
        .await;
    // unsupported ConversationLoadMore
    if matches!(
        &env.command,
        qaqh_ringing::RingingCommand::Conversation(
            qaqh_domain::ConversationCommand::ConversationLoadMore { .. }
        )
    ) {
        return ack_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            reject_ack(
                env.command_id,
                "unsupported_command",
                "Ringing v1 bootstrap already returns the complete persisted conversation history"
                    .into(),
            ),
        );
    }
    // idempotency
    let fingerprint = command_fingerprint(
        env.channel,
        env.seed.as_deref(),
        env.expected_revision,
        &env.command,
    );
    let duplicate_check = {
        let mut pending = state.pending.lock().unwrap_or_else(|e| e.into_inner());
        match pending.record_fingerprint_for_session(&env.command_id, &fingerprint, &session_id) {
            Ok(v) => Ok(!v),
            Err(()) => Err(()),
        }
    };
    if duplicate_check.is_err() {
        return ack_response(
            StatusCode::CONFLICT,
            reject_ack(
                env.command_id.clone(),
                "duplicate_command_mismatch",
                "command_id was already used with another payload".into(),
            ),
        );
    }
    let duplicate = duplicate_check.expect("error branch already returned CONFLICT above");
    if duplicate {
        return ack_response(
            StatusCode::OK,
            accept_ack(
                env.command_id.clone(),
                Some("duplicate command_id (already accepted)".into()),
            ),
        );
    }
    if let Some(fault) = state.test_hooks.take_interaction_fault(&env.command) {
        match fault {
            InteractionFault::PermissionDeny => {
                if let qaqh_ringing::RingingCommand::Tool(
                    qaqh_domain::ToolCommand::ToolPermissionRespond {
                        approved,
                        trust_folder,
                        ..
                    },
                ) = &mut env.command
                {
                    *approved = false;
                    *trust_folder = false;
                }
            }
            InteractionFault::PermissionHang => {
                if matches!(
                    &env.command,
                    qaqh_ringing::RingingCommand::Tool(
                        qaqh_domain::ToolCommand::ToolPermissionRespond { .. }
                    )
                ) {
                    std::future::pending::<()>().await;
                }
            }
            InteractionFault::AskDismiss => {
                if let qaqh_ringing::RingingCommand::Control(
                    qaqh_domain::ControlCommand::InteractionAskRespond { interaction_id, .. },
                ) = &env.command
                {
                    env.command = qaqh_ringing::RingingCommand::Control(
                        qaqh_domain::ControlCommand::InteractionAskDismiss {
                            interaction_id: interaction_id.clone(),
                        },
                    );
                }
            }
            InteractionFault::AskHang => {
                if matches!(
                    &env.command,
                    qaqh_ringing::RingingCommand::Control(
                        qaqh_domain::ControlCommand::InteractionAskRespond { .. }
                    )
                ) {
                    std::future::pending::<()>().await;
                }
            }
        }
    }
    // SessionClose
    if let qaqh_ringing::RingingCommand::Control(ControlCommand::SessionClose {
        seed: close_seed,
    }) = &env.command
    {
        let close_seed = session_close_seed(close_seed, &env.seed);
        if close_seed.is_empty() {
            state
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .rollback(&env.command_id);
            return ack_response(
                StatusCode::BAD_REQUEST,
                reject_ack(
                    env.command_id,
                    "missing_seed",
                    "SessionClose requires seed".into(),
                ),
            );
        }
        // D-4：close 是阻塞 join（worker loop + reader 线程），必须放
        // spawn_blocking，避免占用 tokio worker 线程并长时间持有
        // registry 锁阻塞其它 RPC。
        let close_result = {
            let service = state.service.clone();
            let seed = close_seed.clone();
            let command_id = env.command_id.clone();
            tokio::task::spawn_blocking(move || service.close_session(&seed, Some(&command_id)))
                .await
                .unwrap_or_else(|e| Err(format!("close join error: {e}")))
        };
        if let Err(error) = close_result {
            state
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .rollback(&env.command_id);
            return ack_response(
                StatusCode::BAD_GATEWAY,
                reject_ack(env.command_id, "dispatch_failed", error.to_string()),
            );
        }
        state
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .detach_seed(&session_id, &close_seed);
        state
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mark_terminal(&env.command_id, RingingCommandState::Succeeded, None, None);
        return ack_response(StatusCode::OK, accept_ack(env.command_id, None));
    }
    // SessionArchive / Unarchive / Delete
    if let qaqh_ringing::RingingCommand::Control(
        cmd @ (ControlCommand::SessionArchive { .. }
        | ControlCommand::SessionUnarchive { .. }
        | ControlCommand::SessionDelete { .. }),
    ) = &env.command
    {
        let (op, target) = match cmd {
            ControlCommand::SessionArchive { seed } => ("archive", seed),
            ControlCommand::SessionUnarchive { seed } => ("unarchive", seed),
            ControlCommand::SessionDelete { seed } => ("delete", seed),
            _ => unreachable!(),
        };
        let target = session_close_seed(target, &env.seed);
        if target.is_empty() {
            state
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .rollback(&env.command_id);
            return ack_response(
                StatusCode::BAD_REQUEST,
                reject_ack(
                    env.command_id,
                    "missing_seed",
                    format!("Session{op} requires seed"),
                ),
            );
        }
        // D-4：archive 内含 close（阻塞 join），delete 同理；整体移入
        // spawn_blocking。unarchive（拉起 worker）一并序列化到阻塞线程，
        // 保持同一 command 的执行线程语义一致。
        let result: Result<(), String> = {
            let service = state.service.clone();
            let target = target.clone();
            let command_id = env.command_id.clone();
            tokio::task::spawn_blocking(move || match op {
                "archive" => service
                    .archive_session(&target, Some(&command_id))
                    .map_err(|e| e.to_string()),
                "unarchive" => service
                    .unarchive_session(&target)
                    .map_err(|e| e.to_string()),
                "delete" => {
                    let _ = service.close_session(&target, Some(&command_id));
                    service
                        .delete_session(&target, Some(&command_id))
                        .map_err(|e| e.to_string())
                }
                _ => unreachable!(),
            })
            .await
            .unwrap_or_else(|e| Err(format!("session op join error: {e}")))
        };
        if let Err(error) = result {
            state
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .rollback(&env.command_id);
            return ack_response(
                StatusCode::BAD_GATEWAY,
                reject_ack(env.command_id, "dispatch_failed", error),
            );
        }
        if op == "delete" {
            state
                .leases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .detach_seed(&session_id, &target);
        }
        state
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mark_terminal(&env.command_id, RingingCommandState::Succeeded, None, None);
        return ack_response(StatusCode::OK, accept_ack(env.command_id, None));
    }
    // session.new / session.resume
    match &env.command {
        qaqh_ringing::RingingCommand::Control(ControlCommand::SessionCreate { .. }) => {
            let params = serde_json::to_value(&env.command).unwrap_or_default();
            // service.handle expects params as Value; for session.new it expects seed? Actually SessionCreate is handled via service.handle("session.new")
            let created = match state.service.handle("session.new", &params) {
                Ok(v) => v,
                Err(e) => {
                    state
                        .pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .rollback(&env.command_id);
                    return ack_response(
                        StatusCode::BAD_GATEWAY,
                        reject_ack(env.command_id, "dispatch_failed", e.to_string()),
                    );
                }
            };
            let created_seed = created.as_str().map(str::to_string).or_else(|| {
                created
                    .get("seed")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            });
            if let Some(seed) = created_seed {
                // BUG-2026-09-12-10：attach 失败（lease 已死）必须显式 401，
                // 而不是静默 ack 200 让前端进入「无归属」状态。
                let attached = state
                    .leases
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .attach_seed(&session_id, &seed);
                if !attached {
                    state
                        .pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .rollback(&env.command_id);
                    return ack_response(
                        StatusCode::UNAUTHORIZED,
                        reject_ack(
                            env.command_id,
                            "lease_required",
                            "lease is not active".into(),
                        ),
                    );
                }
                publish_session_created(&state.hub, &seed, &env.command_id);
            }
            state
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .mark_terminal(&env.command_id, RingingCommandState::Succeeded, None, None);
            return ack_response(StatusCode::OK, accept_ack(env.command_id, None));
        }
        qaqh_ringing::RingingCommand::Control(ControlCommand::SessionResume { seed }) => {
            // BUG-2026-09-12-10：attach 必须先于（较慢的）worker 拉起副作用。
            // 前端切会话后会并行 fetch bootstrap/timeline，若 attach 晚于
            // service.handle 完成，这些请求会撞进「尚未 attach」的窗口拿 401。
            let attached = state
                .leases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .attach_seed(&session_id, seed);
            if !attached {
                state
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .rollback(&env.command_id);
                return ack_response(
                    StatusCode::UNAUTHORIZED,
                    reject_ack(
                        env.command_id,
                        "lease_required",
                        "lease is not active".into(),
                    ),
                );
            }
            if let Err(e) = state
                .service
                .handle("session.resume", &serde_json::json!({"seed": seed}))
            {
                state
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .rollback(&env.command_id);
                return ack_response(
                    StatusCode::BAD_GATEWAY,
                    reject_ack(env.command_id, "dispatch_failed", e.to_string()),
                );
            }
            state
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .mark_terminal(&env.command_id, RingingCommandState::Succeeded, None, None);
            return ack_response(StatusCode::OK, accept_ack(env.command_id, None));
        }
        // 仅 attach（无 actor 副作用）：供前端订阅子代理等只读观测 seed 的
        // timeline/频道流。与 SessionResume 的差异见 ControlCommand 文档。
        qaqh_ringing::RingingCommand::Control(ControlCommand::SessionAttach { seed }) => {
            if seed.is_empty() {
                state
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .rollback(&env.command_id);
                return ack_response(
                    StatusCode::BAD_REQUEST,
                    reject_ack(
                        env.command_id,
                        "missing_seed",
                        "session.attach requires a non-empty seed".into(),
                    ),
                );
            }
            let attached = state
                .leases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .attach_seed(&session_id, seed);
            if !attached {
                state
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .rollback(&env.command_id);
                return ack_response(
                    StatusCode::UNAUTHORIZED,
                    reject_ack(
                        env.command_id,
                        "lease_required",
                        "lease is not active".into(),
                    ),
                );
            }
            state
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .mark_terminal(&env.command_id, RingingCommandState::Succeeded, None, None);
            return ack_response(StatusCode::OK, accept_ack(env.command_id, None));
        }
        _ => {}
    }
    // generic worker dispatch
    let seed = env.seed.clone().unwrap_or_default();
    let mut worker_command = env.command.clone();
    if let Err(code) = hydrate_attachment_previews(&state.hub, &seed, &mut worker_command) {
        state
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rollback(&env.command_id);
        return ack_response(
            StatusCode::BAD_REQUEST,
            reject_ack(
                env.command_id,
                &code.clone(),
                "attachment is unavailable or invalid".into(),
            ),
        );
    }
    let worker_env = qaqh_ringing::RingingWorkerCommandEnvelope::new(
        seed.as_str(),
        env.command_id.clone(),
        worker_command,
    )
    .with_expected_revision(env.expected_revision);
    if let Err(e) = state.service.send_ringing_command(&seed, &worker_env) {
        state
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rollback(&env.command_id);
        return ack_response(
            StatusCode::BAD_GATEWAY,
            reject_ack(env.command_id.clone(), "dispatch_failed", e.to_string()),
        );
    }
    state
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .mark_running(&env.command_id);
    ack_response(StatusCode::OK, accept_ack(env.command_id, None))
}

pub(crate) async fn handle_command_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(command_id): Path<String>,
) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
    }
    let Some(session_id) = get_session_id(&headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "application/json")],
            br#"{"code":"lease_required","message":"client session header required"}"#.to_vec(),
        )
            .into_response();
    };
    let Some(status) = state
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .status_for_session(&command_id, &session_id)
    else {
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "application/json")],
            br#"{"code":"command_not_found","message":"command receipt not found"}"#.to_vec(),
        )
            .into_response();
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&status).unwrap_or_default(),
    )
        .into_response()
}
