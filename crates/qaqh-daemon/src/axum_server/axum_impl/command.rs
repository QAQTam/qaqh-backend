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
    command: &qaqh_ringing::RingingCommand,
) -> String {
    let payload = serde_json::to_string(&serde_json::json!({
        "channel": channel,
        "seed": session_id,
        "expected_revision": expected_revision,
        "command": command,
    }))
    .unwrap_or_default();
    qaqh_types::sha256_hex(payload.as_bytes())
}

/// 命令所需的最低 scope。`SessionAttach` 只建立归属（不触碰会话 actor）→ `View`；
/// 其余一律 `Interact`。admin 由 `Identity::scope()` 返回 `Admin` 恒通过。
fn required_scope_for_command(envelope: &RingingV2CommandEnvelope) -> Scope {
    match &envelope.command {
        qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::SessionAttach {
            ..
        }) => Scope::View,
        _ => Scope::Interact,
    }
}

/// 命令的归属目标会话。
///
/// 优先取显式 `envelope.session_id`；会话生命周期命令的目标在其自身字段里。
/// 建立归属的命令（`SessionCreate`/`SessionResume`/`SessionAttach`）返回 `None`
/// ——它们是「先 attach 再操作」的入口，必须豁免 owns 检查，否则移动端永远
/// 无法取得任何会话的归属。
fn command_ownership_target(envelope: &RingingV2CommandEnvelope) -> Option<&str> {
    use qaqh_domain::ControlCommand as C;
    use qaqh_ringing::RingingCommand as R;
    match &envelope.command {
        R::Control(C::SessionCreate { .. })
        | R::Control(C::SessionResume { .. })
        | R::Control(C::SessionAttach { .. }) => return None,
        R::Control(C::SessionClose { session_id })
        | R::Control(C::SessionArchive { session_id })
        | R::Control(C::SessionUnarchive { session_id })
        | R::Control(C::SessionDelete { session_id }) => {
            if !session_id.is_empty() {
                return Some(session_id.as_str());
            }
        }
        _ => {}
    }
    envelope.session_id.as_deref().filter(|s| !s.is_empty())
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

/// 进程内命令入口（spec 阶段 1.1）。
///
/// 承接原 v1 `handle_command` 的解析后逻辑，但不做 HTTP body 组装：
/// - 信封直接收 v2，不再有 v2→v1 JSON 重序列化圈；
/// - 返回 `(StatusCode, RingingV2CommandAck)`，ack → Response 由调用方组装；
/// - lease 归属校验仍发生在命令入口（不变量 4），调用方无需提前检查。
pub(crate) async fn execute_command(
    state: &AppState,
    headers: &HeaderMap,
    identity: &Identity,
    mut envelope: RingingV2CommandEnvelope,
) -> (StatusCode, RingingV2CommandAck) {
    // scope：按命令分级。`SessionAttach` 仅建立会话归属、不触碰 actor（见
    // `ControlCommand::SessionAttach` 文档），view 档即可——否则 view 档设备永远
    // 无法 attach，视察型 app（spec §7）作废。其余命令（消息/审批应答/driver
    // claim/release/close/...）需 interact。admin 全权。
    let required = required_scope_for_command(&envelope);
    if !identity.scope().at_least(required) {
        return (
            StatusCode::FORBIDDEN,
            reject_ack(
                envelope.command_id.clone(),
                "insufficient_scope",
                format!("command requires {required:?} scope"),
            ),
        );
    }
    // 解析调用方 lease 并绑定身份：device 的 lease 必须由本设备建立（防拿他人 cs
    // 冒充其 lease）。无 lease / 未绑定 / 已过期 → 401。
    let Some(session_id) = super::v2::caller_lease_bound_to_identity(state, headers, identity)
    else {
        return (
            StatusCode::UNAUTHORIZED,
            reject_ack(
                String::new(),
                "lease_required",
                "open a Ringing v2 client session first".into(),
            ),
        );
    };
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
    // 非 admin：命令只能作用于本 lease 拥有的会话。建立归属的生命周期命令
    // （create/resume/attach）豁免——移动端照常先 attach 再操作。admin（桌面壳 /
    // CLI / TUI / 探针）全程豁免，行为不变。
    if !identity.is_admin()
        && let Some(target) = command_ownership_target(&envelope)
    {
        let owns = state
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owns_session(&session_id, target);
        if !owns {
            return (
                StatusCode::FORBIDDEN,
                reject_ack(
                    envelope.command_id.clone(),
                    "forbidden_not_owner",
                    "lease does not own the target session".into(),
                ),
            );
        }
    }
    state
        .test_hooks
        .apply_command_ack_fault(envelope.channel, &envelope.command)
        .await;
    // `ConversationLoadMore` 是 v1 分页残留：v2 的 bootstrap/快照已返回完整
    // 历史，没有「再加载更多」的语义。拒绝发生在幂等指纹之前——被拒的命令
    // 不占用 command_id，客户端可以改正后复用。
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
                "conversation.load_more is not part of Ringing v2: bootstrap already returns the \
                 complete persisted history"
                    .into(),
            ),
        );
    }
    // idempotency
    let fingerprint = command_fingerprint(
        envelope.channel,
        envelope.session_id.as_deref(),
        envelope.expected_revision,
        &envelope.command,
    );
    // §5：回执 owner 是稳定身份键（device_id / admin 自报 instance id），
    // 不是临时 client_session_id——换连接后同一 command_id 仍可对账、重复
    // 提交返回原回执；不同设备冒用同一 command_id 仍被指纹守卫拒绝。
    let owner_key = state
        .leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .instance_for_session(&session_id)
        .unwrap_or_else(|| session_id.clone());
    let duplicate_check = {
        let mut pending = state.pending.lock().unwrap_or_else(|e| e.into_inner());
        match pending.record_fingerprint_for_session(&envelope.command_id, &fingerprint, &owner_key)
        {
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
    // ── 路由：生命周期命令在 daemon 侧就地处理，其余进 worker 命令通道 ──
    match route(&envelope.command, &envelope.session_id) {
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
        Route::Create => {
            let params = serde_json::to_value(&envelope.command).unwrap_or_default();
            // session.new 走的是强类型服务面（`service.handle`），不是 worker 通道：
            // 它只物化会话目录与首条事实，没有 actor 可投递。
            let created = match state.service.handle("session.new", &params) {
                Ok(v) => v,
                Err(e) => {
                    return fail(
                        state,
                        envelope.command_id,
                        StatusCode::BAD_GATEWAY,
                        "dispatch_failed",
                        e.to_string(),
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
                    return fail(
                        state,
                        envelope.command_id,
                        StatusCode::UNAUTHORIZED,
                        "lease_required",
                        "lease is not active".to_string(),
                    );
                }
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
            if let Err(e) = state.service.handle(
                "session.resume",
                &serde_json::json!({ "session_id": target }),
            ) {
                return fail(
                    state,
                    envelope.command_id,
                    StatusCode::BAD_GATEWAY,
                    "dispatch_failed",
                    e.to_string(),
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
            let session_id = envelope.session_id.clone().unwrap_or_default();
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
            .with_expected_revision(envelope.expected_revision)
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
            state
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .mark_running(&envelope.command_id);
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
    /// `SessionArchive` / `SessionUnarchive` / `SessionDelete`。
    Lifecycle { op: &'static str, target: String },
    /// `SessionCreate`（`session.new` 服务面）。
    Create,
    /// `SessionResume`：先 attach，再拉起 worker。
    Resume { session: String },
    /// `SessionAttach`：只建立归属。
    Attach { session: String },
    /// 其余命令一律进 worker 命令通道。
    Engine,
}

fn route(command: &qaqh_ringing::RingingCommand, envelope_session: &Option<String>) -> Route {
    use qaqh_domain::ControlCommand as C;
    use qaqh_ringing::RingingCommand as R;
    match command {
        R::Control(C::SessionClose { session_id }) => Route::Close {
            session: session_close_session(session_id, envelope_session),
        },
        R::Control(C::SessionArchive { session_id }) => Route::Lifecycle {
            op: "archive",
            target: session_close_session(session_id, envelope_session),
        },
        R::Control(C::SessionUnarchive { session_id }) => Route::Lifecycle {
            op: "unarchive",
            target: session_close_session(session_id, envelope_session),
        },
        R::Control(C::SessionDelete { session_id }) => Route::Lifecycle {
            op: "delete",
            target: session_close_session(session_id, envelope_session),
        },
        R::Control(C::SessionCreate { .. }) => Route::Create,
        R::Control(C::SessionResume { session_id }) => Route::Resume {
            session: session_id.clone(),
        },
        R::Control(C::SessionAttach { session_id }) => Route::Attach {
            session: session_id.clone(),
        },
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
fn complete(state: &AppState, command_id: String) -> (StatusCode, RingingV2CommandAck) {
    state
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .mark_terminal(&command_id, RingingCommandState::Succeeded, None, None);
    (StatusCode::OK, accept_ack(command_id, None))
}

/// 失败收尾：回滚回执并拒绝。所有拒绝路径共用，避免「回滚了但没拒」或反之。
fn fail(
    state: &AppState,
    command_id: String,
    status: StatusCode,
    code: &str,
    message: impl Into<String>,
) -> (StatusCode, RingingV2CommandAck) {
    rollback_pending(state, &command_id);
    (status, reject_ack(command_id, code, message.into()))
}
