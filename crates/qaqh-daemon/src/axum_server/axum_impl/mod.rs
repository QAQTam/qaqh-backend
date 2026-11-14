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
use qaqh_ringing_runtime::{
    DeviceRegistry, PendingCommandStore, RingingHub, RingingLeaseStore, Scope,
    hydrate_attachment_previews,
};
use qaqh_runtime::{QaqhService, ringing::service_methods};

use crate::server::random_hex;

pub mod auth;
pub mod authz;
pub mod challenge;
pub mod command;
pub mod commands;
pub mod content;
pub mod control;
pub mod driver;
pub mod pairing;
pub mod service_api;
pub(crate) mod test_hooks;
pub mod v2;

pub(crate) use auth::{get_session_id, is_authorized, lease_required_json, unauthorized};
pub(crate) use authz::{Identity, require_scope};
pub(crate) use commands::execute_command;
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
    /// driver 席位（内存，per-session 单席位；holder 为 lease id，不跨重启）。
    pub drivers: Arc<driver::DriverSeats>,
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

/// Daemon 控制面 HTTP 路由表 —— 不属于 Ringing 契约面。
///
/// 与 `qaqh_ringing::RINGING_V2_HTTP_ROUTES` 同构,两张表合起来是路由的**唯一登记处**:
/// - `build_router` 遍历本表注册 handler:表里有而 match 缺 arm = 启动即 panic;
/// - 加路由的唯一途径是往表里写一行,不允许在本模块树出现字面量路径的
///   `.route("…")`/`.nest(…)`/`.merge(…)`。
///
/// 为什么必须入表(实测缺陷,ADR-0003 §缺口 1):此前这 4 条由正则从 `.route()`
/// 抓取,而该正则只捕获 path 不捕获 method —— HTTP 方法曾长期漏登记。
pub const CONTROL_PLANE_ROUTES: &[(&str, &str)] = &[
    ("GET", "/health"),
    ("GET", "/activity"),
    ("POST", "/control/v1/stop"),
    ("POST", "/control/v1/stop-if-idle"),
];

pub fn build_router(state: AppState) -> Router {
    // 控制面表驱动注册:理由见 CONTROL_PLANE_ROUTES。
    let mut router = Router::new();
    for (method, path) in CONTROL_PLANE_ROUTES {
        router = match (*method, *path) {
            ("GET", "/health") => router.route(path, get(health)),
            ("GET", "/activity") => router.route(path, get(activity)),
            ("POST", "/control/v1/stop") => router.route(path, post(handle_stop)),
            ("POST", "/control/v1/stop-if-idle") => router.route(path, post(handle_stop_if_idle)),
            other => panic!("control-plane route without a registered handler: {other:?}"),
        };
    }
    // Ringing v2 面从契约路由表注册(B02 漂移闸):表驱动保证「表里的每条路由
    // 都有 handler」——新路径只加表不加 arm 会启动即 panic;反过来 handler arm
    // 里的路径字符串以表为准,不得自造。
    for (method, path) in qaqh_ringing::RINGING_V2_HTTP_ROUTES {
        router = match (*method, *path) {
            ("POST", "/ringing/v2/clients/open") => router.route(path, post(handle_open_v2)),
            ("POST", "/ringing/v2/leases/renew") => router.route(path, post(handle_renew_v2)),
            ("GET", "/ringing/v2/sessions/{session_id}/bootstrap") => {
                router.route(path, get(handle_bootstrap_v2))
            }
            ("GET", "/ringing/v2/sessions/{session_id}/team") => {
                router.route(path, get(handle_team_snapshot_v2))
            }
            ("GET", "/ringing/v2/sessions/{session_id}/events") => {
                router.route(path, get(handle_events_v2))
            }
            ("POST", "/ringing/v2/commands") => router.route(path, post(commands::handle_command)),
            ("GET", "/ringing/v2/commands/{command_id}") => {
                router.route(path, get(handle_command_status_v2))
            }
            ("GET", "/ringing/v2/sessions/{session_id}/approvals") => {
                router.route(path, get(handle_pending_approvals_v2))
            }
            ("POST", "/ringing/v2/sessions/{session_id}/approvals/respond") => {
                router.route(path, post(handle_approval_respond_v2))
            }
            ("POST", "/ringing/v2/content") => router.route(path, post(handle_content_upload)),
            ("GET", "/ringing/v2/content/{content_id}") => {
                router.route(path, get(handle_content_get))
            }
            ("POST", "/ringing/v2/service/{method}") => router.route(path, post(handle_service)),
            ("POST", "/ringing/v2/pairing/tokens") => {
                router.route(path, post(pairing::handle_pairing_tokens))
            }
            ("POST", "/ringing/v2/pair") => router.route(path, post(pairing::handle_pair)),
            ("GET", "/ringing/v2/devices") => router.route(path, get(pairing::handle_devices)),
            ("POST", "/ringing/v2/devices/{device_id}/revoke") => {
                router.route(path, post(pairing::handle_device_revoke))
            }
            other => panic!("contract route without a registered handler: {other:?}"),
        };
    }
    router
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
