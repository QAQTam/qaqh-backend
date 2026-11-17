//! axum_impl::service_api — see parent module docs.

use super::*;

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
        // (与 validate_typed_result 同源 DTO)。
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
    // ── 用户 shell 面（ADR-0028 切片 3）─────────────────────────────────
    //
    // 为什么在 axum 层执行而不是丢进 runtime dispatch：owner **只能**来自已鉴权
    // 身份，而 `service_methods::dispatch(&service, method, params)` 的签名里根本
    // 没有身份位。先例就是上面的 `clients.list`（presence 归 axum 层）。方法表
    // (METHODS/TYPED_METHODS) 照登记，形状由那批 typed DTO + validators 锁死，
    // 结果侧与此前 `validate_typed_result` 同一份 DTO —— 类型即形状保证。
    //
    // owner 取 `leases.instance_for_session(client_session_id)`：这是 daemon 在
    // open 时按 token 反推写入的稳定身份（admin 为自报 instance、device 为
    // device_id），**不是** params 里能伪装的字段（`authz.rs` 纪律）。
    if method.starts_with("shell.") {
        if let Err(response) = require_scope(&identity, Scope::Interact) {
            return response;
        }
        let Some(owner) = state
            .leases
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .instance_for_session(&client_session_id)
        else {
            return lease_required_json();
        };
        let outcome: Result<serde_json::Value, String> = match method {
            "shell.open" => serde_json::from_value::<service_methods::ShellOpenParams>(
                params.clone(),
            )
            .map_err(|error| error.to_string())
            .and_then(|args| state.service.shell_open(&owner, &args))
            .and_then(|view| serde_json::to_value(view).map_err(|error| error.to_string())),
            "shell.close" => {
                serde_json::from_value::<service_methods::ShellCloseParams>(params.clone())
                    .map_err(|error| error.to_string())
                    .and_then(|args| state.service.shell_close(&owner, &args.shell_id))
                    .and_then(|result| serde_json::to_value(result).map_err(|error| error.to_string()))
            }
            "shell.list" => serde_json::from_value::<service_methods::ShellListParams>(
                params.clone(),
            )
            .map_err(|error| error.to_string())
            .and_then(|_| state.service.shell_list(&owner))
            .and_then(|result| serde_json::to_value(result).map_err(|error| error.to_string())),
            other => Err(format!("unknown shell method: {other}")),
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
    match service_methods::dispatch(&state.service, method, &params) {
        Ok(value) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_vec(&value).unwrap_or_default(),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_vec(&service_methods::error_response(info.kind, &e)).unwrap_or_default(),
        )
            .into_response(),
    }
}

// ---- SSE helpers (P2) ----
