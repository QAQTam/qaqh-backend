//! Daemon control and small read-only status endpoints.
//!
//! Browser WebUI hosting is intentionally absent from this module. The
//! browser-facing gateway was removed with the Tauri migration; there is no
//! `qaqh-daemon webui` subcommand.

use super::*;

pub(crate) async fn health(State(state): State<AppState>) -> impl IntoResponse {
    // P0-3：不再回显 `token_len`——长度本身就是对凭据的旁路信息，`/health`
    // 免鉴权且不受回环守卫约束。
    (StatusCode::OK, format!("ok epoch={}", state.epoch))
}

/// 只读活动快照（冻结事故 P0 观测项）：暴露 has_active_work 与逐会话
/// 活动状态，供冻结排查使用。**需 Bearer 鉴权**（不同于免鉴权的
/// `/health`），仅含 seed/state/turn_id/seq/updated_at，无用户内容。
pub(crate) async fn activity(State(state): State<AppState>) -> Response {
    let (has_active_work, activities) = state.service.activity_snapshot();
    let body = serde_json::json!({
        "has_active_work": has_active_work,
        "activities": activities,
    });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

pub(crate) async fn not_found() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "not found")
}

pub(crate) async fn handle_stop(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
) -> Response {
    if let Err(response) = require_scope(&identity, Scope::Admin) {
        return response;
    }
    // Windows 95 semantics: seal before 200.
    // D-4：shutdown_all 对每个 worker 阻塞 join，放 spawn_blocking。
    let service = state.service.clone();
    let _ = tokio::task::spawn_blocking(move || service.shutdown()).await;
    state.hub.seal_all_orphans();
    state.hub.flush_timeline_persistence();
    let _ = state.shutdown.send(true);
    (StatusCode::OK, "").into_response()
}

pub(crate) async fn handle_stop_if_idle(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
) -> Response {
    if let Err(response) = require_scope(&identity, Scope::Admin) {
        return response;
    }
    if state.service.has_active_work() {
        return (StatusCode::CONFLICT, "").into_response();
    }
    // D-4：同 handle_stop。
    let service = state.service.clone();
    let _ = tokio::task::spawn_blocking(move || service.shutdown()).await;
    state.hub.seal_all_orphans();
    state.hub.flush_timeline_persistence();
    let _ = state.shutdown.send(true);
    (StatusCode::OK, "").into_response()
}
