//! Admitted session-command dispatch and shared receipt constructors.

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
        result: None,
        operation: None,
        revision: None,
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
        result: None,
        operation: None,
        revision: None,
    }
}

/// 命令幂等指纹。进程内入口与 HTTP 面必须共用，否则重放判定会漂移。
pub(crate) fn command_fingerprint(envelope: &RingingV2CommandEnvelope) -> String {
    let payload = serde_json::to_string(&serde_json::json!({
        "contract_revision": envelope.contract_revision,
        "target": envelope.target,
        "preconditions": envelope.preconditions,
        "command": envelope.command,
    }))
    .unwrap_or_default();
    qaqh_types::sha256_hex(payload.as_bytes())
}

/// daemon 鉴权身份 → 内部信封 actor（S6 归因）。
///
/// admin → `user`/`"user"`（保持改造前语义）；device → `api`/`device_id`
/// （`display_name` 取注册表名，供账本可读）。`id` 由 token 反推，不采信客户端自报。
fn identity_worker_actor(
    state: &AppState,
    identity: &Identity,
) -> Option<qaqh_ringing::WorkerActor> {
    match identity {
        Identity::Admin => Some(qaqh_ringing::WorkerActor {
            kind: "user".into(),
            id: "user".into(),
            display_name: None,
        }),
        Identity::Device { device_id, .. } => {
            let display_name = state
                .devices
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(device_id)
                .map(|record| record.name.clone());
            Some(qaqh_ringing::WorkerActor {
                kind: "api".into(),
                id: device_id.clone(),
                display_name,
            })
        }
    }
}

/// Dispatch an admitted command; authorization and receipts are owned by commands.
pub(crate) async fn dispatch_session(
    state: &AppState,
    session_id: &str,
    identity: &Identity,
    envelope: RingingV2CommandEnvelope,
) -> (StatusCode, RingingV2CommandAck) {
    // 生命周期命令在 daemon 侧处理，其余直接交给会话循环。
    match route(&envelope.command, envelope.session_id()) {
        Route::Close {
            session: close_session,
        } => {
            if close_session.is_empty() {
                return fail(
                    state,
                    envelope.command_id,
                    StatusCode::BAD_REQUEST,
                    "missing_session_id",
                    "SessionClose requires seed",
                );
            }
            // 关闭涉及 registry 和持久化状态，放到阻塞任务中处理。
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
                return fail(
                    state,
                    envelope.command_id,
                    StatusCode::BAD_GATEWAY,
                    "dispatch_failed",
                    error,
                );
            }
            state
                .leases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .detach_session(&session_id, &close_session);
            complete(state, envelope.command_id)
        }
        Route::Rename {
            session: target,
            title,
        } => {
            if let Err(error) = state.service.rename_session(&target, &title) {
                return fail(
                    state,
                    envelope.command_id,
                    StatusCode::BAD_GATEWAY,
                    "dispatch_failed",
                    error,
                );
            }
            complete(state, envelope.command_id)
        }
        Route::Lifecycle { op, target } => {
            if target.is_empty() {
                return fail(
                    state,
                    envelope.command_id,
                    StatusCode::BAD_REQUEST,
                    "missing_session_id",
                    format!("Session{op} requires seed"),
                );
            }
            let result = if op == "delete" {
                state.service.delete_session(&target).await
            } else {
                let service = state.service.clone();
                let target = target.clone();
                let command_id = envelope.command_id.clone();
                tokio::task::spawn_blocking(move || match op {
                    "archive" => service.archive_session(&target, Some(&command_id)),
                    "unarchive" => service.unarchive_session(&target),
                    _ => unreachable!(),
                })
                .await
                .unwrap_or_else(|error| Err(format!("session operation task: {error}")))
            };
            if let Err(error) = result {
                return fail(
                    state,
                    envelope.command_id,
                    StatusCode::BAD_GATEWAY,
                    "dispatch_failed",
                    error,
                );
            }
            if op == "delete" {
                state
                    .leases
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .detach_session(&session_id, &target);
            }
            complete(state, envelope.command_id)
        }
        Route::Resume { session: target } => {
            // BUG-2026-09-12-10：attach 必须先于（较慢的）worker 拉起副作用。
            // 前端切会话后会并行 fetch bootstrap/timeline，若 attach 晚于
            // service.handle 完成，这些请求会撞进「尚未 attach」的窗口拿 401。
            let attached = state
                .leases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .attach_session(&session_id, &target);
            if !attached {
                return fail(
                    state,
                    envelope.command_id,
                    StatusCode::UNAUTHORIZED,
                    "lease_required",
                    "lease is not active".to_string(),
                );
            }
            // BUG-2026-10-05-01：这里此前传的是 header 的 `session_id`
            //（= client_session_id 租约标识），不是命令里的目标会话。后果：
            // daemon 把租约 id 当会话恢复——`.active_session` 被写成租约 id、
            // `register_root_agent` 对租约 id 物化出孤儿 identity 目录、
            // actor resume 加载失败后 lifecycle 兜底又静默分配一个幽灵会话
            // 目录（worker 只写一条 system prompt 后永久闲置）。TUI 每次重启
            // 换 client_session_id 就再产生一对。必须恢复**命令声明的目标会话**。
            if let Err(e) = state.service.resume_session(&target) {
                return fail(
                    state,
                    envelope.command_id,
                    StatusCode::BAD_GATEWAY,
                    "dispatch_failed",
                    e,
                );
            }
            complete(state, envelope.command_id)
        }
        // 仅 attach（无 actor 副作用）：供前端订阅子代理等只读观测 seed 的
        // 频道流。与 SessionResume 的差异见 ControlCommand 文档。
        Route::Attach { session: target } => {
            if target.is_empty() {
                return fail(
                    state,
                    envelope.command_id,
                    StatusCode::BAD_REQUEST,
                    "missing_session_id",
                    "session.attach requires a non-empty seed".to_string(),
                );
            }
            let attached = state
                .leases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .attach_session(&session_id, &target);
            if !attached {
                return fail(
                    state,
                    envelope.command_id,
                    StatusCode::UNAUTHORIZED,
                    "lease_required",
                    "lease is not active".to_string(),
                );
            }
            complete(state, envelope.command_id)
        }
        Route::Engine => {
            let session_id = envelope.session_id().unwrap_or_default().to_string();
            let mut worker_command = envelope.command.clone();
            if let Err(code) =
                hydrate_attachment_previews(&state.hub, &session_id, &mut worker_command)
            {
                return fail(
                    state,
                    envelope.command_id,
                    StatusCode::BAD_REQUEST,
                    &code,
                    "attachment is unavailable or invalid".to_string(),
                );
            }
            let worker_env = qaqh_ringing::RingingWorkerCommandEnvelope::new(
                session_id.as_str(),
                envelope.command_id.clone(),
                worker_command,
            )
            .with_actor(identity_worker_actor(state, identity));
            if let Err(e) = state.service.send_ringing_command(&session_id, &worker_env) {
                return fail(
                    state,
                    envelope.command_id.clone(),
                    StatusCode::BAD_GATEWAY,
                    "dispatch_failed",
                    e.to_string(),
                );
            }
            // 回执终态由 execute_command 的返回值直接落库:命令成功进入会话
            // 循环即 Succeeded(回合的业务进展经 SSE 事件承载,不再有第二条
            // 折叠链把回执拖到 Running 僵尸态)。
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
            (StatusCode::OK, accept_ack(envelope.command_id, None))
        }
    }
}

/// 命令在通过公共闸门（scope / lease / 归属 / 幂等 / 故障注入）之后的去向。
///
/// 生命周期命令在 daemon 侧就地完成，其余整包投给 worker。此前这段是「一串
/// `if let` 特判 + 末尾 `match` 兜底」，每条分支各写一遍回执收尾；现在先分类
/// 再执行，收尾只有三个出口：`complete` / `fail` / worker 投递。
enum Route {
    /// `SessionClose`：关闭并释放 actor。
    Close { session: String },
    /// `SessionRename` writes the business title and returns synchronously.
    Rename { session: String, title: String },
    /// `SessionArchive` / `SessionUnarchive` / `SessionDelete`。
    Lifecycle { op: &'static str, target: String },
    /// `SessionResume`：先 attach，再拉起 worker。
    Resume { session: String },
    /// `SessionAttach`：只建立归属。
    Attach { session: String },
    /// 其余命令一律进 worker 命令通道。
    Engine,
}

fn route(command: &qaqh_ringing::RingingCommand, envelope_session: Option<&str>) -> Route {
    use qaqh_ringing::RingingCommand as R;
    let session = || envelope_session.unwrap_or_default().to_string();
    match command {
        R::SessionClose {} => Route::Close { session: session() },
        R::SessionRename { title } => Route::Rename {
            session: session(),
            title: title.clone(),
        },
        R::SessionArchive {} => Route::Lifecycle {
            op: "archive",
            target: session(),
        },
        R::SessionUnarchive {} => Route::Lifecycle {
            op: "unarchive",
            target: session(),
        },
        R::SessionDelete {} => Route::Lifecycle {
            op: "delete",
            target: session(),
        },
        R::SessionResume {} => Route::Resume { session: session() },
        R::SessionAttach {} => Route::Attach { session: session() },
        _ => Route::Engine,
    }
}

/// 回滚未完成的回执，让同一 `command_id` 可以被重试。
fn rollback_pending(state: &AppState, command_id: &str) {
    state
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .rollback(command_id);
}

/// 成功收尾：回执置终态并 ACK。
pub(crate) fn complete(state: &AppState, command_id: String) -> (StatusCode, RingingV2CommandAck) {
    state
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .mark_terminal(&command_id, RingingCommandState::Succeeded, None, None);
    (StatusCode::OK, accept_ack(command_id, None))
}

/// 失败收尾：回滚回执并拒绝。所有拒绝路径共用，避免「回滚了但没拒」或反之。
pub(crate) fn fail(
    state: &AppState,
    command_id: String,
    status: StatusCode,
    code: &str,
    message: impl Into<String>,
) -> (StatusCode, RingingV2CommandAck) {
    rollback_pending(state, &command_id);
    (status, reject_ack(command_id, code, message.into()))
}
