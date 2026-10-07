//! 设备配对(m0-daemon-authz):壳代签发一次性配对令牌 + 设备管理。
//! 规范:docs/spec-daemon-auth-devices.md §11;daemon 处理器在
//! crates/qaqh-daemon/src/axum_server/axum_impl/pairing.rs。
//!
//! 职责边界:QR payload 由**壳**拼装(daemon 不生成二维码)——discovery 的
//! endpoint 就是手机要访问的 base_url(`scheme://advertise_ip:port`,LAN 模式
//! 才有 TLS);签发 `POST /ringing/v2/pairing/tokens` 用 discovery 里的 admin
//! token(本地桌面身份,authz.rs 全程豁免)。二维码只在用户显式点击时签发:
//! 桌面确认 = 发 token。

use serde_json::{json, Value};
use tauri::State;

use qaqh_client::read_discovery;

use crate::daemon::HostState;

const PAIRING_SCOPES: &[&str] = &["view", "interact", "admin"];

fn string_of(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// discovery → (可直达的 daemon base URL, admin token)。
fn admin_endpoint() -> Result<(reqwest::Url, String), String> {
    let discovery = read_discovery().map_err(|error| format!("读取 daemon discovery 失败: {error}"))?;
    let url = reqwest::Url::parse(&discovery.endpoint)
        .map_err(|error| format!("discovery endpoint 非法({}): {error}", discovery.endpoint))?;
    Ok((url, discovery.token))
}

/// base_url 指向回环 = daemon 没开 LAN 模式,手机扫了也连不上:直接拒绝配对。
fn loopback_problem(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else { return true };
    if host == "localhost" {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        return ip.is_loopback();
    }
    let v6 = host.trim_start_matches('[').trim_end_matches(']');
    v6.parse::<std::net::Ipv6Addr>().map(|ip| ip.is_loopback()).unwrap_or(false)
}

/// daemon 的 ringing v2 HTTPS 面走自签证书:信任边界是 admin token(与
/// qaqh-client 的控制通道同一条),证书只用于传输加密,不做链式校验。
async fn admin_json(method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, String> {
    // reqwest 是 rustls-no-provider:ring provider 由 qaqh-client 在 connect 时装,
    // 但配对命令不依赖 client 连接,这里幂等兜底(重复 install 是无害 no-op)。
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (base, token) = admin_endpoint()?;
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .map_err(|error| format!("HTTP 客户端构建失败: {error}"))?;
    let mut request = client
        .request(method, base.join(path).map_err(string_of)?)
        .bearer_auth(token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.map_err(|error| format!("daemon 请求失败: {error}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("daemon 响应 {status}: {text}"));
    }
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).map_err(|error| format!("daemon 响应解析失败: {error}"))
}

fn host_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "qaqh-desktop".into())
}

/// `(scope_grant, device_name, platform?) → { qr_payload, expires_in_ms, base_url }`。
/// qr_payload 是规范 §11 的 JSON 字符串,前端直接喂给二维码编码器。
#[tauri::command]
pub async fn pairing_create(
    _state: State<'_, HostState>,
    scope_grant: String,
    device_name: String,
    platform: Option<String>,
) -> Result<Value, String> {
    let scope = scope_grant.trim();
    if !PAIRING_SCOPES.contains(&scope) {
        return Err(format!("scope 仅允许 {}，收到 {scope}", PAIRING_SCOPES.join("|")));
    }
    let (base, _) = admin_endpoint()?;
    if loopback_problem(&base) {
        return Err(
            "daemon 绑定在回环地址(127.0.0.1),手机无法访问。跨设备配对需以 LAN 模式启动 daemon(--bind <局域网IP>)。"
                .into(),
        );
    }
    let response = admin_json(
        reqwest::Method::POST,
        "/ringing/v2/pairing/tokens",
        Some(json!({
            "scope_grant": scope,
            "device_name": device_name.trim(),
            "platform": platform.unwrap_or_else(|| "mobile".into()),
        })),
    )
    .await?;
    let pairing_token = response["pairing_token"]
        .as_str()
        .ok_or_else(|| "daemon 响应缺 pairing_token".to_string())?
        .to_string();
    let expires_in_ms = response["expires_in_ms"].as_u64().unwrap_or(120_000);
    let tls_fp = response["tls_fp"].as_str().unwrap_or_default();
    let payload = json!({
        "v": 1,
        "kind": "qaqh-pair",
        "base_url": base.as_str().trim_end_matches('/'),
        "pairing_token": pairing_token,
        "tls_fp": tls_fp,
        "host_name": host_name(),
    });
    Ok(json!({
        "qr_payload": serde_json::to_string(&payload).map_err(string_of)?,
        "expires_in_ms": expires_in_ms,
        "base_url": base.as_str(),
    }))
}

/// `() → { devices: [...] }`(不含任何 token 材料)。
#[tauri::command]
pub async fn devices_list() -> Result<Value, String> {
    admin_json(reqwest::Method::GET, "/ringing/v2/devices", None).await
}

/// `(device_id) → ()`:吊销并强制断开该设备的 SSE 租约。
#[tauri::command]
pub async fn device_revoke(device_id: String) -> Result<(), String> {
    // id 只进 URL path:白名单字符,不给注入留面。
    if device_id.is_empty() || !device_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("device_id 非法".into());
    }
    admin_json(
        reqwest::Method::POST,
        &format!("/ringing/v2/devices/{device_id}/revoke"),
        None,
    )
    .await
    .map(|_| ())
}
