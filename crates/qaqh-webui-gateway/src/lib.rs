//! Independent, loopback-only browser gateway for the QAQ-Harness WebUI.
//!
//! This crate intentionally has no daemon routes and no browser-visible bearer
//! token. Phase 1 only establishes the process boundary and discovery gate;
//! session exchange, static assets, and the restricted proxy surface are added
//! in later phases.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use axum::{
    Router,
    extract::{DefaultBodyLimit, Path as AxumPath},
    http::{HeaderName, HeaderValue, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use qaqh_types::{CONTROL_PROTOCOL_VERSION, DaemonDiscovery};
use rust_embed::RustEmbed;
use tokio::net::{TcpListener, TcpStream};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const CONNECT_ATTEMPTS: usize = 5;
const RETRY_DELAY: Duration = Duration::from_millis(100);
const MAX_BODY_BYTES: usize = 64 * 1024;

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

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, config.port))
        .await
        .map_err(|error| format!("bind webui gateway to 127.0.0.1: {}", error))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("resolve webui gateway address: {error}"))?;
    let url = format!("http://{address}");
    println!("qaqh-webui-gateway: listening on {url}");
    log::info!("[webui-gateway] listening on {url}");

    let app = build_router();
    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|error| format!("serve webui gateway: {error}"))?;
    log::info!("[webui-gateway] stopped");
    Ok(())
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

/// Gateway router. It serves only the compiled WebUI asset tree; daemon API
/// routes, nonce exchange, and control endpoints remain unmounted until later
/// phases add them behind explicit allowlists.
pub fn build_router() -> Router {
    Router::new()
        .route("/", get(serve_index))
        .route("/assets/{*path}", get(serve_asset))
        .fallback(serve_spa)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn(security_headers))
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

async fn security_headers(req: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'; object-src 'none'",
        ),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        header::HeaderName::from_static("cross-origin-resource-policy"),
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
        let app = build_router();
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
                "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'; object-src 'none'"
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
            "/__gateway/bootstrap.js",
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
        let app = build_router();
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
}
