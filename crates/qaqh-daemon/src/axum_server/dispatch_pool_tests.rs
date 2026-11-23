//! v4.4 `P1-0a` 反证守卫：分派面同步臂与回执持久化边界**必须**在阻塞池执行，
//! Tokio worker 在派发进行中保持可用。
//!
//! 装置（全部真实链路，零 mock）：专用 runtime 只有 **1 个 worker** 与
//! **1 个 blocking 线程**；先向 blocking 池投递一个 250ms 的引信任务（FIFO，
//! 池线程整段被它占用），再并发跑真实请求：
//!
//! - `POST /ringing/v2/commands`（`provider.presets.list` 同步臂 + admission
//!   回执 persist）与 `GET /activity`（`registry` 锁读）在新实现排队进阻塞池，
//!   必须等引信放完才返回（elapsed ≥ 100ms）；
//! - 同窗口 `GET /health` 环路走空闲 worker，必须毫秒级。
//!
//! 旧实现（同步直跑 Tokio worker）下命令/`activity` 完全不与引信排队、几毫秒
//! 即回 —— 排队断言当场红，这正是宪法 #18 要求的「先证旧实现红」。

use super::{build_router, test_state};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use std::time::{Duration, Instant};
use tower::util::ServiceExt;

const ADMIN: &str = "test-token";
const BLOCKER_MS: u64 = 250;
/// 排队下限：引信独占 250ms，新旧实现的分野在这里取 100ms 保守半程。
const QUEUED_MIN_MS: u64 = 100;
/// worker 存活上限：/health 只读 epoch 字符串，正常应为毫秒级。
const WORKER_FREE_MAX_MS: u64 = 150;

fn pool_bound_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .expect("single-worker/single-blocking-thread runtime")
}

fn request(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {ADMIN}"))
        .body(Body::empty())
        .expect("request")
}

async fn body_json(response: Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

/// 独占唯一 blocking 线程的引信。
fn light_fuse() {
    tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(BLOCKER_MS)));
}

/// 在引信窗口内跑 20 轮 `/health`，返回总耗时。worker 被占则环路必然变慢。
async fn health_loop(app: &Router) -> Duration {
    let start = Instant::now();
    for _ in 0..20 {
        let response = app
            .clone()
            .oneshot(request("GET", "/health"))
            .await
            .expect("health");
        assert_eq!(response.status(), StatusCode::OK);
    }
    start.elapsed()
}

async fn open_admin_client_session(app: &Router) -> String {
    let response = app
        .clone()
        .oneshot({
            Request::builder()
                .method("POST")
                .uri("/ringing/v2/clients/open")
                .header(header::AUTHORIZATION, format!("Bearer {ADMIN}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_value(qaqh_ringing::RingingV2OpenRequest::new(
                        "p1-0a-instance",
                    ))
                    .expect("open request")
                    .to_string(),
                ))
                .expect("request")
        })
        .await
        .expect("open");
    assert_eq!(response.status(), StatusCode::OK, "admin open must succeed");
    let json = body_json(response).await;
    json["client_session_id"]
        .as_str()
        .expect("client_session_id")
        .to_string()
}

async fn post_presets_command(app: &Router, client_session: &str) -> Duration {
    let body = serde_json::json!({
        "schema": qaqh_ringing::RINGING_SCHEMA,
        "version": qaqh_ringing::RINGING_V2_VERSION,
        "contract_revision": qaqh_ringing::COMMAND_CONTRACT_REVISION,
        "command_id": qaqh_ringing_contract::ulid::generate_ulid(),
        "client_instance_id": "p1-0a-instance",
        "client_session_id": client_session,
        "target": { "kind": "global" },
        "preconditions": [],
        "command": { "type": "provider.presets.list" },
    });
    let start = Instant::now();
    let response = app
        .clone()
        .oneshot({
            Request::builder()
                .method("POST")
                .uri("/ringing/v2/commands")
                .header(header::AUTHORIZATION, format!("Bearer {ADMIN}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-qaqh-client-session-id", client_session)
                .body(Body::from(body.to_string()))
                .expect("request")
        })
        .await
        .expect("command");
    let status = response.status();
    let json = body_json(response).await;
    assert_eq!(status, StatusCode::OK, "presets ack must be 200: {json}");
    start.elapsed()
}

#[test]
fn sync_command_dispatch_queues_on_blocking_pool() {
    let runtime = pool_bound_runtime();
    runtime.block_on(async move {
        let state = test_state();
        let app = build_router(state.clone());
        let client_session = open_admin_client_session(&app).await;

        light_fuse();
        let (health_elapsed, command_elapsed) =
            tokio::join!(health_loop(&app), post_presets_command(&app, &client_session));
        // 旧实现（同步臂在 worker 上直跑）不与引信排队，几毫秒即回 —— 本断言红。
        assert!(
            command_elapsed >= Duration::from_millis(QUEUED_MIN_MS),
            "sync command arm must queue behind the blocking-pool fuse (old sync-on-worker dispatch returns immediately): {command_elapsed:?}"
        );
        // worker 在分派面排队期间必须保持可用。
        assert!(
            health_elapsed < Duration::from_millis(WORKER_FREE_MAX_MS),
            "tokio worker must stay free while the dispatch surface waits on the blocking pool: health loop took {health_elapsed:?}"
        );
    });
}

#[test]
fn activity_snapshot_queues_on_blocking_pool() {
    let runtime = pool_bound_runtime();
    runtime.block_on(async move {
        let state = test_state();
        let app = build_router(state.clone());

        light_fuse();
        let activity_start = Instant::now();
        let (health_elapsed, activity_response) =
            tokio::join!(health_loop(&app), app.clone().oneshot(request("GET", "/activity")));
        let activity_elapsed = activity_start.elapsed();
        let activity_response = activity_response.expect("activity");
        assert_eq!(activity_response.status(), StatusCode::OK);

        // 旧实现：activity 在 worker 上直接取 registry 锁，不与引信排队 —— 红。
        assert!(
            activity_elapsed >= Duration::from_millis(QUEUED_MIN_MS),
            "activity_snapshot must queue on the blocking pool behind the fuse (old sync-on-worker path returns immediately): {activity_elapsed:?}"
        );
        assert!(
            health_elapsed < Duration::from_millis(WORKER_FREE_MAX_MS),
            "tokio worker must stay free while activity waits on the blocking pool: health loop took {health_elapsed:?}"
        );
    });
}
