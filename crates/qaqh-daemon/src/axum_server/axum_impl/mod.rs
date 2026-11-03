//! axum_impl — daemon HTTP 层（Ringing V2 + 服务面 + 控制）。
//!
//! 由单文件 `axum_server.rs` 拆分（Phase 2-4）：`mod axum_impl` 内联模块解体为
//! 目录模块，对外 API 不变（`AppState` + `build_router`）。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Router,
    body::Bytes,
    extract::{Extension, Path, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use serde::Deserialize;
use std::convert::Infallible;
use tokio_stream::wrappers::ReceiverStream;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::{limit::RequestBodyLimitLayer, trace::TraceLayer};

use qaqh_ringing::{
    RingingCommandAckStatus, RingingCommandState, RingingV2CommandAck, RingingV2CommandEnvelope,
};
use qaqh_runtime::ringing::{
    DeviceRegistry, PendingCommandStore, RingingLeaseStore, Scope, service_methods,
};
use qaqh_runtime::{QaqhService, RingingHub};

use crate::server::random_hex;

use qaqh_runtime::ringing::hydrate_attachment_previews;

pub mod auth;
pub mod authz;
pub mod challenge;
pub mod command;
pub mod commands;
pub mod content;
pub mod control;
pub mod pairing;
pub mod service_api;
pub(crate) mod test_hooks;
pub mod v2;

pub(crate) use auth::{get_session_id, is_authorized, lease_required_json, unauthorized};
pub(crate) use authz::{Identity, require_scope};
pub(crate) use command::execute_command;
pub(crate) use content::{handle_content_get, handle_content_upload};
pub(crate) use control::{activity, handle_stop, handle_stop_if_idle, health, not_found};
pub(crate) use service_api::handle_service;
pub(crate) use v2::{
    handle_approval_respond_v2, handle_bootstrap_v2, handle_command_status_v2, handle_events_v2,
    handle_open_v2, handle_pending_approvals_v2, handle_renew_v2, handle_team_snapshot_v2,
};

const RENEW_TTL_MS: u64 = 30_000;
const RENEW_INTERVAL_MS: u64 = 10_000;
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_CONNECTIONS: usize = 128;

fn lease_ttl_ms() -> u64 {
    std::env::var("QAQH_TEST_LEASE_TTL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(RENEW_TTL_MS)
}

#[derive(Clone)]
pub struct AppState {
    pub hub: Arc<RingingHub>,
    pub leases: Arc<Mutex<RingingLeaseStore>>,
    pub pending: Arc<Mutex<PendingCommandStore>>,
    pub service: QaqhService,
    /// 本地 admin 凭证：桌面壳 / daemon-CLI / TUI / 探针专用，全权。
    /// 设备凭证走 `DeviceRegistry`（S2），移动端永不接触本 token。
    pub admin_token: String,
    /// 已配对设备注册表（持久化，跨重启存活，可吊销）。
    pub devices: Arc<Mutex<DeviceRegistry>>,
    /// 短寿一次性配对令牌表（仅内存，TTL 120s）。
    pub pairings: Arc<Mutex<pairing::PairingTable>>,
    /// 服务端自签证书指纹（`sha256:<hex>`）；仅非回环 bind 启用 TLS 时非空，
    /// 经 `/pairing/tokens` 进二维码供原生端 pinning。
    pub tls_fingerprint: Option<String>,
    /// 服务端一次性审批 challenge（device 身份走不透明 id，canonical id 不出 daemon）。
    pub challenges: Arc<challenge::ChallengeStore>,
    pub epoch: String,
    pub shutdown: tokio::sync::watch::Sender<bool>,
    pub(crate) test_hooks: Arc<test_hooks::TestHooks>,
}

/// BUG-2026-09-12-11（O-7）：daemon 此前没有任何 HTTP 状态码记录——TraceLayer
/// 走 tracing 且无 subscriber，4xx 全部无声。本中间件把每个非 2xx 响应写入
/// qaqh-daemon.log（方法/路径/状态），让「切会话被拒」这类事故可直接定案。
async fn log_http_errors(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let response = next.run(req).await;
    let status = response.status();
    if status.is_client_error() || status.is_server_error() {
        log::warn!("[http] {method} {path} -> {status}");
    }
    response
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/activity", get(activity))
        .route("/ringing/v2/clients/open", post(handle_open_v2))
        .route("/ringing/v2/leases/renew", post(handle_renew_v2))
        .route(
            "/ringing/v2/sessions/{session_id}/bootstrap",
            get(handle_bootstrap_v2),
        )
        .route(
            "/ringing/v2/sessions/{session_id}/team",
            get(handle_team_snapshot_v2),
        )
        .route(
            "/ringing/v2/sessions/{session_id}/events",
            get(handle_events_v2),
        )
        .route("/ringing/v2/commands/{id}", get(handle_command_status_v2))
        .route("/ringing/v2/commands", post(commands::handle_command))
        .route(
            "/ringing/v2/sessions/{session_id}/approvals",
            get(handle_pending_approvals_v2),
        )
        .route(
            "/ringing/v2/sessions/{session_id}/approvals/respond",
            post(handle_approval_respond_v2),
        )
        .route("/ringing/v2/content/{content_id}", get(handle_content_get))
        .route("/ringing/v2/content", post(handle_content_upload))
        .route("/ringing/v2/service/{method}", post(handle_service))
        .route(
            "/ringing/v2/pairing/tokens",
            post(pairing::handle_pairing_tokens),
        )
        .route("/ringing/v2/pair", post(pairing::handle_pair))
        .route("/ringing/v2/devices", get(pairing::handle_devices))
        .route(
            "/ringing/v2/exec-sessions",
            get(v2::handle_exec_sessions_list),
        )
        .route(
            "/ringing/v2/exec-sessions/{id}/kill",
            post(v2::handle_exec_session_kill),
        )
        .route(
            "/ringing/v2/devices/{device_id}/revoke",
            post(pairing::handle_device_revoke),
        )
        .route("/control/v1/stop", post(handle_stop))
        .route("/control/v1/stop-if-idle", post(handle_stop_if_idle))
        .fallback(not_found)
        // 内层：单次 Bearer 解析（S0 收口，取代逐 handler 手检）。
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            authz::authenticate,
        ))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(ConcurrencyLimitLayer::new(MAX_CONNECTIONS))
        .layer(TraceLayer::new_for_http())
        // 最外层：观察所有出口状态（含各 guard 的 401/403 与限流拒绝）。
        .layer(axum::middleware::from_fn(log_http_errors))
        .with_state(state)
}
