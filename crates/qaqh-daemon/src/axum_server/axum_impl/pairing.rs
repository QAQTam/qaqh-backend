//! axum_impl::pairing — 扫码配对与设备管理（spec-daemon-auth-devices §11）。
//!
//! 配对是**唯一**依赖桌面端在线的时刻：之后一切请求只靠 `device_token`
//! （daemon 在跑即可）。配对令牌一次性、短寿、仅可调 `/pair`。

use super::v2::{api_error_response, json_response};
use super::*;
use std::collections::HashMap;
use std::time::Instant;

use qaqh_ringing::{
    RINGING_V2_VERSION, RingingV2DeviceWire, RingingV2DevicesResponse, RingingV2PairRequest,
    RingingV2PairResponse, RingingV2PairTokenRequest, RingingV2PairTokenResponse,
};
use qaqh_runtime::ringing::{Scope, device_registry};

/// 配对令牌 TTL（一次性）。
const PAIRING_TOKEN_TTL_MS: u64 = 120_000;

#[derive(Debug, Clone)]
struct PairingGrant {
    scope: Scope,
    device_name: String,
    platform: String,
    expires_at: Instant,
    consumed: bool,
}

/// 短寿配对令牌表（仅内存）。键 = `SHA256(pairing_token)` 摘要。
#[derive(Debug, Default)]
pub struct PairingTable {
    grants: HashMap<String, PairingGrant>,
}

impl PairingTable {
    pub fn new() -> Self {
        Self::default()
    }

    fn purge(&mut self) {
        let now = Instant::now();
        // 已消费且过期的直接丢；未消费的过期项保留到被消费时判 `pairing_expired`。
        self.grants
            .retain(|_, grant| grant.consumed || grant.expires_at > now);
    }

    fn issue(&mut self, scope: Scope, device_name: String, platform: String) -> String {
        self.purge();
        let token = format!(
            "{}{}",
            qaqh_session::canonical::generate_ulid(),
            qaqh_session::canonical::generate_ulid()
        );
        let digest = device_registry::token_digest(&token);
        self.grants.insert(
            digest,
            PairingGrant {
                scope,
                device_name,
                platform,
                expires_at: Instant::now() + Duration::from_millis(PAIRING_TOKEN_TTL_MS),
                consumed: false,
            },
        );
        token
    }

    /// 一次性消费。错误码区分 `pairing_used` / `pairing_expired` / `pairing_invalid`。
    fn consume(&mut self, token: &str) -> Result<PairingGrant, &'static str> {
        let digest = device_registry::token_digest(token);
        let Some(grant) = self.grants.get(&digest).cloned() else {
            self.purge();
            return Err("pairing_invalid");
        };
        if grant.consumed {
            return Err("pairing_used");
        }
        if grant.expires_at <= Instant::now() {
            self.grants.remove(&digest);
            return Err("pairing_expired");
        }
        if let Some(stored) = self.grants.get_mut(&digest) {
            stored.consumed = true;
        }
        Ok(grant)
    }
}

fn parse_scope(raw: &str) -> Option<Scope> {
    match raw {
        "view" => Some(Scope::View),
        "interact" => Some(Scope::Interact),
        "admin" => Some(Scope::Admin),
        _ => None,
    }
}

fn scope_str(scope: Scope) -> &'static str {
    match scope {
        Scope::View => "view",
        Scope::Interact => "interact",
        Scope::Admin => "admin",
    }
}

fn parse_body<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, Response> {
    serde_json::from_slice(body).map_err(|error| {
        api_error_response(
            StatusCode::BAD_REQUEST,
            "invalid_body",
            &format!("invalid pairing request: {error}"),
        )
    })
}

/// `POST /ringing/v2/pairing/tokens` — 签发一次性配对令牌（Admin）。
pub(crate) async fn handle_pairing_tokens(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    body: Bytes,
) -> Response {
    if let Err(response) = require_scope(&identity, Scope::Admin) {
        return response;
    }
    let request: RingingV2PairTokenRequest = match parse_body(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let Some(scope) = parse_scope(request.scope_grant.trim()) else {
        return api_error_response(
            StatusCode::BAD_REQUEST,
            "invalid_scope",
            "scope_grant must be one of view|interact|admin",
        );
    };
    let token = state
        .pairings
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .issue(scope, request.device_name, request.platform);
    json_response(
        StatusCode::OK,
        &RingingV2PairTokenResponse {
            pairing_token: token,
            expires_in_ms: PAIRING_TOKEN_TTL_MS,
            tls_fp: state.tls_fingerprint.clone().unwrap_or_default(),
        },
    )
}

/// `POST /ringing/v2/pair` — 消费一次性配对令牌换取设备凭证（免 Bearer）。
pub(crate) async fn handle_pair(State(state): State<AppState>, body: Bytes) -> Response {
    let request: RingingV2PairRequest = match parse_body(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let grant = match state
        .pairings
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .consume(&request.pairing_token)
    {
        Ok(grant) => grant,
        Err(code) => {
            return api_error_response(StatusCode::FORBIDDEN, code, "pairing token rejected");
        }
    };
    let name = if request.device_name.trim().is_empty() {
        grant.device_name
    } else {
        request.device_name
    };
    let platform = if request.platform.trim().is_empty() {
        grant.platform
    } else {
        request.platform
    };
    let (device_id, device_token) = state
        .devices
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .issue(&name, &platform, grant.scope);
    json_response(
        StatusCode::OK,
        &RingingV2PairResponse {
            device_id,
            device_token,
            scope: scope_str(grant.scope).to_string(),
            daemon_version: env!("CARGO_PKG_VERSION").into(),
            protocol_version: RINGING_V2_VERSION,
        },
    )
}

/// `GET /ringing/v2/devices` — 设备列表（Admin，不含任何 token 材料）。
pub(crate) async fn handle_devices(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
) -> Response {
    if let Err(response) = require_scope(&identity, Scope::Admin) {
        return response;
    }
    let devices = state
        .devices
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .list()
        .into_iter()
        .map(|record| RingingV2DeviceWire {
            device_id: record.device_id,
            name: record.name,
            platform: record.platform,
            scope: scope_str(record.scope).to_string(),
            created_at_ms: record.created_at_ms,
            last_seen_ms: record.last_seen_ms,
        })
        .collect();
    json_response(StatusCode::OK, &RingingV2DevicesResponse { devices })
}

/// `POST /ringing/v2/devices/{id}/revoke` — 吊销设备（Admin）。
/// 删注册表项 + 强失效该设备在途 lease（切断已建立的 SSE）。
pub(crate) async fn handle_device_revoke(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Path(device_id): Path<String>,
) -> Response {
    if let Err(response) = require_scope(&identity, Scope::Admin) {
        return response;
    }
    let revoked = state
        .devices
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .revoke(&device_id);
    if revoked.is_none() {
        return api_error_response(StatusCode::NOT_FOUND, "device_not_found", "unknown device id");
    }
    state
        .leases
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .revoke_device(&device_id);
    StatusCode::NO_CONTENT.into_response()
}
