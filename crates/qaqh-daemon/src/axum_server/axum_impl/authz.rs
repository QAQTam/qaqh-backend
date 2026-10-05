//! axum_impl::authz — 身份模型与鉴权中间件。
//!
//! - `authenticate` 中间件**单次解析** Bearer，取代散落各 handler 的
//!   `is_authorized` 手检；命中即把 [`Identity`] 注入 request extensions。
//! - 身份由 token 反推，**绝不采信客户端自报字段**（`client_instance_id` 等）。
//! - scope 裁决：`view < interact < admin`；admin token 全权且全程豁免。
//! - `/health` 与 `/ringing/v2/pair` 免鉴权。

use super::*;
use qaqh_runtime::ringing::{device_registry, Scope};

/// 请求主体。由 Bearer 解析而来，是全部授权判定的唯一来源。
#[derive(Debug, Clone)]
pub enum Identity {
    /// 本地 admin：桌面壳 / daemon-CLI / TUI / 探针。
    Admin,
    /// 已配对设备：`device_id` 由 token 反查得出，`scope` 为注册表授予档位。
    Device { device_id: String, scope: Scope },
}

impl Identity {
    pub fn is_admin(&self) -> bool {
        matches!(self, Identity::Admin)
    }

    pub fn scope(&self) -> Scope {
        match self {
            Identity::Admin => Scope::Admin,
            Identity::Device { scope, .. } => *scope,
        }
    }
}

/// 提取 `Authorization: Bearer <t>` 的 `<t>`（scheme 大小写敏感，与
/// `is_authorized` 一致：只认精确 `Bearer ` 前缀）。
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty())
}

/// 单次解析 Bearer → [`Identity`]。命中 admin token 得 admin；摘要命中未吊销的
/// 设备得 device；否则 `None`。
pub(crate) fn resolve_identity(state: &AppState, headers: &HeaderMap) -> Option<Identity> {
    if is_authorized(headers, &state.admin_token) {
        return Some(Identity::Admin);
    }
    let token = bearer_token(headers)?;
    let digest = device_registry::token_digest(token);
    let mut devices = state
        .devices
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let record = devices.lookup_by_digest(&digest)?.clone();
    devices.touch(&record.device_id);
    Some(Identity::Device {
        device_id: record.device_id,
        scope: record.scope,
    })
}

/// 非 admin 且 scope 不足 → 403 `insufficient_scope`。
pub(crate) fn require_scope(identity: &Identity, min: Scope) -> Result<(), Response> {
    if identity.scope().at_least(min) {
        return Ok(());
    }
    Err((
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "application/json")],
        format!(
            r#"{{"code":"insufficient_scope","message":"this endpoint requires {:?} scope"}}"#,
            min
        ),
    )
        .into_response())
}

/// `/pair` 自带一次性 `pairing_token`，不要求 Bearer；`/health` 为探针免鉴权面。
fn is_public_path(path: &str) -> bool {
    matches!(path, "/health" | "/ringing/v2/pair")
}

/// 鉴权中间件：解析失败即 401；成功把 [`Identity`] 注入 extensions 供 handler 读取。
pub(crate) async fn authenticate(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    if is_public_path(req.uri().path()) {
        return next.run(req).await;
    }
    let Some(identity) = resolve_identity(&state, req.headers()) else {
        return unauthorized();
    };
    req.extensions_mut().insert(identity);
    next.run(req).await
}
