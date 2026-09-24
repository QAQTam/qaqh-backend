//! axum_impl — daemon HTTP 层（Ringing V1 + 服务面 + 控制）。
//!
//! 由单文件 `axum_server.rs` 拆分（Phase 2-4）：`mod axum_impl` 内联模块解体为
//! 目录模块，对外 API 不变（`AppState` + `build_router`）。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Router,
    body::Bytes,
    extract::{Path, Query, Request, State},
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

use qaqh_domain::{ControlCommand, RingingChannel};
use qaqh_ringing::{
    ClientOpenRequest, ClientOpenResponse, RINGING_SCHEMA, RINGING_VERSION, RingingCommandAck,
    RingingCommandAckStatus, RingingCommandEnvelope, RingingCommandState, RingingResetRequired,
};
use qaqh_runtime::ringing::{PendingCommandStore, RingingLeaseStore, service_methods};
use qaqh_runtime::{QaqhService, RingingHub};

use crate::server::random_hex;

use qaqh_runtime::ringing::hydrate_attachment_previews;

pub mod auth;
pub mod command;
pub mod content;
pub mod control;
pub mod service_api;
pub mod sse;
pub(crate) mod test_hooks;
pub mod timeline_api;
pub mod v2;

pub(crate) use auth::{
    get_session_id, is_authorized, lease_required_json, parse_channel, publish_session_created,
    session_close_seed, unauthorized,
};
pub(crate) use command::{
    command_fingerprint, handle_command, handle_command_status, handle_open, handle_renew,
};
pub(crate) use content::{handle_content_get, handle_content_upload};
pub(crate) use control::{activity, handle_stop, handle_stop_if_idle, health, not_found};
pub(crate) use service_api::handle_service;
pub(crate) use sse::{handle_events, handle_timeline_events};
#[cfg(test)]
pub(crate) use sse::{parse_sse_cursor, parse_timeline_cursor};
pub(crate) use timeline_api::{
    handle_bootstrap, handle_pending_approvals, handle_timeline_snapshot,
};
pub(crate) use v2::{
    handle_bootstrap_v2, handle_command_status_v2, handle_command_v2, handle_driver_claim_v2,
    handle_driver_release_v2, handle_events_v2, handle_open_v2, handle_renew_v2,
};

const RENEW_TTL_MS: u64 = 30_000;
const RENEW_INTERVAL_MS: u64 = 10_000;
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_CONNECTIONS: usize = 128;
const TIMELINE_PAGE_LIMIT: usize = 30;

fn lease_ttl_ms() -> u64 {
    std::env::var("QAQH_TEST_LEASE_TTL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(RENEW_TTL_MS)
}

#[derive(Clone)]
pub struct AppState {
    pub hub: Arc<RingingHub>,
    pub v2_hub: Arc<qaqh_runtime::ringing::V2ProjectionHub>,
    pub leases: Arc<Mutex<RingingLeaseStore>>,
    pub pending: Arc<Mutex<PendingCommandStore>>,
    pub service: QaqhService,
    pub token: String,
    pub epoch: String,
    pub shutdown: tokio::sync::watch::Sender<bool>,
    pub(crate) test_hooks: Arc<test_hooks::TestHooks>,
}

#[derive(Deserialize)]
pub struct TimelineQuery {
    /// **排他**游标：返回全局序号 **小于** 它的那一页。`None` = 最新一页。
    ///
    /// 从 `before_turn`（turn_id）改来（BUG-2026-09-15-05）：turn_id 由 worker 的
    /// 计数器生成、会复用（`TimelineAppender::open_turn` 明确容忍并原地 reopen），
    /// 归档投影侧的 id 又只是「已加载消息池内的下标」——两者都当不了稳定游标。
    /// 按 spec §0b 的兼容政策，直接替换而非并存。
    pub before_index: Option<usize>,
    pub limit: Option<usize>,
}

struct JsonResponse(Vec<u8>);
impl IntoResponse for JsonResponse {
    fn into_response(self) -> Response {
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            self.0,
        )
            .into_response()
    }
}

pub(crate) fn session_not_found_response(seed: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({
            "code": "session_not_found",
            "message": format!("test-injected missing session: {seed}"),
        })
        .to_string(),
    )
        .into_response()
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
        .route("/ringing/v1/clients/open", post(handle_open))
        .route("/ringing/v1/leases/renew", post(handle_renew))
        .route("/ringing/v2/clients/open", post(handle_open_v2))
        .route("/ringing/v2/leases/renew", post(handle_renew_v2))
        .route(
            "/ringing/v2/sessions/{seed}/bootstrap",
            get(handle_bootstrap_v2),
        )
        .route(
            "/ringing/v2/sessions/{seed}/events/{channel}",
            get(handle_events_v2),
        )
        .route(
            "/ringing/v2/commands/{id}",
            post(handle_command_v2).get(handle_command_status_v2),
        )
        .route(
            "/ringing/v2/sessions/{seed}/driver/claim",
            post(handle_driver_claim_v2),
        )
        .route(
            "/ringing/v2/sessions/{seed}/driver/release",
            post(handle_driver_release_v2),
        )
        .route(
            "/ringing/v1/commands/{id}",
            post(handle_command).get(handle_command_status),
        )
        .route(
            "/ringing/v1/sessions/{seed}/bootstrap",
            get(handle_bootstrap),
        )
        .route(
            "/ringing/v1/sessions/{seed}/approvals",
            get(handle_pending_approvals),
        )
        .route(
            "/ringing/v1/sessions/{seed}/timeline",
            get(handle_timeline_snapshot),
        )
        .route("/ringing/v1/content/{content_id}", get(handle_content_get))
        .route("/ringing/v1/content", post(handle_content_upload))
        .route("/ringing/v1/service/{method}", post(handle_service))
        .route("/ringing/v1/events/{channel}", get(handle_events))
        .route(
            "/ringing/v1/sessions/{seed}/timeline/events",
            get(handle_timeline_events),
        )
        .route("/control/v1/stop", post(handle_stop))
        .route("/control/v1/stop-if-idle", post(handle_stop_if_idle))
        .fallback(not_found)
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(ConcurrencyLimitLayer::new(MAX_CONNECTIONS))
        .layer(TraceLayer::new_for_http())
        // 最外层：观察所有出口状态（含各 guard 的 401/403 与限流拒绝）。
        .layer(axum::middleware::from_fn(log_http_errors))
        .with_state(state)
}

#[cfg(test)]
pub(crate) mod pure_tests {
    use super::*;
    #[test]
    fn channel_parsing() {
        assert_eq!(parse_channel("control"), Some(RingingChannel::Control));
        assert_eq!(
            parse_channel("conversation"),
            Some(RingingChannel::Conversation)
        );
        assert_eq!(parse_channel("tool"), Some(RingingChannel::Tool));
        assert_eq!(parse_channel("bogus"), None);
    }
    #[test]
    fn sse_cursor_parsing() {
        assert_eq!(
            parse_sse_cursor("epoch-1:tool:42", "epoch-1", RingingChannel::Tool),
            42
        );
        assert_eq!(
            parse_sse_cursor("epoch-2:tool:42", "epoch-1", RingingChannel::Tool),
            0
        );
        assert_eq!(
            parse_sse_cursor("epoch-1:conversation:7", "epoch-1", RingingChannel::Tool),
            0
        );
        assert_eq!(
            parse_sse_cursor("garbage", "epoch-1", RingingChannel::Tool),
            0
        );
    }
    #[test]
    fn timeline_cursor_is_separate() {
        assert_eq!(parse_timeline_cursor("epoch-1:timeline:42", "epoch-1"), 42);
        assert_eq!(parse_timeline_cursor("epoch-1:tool:42", "epoch-1"), 0);
        assert_eq!(parse_timeline_cursor("epoch-2:timeline:42", "epoch-1"), 0);
        assert_eq!(
            parse_timeline_cursor("epoch-1:timeline:42:extra", "epoch-1"),
            0
        );
        // 空 cursor = 首次连接，正常从头开始（不告警）。
        assert_eq!(parse_timeline_cursor("", "epoch-1"), 0);
        // 形状合法但 seq 缺失/非法：同样按 0 重放，且和形状不符一样告警。
        assert_eq!(parse_timeline_cursor("epoch-1:timeline:", "epoch-1"), 0);
        assert_eq!(parse_timeline_cursor("epoch-1:timeline:abc", "epoch-1"), 0);
    }
    #[test]
    fn session_close_seed_resolution_prefers_command_seed() {
        assert_eq!(
            session_close_seed("s-command", &Some("s-envelope".into())),
            "s-command"
        );
        assert_eq!(session_close_seed("s-command", &None), "s-command");
        assert_eq!(
            session_close_seed("", &Some("s-envelope".into())),
            "s-envelope"
        );
        assert_eq!(session_close_seed("", &None), "");
    }
    #[test]
    fn session_create_event_carries_command_causation() {
        let hub = RingingHub::new("epoch-1");
        publish_session_created(&hub, "s-created", "cmd-create");
        let replay = hub.replay_channel_since(RingingChannel::Control, 0, false);
        assert_eq!(replay.events.len(), 1);
        assert_eq!(replay.events[0].seed, "s-created");
        assert_eq!(replay.events[0].causation_id.as_deref(), Some("cmd-create"));
        assert!(matches!(
            &replay.events[0].event,
            qaqh_ringing::RingingEvent::Control(qaqh_domain::ControlEvent::SessionStateChanged {
                state: qaqh_domain::SessionState::Created,
                ..
            })
        ));
    }
}
