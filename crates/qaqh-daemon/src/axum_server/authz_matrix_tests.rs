//! daemon 设备鉴权越权矩阵（spec-daemon-auth-devices §13，daemon 侧）。
//!
//! 覆盖 `build_router` 真实路径（含 `authenticate` 中间件 + 各 handler）：
//! 配对一次性、admin/device 身份、scope 分级（view/interact/admin）、
//! 会话归属（owns_session 403/401）、设备吊销后 401、devices 列表不泄 token。
//!
//! **未覆盖**（属 spec §13 余项，需真实 canonical 会话 fixture）：
//! - 在途 SSE 被 `revoke_device` 切断（handler 已修复，见 `v2.rs::handle_events_v2`；
//!   但 `v2_hub.subscribe/publish_*` 需真实会话，测试装置成本高）；
//! - 归因落账 `Api:device_id`（需真实审批往返后查账本）；
//! - 跨语言 wire 一致性（需 Kotlin/ArkTS fixture）。

use super::*;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use qaqh_runtime::ringing::Scope;
use tower::util::ServiceExt; // oneshot

const ADMIN: &str = "test-token";
const SEED: &str = "seed-matrix";

fn request(
    method: &str,
    uri: &str,
    token: Option<&str>,
    client_session: Option<&str>,
    body: Option<serde_json::Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(cs) = client_session {
        builder = builder.header("x-qaqh-client-session-id", cs);
    }
    let body = match body {
        Some(value) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    builder.body(body).expect("request")
}

async fn body_json(response: Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

fn open_body() -> serde_json::Value {
    serde_json::json!({
        "schema": qaqh_ringing::RINGING_SCHEMA,
        "version": qaqh_ringing::RINGING_V2_VERSION,
        // 设备场景下该自报字段不参与信任判定（daemon 用 device_id 覆盖）。
        "client_instance_id": "spoofed-instance",
    })
}

/// 注册一个设备并 `open` 取得 lease。返回 `(device_token, device_id, client_session_id)`。
async fn open_device(router: &Router, state: &AppState, scope: Scope) -> (String, String, String) {
    let (device_id, token) = state
        .devices
        .lock()
        .unwrap()
        .issue("e2e-device", "node", scope);
    let response = router
        .clone()
        .oneshot(request("POST", "/ringing/v2/clients/open", Some(&token), None, Some(open_body())))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "device open should succeed");
    let json = body_json(response).await;
    let client_session = json["client_session_id"].as_str().unwrap().to_string();
    (token, device_id, client_session)
}

/// 构造一个合法的 v2 命令信封（`command` 为内层命令 JSON）。
fn envelope(client_session: &str, command: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "schema": qaqh_ringing::RINGING_SCHEMA,
        "version": qaqh_ringing::RINGING_V2_VERSION,
        "channel": "control",
        "command_id": qaqh_session::canonical::generate_ulid(),
        "client_instance_id": "inst",
        "client_session_id": client_session,
        "session_id": SEED,
        "command": command,
    })
}

fn session_attach(seed: &str) -> serde_json::Value {
    serde_json::json!({ "channel": "control", "type": "session_attach", "session_id": seed })
}

fn session_close(seed: &str) -> serde_json::Value {
    serde_json::json!({ "channel": "control", "type": "session_close", "session_id": seed })
}

async fn post_command(
    router: &Router,
    token: &str,
    client_session: &str,
    command: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = router
        .clone()
        .oneshot(request(
            "POST",
            "/ringing/v2/commands/control",
            Some(token),
            Some(client_session),
            Some(envelope(client_session, command)),
        ))
        .await
        .unwrap();
    let status = response.status();
    (status, body_json(response).await)
}

/// 经 admin 端点配对出一个设备，返回 `(device_token, device_id)`。
async fn pair_device(router: &Router, scope: &str) -> (String, String) {
    let token_response = router
        .clone()
        .oneshot(request(
            "POST",
            "/ringing/v2/pairing/tokens",
            Some(ADMIN),
            None,
            Some(serde_json::json!({ "scope_grant": scope, "device_name": "e2e", "platform": "node" })),
        ))
        .await
        .unwrap();
    assert_eq!(token_response.status(), StatusCode::OK);
    let pairing_token = body_json(token_response).await["pairing_token"]
        .as_str()
        .unwrap()
        .to_string();
    let pair_response = router
        .clone()
        .oneshot(request(
            "POST",
            "/ringing/v2/pair",
            None,
            None,
            Some(serde_json::json!({ "pairing_token": pairing_token, "device_name": "e2e", "platform": "node" })),
        ))
        .await
        .unwrap();
    assert_eq!(pair_response.status(), StatusCode::OK);
    let json = body_json(pair_response).await;
    (
        json["device_token"].as_str().unwrap().to_string(),
        json["device_id"].as_str().unwrap().to_string(),
    )
}

#[tokio::test]
async fn health_is_public_and_unknown_path_requires_auth() {
    let app = build_router(super::axum_tests::test_state());
    let health = app
        .clone()
        .oneshot(request("GET", "/health", None, None, None))
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);

    let unknown = app
        .oneshot(request("GET", "/no-such-path", None, None, None))
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unknown_or_revoked_token_is_unauthorized() {
    let state = super::axum_tests::test_state();
    let app = build_router(state.clone());

    let bogus = app
        .clone()
        .oneshot(request(
            "GET",
            &format!("/ringing/v2/sessions/{SEED}/bootstrap"),
            Some("not-a-real-token"),
            Some("cs-x"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(bogus.status(), StatusCode::UNAUTHORIZED);

    let (token, device_id, cs) = open_device(&app, &state, Scope::View).await;
    // 吊销走 HTTP 端点（覆盖 registy 删项 + lease 强失效）。
    let revoke = app
        .clone()
        .oneshot(request(
            "POST",
            &format!("/ringing/v2/devices/{device_id}/revoke"),
            Some(ADMIN),
            None,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(revoke.status(), StatusCode::NO_CONTENT);

    let after = app
        .oneshot(request(
            "GET",
            &format!("/ringing/v2/sessions/{SEED}/bootstrap"),
            Some(&token),
            Some(&cs),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(after.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn pairing_requires_admin_and_token_is_one_shot() {
    let state = super::axum_tests::test_state();
    let app = build_router(state.clone());

    // 设备 token 调配对签发 → 403 insufficient_scope。
    let (view_token, _, view_cs) = open_device(&app, &state, Scope::View).await;
    let denied = app
        .clone()
        .oneshot(request(
            "POST",
            "/ringing/v2/pairing/tokens",
            Some(&view_token),
            Some(&view_cs),
            Some(serde_json::json!({ "scope_grant": "view", "device_name": "x", "platform": "y" })),
        ))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(denied).await["code"], "insufficient_scope");

    // admin 签发 → 一次性。
    let issued = app
        .clone()
        .oneshot(request(
            "POST",
            "/ringing/v2/pairing/tokens",
            Some(ADMIN),
            None,
            Some(serde_json::json!({ "scope_grant": "view", "device_name": "x", "platform": "y" })),
        ))
        .await
        .unwrap();
    assert_eq!(issued.status(), StatusCode::OK);
    let pairing_token = body_json(issued).await["pairing_token"]
        .as_str()
        .unwrap()
        .to_string();

    let first = app
        .clone()
        .oneshot(request(
            "POST",
            "/ringing/v2/pair",
            None,
            None,
            Some(serde_json::json!({ "pairing_token": pairing_token })),
        ))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);

    let replay = app
        .clone()
        .oneshot(request(
            "POST",
            "/ringing/v2/pair",
            None,
            None,
            Some(serde_json::json!({ "pairing_token": pairing_token })),
        ))
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(replay).await["code"], "pairing_used");

    let forged = app
        .oneshot(request(
            "POST",
            "/ringing/v2/pair",
            None,
            None,
            Some(serde_json::json!({ "pairing_token": "bogus" })),
        ))
        .await
        .unwrap();
    assert_eq!(forged.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(forged).await["code"], "pairing_invalid");
}

#[tokio::test]
async fn view_token_cannot_read_unattached_session() {
    let state = super::axum_tests::test_state();
    let app = build_router(state.clone());
    let (token, _, cs) = open_device(&app, &state, Scope::View).await;

    let response = app
        .oneshot(request(
            "GET",
            &format!("/ringing/v2/sessions/{SEED}/bootstrap"),
            Some(&token),
            Some(&cs),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(response).await["code"], "forbidden_not_owner");
}

#[tokio::test]
async fn view_token_cannot_issue_non_attach_command() {
    let state = super::axum_tests::test_state();
    let app = build_router(state.clone());
    let (token, _, cs) = open_device(&app, &state, Scope::View).await;

    // 非 attach 命令 → interact 不足 → 403 insufficient_scope。
    let (status, json) = post_command(&app, &token, &cs, session_close(SEED)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json["code"], "insufficient_scope");
}

/// spec §7 的核心：view 档设备须能 `SessionAttach` 建立归属后视察。
/// （`SessionAttach` 仅建立归属、不触碰 actor，故 scope 门槛降为 view。）
#[tokio::test]
async fn view_token_can_attach_then_read() {
    let state = super::axum_tests::test_state();
    let app = build_router(state.clone());
    let (token, _, cs) = open_device(&app, &state, Scope::View).await;

    let (status, json) = post_command(&app, &token, &cs, session_attach(SEED)).await;
    assert_eq!(status, StatusCode::OK, "view token must be allowed to attach: {json}");
    assert_eq!(json["status"], "accepted");

    // attach 后归属建立；会话本身不存在 → 404（而非 403）。
    let response = app
        .oneshot(request(
            "GET",
            &format!("/ringing/v2/sessions/{SEED}/bootstrap"),
            Some(&token),
            Some(&cs),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn interact_token_attach_establishes_ownership() {
    let state = super::axum_tests::test_state();
    let app = build_router(state.clone());
    let (token, _, cs) = open_device(&app, &state, Scope::Interact).await;

    let before = app
        .clone()
        .oneshot(request(
            "GET",
            &format!("/ringing/v2/sessions/{SEED}/team"),
            Some(&token),
            Some(&cs),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(before.status(), StatusCode::FORBIDDEN);

    let (status, _) = post_command(&app, &token, &cs, session_attach(SEED)).await;
    assert_eq!(status, StatusCode::OK);

    let after = app
        .oneshot(request(
            "GET",
            &format!("/ringing/v2/sessions/{SEED}/team"),
            Some(&token),
            Some(&cs),
            None,
        ))
        .await
        .unwrap();
    assert_ne!(after.status(), StatusCode::FORBIDDEN);
    assert_ne!(after.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn command_rejects_foreign_target_session() {
    let state = super::axum_tests::test_state();
    let app = build_router(state.clone());
    let (token, _, cs) = open_device(&app, &state, Scope::Interact).await;

    // 未 attach 的会话上执行非 attach 命令 → 归属检查 403。
    let (status, json) = post_command(&app, &token, &cs, session_close(SEED)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json["code"], "forbidden_not_owner");
}

#[tokio::test]
async fn device_cannot_impersonate_other_lease() {
    let state = super::axum_tests::test_state();
    let app = build_router(state.clone());
    let (_t1, _d1, cs1) = open_device(&app, &state, Scope::View).await;
    let (t2, _d2, cs2) = open_device(&app, &state, Scope::View).await;

    // d2 用自己的 cs 先 attach SEED（建立归属）。
    let (status, _) = post_command(&app, &t2, &cs2, session_attach(SEED)).await;
    assert_eq!(status, StatusCode::OK);

    // d2 拿 d1 的 cs → 身份↔lease 绑定失败 → 401（不能借 d1 的 lease）。
    let impersonate = app
        .clone()
        .oneshot(request(
            "GET",
            &format!("/ringing/v2/sessions/{SEED}/bootstrap"),
            Some(&t2),
            Some(&cs1),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(impersonate.status(), StatusCode::UNAUTHORIZED);

    // 用自己的 cs → 归属成立（会话不存在 → 404）。
    let own = app
        .oneshot(request(
            "GET",
            &format!("/ringing/v2/sessions/{SEED}/bootstrap"),
            Some(&t2),
            Some(&cs2),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(own.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn admin_is_exempt_from_ownership_and_scope() {
    let state = super::axum_tests::test_state();
    let app = build_router(state.clone());

    let open = app
        .clone()
        .oneshot(request("POST", "/ringing/v2/clients/open", Some(ADMIN), None, Some(open_body())))
        .await
        .unwrap();
    let cs = body_json(open).await["client_session_id"]
        .as_str()
        .unwrap()
        .to_string();

    // 未 attach 的任意会话：admin 不被 owns 拦（会话不存在 → 404，而非 403/401）。
    let response = app
        .oneshot(request(
            "GET",
            &format!("/ringing/v2/sessions/{SEED}/bootstrap"),
            Some(ADMIN),
            Some(&cs),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn devices_list_excludes_token_material() {
    let state = super::axum_tests::test_state();
    let app = build_router(state.clone());
    let (device_token, device_id) = pair_device(&app, "view").await;

    let response = app
        .oneshot(request("GET", "/ringing/v2/devices", Some(ADMIN), None, None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    let listed = json["devices"]
        .as_array()
        .unwrap()
        .iter()
        .any(|device| device["device_id"] == device_id);
    assert!(listed, "device must appear in list");
    assert!(
        !json.to_string().contains(&device_token),
        "/devices must not leak token material"
    );
}
