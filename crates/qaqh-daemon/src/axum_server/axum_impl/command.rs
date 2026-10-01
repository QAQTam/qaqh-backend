//! axum_impl::command — 命令面（幂等指纹 + 进程内命令入口 `execute_command`）。
//!
//! hub-fact-bus spec 阶段 1（命令面去圈）：v2 HTTP handler 与 driver
//! claim/release 都直调 `execute_command`，v2→v1 信封重序列化圈与 v1 HTTP
//! handler 面已删除。命令入口只收 v2 信封、只吐 v2 ack；HTTP 状态码由调用方
//! 从 `(StatusCode, RingingV2CommandAck)` 组装。

use super::test_hooks::InteractionFault;
use super::*;

/// Rejected 命令回执构造器（命令入口所有拒绝路径的单一构造点）。
pub(crate) fn reject_ack(command_id: String, code: &str, message: String) -> RingingV2CommandAck {
    RingingV2CommandAck {
        command_id,
        status: RingingCommandAckStatus::Rejected,
        code: Some(code.to_string()),
        message: Some(message),
        retry_after_ms: None,
        existing: None,
    }
}

/// Accepted 命令回执构造器。
pub(crate) fn accept_ack(command_id: String, message: Option<String>) -> RingingV2CommandAck {
    RingingV2CommandAck {
        command_id,
        status: RingingCommandAckStatus::Accepted,
        code: None,
        message,
        retry_after_ms: None,
        existing: None,
    }
}

/// 命令幂等指纹。进程内入口与 HTTP 面必须共用，否则重放判定会漂移。
pub(crate) fn command_fingerprint(
    channel: qaqh_domain::RingingChannel,
    session_id: Option<&str>,
    expected_revision: Option<u64>,
    driver_epoch: Option<u64>,
    command: &qaqh_ringing::RingingCommand,
) -> String {
    let payload = serde_json::to_string(&serde_json::json!({
        "channel": channel,
        "seed": session_id,
        "expected_revision": expected_revision,
        // v2-only CAS input: the same command_id submitted against a different
        // driver epoch is a different payload and must not replay the old ACK.
        "driver_epoch": driver_epoch,
        "command": command,
    }))
    .unwrap_or_default();
    qaqh_types::sha256_hex(payload.as_bytes())
}

/// 进程内命令入口（spec 阶段 1.1）。
///
/// 承接原 v1 `handle_command` 的解析后逻辑，但不做 HTTP body 组装：
/// - 信封直接收 v2，不再有 v2→v1 JSON 重序列化圈；
/// - 返回 `(StatusCode, RingingV2CommandAck)`，ack → Response 由调用方组装；
/// - lease 归属校验仍发生在命令入口（不变量 4），调用方无需提前检查。
pub(crate) async fn execute_command(
    state: &AppState,
    headers: &HeaderMap,
    mut envelope: RingingV2CommandEnvelope,
) -> (StatusCode, RingingV2CommandAck) {
    let Some(session_id) = get_session_id(headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            reject_ack(
                String::new(),
                "lease_required",
                "open a Ringing v2 client session first".into(),
            ),
        );
    };
    let session_active = state
        .leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_active_session(&session_id);
    if !session_active {
        return (
            StatusCode::UNAUTHORIZED,
            reject_ack(
                String::new(),
                "lease_required",
                "lease is not active".into(),
            ),
        );
    }
    if let Err(code) = envelope.validate() {
        return (
            StatusCode::BAD_REQUEST,
            reject_ack(
                envelope.command_id.clone(),
                code,
                "invalid Ringing v2 command envelope".into(),
            ),
        );
    }
    state
        .test_hooks
        .apply_command_ack_fault(envelope.channel, &envelope.command)
        .await;
    // unsupported ConversationLoadMore
    if matches!(
        &envelope.command,
        qaqh_ringing::RingingCommand::Conversation(
            qaqh_domain::ConversationCommand::ConversationLoadMore { .. }
        )
    ) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            reject_ack(
                envelope.command_id,
                "unsupported_command",
                "Ringing v1 bootstrap already returns the complete persisted conversation history"
                    .into(),
            ),
        );
    }
    // idempotency
    let fingerprint = command_fingerprint(
        envelope.channel,
        envelope.session_id.as_deref(),
        envelope.expected_revision,
        envelope.driver_epoch,
        &envelope.command,
    );
    let duplicate_check = {
        let mut pending = state.pending.lock().unwrap_or_else(|e| e.into_inner());
        match pending.record_fingerprint_for_session(
            &envelope.command_id,
            &fingerprint,
            &session_id,
        ) {
            Ok(v) => Ok(!v),
            Err(()) => Err(()),
        }
    };
    if duplicate_check.is_err() {
        return (
            StatusCode::CONFLICT,
            reject_ack(
                envelope.command_id.clone(),
                "duplicate_command_mismatch",
                "command_id was already used with another payload".into(),
            ),
        );
    }
    let duplicate = duplicate_check.expect("error branch already returned CONFLICT above");
    if duplicate {
        return (
            StatusCode::OK,
            accept_ack(
                envelope.command_id.clone(),
                Some("duplicate command_id (already accepted)".into()),
            ),
        );
    }
    if let Some(fault) = state.test_hooks.take_interaction_fault(&envelope.command) {
        match fault {
            InteractionFault::PermissionDeny => {
                if let qaqh_ringing::RingingCommand::Tool(
                    qaqh_domain::ToolCommand::ToolPermissionRespond {
                        approved,
                        trust_folder,
                        ..
                    },
                ) = &mut envelope.command
                {
                    *approved = false;
                    *trust_folder = false;
                }
            }
            InteractionFault::PermissionHang => {
                if matches!(
                    &envelope.command,
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
                ) = &envelope.command
                {
                    envelope.command = qaqh_ringing::RingingCommand::Control(
                        qaqh_domain::ControlCommand::InteractionAskDismiss {
                            interaction_id: interaction_id.clone(),
                        },
                    );
                }
            }
            InteractionFault::AskHang => {
                if matches!(
                    &envelope.command,
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
        session_id: close_session,
    }) = &envelope.command
    {
        let close_session = session_close_session(close_session, &envelope.session_id);
        if close_session.is_empty() {
            state
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .rollback(&envelope.command_id);
            return (
                StatusCode::BAD_REQUEST,
                reject_ack(
                    envelope.command_id,
                    "missing_session_id",
                    "SessionClose requires seed".into(),
                ),
            );
        }
        // D-4：close 是阻塞 join（worker loop + reader 线程），必须放
        // spawn_blocking，避免占用 tokio worker 线程并长时间持有
        // registry 锁阻塞其它 RPC。
        let close_result = {
            let service = state.service.clone();
            let session_id = close_session.clone();
            let command_id = envelope.command_id.clone();
            tokio::task::spawn_blocking(move || {
                service.close_session(&session_id, Some(&command_id))
            })
            .await
            .unwrap_or_else(|e| Err(format!("close join error: {e}")))
        };
        if let Err(error) = close_result {
            state
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .rollback(&envelope.command_id);
            return (
                StatusCode::BAD_GATEWAY,
                reject_ack(envelope.command_id, "dispatch_failed", error.to_string()),
            );
        }
        state
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .detach_session(&session_id, &close_session);
        state
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mark_terminal(
                &envelope.command_id,
                RingingCommandState::Succeeded,
                None,
                None,
            );
        return (StatusCode::OK, accept_ack(envelope.command_id, None));
    }
    // SessionArchive / Unarchive / Delete
    if let qaqh_ringing::RingingCommand::Control(
        cmd @ (ControlCommand::SessionArchive { .. }
        | ControlCommand::SessionUnarchive { .. }
        | ControlCommand::SessionDelete { .. }),
    ) = &envelope.command
    {
        let (op, target) = match cmd {
            ControlCommand::SessionArchive { session_id } => ("archive", session_id),
            ControlCommand::SessionUnarchive { session_id } => ("unarchive", session_id),
            ControlCommand::SessionDelete { session_id } => ("delete", session_id),
            _ => unreachable!(),
        };
        let target = session_close_session(target, &envelope.session_id);
        if target.is_empty() {
            state
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .rollback(&envelope.command_id);
            return (
                StatusCode::BAD_REQUEST,
                reject_ack(
                    envelope.command_id,
                    "missing_session_id",
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
            let command_id = envelope.command_id.clone();
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
                .rollback(&envelope.command_id);
            return (
                StatusCode::BAD_GATEWAY,
                reject_ack(envelope.command_id, "dispatch_failed", error),
            );
        }
        if op == "delete" {
            state
                .leases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .detach_session(&session_id, &target);
        }
        state
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mark_terminal(
                &envelope.command_id,
                RingingCommandState::Succeeded,
                None,
                None,
            );
        return (StatusCode::OK, accept_ack(envelope.command_id, None));
    }
    // session.new / session.resume
    match &envelope.command {
        qaqh_ringing::RingingCommand::Control(ControlCommand::SessionCreate { .. }) => {
            let params = serde_json::to_value(&envelope.command).unwrap_or_default();
            // service.handle expects params as Value; for session.new it expects seed? Actually SessionCreate is handled via service.handle("session.new")
            let created = match state.service.handle("session.new", &params) {
                Ok(v) => v,
                Err(e) => {
                    state
                        .pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .rollback(&envelope.command_id);
                    return (
                        StatusCode::BAD_GATEWAY,
                        reject_ack(envelope.command_id, "dispatch_failed", e.to_string()),
                    );
                }
            };
            let created_session = created.as_str().map(str::to_string);
            // BUG-2026-09-29-01：这里此前写作 `if let Some(session_id)`，把外层
            // `session_id`（header 的 client_session_id）遮蔽成新 seed，导致
            // `attach_session(&seed, &seed)` 恒 false——commands 通道上的
            // SessionCreate 永远 401。attach 的宿主必须是发起命令的 lease。
            if let Some(created) = created_session {
                // BUG-2026-09-12-10：attach 失败（lease 已死）必须显式 401，
                // 而不是静默 ack 200 让前端进入「无归属」状态。
                let attached = state
                    .leases
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .attach_session(&session_id, &created);
                if !attached {
                    state
                        .pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .rollback(&envelope.command_id);
                    return (
                        StatusCode::UNAUTHORIZED,
                        reject_ack(
                            envelope.command_id,
                            "lease_required",
                            "lease is not active".into(),
                        ),
                    );
                }
                // 阶段 3b：SessionCreated 的 v1 广播已删（A1）。
            }
            state
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .mark_terminal(
                    &envelope.command_id,
                    RingingCommandState::Succeeded,
                    None,
                    None,
                );
            return (StatusCode::OK, accept_ack(envelope.command_id, None));
        }
        qaqh_ringing::RingingCommand::Control(ControlCommand::SessionResume {
            session_id: target_session_id,
        }) => {
            // BUG-2026-09-12-10：attach 必须先于（较慢的）worker 拉起副作用。
            // 前端切会话后会并行 fetch bootstrap/timeline，若 attach 晚于
            // service.handle 完成，这些请求会撞进「尚未 attach」的窗口拿 401。
            let attached = state
                .leases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .attach_session(&session_id, target_session_id);
            if !attached {
                state
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .rollback(&envelope.command_id);
                return (
                    StatusCode::UNAUTHORIZED,
                    reject_ack(
                        envelope.command_id,
                        "lease_required",
                        "lease is not active".into(),
                    ),
                );
            }
            if let Err(e) = state.service.handle(
                "session.resume",
                &serde_json::json!({"session_id": session_id}),
            ) {
                state
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .rollback(&envelope.command_id);
                return (
                    StatusCode::BAD_GATEWAY,
                    reject_ack(envelope.command_id, "dispatch_failed", e.to_string()),
                );
            }
            state
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .mark_terminal(
                    &envelope.command_id,
                    RingingCommandState::Succeeded,
                    None,
                    None,
                );
            return (StatusCode::OK, accept_ack(envelope.command_id, None));
        }
        // 仅 attach（无 actor 副作用）：供前端订阅子代理等只读观测 seed 的
        // timeline/频道流。与 SessionResume 的差异见 ControlCommand 文档。
        qaqh_ringing::RingingCommand::Control(ControlCommand::SessionAttach {
            session_id: target_session_id,
        }) => {
            if target_session_id.is_empty() {
                state
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .rollback(&envelope.command_id);
                return (
                    StatusCode::BAD_REQUEST,
                    reject_ack(
                        envelope.command_id,
                        "missing_session_id",
                        "session.attach requires a non-empty seed".into(),
                    ),
                );
            }
            let attached = state
                .leases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .attach_session(&session_id, target_session_id);
            if !attached {
                state
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .rollback(&envelope.command_id);
                return (
                    StatusCode::UNAUTHORIZED,
                    reject_ack(
                        envelope.command_id,
                        "lease_required",
                        "lease is not active".into(),
                    ),
                );
            }
            state
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .mark_terminal(
                    &envelope.command_id,
                    RingingCommandState::Succeeded,
                    None,
                    None,
                );
            return (StatusCode::OK, accept_ack(envelope.command_id, None));
        }
        _ => {}
    }
    // generic worker dispatch
    let session_id = envelope.session_id.clone().unwrap_or_default();
    let mut worker_command = envelope.command.clone();
    if let Err(code) = hydrate_attachment_previews(&state.hub, &session_id, &mut worker_command) {
        state
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rollback(&envelope.command_id);
        return (
            StatusCode::BAD_REQUEST,
            reject_ack(
                envelope.command_id,
                &code.clone(),
                "attachment is unavailable or invalid".into(),
            ),
        );
    }
    let worker_env = qaqh_ringing::RingingWorkerCommandEnvelope::new(
        session_id.as_str(),
        envelope.command_id.clone(),
        worker_command,
    )
    .with_expected_revision(envelope.expected_revision);
    if let Err(e) = state.service.send_ringing_command(&session_id, &worker_env) {
        state
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rollback(&envelope.command_id);
        return (
            StatusCode::BAD_GATEWAY,
            reject_ack(
                envelope.command_id.clone(),
                "dispatch_failed",
                e.to_string(),
            ),
        );
    }
    state
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .mark_running(&envelope.command_id);
    (StatusCode::OK, accept_ack(envelope.command_id, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::{ControlCommand, RingingChannel};

    #[test]
    fn v2_driver_epoch_is_part_of_the_command_fingerprint() {
        let command = qaqh_ringing::RingingCommand::Control(ControlCommand::SessionResume {
            session_id: "seed-1".into(),
        });
        let epoch_one = command_fingerprint(
            RingingChannel::Control,
            Some("seed-1"),
            Some(7),
            Some(1),
            &command,
        );
        let epoch_two = command_fingerprint(
            RingingChannel::Control,
            Some("seed-1"),
            Some(7),
            Some(2),
            &command,
        );
        assert_ne!(
            epoch_one, epoch_two,
            "same command_id against a new driver epoch is a different payload"
        );
    }
}
