//! 路由表 ≡ 真实 router 的等价性测试（ADR-0003 决策 2c）。
//!
//! 两张表（`CONTROL_PLANE_ROUTES` + `qaqh_ringing::RINGING_V2_HTTP_ROUTES`）是路由的
//! 唯一登记处。本测试从**行为侧**证明三件事：
//! 1. 表里每条 `(method, path)` 都被真实注册 —— 响应不得是 router 的 fallback；
//! 2. 未登记路径确实落到 fallback（`404` + body 恰为 `"not found"`）；
//! 3. `build_router` 不 panic ⇒ 每条表项都有 match arm（缺 arm 即 panic，见两张表的循环）。
//!
//! **探针一律带 view scope 设备凭证**，理由是安全性而不是正确性：control 的两个 stop
//! 端点先过 `require_scope(&identity, Scope::Admin)` 才动 shutdown
//! （`axum_impl/control.rs:40-42` 与 `:55-57`），view 凭证拿到 403 即返回，**永不触发真实
//! 关停**；而 `test_state()` 里的 `TEST_SERVICE` 是跨测试的 `OnceLock`（`axum_server.rs:45`），
//! 一旦被关掉，同一测试二进制里后续 daemon 测试会全废。

use super::AppState;
use super::axum_impl::CONTROL_PLANE_ROUTES;
use super::build_router;
use super::test_state;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use qaqh_ringing::RINGING_V2_HTTP_ROUTES;
use qaqh_ringing_runtime::Scope;
use tower::util::ServiceExt;

/// `axum_impl/control.rs:32-34` 的 fallback 签名。
const FALLBACK_BODY: &str = "not found";

/// 表里的路径参数填合成值：handler 会因无 lease / 无此 id / 无此方法而拒绝，
/// 但那**不是 router 的 fallback**，所以断言仍然成立。
///
/// 新加占位符而忘了在此登记合成值 ⇒ 直接失败（fail-closed），不允许静默漏测。
fn synth(path: &str) -> String {
    let mut out = path.to_owned();
    for (placeholder, value) in [
        (
            "{session_id}",
            "0199a0f0-0000-7000-8000-000000000001",
        ),
        ("{command_id}", "cmd-synthetic"),
        (
            "{content_id}",
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        ),
        ("{device_id}", "device-synthetic"),
        ("{method}", "daemon.version"),
    ] {
        out = out.replace(placeholder, value);
    }
    assert!(
        !out.contains('{'),
        "路由 {path} 含未登记合成值的路径参数，请在 synth() 里补一条"
    );
    out
}

fn view_token(state: &AppState) -> String {
    let (_device_id, token) = state
        .devices
        .lock()
        .expect("device registry lock")
        .issue("route-table-probe", "node", Scope::View);
    token
}

fn probe(method: &str, uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("probe request")
}

/// 只有「404 且 body 是 fallback 的固定文本」才算未注册。handler 自己返回的 404
/// （例如 content 不存在）body 形状不同，不会被误判成路由缺失。
async fn is_router_fallback(response: Response) -> bool {
    if response.status() != StatusCode::NOT_FOUND {
        return false;
    }
    let bytes = axum::body::to_bytes(response.into_body(), 64)
        .await
        .expect("fallback body");
    bytes.as_ref() == FALLBACK_BODY.as_bytes()
}

/// 名字带 `v2_` 前缀是刻意的：CI 只跑 `cargo test -p qaqh-daemon v2_`
/// （`.github/workflows/sdk-contract.yml:39`），否则这道闸在 CI 里根本不会被执行。
#[tokio::test]
async fn v2_route_table_equals_the_real_router() {
    let state = test_state();
    // 走到这里没 panic，已经证明「每张表的每行都有 match arm」。
    let app = build_router(state.clone());
    let token = view_token(&state);

    let declared = CONTROL_PLANE_ROUTES.iter().chain(RINGING_V2_HTTP_ROUTES.iter());
    assert_eq!(
        CONTROL_PLANE_ROUTES.len() + RINGING_V2_HTTP_ROUTES.len(),
        20,
        "路由登记处总数变了：若是有意的，同步重切 snapshots/routes.txt 并按 guard-tightening 处理"
    );

    for (method, path) in declared {
        let uri = synth(path);
        let response = app
            .clone()
            .oneshot(probe(method, &uri, &token))
            .await
            .expect("router oneshot");
        assert!(
            !is_router_fallback(response).await,
            "表里登记的 {method} {path}（合成后 {uri}）在真实 router 上是 404 not found —— 表与 router 漂移了"
        );
    }

    // 反向：未登记路径必须是 fallback，否则上面的断言只是「什么都不是」。
    // 注意不能拿「已登记但 id 不存在」的路径当幽灵（handler 自己的 404 不是 fallback）。
    for ghost in [
        "/nope",
        "/healths",
        "/activity-x",
        "/control/v1/stop-now",
        "/ringing/v2/clients/openn",
        "/ringing/v2/sessions/0199a0f0-0000-7000-8000-000000000001/nope",
    ] {
        let method = if ghost.starts_with("/control/v1") {
            "POST"
        } else {
            "GET"
        };
        let response = app
            .clone()
            .oneshot(probe(method, ghost, &token))
            .await
            .expect("ghost oneshot");
        assert!(
            is_router_fallback(response).await,
            "未登记路径 {method} {ghost} 没有被 fallback 接住 —— fallback 挂了或被绕过"
        );
    }
}
