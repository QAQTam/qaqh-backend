//! axum_impl::service_api — see parent module docs.

use super::*;

/// 用户 shell 面（RPC 三条 + ws 数据面）**唯一**的 owner 取法。
///
/// 三步都在这一处，谁都不许自己走一遍：
/// 1. `Interact` 档 —— 写类命令同律（`commands.rs::required_scope` 的同一份 Scope）；
/// 2. lease 必须**属于本身份**：device 拿别人的 `x-qaqh-client-session-id` 不算数
///    （`v2::caller_lease_bound_to_identity`，A3「身份由 token 反推」）；
/// 3. owner = daemon 在 open 时按 token 反查写入的稳定 `instance_key`
///    （`lease_store.instance_for_session`），params/path 里没有 owner 位。
///
/// 为什么要第 2 步：只按 header 里的 cs 取 `instance_for_session`，则「有效 token +
/// 猜到的 cs」就能把 shell 挂到**别人**的 owner 名下，还能操作别人已开的壳。这正是
/// `authz.rs` 顶部那句话要挡的情形，此前 shell 臂漏了它。
pub(crate) fn shell_owner(
    state: &AppState,
    headers: &HeaderMap,
    identity: &Identity,
) -> Result<String, Response> {
    require_scope(identity, Scope::Interact)?;
    let Some(caller) = v2::caller_lease_bound_to_identity(state, headers, identity) else {
        return Err(lease_required_json());
    };
    let owner = state
        .leases
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .instance_for_session(&caller);
    owner.ok_or_else(lease_required_json)
}

/// `POST /ringing/v2/service/{method}` — 服务面 RPC（Read/Write 两类，
/// 方法表见 `qaqh_runtime::ringing::service_methods`）。旧
/// `/queries/{name}` 与 `/actions/{name}` 双端点已并入此处。
pub(crate) async fn handle_service(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    Path(name): Path<String>,
    body: Bytes,
) -> Response {
    let Some(client_session_id) = get_session_id(&headers) else {
        return lease_required_json();
    };
    // 单一规范形态 `module.method`：slash 别名已拆除，查不到即 404。
    let Some(info) = service_methods::lookup(name.trim_matches('/')) else {
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "application/json")],
            br#"{"code":"unknown_method","message":"unknown service method"}"#.to_vec(),
        )
            .into_response();
    };
    if info.admin_only
        && let Err(response) = require_scope(&identity, Scope::Admin)
    {
        return response;
    }
    let params: serde_json::Value = if body.is_empty() {
        serde_json::json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    [(header::CONTENT_TYPE, "application/json")],
                    serde_json::to_vec(
                        &serde_json::json!({"code":"invalid_body","message":format!("{e}")}),
                    )
                    .unwrap_or_default(),
                )
                    .into_response();
            }
        }
    };
    if info.requires_session && service_methods::session_param_value(&params).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "application/json")],
            br#"{"code":"invalid_envelope","message":"session_id is required"}"#.to_vec(),
        )
            .into_response();
    }
    // 任何带会话键的请求：会话必须归属本 lease（会话键只认 `session_id`）。
    if let Some(session_id) = service_methods::session_param_value(&params) {
        let owns = state
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owns_session(&client_session_id, session_id);
        if !owns {
            return (
                StatusCode::UNAUTHORIZED,
                [(header::CONTENT_TYPE, "application/json")],
                br#"{"code":"lease_required","message":"attach the session before calling"}"#
                    .to_vec(),
            )
                .into_response();
        }
    }
    let method = name.trim_matches('/');
    if method == "clients.list" {
        // daemon 基础设施查询:lease store 是连接 presence,归 axum 层所有,
        // 不进 runtime dispatch。结果由 typed Vec 直接序列化,形状由类型保证
        // (与 typed 表 descriptor 的 result 类型同源 DTO)。
        let clients = state
            .leases
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .active_clients();
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_vec(&clients).unwrap_or_default(),
        )
            .into_response();
    }
    // ── 用户 shell 面（ADR-0028 切片 3/4）───────────────────────────────
    //
    // 为什么在 axum 层执行而不是丢进 runtime dispatch：owner **只能**来自已鉴权
    // 身份，而 `service_methods::dispatch(&service, method, params)` 的签名里根本
    // 没有身份位。先例就是上面的 `clients.list`（presence 归 axum 层）。方法表
    // (METHODS/TYPED_METHODS) 照登记，形状由那批 typed DTO 锁死；P1-4 起
    // 结果侧不再有 `validate_typed_result` 运行时往返——DTO 类型即形状唯一权威。
    //
    // owner/scope 的取法在 `shell_owner` 这一处，ws 数据面（`shell_ws`）调的是**同一个**
    // 函数：终端流与 RPC 面对同一个身份只能有一套判定，否则「谁能连这条流」就有了两个
    // 答案（D3 硬约束第 2 条）。
    if method.starts_with("shell.") {
        let owner = match shell_owner(&state, &headers, &identity) {
            Ok(owner) => owner,
            Err(response) => return response,
        };
        let service = state.service.clone();
        let method_owned = method.to_string();
        let outcome: Result<serde_json::Value, String> =
            match tokio::task::spawn_blocking(move || match method_owned.as_str() {
                "shell.open" => {
                    serde_json::from_value::<service_methods::ShellOpenParams>(params)
                        .map_err(|error| error.to_string())
                        .and_then(|args| service.shell_open(&owner, &args))
                        .and_then(|view| {
                            serde_json::to_value(view).map_err(|error| error.to_string())
                        })
                }
                "shell.close" => {
                    serde_json::from_value::<service_methods::ShellCloseParams>(params)
                        .map_err(|error| error.to_string())
                        .and_then(|args| service.shell_close(&owner, &args.shell_id))
                        .and_then(|result| {
                            serde_json::to_value(result).map_err(|error| error.to_string())
                        })
                }
                "shell.list" => {
                    serde_json::from_value::<service_methods::ShellListParams>(params)
                        .map_err(|error| error.to_string())
                        .and_then(|_| service.shell_list(&owner))
                        .and_then(|result| {
                            serde_json::to_value(result).map_err(|error| error.to_string())
                        })
                }
                other => Err(format!("unknown shell method: {other}")),
            })
            .await
            {
                Ok(result) => result,
                Err(join_error) => Err(format!("shell service worker failed: {join_error}")),
            };
        return match outcome {
            Ok(value) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::to_vec(&value).unwrap_or_default(),
            )
                .into_response(),
            Err(message) => (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::to_vec(&serde_json::json!({
                    "code": "action_failed",
                    "message": message,
                }))
                .unwrap_or_default(),
            )
                .into_response(),
        };
    }
    let service = state.service.clone();
    let method_owned = method.to_string();
    let dispatch_result = tokio::task::spawn_blocking(move || {
        service_methods::dispatch(&service, &method_owned, &params)
    })
    .await;
    match dispatch_result {
        Ok(Ok(value)) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_vec(&value).unwrap_or_default(),
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_vec(&service_methods::error_response(info.kind, &e)).unwrap_or_default(),
        )
            .into_response(),
        Err(join_error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_vec(&serde_json::json!({
                "code": "internal_error",
                "message": format!("service worker failed: {join_error}"),
            }))
            .unwrap_or_default(),
        )
            .into_response(),
    }
}

// ---- SSE helpers (P2) ----
