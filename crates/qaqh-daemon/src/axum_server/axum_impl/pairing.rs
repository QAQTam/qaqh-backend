//! axum_impl::pairing — 扫码配对与设备管理（spec-daemon-auth-devices §11）。
//!
//! 配对是**唯一**依赖桌面端在线的时刻：之后一切请求只靠 `device_token`
//! （daemon 在跑即可）。配对令牌一次性、短寿、仅可调 `/pair`。

use super::v2::{api_error_response, json_response};
use super::*;

use qaqh_ringing::{
    RINGING_V2_VERSION, RingingV2DeviceWire, RingingV2DevicesResponse, RingingV2PairRequest,
    RingingV2PairResponse, RingingV2PairTokenRequest, RingingV2PairTokenResponse,
};
use qaqh_ringing_runtime::Scope;

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
    let (token, ttl_ms) = {
        let mut pairings = state
            .pairings
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let token = pairings.issue(scope, request.device_name, request.platform);
        (token, pairings.ttl_ms())
    };
    json_response(
        StatusCode::OK,
        &RingingV2PairTokenResponse {
            pairing_token: token,
            expires_in_ms: ttl_ms,
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
    // v4.4 P1-3：注册表落 `paired_devices`；写失败是硬故障（500），
    // 不得静默发出一个没进权威表的 device_token。
    let (device_id, device_token) = match state.devices.issue(&name, &platform, grant.scope).await {
        Ok(pair) => pair,
        Err(error) => {
            log::error!("[pair] device registration failed: {error}");
            return api_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "device registration failed",
            );
        }
    };
    json_response(
        StatusCode::OK,
        &RingingV2PairResponse {
            device_id,
            device_token,
            scope: scope_str(grant.scope).to_string(),
            daemon_version: env!("QAQH_BUILD_VERSION").into(),
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
    let records = match state.devices.list().await {
        Ok(records) => records,
        Err(error) => {
            log::error!("[pair] device list failed: {error}");
            return api_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "device registry unavailable",
            );
        }
    };
    let devices = records
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
    let revoked = match state.devices.revoke(&device_id).await {
        Ok(revoked) => revoked,
        Err(error) => {
            log::error!("[pair] device revoke failed: {error}");
            return api_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "device revoke failed",
            );
        }
    };
    if revoked.is_none() {
        return api_error_response(
            StatusCode::NOT_FOUND,
            "device_not_found",
            "unknown device id",
        );
    }
    state
        .leases
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .revoke_device(&device_id);
    // ADR-0028 D5：吊销是用户 shell 面三条**显式** kill 路之一（断联不杀，吊销
    // 必杀）。owner 口径与 shell 面同源 —— 都是 device_id。
    let shells = state.service.shutdown_shells_of(&device_id);
    if shells > 0 {
        log::info!("[daemon] revoke {device_id}: stopped {shells} user shell(s)");
    }
    StatusCode::NO_CONTENT.into_response()
}
