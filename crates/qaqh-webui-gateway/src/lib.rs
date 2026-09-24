//! Independent, loopback-only browser gateway for the QAQ-Harness WebUI.
//!
//! The browser never receives the daemon bearer token or a daemon lease id.
//! It receives an opaque HttpOnly session cookie and a CSRF token; all daemon
//! calls are made by this process after explicit origin, allowlist, and seed
//! scope checks.

mod approval;
mod daemon;
mod security;
mod session;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use approval::{ApprovalKind, ApprovalRequest, command_for};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{ConnectInfo, DefaultBodyLimit, Path as AxumPath, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use daemon::DaemonClient;
use qaqh_domain::{ControlCommand, ConversationCommand, RingingChannel};
use qaqh_ringing::{
    RingingCommand, RingingCommandAck, RingingCommandAckStatus, RingingCommandEnvelope,
    RingingV2CommandEnvelope,
};
use qaqh_types::{CONTROL_PROTOCOL_VERSION, DaemonDiscovery};
use reqwest::Response as UpstreamResponse;
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::{Value, json};
use session::{BrowserSession, NonceStore, SESSION_IDLE_TTL, SessionStore};
use tokio::net::{TcpListener, TcpStream};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const CONNECT_ATTEMPTS: usize = 5;
const RETRY_DELAY: Duration = Duration::from_millis(100);
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

#[derive(RustEmbed)]
#[folder = "../../webui/out/renderer"]
struct WebUi;

/// Configuration for the explicit `qaqh-daemon webui` command.
///
/// There is deliberately no bind-address or daemon-token option here. The
/// gateway is always loopback-only and obtains its daemon identity exclusively
/// from the local discovery file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayConfig {
    port: u16,
    expected_build_id: String,
    expected_protocol_version: u16,
}

impl GatewayConfig {
    pub fn new(expected_build_id: impl Into<String>) -> Self {
        Self {
            port: 0,
            expected_build_id: expected_build_id.into(),
            expected_protocol_version: CONTROL_PROTOCOL_VERSION,
        }
    }

    /// Parse only the small explicit CLI surface accepted by Phase 1.
    pub fn parse(args: &[String], expected_build_id: impl Into<String>) -> Result<Self, String> {
        let mut config = Self::new(expected_build_id);
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--port" => {
                    index += 1;
                    let value = args.get(index).ok_or("--port requires a value")?;
                    config.port = value
                        .parse()
                        .map_err(|_| format!("invalid --port: {value}"))?;
                }
                other => return Err(format!("unknown webui flag: {other}")),
            }
            index += 1;
        }
        Ok(config)
    }
}

#[derive(Clone)]
struct GatewayState {
    daemon: Arc<DaemonClient>,
    nonces: Arc<NonceStore>,
    sessions: Arc<SessionStore>,
    allowed_hosts: Arc<Vec<String>>,
    allowed_origins: Arc<Vec<String>>,
}

/// Start the browser gateway after validating the local daemon discovery
/// record and proving that the discovered endpoint is reachable.
pub async fn run(config: GatewayConfig) -> Result<(), String> {
    let discovery_path = qaqh_types::platform::daemon_discovery_path();
    let discovery = read_discovery(&discovery_path)?;
    validate_discovery(
        &discovery,
        &config.expected_build_id,
        config.expected_protocol_version,
    )?;
    let endpoint = parse_loopback_endpoint(&discovery.endpoint)?;
    ensure_reachable(&endpoint).await?;
    let daemon = Arc::new(DaemonClient::new(&discovery)?);
    daemon.verify_epoch().await?;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, config.port))
        .await
        .map_err(|error| format!("bind webui gateway to 127.0.0.1: {}", error))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("resolve webui gateway address: {error}"))?;
    let (allowed_hosts, allowed_origins) = browser_origins(address.port());
    let state = GatewayState {
        daemon,
        nonces: Arc::new(NonceStore::new()),
        sessions: Arc::new(SessionStore::new()),
        allowed_hosts: Arc::new(allowed_hosts),
        allowed_origins: Arc::new(allowed_origins),
    };
    let url = format!("http://{address}");
    println!("qaqh-webui-gateway: listening on {url}");
    log::info!("[webui-gateway] listening on {url}");

    let app = build_router(state);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .map_err(|error| format!("serve webui gateway: {error}"))?;
    log::info!("[webui-gateway] stopped");
    Ok(())
}

fn browser_origins(port: u16) -> (Vec<String>, Vec<String>) {
    let hosts = vec![
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ];
    let origins = hosts
        .iter()
        .map(|host| format!("http://{host}"))
        .collect::<Vec<_>>();
    (hosts, origins)
}

fn read_discovery(path: &Path) -> Result<DaemonDiscovery, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("read daemon discovery {}: {error}", path.display()))?;
    serde_json::from_str(&content).map_err(|error| format!("parse daemon discovery: {error}"))
}

fn validate_discovery(
    discovery: &DaemonDiscovery,
    expected_build_id: &str,
    expected_protocol_version: u16,
) -> Result<(), String> {
    if discovery.pid == 0 || !qaqh_types::platform::process_is_running(discovery.pid) {
        return Err("daemon discovery points to a process that is not running".into());
    }
    if discovery.server_epoch.is_empty() {
        return Err("daemon discovery has an empty server epoch".into());
    }
    if discovery.token.is_empty() {
        return Err("daemon discovery has an empty bearer token".into());
    }
    if expected_build_id.is_empty() || discovery.build_id != expected_build_id {
        return Err("daemon discovery build id does not match this binary".into());
    }
    if discovery.protocol_version != expected_protocol_version {
        return Err("daemon discovery protocol version does not match this binary".into());
    }
    parse_loopback_endpoint(&discovery.endpoint)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LoopbackEndpoint {
    host: IpAddr,
    port: u16,
}

fn parse_loopback_endpoint(raw: &str) -> Result<LoopbackEndpoint, String> {
    let uri: axum::http::Uri = raw
        .parse()
        .map_err(|_| "daemon discovery endpoint is not a valid URI".to_string())?;
    if uri.scheme_str() != Some("http") {
        return Err("daemon discovery endpoint must use http on loopback".into());
    }
    let authority = uri
        .authority()
        .ok_or("daemon discovery endpoint is missing an authority")?;
    if authority.as_str().contains('@') {
        return Err("daemon discovery endpoint must not contain userinfo".into());
    }
    if uri.path() != "/" && !uri.path().is_empty() {
        return Err("daemon discovery endpoint must not contain a path".into());
    }
    if uri.query().is_some() || raw.contains('#') {
        return Err("daemon discovery endpoint must not contain query or fragment".into());
    }

    let host_text = uri
        .host()
        .ok_or("daemon discovery endpoint is missing a host")?;
    let host_text = host_text
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host_text);
    let host = if host_text.eq_ignore_ascii_case("localhost") {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        host_text
            .parse::<IpAddr>()
            .map_err(|_| "daemon discovery endpoint host must be a loopback IP".to_string())?
    };
    if !host.is_loopback() {
        return Err("daemon discovery endpoint must use a loopback IP".into());
    }
    let port = uri
        .port_u16()
        .ok_or("daemon discovery endpoint must include an explicit port")?;
    if port == 0 {
        return Err("daemon discovery endpoint port must be non-zero".into());
    }
    Ok(LoopbackEndpoint { host, port })
}

async fn ensure_reachable(endpoint: &LoopbackEndpoint) -> Result<(), String> {
    let address = SocketAddr::new(endpoint.host, endpoint.port);
    for attempt in 0..CONNECT_ATTEMPTS {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(address)).await {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(error)) if attempt + 1 == CONNECT_ATTEMPTS => {
                return Err(format!(
                    "daemon discovery endpoint is not reachable: {error}"
                ));
            }
            Err(_) if attempt + 1 == CONNECT_ATTEMPTS => {
                return Err("daemon discovery endpoint connection timed out".into());
            }
            _ => tokio::time::sleep(RETRY_DELAY).await,
        }
    }
    Err("daemon discovery endpoint is not reachable".into())
}

fn build_router(state: GatewayState) -> Router {
    Router::new()
        .route("/", get(serve_index))
        .route("/assets/{*path}", get(serve_asset))
        .route("/__gateway/bootstrap.js", get(bootstrap_js))
        .route("/__gateway/session", post(create_session))
        .route("/__gateway/logout", post(logout))
        .route("/__gateway/sessions", get(list_sessions))
        .route("/__gateway/sessions/{seed}/attach", post(attach_seed))
        .route("/__gateway/approvals", post(list_approvals))
        .route("/__gateway/approvals/{id}", post(respond_approval))
        .route(
            "/__gateway/ringing/commands/{channel}",
            post(proxy_command).get(proxy_command_status),
        )
        .route(
            "/__gateway/ringing/content/{content_id}",
            get(proxy_content_get),
        )
        .route("/__gateway/ringing/content", post(proxy_content_upload))
        .route("/__gateway/ringing/events/{channel}", get(proxy_events))
        .route(
            "/__gateway/ringing/sessions/{seed}/bootstrap",
            get(proxy_bootstrap),
        )
        .route(
            "/__gateway/ringing/sessions/{seed}/timeline",
            get(proxy_timeline),
        )
        .route(
            "/__gateway/ringing/sessions/{seed}/timeline/events",
            get(proxy_timeline_events),
        )
        .route("/__gateway/ringing/service/{method}", post(proxy_service))
        .fallback(serve_spa)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

async fn serve_index() -> Response {
    embedded_response("index.html")
}

async fn serve_asset(AxumPath(path): AxumPath<String>) -> Response {
    embedded_response(&format!("assets/{path}"))
}

async fn serve_spa(uri: Uri) -> Response {
    let path = uri.path();
    if is_reserved_path(path) || Path::new(path).extension().is_some() {
        return not_found_response();
    }
    embedded_response("index.html")
}

fn not_found_response() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

fn embedded_response(path: &str) -> Response {
    if !allowed_asset_path(path) {
        return not_found_response();
    }
    let Some(file) = WebUi::get(path) else {
        return not_found_response();
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, mime_for(path))],
        file.data.into_owned(),
    )
        .into_response()
}

fn is_reserved_path(path: &str) -> bool {
    ["/debug", "/__gateway", "/ringing", "/control", "/assets"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

fn allowed_asset_path(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') || path.contains('\\') {
        return false;
    }
    if !path.split('/').all(|component| {
        !component.is_empty()
            && component != "."
            && component != ".."
            && !component.starts_with('.')
    }) {
        return false;
    }
    matches!(
        Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str()),
        Some("html" | "js" | "css" | "json" | "svg" | "png" | "ico" | "woff2" | "wasm")
    )
}

fn mime_for(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

async fn bootstrap_js(
    State(state): State<GatewayState>,
    ConnectInfo(address): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = browser_request_allowed(&state, &headers, false) {
        return response;
    }
    let nonce = match state.nonces.issue(address.ip()) {
        Ok(nonce) => nonce,
        Err(code) => return error_response(StatusCode::TOO_MANY_REQUESTS, code),
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        format!("window.__QAQH_GATEWAY__={{\"nonce\":\"{nonce}\"}};\n"),
    )
        .into_response()
}

#[derive(Deserialize)]
struct SessionRequest {
    nonce: String,
}

async fn create_session(
    State(state): State<GatewayState>,
    ConnectInfo(address): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<SessionRequest>,
) -> Response {
    if let Err(response) = browser_request_allowed(&state, &headers, true) {
        return response;
    }
    if let Err(code) = state.nonces.redeem(address.ip(), &request.nonce) {
        let status = if code.ends_with("rate") {
            StatusCode::TOO_MANY_REQUESTS
        } else {
            StatusCode::FORBIDDEN
        };
        return error_response(status, code);
    }

    let client_instance_id = format!("webui-gateway-{}", session::random_token());
    let lease = match state.daemon.open(&client_instance_id).await {
        Ok(lease) => lease,
        Err(error) => {
            log::warn!("[webui-gateway] open failed: {error}");
            return error_response(StatusCode::BAD_GATEWAY, "daemon_unavailable");
        }
    };
    let browser_session = BrowserSession::new(lease);
    if state.sessions.insert(browser_session.clone()).is_err() {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "session_limit");
    }
    spawn_renewal(state.clone(), browser_session.clone());

    let body = json!({
        "csrf_token": browser_session.csrf_token(),
        "expires_in": SESSION_IDLE_TTL.as_secs(),
    });
    let mut response = Json(body).into_response();
    let cookie = security::session_cookie(browser_session.id(), SESSION_IDLE_TTL.as_secs());
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

async fn logout(State(state): State<GatewayState>, headers: HeaderMap) -> Response {
    let session = match authenticate(&state, &headers, true) {
        Ok(session) => session,
        Err(response) => return response,
    };
    if !security::csrf_matches(&headers, session.csrf_token()) {
        return error_response(StatusCode::FORBIDDEN, "csrf_failed");
    }
    state.sessions.remove(session.id());
    let mut response = StatusCode::NO_CONTENT.into_response();
    if let Ok(value) = HeaderValue::from_str(&security::clear_session_cookie()) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

async fn list_sessions(State(state): State<GatewayState>, headers: HeaderMap) -> Response {
    let session = match authenticate(&state, &headers, true) {
        Ok(session) => session,
        Err(response) => return response,
    };
    match fetch_sessions(&state, &session).await {
        Ok(value) => Json(value).into_response(),
        Err(response) => response,
    }
}

#[allow(clippy::result_large_err)]
async fn fetch_sessions(state: &GatewayState, session: &BrowserSession) -> Result<Value, Response> {
    let lease = session.lease_snapshot();
    let upstream = state
        .daemon
        .post_json("/ringing/v2/service/session.list", &lease, &json!({}))
        .await
        .map_err(|_| error_response(StatusCode::BAD_GATEWAY, "daemon_unavailable"))?;
    if !upstream.status().is_success() {
        return Err(forward_response(upstream).await);
    }
    let value: Value = upstream
        .json()
        .await
        .map_err(|_| error_response(StatusCode::BAD_GATEWAY, "invalid_daemon_response"))?;
    Ok(sanitize_session_list(value))
}

fn sanitize_session_list(value: Value) -> Value {
    let Some(entries) = value.as_array() else {
        return json!([]);
    };
    let sanitized = entries
        .iter()
        .map(|entry| {
            json!({
                "seed": entry.get("seed").cloned().unwrap_or(Value::Null),
                "title": entry.get("title").cloned().unwrap_or(Value::Null),
                "created_at": entry.get("created_at").cloned().unwrap_or(Value::Null),
                "updated_at": entry.get("updated_at").cloned().unwrap_or(Value::Null),
                "message_count": entry.get("message_count").cloned().unwrap_or(Value::Null),
                "turn_count": entry.get("turn_count").cloned().unwrap_or(Value::Null),
                "running": entry.get("running").cloned().unwrap_or(Value::Bool(false)),
                "archived": entry.get("archived").cloned().unwrap_or(Value::Bool(false)),
                "ephemeral": entry.get("ephemeral").cloned().unwrap_or(Value::Bool(false)),
            })
        })
        .collect::<Vec<_>>();
    Value::Array(sanitized)
}

async fn attach_seed(
    State(state): State<GatewayState>,
    AxumPath(seed): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let session = match authenticate(&state, &headers, true) {
        Ok(session) => session,
        Err(response) => return response,
    };
    if !security::csrf_matches(&headers, session.csrf_token()) {
        return error_response(StatusCode::FORBIDDEN, "csrf_failed");
    }
    if !session.allow_command() {
        return error_response(StatusCode::TOO_MANY_REQUESTS, "command_rate_limited");
    }
    if !valid_seed(&seed) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_seed");
    }

    let client_instance_id = session.lease_snapshot().client_instance_id;
    let lease = match state.daemon.open(&client_instance_id).await {
        Ok(lease) => lease,
        Err(_) => return error_response(StatusCode::BAD_GATEWAY, "daemon_unavailable"),
    };
    session.replace_lease(lease.clone());
    session.set_active_seed(None);

    let command = RingingCommand::Control(ControlCommand::SessionAttach { seed: seed.clone() });
    let envelope =
        RingingV2CommandEnvelope::new(session::random_token(), client_instance_id, command)
            .with_client_session_id(lease.client_session_id.clone())
            .with_seed(seed.clone());
    let body = match serde_json::to_value(envelope) {
        Ok(body) => body,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "encode_failed"),
    };
    let response = match state
        .daemon
        .post_json("/ringing/v2/commands/control", &lease, &body)
        .await
    {
        Ok(response) => response,
        Err(_) => return error_response(StatusCode::BAD_GATEWAY, "daemon_unavailable"),
    };
    if !response.status().is_success() {
        return forward_response(response).await;
    }
    let ack: RingingCommandAck = match response.json().await {
        Ok(ack) => ack,
        Err(_) => return error_response(StatusCode::BAD_GATEWAY, "invalid_daemon_response"),
    };
    if ack.status != RingingCommandAckStatus::Accepted {
        return error_response(StatusCode::FORBIDDEN, "attach_rejected");
    }
    session.set_active_seed(Some(seed));
    StatusCode::NO_CONTENT.into_response()
}

async fn list_approvals(State(state): State<GatewayState>, headers: HeaderMap) -> Response {
    let session = match authenticate(&state, &headers, true) {
        Ok(session) => session,
        Err(response) => return response,
    };
    if !security::csrf_matches(&headers, session.csrf_token()) {
        return error_response(StatusCode::FORBIDDEN, "csrf_failed");
    }
    if !session.allow_service() {
        return error_response(StatusCode::TOO_MANY_REQUESTS, "service_rate_limited");
    }
    let Some(seed) = session.active_seed() else {
        return error_response(StatusCode::CONFLICT, "no_active_seed");
    };
    let lease = session.lease_snapshot();
    let response = match state
        .daemon
        .get(
            &format!("/ringing/v1/sessions/{}/approvals", encode_path(&seed)),
            &lease,
        )
        .await
    {
        Ok(response) => response,
        Err(_) => return error_response(StatusCode::BAD_GATEWAY, "daemon_unavailable"),
    };
    if !response.status().is_success() {
        return forward_response(response).await;
    }
    let pending: Value = match response.json().await {
        Ok(value) => value,
        Err(_) => return error_response(StatusCode::BAD_GATEWAY, "invalid_daemon_response"),
    };
    match issue_approval_views(&session, &seed, &pending) {
        Ok(views) => Json(views).into_response(),
        Err(code) => error_response(StatusCode::BAD_GATEWAY, code),
    }
}

fn issue_approval_views(
    session: &BrowserSession,
    seed: &str,
    pending: &Value,
) -> Result<Vec<Value>, &'static str> {
    let mut views = Vec::new();

    if let Some(tool) = pending
        .get("pending_permission")
        .filter(|value| !value.is_null())
    {
        let source_id = tool
            .get("tool_call_id")
            .and_then(Value::as_str)
            .filter(|value| valid_daemon_id(value))
            .ok_or("invalid_pending_permission")?;
        let details = json!({
            "tool_name": tool.get("tool_name").cloned().unwrap_or(Value::Null),
            "action_summary": tool.get("action_summary").cloned().unwrap_or(Value::Null),
            "reason": tool.get("reason").cloned().unwrap_or(Value::Null),
            "paths": tool.get("paths").cloned().unwrap_or_else(|| json!([])),
            "category": tool.get("category").cloned().unwrap_or(Value::Null),
            "level": tool.get("level").cloned().unwrap_or(Value::Null),
            "risk": tool.get("risk").cloned().unwrap_or(Value::Null),
            "consequence": tool.get("consequence").cloned().unwrap_or(Value::Null),
        });
        let challenge = session
            .issue_approval(
                seed,
                ApprovalKind::ToolPermission,
                source_id.to_string(),
                details,
            )
            .map_err(|_| "approval_limit")?;
        views.push(challenge.view());
    }

    if let Some(interaction) = pending
        .get("pending_interaction")
        .filter(|value| !value.is_null())
    {
        let source_id = interaction
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| valid_daemon_id(value))
            .ok_or("invalid_pending_interaction")?;
        let kind = match interaction.get("kind").and_then(Value::as_str) {
            Some("ask") => ApprovalKind::Ask,
            Some("plan") => ApprovalKind::Plan,
            _ => return Err("invalid_pending_interaction"),
        };
        let details = interaction
            .get("details")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let challenge = session
            .issue_approval(seed, kind, source_id.to_string(), details)
            .map_err(|_| "approval_limit")?;
        views.push(challenge.view());
    }

    Ok(views)
}

async fn respond_approval(
    State(state): State<GatewayState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let session = match authenticate(&state, &headers, true) {
        Ok(session) => session,
        Err(response) => return response,
    };
    if !security::csrf_matches(&headers, session.csrf_token()) {
        return error_response(StatusCode::FORBIDDEN, "csrf_failed");
    }
    if !session.allow_command() {
        return error_response(StatusCode::TOO_MANY_REQUESTS, "command_rate_limited");
    }
    if !valid_opaque_id(&id) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_challenge_id");
    }
    if body.len() > 64 * 1024 {
        return error_response(StatusCode::PAYLOAD_TOO_LARGE, "approval_body_too_large");
    }
    let request: ApprovalRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_approval_body"),
    };
    let Some(active_seed) = session.active_seed() else {
        return error_response(StatusCode::CONFLICT, "no_active_seed");
    };
    let challenge = match session.consume_approval(&id, &active_seed) {
        Ok(challenge) => challenge,
        Err(code) => {
            let status = match code {
                "approval_scope_violation" => StatusCode::FORBIDDEN,
                "approval_not_found" => StatusCode::GONE,
                _ => StatusCode::BAD_REQUEST,
            };
            return error_response(status, code);
        }
    };
    let command = match command_for(&challenge, &request) {
        Ok(command) => command,
        Err(code) => return error_response(StatusCode::BAD_REQUEST, code),
    };
    let channel = command.channel();
    let lease = session.lease_snapshot();
    let envelope = RingingV2CommandEnvelope::new(
        session::random_token(),
        lease.client_instance_id.clone(),
        command,
    )
    .with_client_session_id(lease.client_session_id.clone())
    .with_seed(active_seed);
    if let Err(error) = envelope.validate() {
        return error_response(StatusCode::BAD_REQUEST, error);
    }
    let body = match serde_json::to_value(envelope) {
        Ok(body) => body,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "encode_failed"),
    };
    let response = match state
        .daemon
        .post_json(
            &format!("/ringing/v2/commands/{}", channel_path(channel)),
            &lease,
            &body,
        )
        .await
    {
        Ok(response) => response,
        Err(_) => return error_response(StatusCode::BAD_GATEWAY, "daemon_unavailable"),
    };
    if !response.status().is_success() {
        return forward_response(response).await;
    }
    let ack: RingingCommandAck = match response.json().await {
        Ok(ack) => ack,
        Err(_) => return error_response(StatusCode::BAD_GATEWAY, "invalid_daemon_response"),
    };
    if ack.status != RingingCommandAckStatus::Accepted {
        return error_response(StatusCode::CONFLICT, "approval_rejected");
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn proxy_command(
    State(state): State<GatewayState>,
    AxumPath(channel): AxumPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let session = match authenticate(&state, &headers, true) {
        Ok(session) => session,
        Err(response) => return response,
    };
    if !security::csrf_matches(&headers, session.csrf_token()) {
        return error_response(StatusCode::FORBIDDEN, "csrf_failed");
    }
    if !session.allow_command() {
        return error_response(StatusCode::TOO_MANY_REQUESTS, "command_rate_limited");
    }
    let Some(active_seed) = session.active_seed() else {
        return error_response(StatusCode::CONFLICT, "no_active_seed");
    };
    let mut envelope: RingingCommandEnvelope = match serde_json::from_slice(&body) {
        Ok(envelope) => envelope,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_envelope"),
    };
    let Some(expected_channel) = channel_from_path(&channel) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_channel");
    };
    if envelope.channel != expected_channel || !sanitize_command(&mut envelope.command) {
        return error_response(StatusCode::FORBIDDEN, "command_not_allowed");
    }
    let lease = session.lease_snapshot();
    // 浏览器侧仍按 v1 形状提交（网关自有 API），转发给 daemon 时构造 v2 信封。
    let mut v2 = RingingV2CommandEnvelope::new(
        envelope.command_id,
        lease.client_instance_id.clone(),
        envelope.command,
    )
    .with_client_session_id(lease.client_session_id.clone())
    .with_seed(active_seed);
    v2.expected_revision = envelope.expected_revision;
    if let Err(error) = v2.validate() {
        return error_response(StatusCode::BAD_REQUEST, error);
    }
    let body = match serde_json::to_value(v2) {
        Ok(body) => body,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "encode_failed"),
    };
    let response = match state
        .daemon
        .post_json(&format!("/ringing/v2/commands/{channel}"), &lease, &body)
        .await
    {
        Ok(response) => response,
        Err(_) => return error_response(StatusCode::BAD_GATEWAY, "daemon_unavailable"),
    };
    forward_response(response).await
}

async fn proxy_command_status(
    State(state): State<GatewayState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let session = match authenticate(&state, &headers, true) {
        Ok(session) => session,
        Err(response) => return response,
    };
    if !valid_opaque_id(&id) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_command_id");
    }
    let lease = session.lease_snapshot();
    let response = match state
        .daemon
        .get(&format!("/ringing/v2/commands/{id}"), &lease)
        .await
    {
        Ok(response) => response,
        Err(_) => return error_response(StatusCode::BAD_GATEWAY, "daemon_unavailable"),
    };
    forward_response(response).await
}

async fn proxy_content_get(
    State(state): State<GatewayState>,
    AxumPath(content_id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let session = match authenticate(&state, &headers, true) {
        Ok(session) => session,
        Err(response) => return response,
    };
    if !valid_opaque_id(&content_id) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_content_id");
    }
    let lease = session.lease_snapshot();
    let response = match state
        .daemon
        .get(&format!("/ringing/v2/content/{content_id}"), &lease)
        .await
    {
        Ok(response) => response,
        Err(_) => return error_response(StatusCode::BAD_GATEWAY, "daemon_unavailable"),
    };
    forward_response(response).await
}

async fn proxy_content_upload(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let session = match authenticate(&state, &headers, true) {
        Ok(session) => session,
        Err(response) => return response,
    };
    if !security::csrf_matches(&headers, session.csrf_token()) {
        return error_response(StatusCode::FORBIDDEN, "csrf_failed");
    }
    let lease = session.lease_snapshot();
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream");
    let response = match state
        .daemon
        .post_bytes("/ringing/v2/content", &lease, body.to_vec(), content_type)
        .await
    {
        Ok(response) => response,
        Err(_) => return error_response(StatusCode::BAD_GATEWAY, "daemon_unavailable"),
    };
    forward_response(response).await
}

async fn proxy_events(
    State(state): State<GatewayState>,
    AxumPath(channel): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let session = match authenticate(&state, &headers, true) {
        Ok(session) => session,
        Err(response) => return response,
    };
    if channel_from_path(&channel).is_none() {
        return error_response(StatusCode::BAD_REQUEST, "invalid_channel");
    }
    if session.active_seed().is_none() {
        return error_response(StatusCode::CONFLICT, "no_active_seed");
    }
    let lease = session.lease_snapshot();
    let response = match state
        .daemon
        .get_stream(&format!("/ringing/v1/events/{channel}"), &lease, &headers)
        .await
    {
        Ok(response) => response,
        Err(_) => return error_response(StatusCode::BAD_GATEWAY, "daemon_unavailable"),
    };
    stream_response(response)
}

async fn proxy_bootstrap(
    State(state): State<GatewayState>,
    AxumPath(seed): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    proxy_seeded_get(state, seed, headers, "bootstrap").await
}

async fn proxy_timeline(
    State(state): State<GatewayState>,
    AxumPath(seed): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    proxy_seeded_get(state, seed, headers, "timeline").await
}

async fn proxy_timeline_events(
    State(state): State<GatewayState>,
    AxumPath(seed): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let session = match authenticate(&state, &headers, true) {
        Ok(session) => session,
        Err(response) => return response,
    };
    if session.active_seed().as_deref() != Some(seed.as_str()) {
        return error_response(StatusCode::FORBIDDEN, "seed_scope_violation");
    }
    let lease = session.lease_snapshot();
    let response = match state
        .daemon
        .get_stream(
            &format!(
                "/ringing/v2/sessions/{}/timeline/events",
                encode_path(&seed)
            ),
            &lease,
            &headers,
        )
        .await
    {
        Ok(response) => response,
        Err(_) => return error_response(StatusCode::BAD_GATEWAY, "daemon_unavailable"),
    };
    stream_response(response)
}

async fn proxy_seeded_get(
    state: GatewayState,
    seed: String,
    headers: HeaderMap,
    suffix: &str,
) -> Response {
    let session = match authenticate(&state, &headers, true) {
        Ok(session) => session,
        Err(response) => return response,
    };
    if session.active_seed().as_deref() != Some(seed.as_str()) {
        return error_response(StatusCode::FORBIDDEN, "seed_scope_violation");
    }
    let lease = session.lease_snapshot();
    let path = if suffix == "bootstrap" {
        format!("/ringing/v2/sessions/{}/bootstrap", encode_path(&seed))
    } else {
        format!("/ringing/v2/sessions/{}/timeline", encode_path(&seed))
    };
    let response = match state.daemon.get(&path, &lease).await {
        Ok(response) => response,
        Err(_) => return error_response(StatusCode::BAD_GATEWAY, "daemon_unavailable"),
    };
    forward_response(response).await
}

async fn proxy_service(
    State(state): State<GatewayState>,
    AxumPath(method): AxumPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let session = match authenticate(&state, &headers, true) {
        Ok(session) => session,
        Err(response) => return response,
    };
    if !security::csrf_matches(&headers, session.csrf_token()) {
        return error_response(StatusCode::FORBIDDEN, "csrf_failed");
    }
    if !session.allow_service() {
        return error_response(StatusCode::TOO_MANY_REQUESTS, "service_rate_limited");
    }
    if method == "session.list" {
        return match fetch_sessions(&state, &session).await {
            Ok(value) => Json(value).into_response(),
            Err(response) => response,
        };
    }
    if !service_allowed(&method) {
        return error_response(StatusCode::FORBIDDEN, "service_not_allowed");
    }
    let mut params: Value = if body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_body"),
        }
    };
    if service_requires_seed(&method) {
        let Some(active_seed) = session.active_seed() else {
            return error_response(StatusCode::CONFLICT, "no_active_seed");
        };
        if let Some(object) = params.as_object_mut() {
            object.insert("seed".into(), Value::String(active_seed.clone()));
            if method.starts_with("fs.") {
                object.insert("scope_seed".into(), Value::String(active_seed));
            }
        } else {
            params = json!({ "seed": active_seed });
        }
    }
    let lease = session.lease_snapshot();
    let response = match state
        .daemon
        .post_json(
            &format!("/ringing/v2/service/{}", encode_path(&method)),
            &lease,
            &params,
        )
        .await
    {
        Ok(response) => response,
        Err(_) => return error_response(StatusCode::BAD_GATEWAY, "daemon_unavailable"),
    };
    if matches!(
        method.as_str(),
        "session.meta" | "workspace.get" | "workspace.list"
    ) {
        if !response.status().is_success() {
            return forward_response(response).await;
        }
        return match response.json::<Value>().await {
            Ok(value) => Json(sanitize_service_response(&method, value)).into_response(),
            Err(_) => error_response(StatusCode::BAD_GATEWAY, "invalid_daemon_response"),
        };
    }
    forward_response(response).await
}

fn sanitize_service_response(method: &str, value: Value) -> Value {
    match method {
        "session.meta" => sanitize_session_meta(&value),
        "workspace.get" => sanitize_workspace(&value),
        "workspace.list" => Value::Array(
            value
                .as_array()
                .map(|entries| entries.iter().map(sanitize_workspace).collect())
                .unwrap_or_default(),
        ),
        _ => value,
    }
}

fn sanitize_session_meta(value: &Value) -> Value {
    json!({
        "seed": value.get("seed").cloned().unwrap_or(Value::Null),
        "title": value.get("title").cloned().unwrap_or(Value::Null),
        "created_at": value.get("created_at").cloned().unwrap_or(Value::Null),
        "updated_at": value.get("updated_at").cloned().unwrap_or(Value::Null),
        "message_count": value.get("message_count").cloned().unwrap_or(Value::Null),
        "turn_count": value.get("turn_count").cloned().unwrap_or(Value::Null),
        "running": value.get("running").cloned().unwrap_or(Value::Bool(false)),
        "archived": value.get("archived").cloned().unwrap_or(Value::Bool(false)),
        "ephemeral": value.get("ephemeral").cloned().unwrap_or(Value::Bool(false)),
    })
}

fn sanitize_workspace(value: &Value) -> Value {
    json!({
        "id": value.get("id").cloned().unwrap_or(Value::Null),
        "title": value.get("title").cloned().unwrap_or(Value::Null),
        "order": value.get("order").cloned().unwrap_or(Value::Null),
        "missing_dir": value.get("missing_dir").cloned().unwrap_or(Value::Bool(false)),
    })
}

#[allow(clippy::result_large_err)]
fn authenticate(
    state: &GatewayState,
    headers: &HeaderMap,
    require_origin: bool,
) -> Result<Arc<BrowserSession>, Response> {
    browser_request_allowed(state, headers, require_origin)?;
    let session_id = security::cookie_value(headers, security::SESSION_COOKIE)
        .ok_or_else(|| error_response(StatusCode::UNAUTHORIZED, "session_required"))?;
    let session = state
        .sessions
        .get(&session_id)
        .ok_or_else(|| error_response(StatusCode::UNAUTHORIZED, "session_expired"))?;
    session.touch();
    Ok(session)
}

#[allow(clippy::result_large_err)]
fn browser_request_allowed(
    state: &GatewayState,
    headers: &HeaderMap,
    require_origin: bool,
) -> Result<(), Response> {
    if !security::host_allowed(headers, &state.allowed_hosts) {
        return Err(error_response(
            StatusCode::MISDIRECTED_REQUEST,
            "invalid_host",
        ));
    }
    if !security::sec_fetch_site_allowed(headers) {
        return Err(error_response(StatusCode::FORBIDDEN, "cross_site"));
    }
    if require_origin && !security::origin_allowed(headers, &state.allowed_origins) {
        return Err(error_response(StatusCode::FORBIDDEN, "invalid_origin"));
    }
    Ok(())
}

fn spawn_renewal(state: GatewayState, session: Arc<BrowserSession>) {
    tokio::spawn(async move {
        let mut cancel = session.subscribe_cancel();
        loop {
            let lease = session.lease_snapshot();
            if lease.server_epoch != state.daemon.epoch()
                || lease.last_renew.elapsed() >= Duration::from_millis(lease.ttl_ms)
            {
                state.sessions.remove(session.id());
                break;
            }
            let interval = Duration::from_millis((lease.renew_interval_ms / 2).max(1_000));
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                changed = cancel.changed() => {
                    if changed.is_err() || *cancel.borrow() {
                        break;
                    }
                    continue;
                }
            }
            // A seed switch renegotiates the lease and replaces the old
            // client_session_id. Never renew the stale snapshot; loop and pick
            // up the new lease instead.
            if session.lease_snapshot().client_session_id != lease.client_session_id {
                continue;
            }
            if state.daemon.renew(&lease).await.is_err() {
                state.sessions.remove(session.id());
                break;
            }
            session.mark_renewed();
        }
    });
}

fn sanitize_command(command: &mut RingingCommand) -> bool {
    // Approval commands are accepted only through `/__gateway/approvals/{id}`,
    // where the browser must present a server-issued one-shot challenge.
    match command {
        RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
            as_system,
            ..
        }) => {
            // System-role injection is an internal daemon capability, never a
            // browser-selectable message option.
            *as_system = false;
            true
        }
        RingingCommand::Conversation(ConversationCommand::ConversationCancel { .. }) => true,
        _ => false,
    }
}

fn service_allowed(method: &str) -> bool {
    matches!(
        method,
        "daemon.version"
            | "session.list"
            | "session.meta"
            | "session.activity"
            | "session.dashboard"
            | "session.get_activity"
            | "workspace.get"
            | "workspace.list"
            | "fs.list"
            | "fs.read"
            | "todo.status"
            | "todo.list"
            | "plan.read"
            | "plan.context_stats"
            | "stats.token_usage"
            | "git.diff"
            | "git.branch"
            | "git.branches"
            | "git.file_diff"
    )
}

fn service_requires_seed(method: &str) -> bool {
    !matches!(
        method,
        "daemon.version" | "session.list" | "session.activity" | "workspace.list"
    )
}

fn channel_from_path(channel: &str) -> Option<RingingChannel> {
    match channel {
        "control" => Some(RingingChannel::Control),
        "conversation" => Some(RingingChannel::Conversation),
        "tool" => Some(RingingChannel::Tool),
        _ => None,
    }
}

fn valid_seed(seed: &str) -> bool {
    seed.len() == 8 && seed.chars().all(|character| character.is_ascii_hexdigit())
}

fn valid_opaque_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn valid_daemon_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

fn channel_path(channel: RingingChannel) -> &'static str {
    match channel {
        RingingChannel::Control => "control",
        RingingChannel::Conversation => "conversation",
        RingingChannel::Tool => "tool",
    }
}

fn encode_path(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn error_response(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({ "code": code }))).into_response()
}

async fn forward_response(response: UpstreamResponse) -> Response {
    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(_) => return error_response(StatusCode::BAD_GATEWAY, "daemon_response_failed"),
    };
    let mut out = Response::builder()
        .status(status)
        .body(Body::from(bytes))
        .unwrap_or_else(|_| error_response(StatusCode::BAD_GATEWAY, "proxy_failed"));
    if let Some(content_type) = content_type {
        out.headers_mut().insert(header::CONTENT_TYPE, content_type);
    }
    out
}

fn stream_response(response: UpstreamResponse) -> Response {
    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let mut out = Response::builder()
        .status(status)
        .body(Body::from_stream(response.bytes_stream()))
        .unwrap_or_else(|_| error_response(StatusCode::BAD_GATEWAY, "proxy_failed"));
    if let Some(content_type) = content_type {
        out.headers_mut().insert(header::CONTENT_TYPE, content_type);
    }
    out
}

async fn security_headers(req: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self'; script-src-attr 'none'; style-src 'self'; style-src-attr 'none'; img-src 'self'; font-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'; object-src 'none'",
        ),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let ctrl_c = tokio::signal::ctrl_c();
        let terminate = async {
            match signal(SignalKind::terminate()) {
                Ok(mut stream) => {
                    stream.recv().await;
                }
                Err(error) => {
                    log::error!("[webui-gateway] SIGTERM handler install failed: {error}");
                    std::future::pending::<()>().await;
                }
            }
        };
        tokio::select! {
            _ = ctrl_c => {}
            _ = terminate => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::util::ServiceExt;

    fn discovery(endpoint: &str) -> DaemonDiscovery {
        DaemonDiscovery {
            endpoint: endpoint.into(),
            token: "test-token".into(),
            pid: std::process::id(),
            server_epoch: "epoch".into(),
            protocol_version: CONTROL_PROTOCOL_VERSION,
            daemon_version: "1.0.1".into(),
            build_id: "build-1".into(),
            channel: "dev".into(),
            executable: "qaqh-daemon".into(),
        }
    }

    fn test_state() -> GatewayState {
        let (hosts, origins) = browser_origins(41234);
        GatewayState {
            daemon: Arc::new(DaemonClient::new(&discovery("http://127.0.0.1:1")).unwrap()),
            nonces: Arc::new(NonceStore::new()),
            sessions: Arc::new(SessionStore::new()),
            allowed_hosts: Arc::new(hosts),
            allowed_origins: Arc::new(origins),
        }
    }

    #[test]
    fn config_accepts_random_or_fixed_port_only() {
        let random = GatewayConfig::parse(&[], "build-1").unwrap();
        assert_eq!(random.port, 0);
        let fixed = GatewayConfig::parse(&["--port".into(), "41234".into()], "build-1").unwrap();
        assert_eq!(fixed.port, 41234);
        assert!(
            GatewayConfig::parse(&["--bind".into(), "0.0.0.0".into()], "build-1").is_err(),
            "the gateway must not expose a bind override"
        );
    }

    #[test]
    fn endpoint_parser_rejects_non_loopback_and_loose_authority() {
        for endpoint in [
            "http://192.168.1.5:41234",
            "http://evil.example:41234",
            "https://127.0.0.1:41234",
            "http://user@127.0.0.1:41234",
            "http://127.0.0.1:41234/control/v1",
            "http://127.0.0.1:41234?token=secret",
            "http://127.0.0.1",
        ] {
            assert!(
                parse_loopback_endpoint(endpoint).is_err(),
                "{endpoint} must be rejected"
            );
        }
        assert_eq!(
            parse_loopback_endpoint("http://127.0.0.1:41234").unwrap(),
            LoopbackEndpoint {
                host: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 41234,
            }
        );
        assert_eq!(
            parse_loopback_endpoint("http://[::1]:41234").unwrap(),
            LoopbackEndpoint {
                host: IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
                port: 41234,
            }
        );
    }

    #[test]
    fn discovery_validation_requires_same_build_and_loopback() {
        let valid = discovery("http://127.0.0.1:41234");
        assert!(validate_discovery(&valid, "build-1", CONTROL_PROTOCOL_VERSION).is_ok());

        let mut wrong_build = valid.clone();
        wrong_build.build_id = "build-2".into();
        assert!(validate_discovery(&wrong_build, "build-1", CONTROL_PROTOCOL_VERSION).is_err());

        let mut wrong_protocol = valid.clone();
        wrong_protocol.protocol_version += 1;
        assert!(validate_discovery(&wrong_protocol, "build-1", CONTROL_PROTOCOL_VERSION).is_err());

        let mut lan = valid.clone();
        lan.endpoint = "http://192.168.1.5:41234".into();
        assert!(validate_discovery(&lan, "build-1", CONTROL_PROTOCOL_VERSION).is_err());

        let mut no_token = valid;
        no_token.token.clear();
        assert!(validate_discovery(&no_token, "build-1", CONTROL_PROTOCOL_VERSION).is_err());
    }

    #[tokio::test]
    async fn router_serves_assets_and_sets_security_headers() {
        let app = build_router(test_state());
        let response = app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .and_then(|value| value.to_str().ok()),
            Some(
                "default-src 'none'; script-src 'self'; script-src-attr 'none'; style-src 'self'; style-src-attr 'none'; img-src 'self'; font-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'; object-src 'none'"
            )
        );
        assert_eq!(
            response.headers().get(header::X_FRAME_OPTIONS),
            Some(&HeaderValue::from_static("DENY"))
        );
        let body = to_bytes(response.into_body(), MAX_BODY_BYTES)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("test-token"));

        for path in [
            "/debug/",
            "/__gateway/unknown",
            "/assets/missing.js",
            "/assets/foo.map",
            "/assets/%2e%2e/index.html",
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/settings")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn router_serves_hashed_assets_and_rejects_source_maps() {
        let app = build_router(test_state());
        let asset = WebUi::iter()
            .find(|path| path.starts_with("assets/") && path.ends_with(".js"))
            .expect("Vite build must emit a JavaScript asset");
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/{asset}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE),
            Some(&HeaderValue::from_static("text/javascript; charset=utf-8"))
        );
        assert!(!allowed_asset_path("assets/index.js.map"));
        assert!(!allowed_asset_path("assets/.hidden.js"));
        assert!(!allowed_asset_path("assets/../index.html"));
    }

    #[tokio::test]
    async fn session_exchange_requires_nonce_and_origin() {
        let state = test_state();
        let app = build_router(state.clone());
        let mut request = Request::builder()
            .method("POST")
            .uri("/__gateway/session")
            .header("host", "127.0.0.1:41234")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"nonce":"missing"}"#))
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))));
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let nonce = state.nonces.issue("127.0.0.1".parse().unwrap()).unwrap();
        let mut request = Request::builder()
            .method("POST")
            .uri("/__gateway/session")
            .header("host", "127.0.0.1:41234")
            .header("origin", "http://127.0.0.1:41234")
            .header("sec-fetch-site", "same-origin")
            .header("content-type", "application/json")
            .body(Body::from(json!({ "nonce": nonce }).to_string()))
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))));
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn command_and_service_allowlists_are_explicit() {
        let mut send = RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
            text: "hi".into(),
            images: Vec::new(),
            attachments: None,
            message_id: None,
            input_purpose: qaqh_domain::ConversationInputPurpose::TriggerTurn,
            as_system: true,
        });
        assert!(sanitize_command(&mut send));
        assert!(matches!(
            send,
            RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
                as_system: false,
                ..
            })
        ));

        let mut create = RingingCommand::Control(ControlCommand::SessionCreate {
            close_current: false,
            cwd: None,
            tool_mode: None,
            custom_tools: Vec::new(),
        });
        assert!(!sanitize_command(&mut create));
        let mut permission =
            RingingCommand::Tool(qaqh_domain::ToolCommand::ToolPermissionRespond {
                tool_call_id: "call-1".into(),
                approved: true,
                trust_folder: false,
            });
        assert!(!sanitize_command(&mut permission));
        assert!(service_allowed("fs.read"));
        assert!(!service_allowed("config.save"));
        assert!(!service_allowed("workspace.delete"));
    }

    #[test]
    fn approvals_are_opaque_seed_bound_and_single_use() {
        let session = BrowserSession::new(session::Lease::new(
            "instance".into(),
            "lease".into(),
            "epoch".into(),
            30_000,
            10_000,
        ));
        let pending = json!({
            "pending_permission": {
                "tool_call_id": "canonical-tool-call",
                "tool_name": "exec",
                "action_summary": "run cargo test",
                "reason": "requires approval",
                "paths": ["/tmp/workspace"],
                "category": "exec",
                "level": 3,
                "risk": "high",
                "consequence": "executes a command"
            },
            "pending_interaction": {
                "id": "canonical-ask",
                "kind": "ask",
                "details": { "questions": [] }
            }
        });
        let views = issue_approval_views(&session, "0123abcd", &pending).unwrap();
        assert_eq!(views.len(), 2);
        let tool_view = views
            .iter()
            .find(|view| view["kind"] == "tool_permission")
            .unwrap();
        assert_ne!(tool_view["challenge_id"], "canonical-tool-call");
        assert!(tool_view["details"].get("tool_call_id").is_none());

        let challenge_id = tool_view["challenge_id"].as_str().unwrap();
        let challenge = session.consume_approval(challenge_id, "0123abcd").unwrap();
        assert_eq!(challenge.source_id, "canonical-tool-call");
        assert_eq!(
            session
                .consume_approval(challenge_id, "0123abcd")
                .unwrap_err(),
            "approval_not_found"
        );

        let ask_view = views.iter().find(|view| view["kind"] == "ask").unwrap();
        let ask_id = ask_view["challenge_id"].as_str().unwrap();
        assert_eq!(
            session.consume_approval(ask_id, "deadbeef").unwrap_err(),
            "approval_scope_violation"
        );
    }

    #[test]
    fn sanitized_views_remove_paths_and_models() {
        let session = sanitize_session_meta(&json!({
            "seed": "0123abcd",
            "title": "demo",
            "cwd": "/home/secret",
            "model": "internal-model",
            "skills": { "entries": [] }
        }));
        assert!(session.get("cwd").is_none());
        assert!(session.get("model").is_none());
        assert!(session.get("skills").is_none());

        let workspace = sanitize_workspace(&json!({
            "id": "w1",
            "title": "demo",
            "path": "/home/secret",
            "session_ids": ["0123abcd"]
        }));
        assert!(workspace.get("path").is_none());
        assert!(workspace.get("session_ids").is_none());
    }
}
