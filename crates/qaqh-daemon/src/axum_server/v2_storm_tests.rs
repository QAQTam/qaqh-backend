//! P0-2026-11-10（docs/p0-2026-11-10-ringingv2-attach-livelock-report.md）回归守卫：
//! RingingV2 读面「原子 attach + 回放尾」（H1'）。
//!
//! 守卫语义（修复前这三条全部反向失败，见报告 §5）：
//! - 水位与订阅之间的事件**必须经回放尾补发**，不再 `cursor_gap` 掐流；
//! - 洪峰持续在产时 attach **必须成功**，业务帧必须送达；
//! - 慢消费者从环追帧，**不作废整条流**；
//! - 仅两类真缺口保留 `ringing.resync_required` 兜底：游标早于回放尾覆盖、epoch 失配。
//!
//! 全程真实链路：`build_router` + 真实 `handle_events_v2` + 真实 owner 泵，零 mock。

use super::test_state;
use crate::axum_server::axum_impl::AppState;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use http_body_util::BodyExt;
use qaqh_ringing::RingingV2OpenRequest;
use qaqh_ringing_contract::SessionEvent;
use std::time::Duration;
use tower::util::ServiceExt;

const ADMIN: &str = "test-token";
const EPOCH: &str = "test-epoch";
/// 订阅读取预算：live 流不会自行结束，到预算即返回已收内容（ended=false）。
const READ_BUDGET: Duration = Duration::from_millis(600);

fn request(
    method: &str,
    uri: &str,
    client_session: Option<&str>,
    body: Option<serde_json::Value>,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {ADMIN}"));
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

async fn json_body(response: Response) -> (serde_json::Value, usize) {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let len = bytes.len();
    (
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        len,
    )
}

/// 逐帧读 SSE 至流结束或预算耗尽。返回 (文本, 是否自然结束, 字节数)。
/// 预算耗尽 = live 存活（attach 成功）。
async fn read_sse(response: Response, budget: Duration) -> (String, bool, usize) {
    let mut body = response.into_body();
    let mut text = String::new();
    let mut total = 0usize;
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return (text, false, total);
        }
        match tokio::time::timeout(remaining, body.frame()).await {
            Err(_elapsed) => return (text, false, total),
            Ok(None) => return (text, true, total),
            Ok(Some(Err(_))) => return (text, true, total),
            Ok(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    total += data.len();
                    text.push_str(&String::from_utf8_lossy(&data));
                }
            }
        }
    }
}

/// 直接落业务行（与 runtime create_business_row 同构造），绕开默认 provider 校验：
/// 守卫对象是读面 attach 协议，不需要能跑的模型。
fn seed_business_row(state: &AppState, title: &str) -> String {
    let session_id = format!("ses_{}", qaqh_ringing_contract::ulid::generate_ulid());
    let row = qaqh_session::db::store::NewSession {
        session_id: session_id.clone(),
        parent_session_id: None,
        title: title.to_string(),
        title_source: Some("user".to_string()),
        cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
        model: Some("storm-mock".to_string()),
        tool_mode: None,
        model_selection: None,
        custom_tools: None,
        skills: None,
        frozen_annotation: None,
        ephemeral: false,
    };
    let sid = session_id.clone();
    state
        .service
        .owners()
        .db()
        .write_blocking(move |conn| qaqh_session::db::store::create_session(conn, &row))
        .unwrap_or_else(|error| panic!("seed business row {sid}: {error}"));
    session_id
}

async fn storm_fixture(title: &str) -> (AppState, Router, String, String) {
    let state = test_state();
    let app = crate::axum_server::build_router(state.clone());
    let session_id = seed_business_row(&state, title);
    let open = app
        .clone()
        .oneshot(request(
            "POST",
            "/ringing/v2/clients/open",
            None,
            Some(
                serde_json::to_value(RingingV2OpenRequest::new("storm-instance"))
                    .expect("open body"),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(open.status(), StatusCode::OK, "admin open should lease");
    let (json, _) = json_body(open).await;
    let cs = json["client_session_id"]
        .as_str()
        .expect("client_session_id")
        .to_string();
    // 热身：第一次 bootstrap 拉起 owner 泵，吸收启动噪声。
    let _ = bootstrap_watermark(&app, &session_id, &cs).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    (state, app, session_id, cs)
}

async fn bootstrap_watermark(app: &Router, seed: &str, cs: &str) -> (u64, usize) {
    let boot = app
        .clone()
        .oneshot(request(
            "GET",
            &format!("/ringing/v2/sessions/{seed}/bootstrap"),
            Some(cs),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(boot.status(), StatusCode::OK);
    let (json, len) = json_body(boot).await;
    (json["state"]["seq"].as_u64().expect("state.seq"), len)
}

async fn subscribe_raw(app: &Router, seed: &str, cs: &str, cursor: Option<&str>) -> Response {
    let uri = match cursor {
        Some(c) => format!("/ringing/v2/sessions/{seed}/events?since_cursor={c}"),
        None => format!("/ringing/v2/sessions/{seed}/events"),
    };
    let events = app
        .clone()
        .oneshot(request("GET", &uri, Some(cs), None))
        .await
        .unwrap();
    assert_eq!(events.status(), StatusCode::OK);
    events
}

/// 守卫①：水位与订阅之间恰好一个事件 → **必须补发该帧**，不得 resync。
/// （修复前实得 `ringing.resync_required(cursor_gap)` 掐流。）
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_in_attach_window_is_replayed_not_resynced() {
    let (state, app, seed, cs) = storm_fixture("storm-replay").await;
    let owner = state.service.owners().get_or_create(&seed);

    let (seq, _) = bootstrap_watermark(&app, &seed, &cs).await;
    owner.publish(SessionEvent::DriverChanged { driver: None });
    for _ in 0..400 {
        if owner.current_seq() > seq {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(owner.current_seq() > seq, "装置失效：泵未消费发布事件");

    let (stream, ended, _) = read_sse(
        subscribe_raw(&app, &seed, &cs, Some(&format!("{EPOCH}:{seq}"))).await,
        READ_BUDGET,
    )
    .await;
    assert!(
        !stream.contains("ringing.resync_required"),
        "尾覆盖内的事件不得触发 resync：{stream}"
    );
    assert!(!ended, "补发后流应保持 live");
    assert!(
        stream.contains("driver_changed"),
        "水位窗口内的事件必须经回放尾补发到帧流：{stream}"
    );
}

/// 守卫②：持续洪峰（每圈 20 帧 × 8 圈）下 attach **必须圈圈成功**，
/// 业务帧照单送达；活锁特征（0 送达 + 圈圈掐流）必须消失。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sustained_flood_attach_succeeds_every_cycle() {
    let (state, app, seed, cs) = storm_fixture("storm-livelock-guard").await;
    let owner = state.service.owners().get_or_create(&seed);

    let mut resync_cycles = 0;
    let mut delivered = 0;
    let mut window_frames_total = 0;
    for cycle in 0..8 {
        let (seq, _) = bootstrap_watermark(&app, &seed, &cs).await;
        for _ in 0..20 {
            owner.publish(SessionEvent::DriverChanged { driver: None });
            window_frames_total += 1;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
        let (stream, ended, _) = read_sse(
            subscribe_raw(&app, &seed, &cs, Some(&format!("{EPOCH}:{seq}"))).await,
            READ_BUDGET,
        )
        .await;
        if stream.contains("ringing.resync_required") {
            resync_cycles += 1;
        }
        assert!(!ended, "第 {cycle} 圈：洪峰中 attach 成功后流不应被掐断");
        delivered += stream.matches("event: ringing.event").count();
    }
    assert_eq!(resync_cycles, 0, "活锁回归：任何一圈 resync 都是 H1' 失效");
    assert!(
        delivered >= window_frames_total,
        "窗口洪峰 {window_frames_total} 帧，仅送达 {delivered}——回放尾丢帧"
    );
    eprintln!(
        "[storm-guard] 8 圈 attach 全成功：洪峰 {window_frames_total} 帧送达 {delivered}"
    );
}

/// 守卫③：慢消费者（前端卡死 300ms 不 poll）期间 1500 帧洪峰灌入 →
/// 从环追帧照单全收，**不作废整条流**。（修复前：≤64 帧即 resync。）
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_consumer_catches_up_from_ring_without_resync() {
    let (state, app, seed, cs) = storm_fixture("storm-lagged-guard").await;
    let owner = state.service.owners().get_or_create(&seed);

    let events = subscribe_raw(&app, &seed, &cs, None).await;
    for _ in 0..1_500 {
        owner.publish(SessionEvent::DriverChanged { driver: None });
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (stream, ended, _) = read_sse(events, Duration::from_millis(2500)).await;
    assert!(
        !stream.contains("ringing.resync_required"),
        "环覆盖内的滞后必须追帧，不得 resync（{} 字节）",
        stream.len()
    );
    assert!(!ended, "追帧后流应保持 live");
    let delivered = stream.matches("event: ringing.event").count();
    assert!(
        delivered >= 1_400,
        "1500 帧洪峰仅恢复 {delivered} 帧——环覆盖或去重水位有洞"
    );
}

/// 守卫④：兜底语义仍在——两类真缺口必须继续 resync（协议降级路径不许被
/// 顺手删掉）：游标早于回放尾覆盖范围；epoch 失配。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn genuine_gaps_still_fall_back_to_resync() {
    let (state, app, seed, cs) = storm_fixture("storm-gap-guard").await;
    let owner = state.service.owners().get_or_create(&seed);

    // (a) 远古游标：灌 2600 帧 > REPLAY_FRAME_CAP(2048)，最早帧已淘汰出环。
    let (watermark, _) = bootstrap_watermark(&app, &seed, &cs).await;
    for _ in 0..2_600 {
        owner.publish(SessionEvent::DriverChanged { driver: None });
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (stream, ended, _) = read_sse(
        subscribe_raw(&app, &seed, &cs, Some(&format!("{EPOCH}:{watermark}"))).await,
        READ_BUDGET,
    )
    .await;
    assert!(
        ended && stream.contains("ringing.resync_required") && stream.contains("cursor_gap"),
        "游标跌出回放尾必须 resync(cursor_gap) 掐流引导快照：{stream}"
    );

    // (b) epoch 失配：旧代客户端必须 resync。
    let (stream, ended, _) = read_sse(
        subscribe_raw(&app, &seed, &cs, Some(&format!("stale-epoch:1"))).await,
        READ_BUDGET,
    )
    .await;
    assert!(
        ended && stream.contains("ringing.resync_required") && stream.contains("epoch_mismatch"),
        "epoch 失配必须 resync(epoch_mismatch)：{stream}"
    );
}
