//! axum_impl::debug_control — see parent module docs.

use super::*;

use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "webui-dist"]
struct WebUi;

pub(crate) async fn health(State(state): State<AppState>) -> impl IntoResponse {
    // P0-3：不再回显 `token_len`——长度本身就是对凭据的旁路信息，`/health`
    // 免鉴权且不受回环守卫约束。
    (StatusCode::OK, format!("ok epoch={}", state.epoch))
}

/// Debug 桥一次性兑换券（nonce）存储。
///
/// `/debug/__qaqh_bridge__.js` 只下发 nonce，不再把全权 token 交给页面脚本；
/// 前端凭 nonce 调 [`handle_debug_token`]（`POST /debug/__qaqh_token__`）
/// **一次性**换取 token，兑换即作废。券带 TTL，过期同样作废——即使 nonce 被
/// 读到，也只在极短窗口内、且只能兑换一次。
pub struct DebugNonceStore {
    ttl: Duration,
    inner: Mutex<HashMap<String, std::time::Instant>>,
}

impl Default for DebugNonceStore {
    fn default() -> Self {
        Self::new()
    }
}

impl DebugNonceStore {
    pub fn new() -> Self {
        Self {
            ttl: Duration::from_secs(60),
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// 签发一张券（并顺手回收过期券，避免长驻 daemon 无界增长）。
    fn issue(&self) -> String {
        let nonce = random_hex();
        let now = std::time::Instant::now();
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, issued| now.duration_since(*issued) < self.ttl);
        map.insert(nonce.clone(), now);
        nonce
    }

    /// 兑换并作废。已兑换 / 不存在 / 过期一律 `false`（fail-closed）。
    fn redeem(&self, nonce: &str) -> bool {
        if nonce.is_empty() {
            return false;
        }
        let now = std::time::Instant::now();
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match map.remove(nonce) {
            Some(issued) => now.duration_since(issued) < self.ttl,
            None => false,
        }
    }
}

/// 只读活动快照（冻结事故 P0 观测项）：暴露 has_active_work 与逐会话
/// 活动状态，冻结会话可直接从外部探测。与 /health 同级免鉴权，仅含
/// seed/state/turn_id/seq/updated_at，无用户内容。
pub(crate) async fn activity(State(state): State<AppState>) -> impl IntoResponse {
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
}

pub(crate) async fn not_found() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "not found")
}

pub(crate) fn renderer_root() -> PathBuf {
    if let Ok(dir) = std::env::var("QAQH_DEBUG_RENDERER_DIR") {
        return PathBuf::from(dir);
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_default();
    let candidates = [
        cwd.join("out").join("renderer"),
        cwd.join("resources").join("out").join("renderer"),
        exe_dir.join("out").join("renderer"),
    ];
    for c in &candidates {
        if c.join("index.html").exists() {
            return c.clone();
        }
    }
    candidates[0].clone()
}

pub(crate) fn mime_for(path: &StdPath) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") | Some("map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("wasm") => "application/wasm",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

pub(crate) fn safe_join(root: &StdPath, url_path: &str) -> Option<PathBuf> {
    let decoded = url_path.replace("%20", " ").replace("%2E", ".");
    let mut parts = Vec::new();
    for comp in StdPath::new(&decoded).components() {
        match comp {
            Component::Normal(seg) => parts.push(seg),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    let mut joined = root.to_path_buf();
    for seg in parts {
        joined.push(seg);
    }
    fn strip_unc(p: &StdPath) -> PathBuf {
        let s = p.to_string_lossy();
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
        PathBuf::from(s.to_string())
    }
    let canonical_root = strip_unc(&root.canonicalize().unwrap_or_else(|_| root.to_path_buf()));
    let canonical_joined = strip_unc(&joined.canonicalize().unwrap_or(joined));
    if !canonical_joined.starts_with(&canonical_root) {
        return None;
    }
    Some(canonical_joined)
}

pub(crate) async fn handle_debug_bridge(State(state): State<AppState>) -> Response {
    // P0-3：桥脚本只下发**一次性 nonce**，不再内联全权 token。
    let nonce = state.debug_nonces.issue();
    let body = format!("window.__QAQH_DEBUG__={{\"nonce\":\"{nonce}\"}};\n");
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

/// `Sec-Fetch-Site` 白名单：缺省（curl/原生壳等非浏览器客户端）放行；
/// 浏览器发起的请求只允许 `same-origin`（本页自用）与 `none`（地址栏直接
/// 导航）。跨站页面（DNS rebinding / 外站 `<script>` 之外的 fetch）会被拒。
fn sec_fetch_site_allowed(headers: &HeaderMap) -> bool {
    match headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
    {
        None => true,
        Some(site) => site.eq_ignore_ascii_case("same-origin") || site.eq_ignore_ascii_case("none"),
    }
}

/// `POST /debug/__qaqh_token__`：凭一次性 nonce 兑换全权 token。
///
/// 兑换即作废（见 [`DebugNonceStore::redeem`]）；响应 `no-store`，不得被缓存。
/// 挂在 `/debug` 前缀下，因此回环 IP + Host 白名单 + CORP/nosniff 三层已覆盖；
/// 这里再要求 `Sec-Fetch-Site` 同源，堵住 rebinding 页面的跨站兑换。
pub(crate) async fn handle_debug_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !sec_fetch_site_allowed(&headers) {
        return (
            StatusCode::FORBIDDEN,
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            br#"{"code":"cross_site","message":"debug token exchange is same-origin only"}"#
                .as_slice(),
        )
            .into_response();
    }
    let params: serde_json::Value = if body.is_empty() {
        serde_json::json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    [(header::CONTENT_TYPE, "application/json")],
                    br#"{"code":"invalid_body","message":"expected {\"nonce\":\"...\"}"}"#
                        .as_slice(),
                )
                    .into_response();
            }
        }
    };
    let Some(nonce) = params.get("nonce").and_then(|value| value.as_str()) else {
        return (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "application/json")],
            br#"{"code":"invalid_envelope","message":"nonce is required"}"#.as_slice(),
        )
            .into_response();
    };
    if !state.debug_nonces.redeem(nonce) {
        return (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "application/json")],
            br#"{"code":"invalid_nonce","message":"nonce is unknown, expired, or already redeemed"}"#
                .as_slice(),
        )
            .into_response();
    }
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        serde_json::json!({ "token": state.token }).to_string(),
    )
        .into_response()
}

pub(crate) async fn handle_debug(
    State(_state): State<AppState>,
    Path(path): Path<String>,
) -> Response {
    let rel = if path.is_empty() { "index.html" } else { &path };
    // 优先尝试编译时嵌入的产物（单文件分发），缺失时回退到文件系统（dev 实时构建）
    let decoded_rel = rel.replace("%20", " ").replace("%2E", ".");
    if !decoded_rel.contains("..") && !decoded_rel.starts_with('/') {
        if let Some(embedded) = WebUi::get(&decoded_rel) {
            let data = embedded.data;
            let mime = mime_for(StdPath::new(&decoded_rel));
            // index.html 注入桥脚本（与文件系统路径一致，CSP 兼容）
            let is_index = decoded_rel == "index.html" || decoded_rel.ends_with("/index.html");
            if is_index {
                let mut html = String::from_utf8_lossy(&data).into_owned();
                let script = "<script src=\"./__qaqh_bridge__.js\"></script>";
                if let Some(idx) = html.find("</head>") {
                    html.insert_str(idx, script);
                } else {
                    html.push_str(script);
                }
                return (
                    [
                        (header::CONTENT_TYPE, mime),
                        (header::CACHE_CONTROL, "no-cache"),
                    ],
                    html,
                )
                    .into_response();
            }
            return (
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                data.into_owned(),
            )
                .into_response();
        }
        // 尝试嵌入的 index.html 作为 SPA 回退（前端路由）——仅当 rel 非文件且嵌入存在
        if WebUi::get(&decoded_rel).is_none()
            && !decoded_rel.contains('.')
            && let Some(embedded) = WebUi::get("index.html")
        {
            let data = embedded.data;
            let mime = mime_for(StdPath::new("index.html"));
            let mut html = String::from_utf8_lossy(&data).into_owned();
            let script = "<script src=\"./__qaqh_bridge__.js\"></script>";
            if let Some(idx) = html.find("</head>") {
                html.insert_str(idx, script);
            } else {
                html.push_str(script);
            }
            return (
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                html,
            )
                .into_response();
        }
    }
    let root = renderer_root();
    let Some(file) = safe_join(&root, rel) else {
        return (
            StatusCode::BAD_REQUEST,
            [(header::CACHE_CONTROL, "no-cache")],
            "invalid path",
        )
            .into_response();
    };
    if !file.exists() || !file.is_file() {
        // 文件系统缺失 → 仅对 SPA 路由（无扩展名）回退到嵌入 index.html
        if !decoded_rel.contains('.')
            && let Some(embedded) = WebUi::get("index.html")
        {
            let data = embedded.data;
            let mime = mime_for(StdPath::new("index.html"));
            let mut html = String::from_utf8_lossy(&data).into_owned();
            let script = "<script src=\"./__qaqh_bridge__.js\"></script>";
            if let Some(idx) = html.find("</head>") {
                html.insert_str(idx, script);
            } else {
                html.push_str(script);
            }
            return (
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                html,
            )
                .into_response();
        }
        return (
            StatusCode::NOT_FOUND,
            [(header::CACHE_CONTROL, "no-cache")],
            "not found",
        )
            .into_response();
    }
    let bytes = match tokio::fs::read(&file).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    let mime = mime_for(&file);
    if file.file_name().and_then(|n| n.to_str()) == Some("index.html") {
        let mut html = String::from_utf8_lossy(&bytes).into_owned();
        let script = "<script src=\"./__qaqh_bridge__.js\"></script>";
        if let Some(idx) = html.find("</head>") {
            html.insert_str(idx, script);
        } else {
            html.push_str(script);
        }
        return (
            [
                (header::CONTENT_TYPE, mime),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            html,
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        bytes,
    )
        .into_response()
}

pub(crate) async fn handle_debug_index(State(state): State<AppState>) -> Response {
    handle_debug(State(state), Path("index.html".to_string())).await
}

pub(crate) async fn loopback_guard(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if req.uri().path().starts_with("/debug")
        && let Some(ConnectInfo(addr)) = req.extensions().get::<ConnectInfo<SocketAddr>>().cloned()
        && !addr.ip().is_loopback()
    {
        return (
            StatusCode::FORBIDDEN,
            [
                (header::CONTENT_TYPE, "text/plain"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            "webUI hosting is restricted to loopback connections",
        )
            .into_response();
    }
    next.run(req).await
}

/// `/debug` 托管面的 Host 白名单判定。
///
/// 回环守卫只看得见对端 IP，看不见浏览器带来的 `Host`：DNS rebinding 下攻击者页面会带着
/// 自己的域名访问 `127.0.0.1:<port>`，对端 IP 仍是回环，仅凭 IP 无法识别。因此 `/debug`
/// 必须额外要求 Host 为回环名字（浏览器发起的 rebinding 恒带 Host，缺 Host 一律 fail-closed）。
pub(crate) fn loopback_host_allowed(host: &str) -> bool {
    let host = host.trim();
    if host.is_empty() {
        return false;
    }
    // 裸 IPv6 回环（非规范 Host 写法，宽容接受）
    if host == "::1" {
        return true;
    }
    let name = if let Some(rest) = host.strip_prefix('[') {
        // IPv6 字面量：`[::1]:51325` / `[::1]`；括号后只允许空串或端口
        match rest.split_once(']') {
            Some((addr, tail)) if tail.is_empty() || tail.starts_with(':') => addr,
            _ => return false,
        }
    } else {
        // 名称/IPv4 可选带端口：`127.0.0.1:51325`、`localhost:51325`
        host.split_once(':').map_or(host, |(name, _)| name)
    };
    name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1" || name == "::1"
}

/// `/debug` 的 Host 白名单守卫（与 `loopback_guard` 同层；两者共同构成回环边界）。
pub(crate) async fn host_guard(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if req.uri().path().starts_with("/debug") {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if !loopback_host_allowed(host) {
            return (
                StatusCode::MISDIRECTED_REQUEST,
                [
                    (header::CONTENT_TYPE, "text/plain"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                "webUI hosting requires a loopback Host header",
            )
                .into_response();
        }
    }
    next.run(req).await
}

/// `/debug` 响应加固：跨源 no-cors 子资源加载（`<script src=...>`）必须在浏览器侧被拒，
/// 否则任意网页都能把桥脚本执行进自己的 realm 并读到 `window.__QAQH_DEBUG__`。
/// 与 `host_guard` 互为补充：一个堵「跨源读取」，一个堵「rebinding 同源」。
pub(crate) async fn debug_headers(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let is_debug = req.uri().path().starts_with("/debug");
    let mut response = next.run(req).await;
    if is_debug {
        let headers = response.headers_mut();
        headers.insert(
            header::HeaderName::from_static("cross-origin-resource-policy"),
            axum::http::HeaderValue::from_static("same-origin"),
        );
        headers.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            axum::http::HeaderValue::from_static("nosniff"),
        );
    }
    response
}

pub(crate) async fn handle_stop(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
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
    headers: HeaderMap,
) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
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

#[cfg(test)]
mod tests {
    use super::*;

    /// T-2-3：券一次性、未知/空券一律 fail-closed。
    #[test]
    fn debug_nonce_is_single_use() {
        let store = DebugNonceStore::new();
        let nonce = store.issue();
        assert!(store.redeem(&nonce), "first redemption must succeed");
        assert!(!store.redeem(&nonce), "second redemption must fail");
        assert!(!store.redeem("deadbeef"), "unknown nonce must fail");
        assert!(!store.redeem(""), "empty nonce must fail");
    }

    /// `Sec-Fetch-Site` 白名单：缺省/同源/直接导航放行，跨站拒绝。
    #[test]
    fn sec_fetch_site_allowlist() {
        let mut headers = HeaderMap::new();
        assert!(
            sec_fetch_site_allowed(&headers),
            "absent header = non-browser client"
        );
        for allowed in ["same-origin", "none", "SAME-ORIGIN"] {
            headers.insert("sec-fetch-site", allowed.parse().unwrap());
            assert!(sec_fetch_site_allowed(&headers), "{allowed} must pass");
        }
        for rejected in ["cross-site", "same-site"] {
            headers.insert("sec-fetch-site", rejected.parse().unwrap());
            assert!(
                !sec_fetch_site_allowed(&headers),
                "{rejected} must be rejected"
            );
        }
    }
}
