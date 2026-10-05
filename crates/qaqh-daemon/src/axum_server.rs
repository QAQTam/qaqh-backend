//! axum 迁移 P1.5：主干直切，已去 feature-gated。
//! P0 已完成 health；P1 补齐无状态 REST + 中间件/限流骨架；P1.5 起为默认 HTTP 栈。

mod axum_impl;
#[cfg(test)]
mod authz_matrix_tests;

pub(crate) use axum_impl::reclaim_dead_driver_seats;
pub(crate) use axum_impl::challenge::ChallengeStore;
pub(crate) use axum_impl::pairing::PairingTable;
pub(crate) use axum_impl::test_hooks::TestHooks;
pub use axum_impl::{AppState, build_router};

#[cfg(test)]
/// 进程级 SessionManager 初始化守卫。
///
/// `SessionManager::init` 内部是 `OnceLock::set().expect(...)`——同一测试
/// 二进制的多个用例共享进程，谁先谁后不确定，裸调 `init` 必然 `already
/// initialized` panic。所有测试模块统一走这里，只初始化一次。
///
/// 同时做**测试数据根隔离**：默认指向进程唯一的临时目录，避免测试在真实
/// `<USERPROFILE>\.qaqh` 里建会话（泄漏的会话会进 `session.list` 干扰前端）。
/// 调用方显式设了 `QAQH_DATA_DIR` 时不覆盖。
#[cfg(test)]
fn init_session_manager() {
    static INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INIT.get_or_init(|| {
        // SAFETY: 处于 `OnceLock` 初始化窗口，此后不再修改进程环境。
        unsafe {
            if std::env::var("QAQH_DATA_DIR")
                .map(|value| value.trim().is_empty())
                .unwrap_or(true)
            {
                let root = std::env::temp_dir()
                    .join(format!("qaqh-daemon-test-{}", std::process::id()));
                let _ = std::fs::create_dir_all(&root);
                std::env::set_var("QAQH_DATA_DIR", &root);
            }
            std::env::set_var("QAQH_ALLOW_TEST_DATA_ROOT", "1");
        }
        qaqh_session::SessionManager::init(qaqh_types::platform::data_dir())
    });
}

#[cfg(test)]
mod sse_tests {
    //! SSE 终止帧路径回归（BUG-2026-09-12-11 遗留 / issue #35）。
    //!
    //! 覆盖真实 handler 路径（`build_router` →
    //! `/ringing/v2/sessions/{seed}/timeline/events`）：
    //! 慢消费者 `Lagged` → `ringing.stream_terminated` 终止帧 → 关流，
    //! 且终止后新订阅仍能正常收流。
    //!
    //! **慢消费者装置（确定性，不靠"真的慢"）**
    //!
    //! tokio broadcast 的语义是：
    //!  - 在 Sender 已有积压历史之后才 `subscribe()` 的 receiver **看不到
    //!    历史**（首次 `recv()` 得到 `Empty`）——因此必须让 handler 的
    //!    receiver **先于**溢出存在；
    //!  - 一个已存在的 receiver 若一直不排空，其下一次 `recv()` 在环溢出后
    //!    必然返回 `Lagged(超出 capacity 的条数)`，且环内**零保留**
    //!    （本仓库 tokio 1.x 实测：`cap=4, sent=5 → Lagged(1)`）。
    //!
    //! 于是顺序固定为：**先起 SSE（handler 建立订阅）→ 再用
    //! `RingingHub::overflow_channel_live` / `publish_timeline` 灌
    //! `capacity + 1` 条 → handler 的 `recv()` 立即 `Lagged`**。
    //!
    //! 依赖测试窗口内 live 容量恒为 `LIVE_BROADCAST_CAPACITY`（1024），
    //! 由 `RingingHub::live_capacity` 取用，避免与实现漂移。

    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::time::Duration;
    use tokio_stream::StreamExt as _;
    use tower::util::ServiceExt;

    const TOKEN: &str = "test-token";
    const SESSION: &str = "cs-lag";
    const SEED: &str = "seed-live";

    fn test_state_with_hub(hub: std::sync::Arc<qaqh_runtime::RingingHub>) -> AppState {
        let leases = std::sync::Arc::new(std::sync::Mutex::new(
            qaqh_runtime::ringing::RingingLeaseStore::new(),
        ));
        {
            let mut g = leases.lock().unwrap();
            g.open(SESSION.into(), "ci-lag".into());
            g.attach_session(SESSION, SEED);
        }
        let pending = std::sync::Arc::new(std::sync::Mutex::new(
            qaqh_runtime::ringing::PendingCommandStore::new(),
        ));
        let driver_watch = std::sync::Arc::new(std::sync::Mutex::new(
            qaqh_runtime::ringing::RingingDriverWatch::new(),
        ));
        let (shutdown, _) = tokio::sync::watch::channel(false);
        // 与 `axum_tests::test_state` 同源：SessionManager 是进程级单例
        // （`init` 用 `OnceLock::set`，重复调用会 panic）——测试二进制的多个
        // 用例共享同一进程，这里用与 `axum_tests::test_state` 相同的
        // `OnceLock::get_or_init` 形态保证只初始化一次。
        super::init_session_manager();
        AppState {
            hub,
            v2_hub: std::sync::Arc::new(qaqh_runtime::ringing::V2ProjectionHub::new("lag-epoch")),
            leases,
            driver_watch,
            pending,
            service: qaqh_runtime::QaqhService::init(qaqh_session::SessionManager::global())
                .clone(),
            admin_token: TOKEN.into(),
            devices: std::sync::Arc::new(std::sync::Mutex::new(
                qaqh_runtime::ringing::DeviceRegistry::new(),
            )),
            pairings: std::sync::Arc::new(std::sync::Mutex::new(PairingTable::new())),
            tls_fingerprint: None,
            challenges: std::sync::Arc::new(ChallengeStore::default()),
            epoch: "lag-epoch".into(),
            shutdown,
            test_hooks: std::sync::Arc::new(TestHooks::disabled()),
        }
    }

    async fn next_sse_frame(
        stream: &mut std::pin::Pin<
            Box<dyn futures_util::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Send>,
        >,
        timeout: Duration,
    ) -> Option<(String, String)> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut buf = Vec::<u8>::new();
        loop {
            if let Some(frame) = split_frame(&buf) {
                return Some(frame);
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, stream.next()).await {
                Ok(Some(Ok(chunk))) => buf.extend_from_slice(&chunk),
                Ok(Some(Err(_))) | Ok(None) => return split_frame(&buf),
                Err(_) => return split_frame(&buf),
            }
        }
    }

    /// 打开 timeline SSE：返回 (状态, 事件流)。
    async fn open_timeline_sse(
        app: Router,
    ) -> (
        StatusCode,
        std::pin::Pin<
            Box<dyn futures_util::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Send>,
        >,
    ) {
        open_timeline_sse_with_cursor(app, None).await
    }

    /// 打开 timeline SSE，可带 `Last-Event-ID`（模拟客户端重连时的 cursor）。
    async fn open_timeline_sse_with_cursor(
        app: Router,
        last_event_id: Option<&str>,
    ) -> (
        StatusCode,
        std::pin::Pin<
            Box<dyn futures_util::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Send>,
        >,
    ) {
        let mut builder = Request::builder()
            .uri(format!("/ringing/v2/sessions/{SEED}/timeline/events"))
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("x-qaqh-client-session-id", SESSION);
        if let Some(cursor) = last_event_id {
            builder = builder.header("last-event-id", cursor);
        }
        let req = builder.body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let stream = axum::body::Body::into_data_stream(resp.into_body());
        (status, Box::pin(stream))
    }

    /// 取完整帧（以空行分隔），返回 (event, data)。
    fn split_frame(buf: &[u8]) -> Option<(String, String)> {
        let text = String::from_utf8_lossy(buf);
        let block = text.split("\n\n").next()?;
        if !text.contains("\n\n") && !text.ends_with('\n') {
            return None;
        }
        let mut event = String::new();
        let mut data = String::new();
        for line in block.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                event = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(v.trim());
            }
        }
        if event.is_empty() && data.is_empty() {
            None
        } else {
            Some((event, data))
        }
    }

    /// ③ timeline 流：慢消费者 → `ringing.stream_terminated`(seed) → 关流。
    ///
    /// 与 ① 同一装置，只是把订阅点换成 `subscribe_timeline`、溢出源换成
    /// 生产入口 `hub.publish_timeline`（每次发布都会 `timeline_live.send`）。
    #[tokio::test]
    async fn timeline_stream_lagged_sends_termination_frame_then_closes() {
        let hub = std::sync::Arc::new(qaqh_runtime::RingingHub::new("lag-epoch"));
        // 先立一个"记数"订阅，确保 timeline_live 广播在 handler 订阅前已存在
        // 且已有消费者（tokio：无消费者的 Sender 会丢弃历史）。
        let _recorder = hub.subscribe_timeline();

        let state = test_state_with_hub(hub.clone());
        let (status, mut stream) = open_timeline_sse(build_router(state)).await;
        assert_eq!(status, StatusCode::OK);

        // handler 的 timeline receiver 已存在 → 灌满环并把 handler 挤出。
        let capacity =
            qaqh_runtime::RingingHub::live_capacity(qaqh_domain::RingingChannel::Conversation);
        for i in 0..capacity + 1 {
            hub.publish_timeline(
                SEED,
                qaqh_domain::TimelineIntent::TurnOpened {
                    turn_id: format!("t{i}"),
                    user_text: format!("q{i}"),
                },
            )
            .expect("publish timeline intent");
        }

        let mut terminated = None;
        // 满环下 handler 的首个 `recv()` 即 Lagged（实测 0 帧先行），但按契约
        // 扫描到终止帧即可，不依赖"零帧"这一实现细节。
        for _ in 0..capacity + 8 {
            match next_sse_frame(&mut stream, Duration::from_secs(5)).await {
                Some((event, data)) if event == "ringing.stream_terminated" => {
                    terminated = Some(data);
                    break;
                }
                Some(_) => continue,
                None => break,
            }
        }
        let data = terminated.expect("timeline termination frame must arrive");
        let v: serde_json::Value = serde_json::from_str(&data).expect("valid json payload");
        assert_eq!(v["code"], "lagged");
        assert_eq!(v["session_id"], SEED);
        assert!(
            v["skipped"].as_u64().unwrap_or(0) > 0,
            "payload must carry the skipped count: {data}"
        );

        // 终止帧之后不再有数据帧。
        let tail = next_sse_frame(&mut stream, Duration::from_millis(300)).await;
        assert!(
            tail.is_none(),
            "no frame may follow the timeline termination frame: {tail:?}"
        );
    }

    #[tokio::test]
    async fn injected_timeline_gap_drops_first_entry_and_keeps_streaming() {
        let hub = std::sync::Arc::new(qaqh_runtime::RingingHub::new("lag-epoch"));
        let mut state = test_state_with_hub(hub.clone());
        state.test_hooks = std::sync::Arc::new(TestHooks::for_test_timeline_gap());
        let (status, mut stream) = open_timeline_sse(build_router(state)).await;
        assert_eq!(status, StatusCode::OK);

        for turn in ["t-gap-1", "t-gap-2", "t-gap-3"] {
            hub.publish_timeline(
                SEED,
                qaqh_domain::TimelineIntent::TurnOpened {
                    turn_id: turn.into(),
                    user_text: "force a timeline gap".into(),
                },
            )
            .expect("publish timeline intent");
        }

        // 第一条真实 entry 被丢弃，客户端直接看到第二条 —— 这就是 gap。
        let (event, data) = next_sse_frame(&mut stream, Duration::from_secs(5))
            .await
            .expect("frame after the dropped entry must arrive");
        assert_eq!(event, "timeline.entry");
        let value: serde_json::Value = serde_json::from_str(&data).expect("valid timeline frame");
        assert_eq!(value["entry"]["timeline_seq"], 2);

        // 钩子只丢一帧，不关流：后续 entry 必须继续正常下发。
        let (event, data) = next_sse_frame(&mut stream, Duration::from_secs(5))
            .await
            .expect("the stream must stay open after gap injection");
        assert_eq!(event, "timeline.entry");
        let value: serde_json::Value = serde_json::from_str(&data).expect("valid timeline frame");
        assert_eq!(value["entry"]["timeline_seq"], 3);
    }

    /// 客户端模型级回归：cosplay TUI 的 `expected == cursor + 1` 判定，断言
    /// gap 钩子制造的是「跳号」而不是「重复下发已送达 entry」。
    #[tokio::test]
    async fn injected_timeline_gap_triggers_client_cursor_check_without_duplicates() {
        let hub = std::sync::Arc::new(qaqh_runtime::RingingHub::new("lag-epoch"));
        let mut state = test_state_with_hub(hub.clone());
        state.test_hooks = std::sync::Arc::new(TestHooks::for_test_timeline_gap());

        // 客户端已经收到 seq1，带 cursor=1 重连（这是 TUI 最常见的重连态）。
        hub.publish_timeline(
            SEED,
            qaqh_domain::TimelineIntent::TurnOpened {
                turn_id: "t-prior".into(),
                user_text: "already delivered".into(),
            },
        )
        .expect("publish prior entry");

        let (status, mut stream) =
            open_timeline_sse_with_cursor(build_router(state), Some("lag-epoch:timeline:1")).await;
        assert_eq!(status, StatusCode::OK);

        // 重连后 journal 里又出现三条：seq2 被钩子丢弃，seq3/seq4 才是客户端看到的。
        for turn in ["t-new-a", "t-new-b", "t-new-c"] {
            hub.publish_timeline(
                SEED,
                qaqh_domain::TimelineIntent::TurnOpened {
                    turn_id: turn.into(),
                    user_text: "after reconnect".into(),
                },
            )
            .expect("publish timeline intent");
        }

        let mut cursor = 1_u64;
        let mut gap_triggered = false;
        let mut delivered: Vec<u64> = Vec::new();
        while let Some((event, data)) = next_sse_frame(&mut stream, Duration::from_secs(5)).await {
            assert_eq!(event, "timeline.entry");
            let value: serde_json::Value =
                serde_json::from_str(&data).expect("valid timeline frame");
            let seq = value["entry"]["timeline_seq"]
                .as_u64()
                .expect("timeline_seq");
            assert!(
                seq > cursor,
                "gap injection must never re-send an already delivered entry: cursor={cursor} seq={seq}"
            );
            if seq != cursor + 1 {
                gap_triggered = true;
            }
            cursor = seq;
            delivered.push(seq);
            if delivered.len() == 2 {
                break;
            }
        }

        assert_eq!(
            delivered,
            vec![3, 4],
            "client must see seq3 (gap) and then the stream must keep flowing with seq4"
        );
        assert!(
            gap_triggered,
            "client-side `expected == cursor + 1` check must fire on the injected gap"
        );
    }

    #[tokio::test]
    async fn injected_timeline_gap_does_not_leak_foreign_session_entries() {
        let hub = std::sync::Arc::new(qaqh_runtime::RingingHub::new("lag-epoch"));
        let mut state = test_state_with_hub(hub.clone());
        state.test_hooks = std::sync::Arc::new(TestHooks::for_test_timeline_gap());
        let (status, mut stream) = open_timeline_sse(build_router(state)).await;
        assert_eq!(status, StatusCode::OK);

        hub.publish_timeline(
            "seed-foreign",
            qaqh_domain::TimelineIntent::TurnOpened {
                turn_id: "t-foreign".into(),
                user_text: "foreign timeline data".into(),
            },
        )
        .expect("publish foreign timeline intent");

        assert!(
            next_sse_frame(&mut stream, Duration::from_millis(300))
                .await
                .is_none(),
            "foreign seed must be filtered before gap injection"
        );
    }
}

#[cfg(test)]
mod axum_tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::util::ServiceExt; // for oneshot

    static TEST_SERVICE: std::sync::OnceLock<qaqh_runtime::QaqhService> =
        std::sync::OnceLock::new();
    pub(super) fn test_state() -> AppState {
        let hub = std::sync::Arc::new(qaqh_runtime::RingingHub::with_persistence(
            String::from("test-epoch"),
            std::env::temp_dir().join("qaqh-axum-test"),
        ));
        let leases = std::sync::Arc::new(std::sync::Mutex::new(
            qaqh_runtime::ringing::RingingLeaseStore::new(),
        ));
        let pending = std::sync::Arc::new(std::sync::Mutex::new(
            qaqh_runtime::ringing::PendingCommandStore::new(),
        ));
        let driver_watch = std::sync::Arc::new(std::sync::Mutex::new(
            qaqh_runtime::ringing::RingingDriverWatch::new(),
        ));
        let service = TEST_SERVICE
            .get_or_init(|| {
                super::init_session_manager();
                qaqh_runtime::QaqhService::init(qaqh_session::SessionManager::global())
            })
            .clone();
        let (shutdown, _) = tokio::sync::watch::channel(false);
        // 与 `axum_tests::test_state` 同源：SessionManager 是进程级单例
        // （`init` 用 `OnceLock::set`，重复调用会 panic）——测试二进制的多个
        // 用例共享同一进程，这里用与 `axum_tests::test_state` 相同的
        // `OnceLock::get_or_init` 形态保证只初始化一次。
        super::init_session_manager();
        AppState {
            hub,
            v2_hub: std::sync::Arc::new(qaqh_runtime::ringing::V2ProjectionHub::new("test-epoch")),
            leases,
            driver_watch,
            pending,
            service,
            admin_token: String::from("test-token"),
            devices: std::sync::Arc::new(std::sync::Mutex::new(
                qaqh_runtime::ringing::DeviceRegistry::new(),
            )),
            pairings: std::sync::Arc::new(std::sync::Mutex::new(PairingTable::new())),
            tls_fingerprint: None,
            challenges: std::sync::Arc::new(ChallengeStore::default()),
            epoch: String::from("test-epoch"),
            shutdown,
            test_hooks: std::sync::Arc::new(TestHooks::disabled()),
        }
    }

    #[tokio::test]
    async fn health_ok() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn activity_requires_auth() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/activity")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn activity_exposes_has_active_work() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/activity")
            .header("authorization", "Bearer test-token")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // test_state() 无 agent：has_active_work 必须为 false，activities 为空表。
        assert_eq!(value["has_active_work"], serde_json::json!(false));
        assert_eq!(value["activities"], serde_json::json!([]));
    }

    /// #345：v2 content 端点按 id 取（不带 seed），归属校验用条目自己的 seed；
    /// v1 content 路由已硬切。
    #[tokio::test]
    async fn v2_content_route_is_session_free_and_v1_is_hard_cut() {
        let state = test_state();
        let app = build_router(state.clone());
        let content_id = state.hub.put_content(
            "seed-content",
            "application/json",
            br#"{"kind":"ask","questions":[]}"#.to_vec(),
            false,
        );
        {
            let mut leases = state.leases.lock().unwrap();
            leases.open("cs-owner".into(), "ci-owner".into());
            leases.attach_session("cs-owner", "seed-content");
            leases.open("cs-other".into(), "ci-other".into());
            leases.attach_session("cs-other", "seed-other");
        }

        let get = |session: &str, path: String| {
            Request::builder()
                .uri(path)
                .header("authorization", "Bearer test-token")
                .header("x-qaqh-client-session-id", session)
                .body(Body::empty())
                .unwrap()
        };

        // owner 读：200 + 原文
        let resp = app
            .clone()
            .oneshot(get("cs-owner", format!("/ringing/v2/content/{content_id}")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), br#"{"kind":"ask","questions":[]}"#);

        // Range：单区间返回 206 + Content-Range，不改变对象内容。
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/ringing/v2/content/{content_id}"))
                    .header("authorization", "Bearer test-token")
                    .header("x-qaqh-client-session-id", "cs-owner")
                    .header("range", "bytes=2-5")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        let expected_content_range =
            format!("bytes 2-5/{}", br#"{"kind":"ask","questions":[]}"#.len());
        assert_eq!(
            resp.headers()
                .get("content-range")
                .and_then(|value| value.to_str().ok()),
            Some(expected_content_range.as_str())
        );
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), &br#"{"kind":"ask","questions":[]}"#[2..=5]);

        // 越界 Range 是 416，而不是静默回全量。
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/ringing/v2/content/{content_id}"))
                    .header("authorization", "Bearer test-token")
                    .header("x-qaqh-client-session-id", "cs-owner")
                    .header("range", "bytes=999-1000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        let expected_content_range =
            format!("bytes */{}", br#"{"kind":"ask","questions":[]}"#.len());
        assert_eq!(
            resp.headers()
                .get("content-range")
                .and_then(|value| value.to_str().ok()),
            Some(expected_content_range.as_str())
        );

        // canonical ref 形态（`sha256:<hex>`）也能取到同一条目
        let resp = app
            .clone()
            .oneshot(get(
                "cs-owner",
                format!("/ringing/v2/content/sha256:{content_id}"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // 不拥有该 seed 的会话：403（不泄漏正文）
        let resp = app
            .clone()
            .oneshot(get("cs-other", format!("/ringing/v2/content/{content_id}")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // 未知 id：404
        let resp = app
            .clone()
            .oneshot(get("cs-owner", "/ringing/v2/content/nope".to_string()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // v1 硬切：同一条目在旧路径上不可达
        let resp = app
            .oneshot(get("cs-owner", format!("/ringing/v1/content/{content_id}")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// 纯 v2：timeline 的 v1 路径已硬切（v2 路径见其它 timeline 用例）。
    #[tokio::test]
    async fn timeline_v1_routes_are_hard_cut() {
        let state = test_state();
        let app = build_router(state);
        for uri in [
            "/ringing/v1/sessions/seed-1/timeline",
            "/ringing/v1/sessions/seed-1/timeline/events",
        ] {
            let req = Request::builder()
                .uri(uri)
                .header("authorization", "Bearer test-token")
                .header("x-qaqh-client-session-id", "cs-1")
                .body(Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "uri={uri}");
        }
    }

    /// 纯 v2：bootstrap 的 v1 路径已硬切，权威快照只剩 v2 路径。
    #[tokio::test]
    async fn bootstrap_v1_route_is_hard_cut() {
        let state = test_state();
        let app = build_router(state);
        let req = Request::builder()
            .uri("/ringing/v1/sessions/seed-1/bootstrap")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", "cs-1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// 纯 v2：v1 三频道 SSE 已硬切，投影事件只剩每 seed 一条的 v2 单流。
    #[tokio::test]
    async fn events_v1_route_is_hard_cut() {
        let state = test_state();
        let app = build_router(state);
        for uri in [
            "/ringing/v1/events/tool",
            "/ringing/v1/events/conversation",
            "/ringing/v1/events/control",
        ] {
            let req = Request::builder()
                .uri(uri)
                .header("authorization", "Bearer test-token")
                .header("x-qaqh-client-session-id", "cs-1")
                .body(Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "uri={uri}");
        }
    }

    #[tokio::test]
    async fn injected_session_404_short_circuits_timeline_snapshot() {
        let mut state = test_state();
        state.test_hooks = std::sync::Arc::new(TestHooks::for_test_session_404("missing"));
        let app = build_router(state);
        let req = Request::builder()
            .uri("/ringing/v2/sessions/missing/timeline")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", "cs-test")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["code"], "session_not_found");
    }

    #[tokio::test]
    async fn open_requires_auth() {
        let app = build_router(test_state());
        let req = Request::builder()
            .method("POST")
            .uri("/ringing/v2/clients/open")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"schema":"qaqh.Ringing","version":2,"client_instance_id":"ci"}"#,
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn open_success() {
        let app = build_router(test_state());
        let body = serde_json::json!({
            "schema":"qaqh.Ringing","version":2,"client_instance_id":"ci-1"
        });
        let req = Request::builder()
            .method("POST")
            .uri("/ringing/v2/clients/open")
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-token")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// 纯 v2：v1 的 open / renew / service 三条路径已硬切。
    #[tokio::test]
    async fn v1_open_renew_service_are_hard_cut() {
        let app = build_router(test_state());
        for (method, uri) in [
            ("POST", "/ringing/v1/clients/open"),
            ("POST", "/ringing/v1/leases/renew"),
            ("POST", "/ringing/v1/service/session.list"),
            ("POST", "/ringing/v1/commands/control"),
            ("GET", "/ringing/v1/commands/cmd-1"),
        ] {
            let req = Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", "Bearer test-token")
                .header("x-qaqh-client-session-id", "cs-1")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "uri={uri}");
        }
    }

    #[tokio::test]
    async fn v2_open_bootstrap_uses_canonical_projection() {
        use qaqh_session::canonical::{
            CanonicalSessionIdentity, CanonicalSessionStore, WriterId, generate_ulid,
        };
        use qaqh_session::session_fact_v2::{
            EventId, FactPayload, FactSchema, MetadataSource, SessionCreated, SessionFact,
            SessionMetadataChanged, SessionMetadataPatch,
        };

        let sessions_dir = qaqh_types::platform::sessions_dir();
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let dir = tempfile::tempdir_in(&sessions_dir).unwrap();
        let session_id = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let identity = CanonicalSessionIdentity::open_or_create(dir.path()).unwrap();
        let now = 1_789_830_000_000;
        let mut store = CanonicalSessionStore::open(
            dir.path(),
            identity.session_id.clone(),
            identity.log_id.clone(),
        )
        .unwrap();
        let lease = store
            .acquire_writer(WriterId::new("v2-route-test"), now, 60_000)
            .unwrap();
        let fact = SessionFact {
            schema: FactSchema::v2(),
            session_id: identity.session_id.clone(),
            log_id: identity.log_id.clone(),
            fact_seq: 0,
            event_id: EventId::new(generate_ulid()),
            ts_ms: now,
            causation_id: None,
            turn_id: None,
            call_id: None,
            interaction_id: None,
            payload: FactPayload::SessionCreated(SessionCreated {
                created_at_ms: now,
                cwd: "/tmp".into(),
                model: "test".into(),
                parent_session_id: None,
                schema_caps: Vec::new(),
            }),
        };
        store.append(&lease, fact, now).unwrap();

        let state = test_state();
        let v2_hub = state.v2_hub.clone();
        let app = build_router(state);
        let open = Request::builder()
            .method("POST")
            .uri("/ringing/v2/clients/open")
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-token")
            .body(Body::from(
                serde_json::to_vec(&qaqh_ringing::RingingV2OpenRequest::new("ci-v2")).unwrap(),
            ))
            .unwrap();
        let response = app.clone().oneshot(open).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let open: qaqh_ringing::RingingV2OpenResponse = serde_json::from_slice(&body).unwrap();

        let bootstrap = Request::builder()
            .uri(format!("/ringing/v2/sessions/{session_id}/bootstrap"))
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", open.client_session_id.clone())
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(bootstrap).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["version"], 2);
        assert_eq!(value["session_id"], session_id);
        assert_eq!(value["control"]["state"]["revision"], 1);
        let snapshot_cursor = value["snapshot_cursor"].as_str().unwrap().to_string();
        assert!(snapshot_cursor.starts_with("v2."));

        let changed = SessionFact {
            schema: FactSchema::v2(),
            session_id: identity.session_id.clone(),
            log_id: identity.log_id.clone(),
            fact_seq: 0,
            event_id: EventId::new(generate_ulid()),
            ts_ms: now + 1,
            causation_id: None,
            turn_id: None,
            call_id: None,
            interaction_id: None,
            payload: FactPayload::SessionMetadataChanged(SessionMetadataChanged {
                patch: SessionMetadataPatch {
                    cwd: None,
                    model: Some("test-2".into()),
                    archived: None,
                    search_visibility: None,
                    parent_session_id: None,
                    schema_caps: None,
                },
                source: MetadataSource::Api,
                changed_at_ms: now + 1,
            }),
        };
        let changed = store.append(&lease, changed, now + 1).unwrap();
        qaqh_session::projection::ProjectionSink::publish(
            v2_hub.as_ref(),
            dir.path(),
            &changed.fact,
            &changed.events,
        );

        let events = Request::builder()
            .uri(format!(
                "/ringing/v2/sessions/{session_id}/events?since_cursor={snapshot_cursor}"
            ))
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", open.client_session_id)
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(events).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        use futures_util::StreamExt as _;
        let mut stream = response.into_body().into_data_stream();
        let chunk = stream.next().await.unwrap().unwrap();
        let text = String::from_utf8_lossy(&chunk);
        assert!(text.contains("event: ringing.event"), "{text}");
        assert!(text.contains("\"fact_seq\":2"), "{text}");
    }

    #[tokio::test]
    async fn v2_command_replay_returns_typed_existing_result() {
        use qaqh_domain::ControlCommand;
        use qaqh_ringing::{
            RingingCommand, RingingCommandAckStatus, RingingCommandState, RingingV2AskOutcome,
            RingingV2CommandAck, RingingV2CommandEnvelope, RingingV2CommandResult,
        };

        let state = test_state();
        let pending = state.pending.clone();
        let app = build_router(state);

        let open = Request::builder()
            .method("POST")
            .uri("/ringing/v2/clients/open")
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-token")
            .body(Body::from(
                serde_json::to_vec(&qaqh_ringing::RingingV2OpenRequest::new("ci-v2")).unwrap(),
            ))
            .unwrap();
        let response = app.clone().oneshot(open).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let open: qaqh_ringing::RingingV2OpenResponse = serde_json::from_slice(&body).unwrap();

        let envelope = RingingV2CommandEnvelope::new(
            "cmd-replay",
            "ci-v2",
            RingingCommand::Control(ControlCommand::SessionResume {
                session_id: "seed-1".into(),
            }),
        )
        .with_client_session_id(open.client_session_id.clone())
        .with_session_id("seed-1");
        let fingerprint = crate::axum_server::axum_impl::command_fingerprint(
            envelope.channel,
            envelope.session_id.as_deref(),
            envelope.expected_revision,
            envelope.driver_epoch,
            &envelope.command,
        );
        let expected = RingingV2CommandResult::AskResolved {
            interaction_id: "ask-1".into(),
            outcome: RingingV2AskOutcome::Answered,
        };
        {
            let mut pending = pending.lock().unwrap();
            assert!(
                pending
                    .record_fingerprint_for_session(
                        "cmd-replay",
                        &fingerprint,
                        &open.client_session_id
                    )
                    .expect("record")
            );
            pending.mark_terminal_with_result(
                "cmd-replay",
                RingingCommandState::Succeeded,
                Some("evt-replay".into()),
                None,
                Some(expected.clone()),
            );
        }

        let replay = Request::builder()
            .method("POST")
            .uri("/ringing/v2/commands/control")
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", open.client_session_id.clone())
            .body(Body::from(serde_json::to_vec(&envelope).unwrap()))
            .unwrap();
        let response = app.clone().oneshot(replay).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let ack: RingingV2CommandAck = serde_json::from_slice(&body).unwrap();
        assert_eq!(ack.status, RingingCommandAckStatus::Accepted);
        let existing = ack.existing.expect("replayed ack carries existing receipt");
        match existing {
            qaqh_ringing::RingingV2ExistingResult::CommandReceipt {
                state,
                terminal_event_id,
                result,
                ..
            } => {
                assert_eq!(state, RingingCommandState::Succeeded);
                assert_eq!(terminal_event_id.as_deref(), Some("evt-replay"));
                assert_eq!(result, Some(expected.clone()));
            }
            other => panic!("expected command_receipt, got {other:?}"),
        }

        let status = Request::builder()
            .uri("/ringing/v2/commands/cmd-replay")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", open.client_session_id)
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(status).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let status: qaqh_ringing::RingingV2CommandStatus = serde_json::from_slice(&body).unwrap();
        assert_eq!(status.state, RingingCommandState::Succeeded);
        assert_eq!(status.result, Some(expected));
    }

    #[tokio::test]
    async fn v2_command_replay_with_other_payload_is_conflict() {
        use qaqh_domain::ControlCommand;
        use qaqh_ringing::{RingingCommand, RingingV2CommandAck, RingingV2CommandEnvelope};

        let state = test_state();
        let pending = state.pending.clone();
        let app = build_router(state);

        let open = Request::builder()
            .method("POST")
            .uri("/ringing/v2/clients/open")
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-token")
            .body(Body::from(
                serde_json::to_vec(&qaqh_ringing::RingingV2OpenRequest::new("ci-v2")).unwrap(),
            ))
            .unwrap();
        let response = app.clone().oneshot(open).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let open: qaqh_ringing::RingingV2OpenResponse = serde_json::from_slice(&body).unwrap();

        let envelope = RingingV2CommandEnvelope::new(
            "cmd-mismatch",
            "ci-v2",
            RingingCommand::Control(ControlCommand::SessionResume {
                session_id: "seed-1".into(),
            }),
        )
        .with_client_session_id(open.client_session_id.clone())
        .with_session_id("seed-1");
        {
            let mut pending = pending.lock().unwrap();
            assert!(
                pending
                    .record_fingerprint_for_session(
                        "cmd-mismatch",
                        "different-fingerprint",
                        &open.client_session_id
                    )
                    .expect("record")
            );
        }

        let replay = Request::builder()
            .method("POST")
            .uri("/ringing/v2/commands/control")
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", open.client_session_id)
            .body(Body::from(serde_json::to_vec(&envelope).unwrap()))
            .unwrap();
        let response = app.oneshot(replay).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let ack: RingingV2CommandAck = serde_json::from_slice(&body).unwrap();
        assert_eq!(ack.code.as_deref(), Some("duplicate_command_mismatch"));
        assert!(ack.existing.is_none());
    }

    #[tokio::test]
    async fn v2_second_answer_returns_winning_verdict() {
        use qaqh_domain::ControlCommand;
        use qaqh_ringing::{
            RingingCommand, RingingV2AskOutcome, RingingV2CommandAck, RingingV2CommandEnvelope,
            RingingV2CommandResult, RingingV2ExistingResult,
        };
        use qaqh_session::canonical::{
            CanonicalSessionIdentity, CanonicalSessionStore, WriterId, generate_ulid,
            sha256_content_hash,
        };
        use qaqh_session::session_fact_v2::{
            ActorKind, ActorRef, ContentRef, EventId, FactPayload, FactSchema, InteractionDecision,
            InteractionId, InteractionKind, InteractionRequested, InteractionResolved,
            SessionCreated, SessionFact, ToolCallId, TurnId,
        };

        let sessions_dir = qaqh_types::platform::sessions_dir();
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let dir = tempfile::tempdir_in(&sessions_dir).unwrap();
        let session_id = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let identity = CanonicalSessionIdentity::open_or_create(dir.path()).unwrap();
        let now = 1_789_830_000_000;
        let mut store = CanonicalSessionStore::open(
            dir.path(),
            identity.session_id.clone(),
            identity.log_id.clone(),
        )
        .unwrap();
        let lease = store
            .acquire_writer(WriterId::new("v2-second-answer-test"), now, 60_000)
            .unwrap();

        let turn_id = TurnId::new(format!("turn_{}", generate_ulid()));
        let call_id = ToolCallId::new(format!("call_{}", generate_ulid()));
        let interaction_id = InteractionId::new(format!("int_{}", generate_ulid()));
        let envelope = |ts_ms: i64, payload: FactPayload| SessionFact {
            schema: FactSchema::v2(),
            session_id: identity.session_id.clone(),
            log_id: identity.log_id.clone(),
            fact_seq: 0,
            event_id: EventId::new(generate_ulid()),
            ts_ms,
            causation_id: None,
            turn_id: Some(turn_id.clone()),
            call_id: Some(call_id.clone()),
            interaction_id: Some(interaction_id.clone()),
            payload,
        };

        store
            .append(
                &lease,
                envelope(
                    now,
                    FactPayload::SessionCreated(SessionCreated {
                        created_at_ms: now,
                        cwd: "/tmp".into(),
                        model: "test".into(),
                        parent_session_id: None,
                        schema_caps: Vec::new(),
                    }),
                ),
                now,
            )
            .unwrap();
        store
            .append(
                &lease,
                envelope(
                    now + 1,
                    FactPayload::InteractionRequested(InteractionRequested {
                        interaction_id: interaction_id.clone(),
                        call_id: Some(call_id.clone()),
                        turn_id: turn_id.clone(),
                        kind: InteractionKind::Ask,
                        request_ref: ContentRef::new(sha256_content_hash(b"ask-request")),
                        expires_at_ms: None,
                        requested_at_ms: now + 1,
                    }),
                ),
                now + 1,
            )
            .unwrap();
        store
            .append(
                &lease,
                envelope(
                    now + 2,
                    FactPayload::InteractionResolved(InteractionResolved {
                        interaction_id: interaction_id.clone(),
                        decision_ref: ContentRef::new(sha256_content_hash(b"answered")),
                        decision: Some(InteractionDecision::Answered),
                        resolved_by: ActorRef {
                            kind: ActorKind::User,
                            id: "user".into(),
                            display_name: None,
                        },
                        resolution_seq: 1,
                        resolved_at_ms: now + 2,
                    }),
                ),
                now + 2,
            )
            .unwrap();

        let app = build_router(test_state());
        let open = Request::builder()
            .method("POST")
            .uri("/ringing/v2/clients/open")
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-token")
            .body(Body::from(
                serde_json::to_vec(&qaqh_ringing::RingingV2OpenRequest::new("ci-v2")).unwrap(),
            ))
            .unwrap();
        let response = app.clone().oneshot(open).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let open: qaqh_ringing::RingingV2OpenResponse = serde_json::from_slice(&body).unwrap();

        let command = RingingV2CommandEnvelope::new(
            "cmd-second-answer",
            "ci-v2",
            RingingCommand::Control(ControlCommand::InteractionAskRespond {
                interaction_id: interaction_id.as_str().to_string(),
                answers: Vec::new(),
            }),
        )
        .with_client_session_id(open.client_session_id.clone())
        .with_session_id(session_id);
        let request = Request::builder()
            .method("POST")
            .uri("/ringing/v2/commands/control")
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", open.client_session_id)
            .body(Body::from(serde_json::to_vec(&command).unwrap()))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let ack: RingingV2CommandAck = serde_json::from_slice(&body).unwrap();
        assert_eq!(ack.code.as_deref(), Some("interaction_already_resolved"));
        match ack.existing.expect("winning verdict") {
            RingingV2ExistingResult::InteractionResolved { result } => assert_eq!(
                result,
                RingingV2CommandResult::AskResolved {
                    interaction_id: interaction_id.as_str().to_string(),
                    outcome: RingingV2AskOutcome::Answered,
                }
            ),
            other => panic!("expected interaction_resolved, got {other:?}"),
        }
    }

    async fn open_v2_session(
        app: axum::Router,
        client_instance_id: &str,
    ) -> qaqh_ringing::RingingV2OpenResponse {
        let open = Request::builder()
            .method("POST")
            .uri("/ringing/v2/clients/open")
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-token")
            .body(Body::from(
                serde_json::to_vec(&qaqh_ringing::RingingV2OpenRequest::new(client_instance_id))
                    .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(open).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    async fn post_driver(
        app: axum::Router,
        session_id: &str,
        action: &str,
        client_session_id: &str,
    ) -> serde_json::Value {
        let request = Request::builder()
            .method("POST")
            .uri(format!("/ringing/v2/sessions/{session_id}/driver/{action}"))
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", client_session_id)
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    async fn post_v2_service(
        app: axum::Router,
        session_id: &str,
        method: &str,
        params: &serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let request = Request::builder()
            .method("POST")
            .uri(format!("/ringing/v2/service/{method}"))
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", session_id)
            .body(Body::from(serde_json::to_vec(params).unwrap()))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let value = serde_json::from_slice(&body).unwrap();
        (status, value)
    }

    async fn post_v2_command(
        app: axum::Router,
        session_id: &str,
        envelope: &qaqh_ringing::RingingV2CommandEnvelope,
    ) -> (StatusCode, qaqh_ringing::RingingV2CommandAck) {
        let request = Request::builder()
            .method("POST")
            .uri(format!(
                "/ringing/v2/commands/{}",
                envelope.channel.as_str()
            ))
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", session_id)
            .body(Body::from(serde_json::to_vec(envelope).unwrap()))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let ack = serde_json::from_slice(&body).unwrap();
        (status, ack)
    }

    /// Seed a canonical session carrying a `DriverChanged` fact.
    fn session_canonical_driver_session(
        holder: Option<&str>,
        driver_epoch: u64,
    ) -> (tempfile::TempDir, String) {
        use qaqh_session::canonical::{
            CanonicalSessionIdentity, CanonicalSessionStore, WriterId, generate_ulid,
        };
        use qaqh_session::session_fact_v2::{
            DriverChanged, EventId, FactPayload, FactSchema, SessionCreated, SessionFact,
        };

        let sessions_dir = qaqh_types::platform::sessions_dir();
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let dir = tempfile::tempdir_in(&sessions_dir).unwrap();
        let session_id = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let identity = CanonicalSessionIdentity::open_or_create(dir.path()).unwrap();
        let now = 1_789_830_000_000;
        let mut store = CanonicalSessionStore::open(
            dir.path(),
            identity.session_id.clone(),
            identity.log_id.clone(),
        )
        .unwrap();
        let lease = store
            .acquire_writer(WriterId::new("driver-seed"), now, 600_000)
            .unwrap();
        let fact = |ts_ms: i64, payload: FactPayload| SessionFact {
            schema: FactSchema::v2(),
            session_id: identity.session_id.clone(),
            log_id: identity.log_id.clone(),
            fact_seq: 0,
            event_id: EventId::new(generate_ulid()),
            ts_ms,
            causation_id: None,
            turn_id: None,
            call_id: None,
            interaction_id: None,
            payload,
        };
        store
            .append(
                &lease,
                fact(
                    now,
                    FactPayload::SessionCreated(SessionCreated {
                        created_at_ms: now,
                        cwd: "/tmp".into(),
                        model: "test".into(),
                        parent_session_id: None,
                        schema_caps: Vec::new(),
                    }),
                ),
                now,
            )
            .unwrap();
        if driver_epoch > 0 {
            store
                .append(
                    &lease,
                    fact(
                        now + 1,
                        FactPayload::DriverChanged(DriverChanged {
                            holder: holder.map(str::to_string),
                            driver_epoch,
                            changed_at_ms: now + 1,
                        }),
                    ),
                    now + 1,
                )
                .unwrap();
        }
        (dir, session_id)
    }

    /// Seed a canonical session carrying a *resolved* interaction of `kind`
    /// with a typed `decision`, for the concurrent-answer (V2-R4) fixtures.
    fn session_resolved_interaction_session(
        kind: qaqh_session::session_fact_v2::InteractionKind,
        decision: qaqh_session::session_fact_v2::InteractionDecision,
        tag: &str,
    ) -> (
        tempfile::TempDir,
        String,
        qaqh_session::session_fact_v2::InteractionId,
        qaqh_session::session_fact_v2::ToolCallId,
    ) {
        use qaqh_session::canonical::{
            CanonicalSessionIdentity, CanonicalSessionStore, WriterId, generate_ulid,
            sha256_content_hash,
        };
        use qaqh_session::session_fact_v2::{
            ActorKind, ActorRef, ContentRef, EventId, FactPayload, FactSchema, InteractionId,
            InteractionRequested, InteractionResolved, SessionCreated, SessionFact, ToolCallId,
            TurnId,
        };

        let sessions_dir = qaqh_types::platform::sessions_dir();
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let dir = tempfile::tempdir_in(&sessions_dir).unwrap();
        let session_id = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let identity = CanonicalSessionIdentity::open_or_create(dir.path()).unwrap();
        let now = 1_789_830_000_000;
        let mut store = CanonicalSessionStore::open(
            dir.path(),
            identity.session_id.clone(),
            identity.log_id.clone(),
        )
        .unwrap();
        let lease = store
            .acquire_writer(WriterId::new(tag), now, 600_000)
            .unwrap();
        let turn_id = TurnId::new(format!("turn_{}", generate_ulid()));
        let call_id = ToolCallId::new(format!("call_{}", generate_ulid()));
        let interaction_id = InteractionId::new(format!("int_{}", generate_ulid()));
        let envelope = |ts_ms: i64, payload: FactPayload| SessionFact {
            schema: FactSchema::v2(),
            session_id: identity.session_id.clone(),
            log_id: identity.log_id.clone(),
            fact_seq: 0,
            event_id: EventId::new(generate_ulid()),
            ts_ms,
            causation_id: None,
            turn_id: Some(turn_id.clone()),
            call_id: Some(call_id.clone()),
            interaction_id: Some(interaction_id.clone()),
            payload,
        };
        store
            .append(
                &lease,
                envelope(
                    now,
                    FactPayload::SessionCreated(SessionCreated {
                        created_at_ms: now,
                        cwd: "/tmp".into(),
                        model: "test".into(),
                        parent_session_id: None,
                        schema_caps: Vec::new(),
                    }),
                ),
                now,
            )
            .unwrap();
        store
            .append(
                &lease,
                envelope(
                    now + 1,
                    FactPayload::InteractionRequested(InteractionRequested {
                        interaction_id: interaction_id.clone(),
                        call_id: Some(call_id.clone()),
                        turn_id: turn_id.clone(),
                        kind,
                        request_ref: ContentRef::new(sha256_content_hash(b"request")),
                        expires_at_ms: None,
                        requested_at_ms: now + 1,
                    }),
                ),
                now + 1,
            )
            .unwrap();
        store
            .append(
                &lease,
                envelope(
                    now + 2,
                    FactPayload::InteractionResolved(InteractionResolved {
                        interaction_id: interaction_id.clone(),
                        decision_ref: ContentRef::new(sha256_content_hash(b"decision")),
                        decision: Some(decision),
                        resolved_by: ActorRef {
                            kind: ActorKind::User,
                            id: "v2-fixture".into(),
                            display_name: None,
                        },
                        resolution_seq: 1,
                        resolved_at_ms: now + 2,
                    }),
                ),
                now + 2,
            )
            .unwrap();
        (dir, session_id, interaction_id, call_id)
    }

    /// V2-R4 for permission and plan: a second answer for an already-resolved
    /// interaction is rejected with a typed winning verdict, mirroring ask.
    #[tokio::test]
    async fn v2_second_answer_returns_winning_verdict_for_permission_and_plan() {
        use qaqh_domain::{ControlCommand, ToolCommand};
        use qaqh_ringing::{
            RingingCommand, RingingV2CommandAck, RingingV2CommandEnvelope, RingingV2CommandResult,
            RingingV2ExistingResult,
        };
        use qaqh_session::session_fact_v2::{InteractionDecision, InteractionKind};

        let app = build_router(test_state());
        let client = open_v2_session(app.clone(), "ci-v2").await;

        // (kind, winning decision, second-answer command, expected typed result)
        let permission = {
            let (dir, session_id, interaction_id, call_id) = session_resolved_interaction_session(
                InteractionKind::Permission,
                InteractionDecision::Approved,
                "v2-permission-fixture",
            );
            let command = RingingV2CommandEnvelope::new(
                "cmd-second-permission",
                "ci-v2",
                RingingCommand::Tool(ToolCommand::ToolPermissionRespond {
                    tool_call_id: call_id.as_str().to_string(),
                    approved: false,
                    trust_folder: false,
                }),
            )
            .with_client_session_id(client.client_session_id.clone())
            .with_session_id(session_id);
            let expected = RingingV2CommandResult::PermissionResolved {
                interaction_id: interaction_id.as_str().to_string(),
                approved: true,
            };
            (dir, command, expected)
        };
        let plan = {
            let (dir, session_id, interaction_id, _call_id) = session_resolved_interaction_session(
                InteractionKind::Plan,
                InteractionDecision::Rejected,
                "v2-plan-fixture",
            );
            let command = RingingV2CommandEnvelope::new(
                "cmd-second-plan",
                "ci-v2",
                RingingCommand::Control(ControlCommand::PlanReviewRespond {
                    interaction_id: interaction_id.as_str().to_string(),
                    approved: true,
                    message: None,
                    autonomous: false,
                }),
            )
            .with_client_session_id(client.client_session_id.clone())
            .with_session_id(session_id);
            let expected = RingingV2CommandResult::PlanReviewResolved {
                interaction_id: interaction_id.as_str().to_string(),
                approved: false,
            };
            (dir, command, expected)
        };

        for (label, _dir, command, expected) in [
            ("permission", permission.0, permission.1, permission.2),
            ("plan", plan.0, plan.1, plan.2),
        ] {
            let (status, ack): (StatusCode, RingingV2CommandAck) =
                post_v2_command(app.clone(), &client.client_session_id, &command).await;
            assert_eq!(status, StatusCode::OK, "{label}");
            assert_eq!(
                ack.code.as_deref(),
                Some("interaction_already_resolved"),
                "{label}"
            );
            match ack.existing.expect("winning verdict") {
                RingingV2ExistingResult::InteractionResolved { result } => {
                    assert_eq!(result, expected, "{label}");
                }
                other => panic!("{label}: expected interaction_resolved, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn v2_driver_gate_rejects_non_driver_and_stale_epoch() {
        use qaqh_domain::ConversationCommand;
        use qaqh_ringing::{RingingCommand, RingingV2CommandEnvelope};

        let app = build_router(test_state());
        let a = open_v2_session(app.clone(), "ci-a").await;
        let b = open_v2_session(app.clone(), "ci-b").await;
        // Canonical seat held by `a`, whose lease is live.
        let (_dir, session_id) = session_canonical_driver_session(Some(&a.client_session_id), 1);

        let gated = |command_id: &str, client_session_id: &str| {
            RingingV2CommandEnvelope::new(
                command_id,
                "ci-v2",
                RingingCommand::Conversation(ConversationCommand::ConversationCancel {
                    turn_id: None,
                }),
            )
            .with_client_session_id(client_session_id)
            .with_session_id(session_id.clone())
        };

        let (_, rejected) = post_v2_command(
            app.clone(),
            &b.client_session_id,
            &gated("cmd-b-cancel", &b.client_session_id),
        )
        .await;
        assert_eq!(rejected.code.as_deref(), Some("not_driver"));

        let (_, stale) = post_v2_command(
            app.clone(),
            &a.client_session_id,
            &gated("cmd-a-stale", &a.client_session_id).with_driver_epoch(0),
        )
        .await;
        assert_eq!(stale.code.as_deref(), Some("stale_driver_epoch"));

        let (_, current) = post_v2_command(
            app,
            &a.client_session_id,
            &gated("cmd-a-current", &a.client_session_id).with_driver_epoch(1),
        )
        .await;
        assert_ne!(current.code.as_deref(), Some("not_driver"));
        assert_ne!(current.code.as_deref(), Some("stale_driver_epoch"));
    }

    #[tokio::test]
    async fn v2_service_workspace_write_is_driver_gated() {
        let state = test_state();
        let app = build_router(state.clone());
        let a = open_v2_session(app.clone(), "ci-a").await;
        let b = open_v2_session(app.clone(), "ci-b").await;
        let (_dir, session_id) = session_canonical_driver_session(Some(&a.client_session_id), 1);
        {
            let mut leases = state.leases.lock().unwrap();
            leases.attach_session(&a.client_session_id, &session_id);
            leases.attach_session(&b.client_session_id, &session_id);
        }
        let params = serde_json::json!({"session_id": session_id, "path": "/tmp/workspace"});

        let (status, rejected) =
            post_v2_service(app.clone(), &b.client_session_id, "workspace.set", &params).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(rejected["code"], "not_driver");

        let (status, allowed) =
            post_v2_service(app, &a.client_session_id, "workspace.set", &params).await;
        assert_ne!(
            status,
            StatusCode::FORBIDDEN,
            "the canonical driver must pass the service gate: {allowed}"
        );
    }

    #[tokio::test]
    async fn v2_driver_busy_claim_is_rejected_before_dispatch() {
        let app = build_router(test_state());
        let a = open_v2_session(app.clone(), "ci-a").await;
        let b = open_v2_session(app.clone(), "ci-b").await;
        let (_dir, session_id) = session_canonical_driver_session(Some(&a.client_session_id), 1);

        let busy = post_driver(app.clone(), &session_id, "claim", &b.client_session_id).await;
        assert_eq!(busy["accepted"], false);
        assert_eq!(busy["reason"], "driver_busy");
        assert_eq!(busy["holder"], a.client_session_id);
        assert_eq!(busy["driver_epoch"], 1);

        let already = post_driver(app.clone(), &session_id, "claim", &a.client_session_id).await;
        assert_eq!(already["accepted"], true);
        assert_eq!(already["reason"], "already_holder");
        assert_eq!(already["driver_epoch"], 1);

        let not_driver = post_driver(app, &session_id, "release", &b.client_session_id).await;
        assert_eq!(not_driver["accepted"], false);
        assert_eq!(not_driver["reason"], "not_driver");
    }

    #[tokio::test]
    async fn v2_bootstrap_reports_canonical_driver_state() {
        let app = build_router(test_state());
        let a = open_v2_session(app.clone(), "ci-a").await;
        let b = open_v2_session(app.clone(), "ci-b").await;
        let (_dir, session_id) = session_canonical_driver_session(Some(&a.client_session_id), 3);

        let bootstrap = |client_session_id: String| {
            let app = app.clone();
            let session_id = session_id.clone();
            async move {
                let request = Request::builder()
                    .uri(format!("/ringing/v2/sessions/{session_id}/bootstrap"))
                    .header("authorization", "Bearer test-token")
                    .header("x-qaqh-client-session-id", client_session_id)
                    .body(Body::empty())
                    .unwrap();
                let response = app.oneshot(request).await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
                    .await
                    .unwrap();
                let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
                value["control"]["state"]["driver"].clone()
            }
        };

        let holder_view = bootstrap(a.client_session_id.clone()).await;
        assert_eq!(holder_view["holder"], a.client_session_id);
        assert_eq!(holder_view["driver_epoch"], 3);
        assert_eq!(holder_view["can_claim"], false);

        let other_view = bootstrap(b.client_session_id).await;
        assert_eq!(other_view["holder"], a.client_session_id);
        assert_eq!(other_view["can_claim"], true);
    }

    /// V2-C7: a session whose canonical log has no commit yet cannot mint a
    /// snapshot cursor; the route must surface the documented
    /// `snapshot_missing` reason (409) instead of a generic 500.
    #[tokio::test]
    async fn v2_bootstrap_reports_snapshot_missing_for_uncommitted_session() {
        use qaqh_session::canonical::CanonicalSessionIdentity;

        let app = build_router(test_state());
        let client = open_v2_session(app.clone(), "ci-v2").await;

        let sessions_dir = qaqh_types::platform::sessions_dir();
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let dir = tempfile::tempdir_in(&sessions_dir).unwrap();
        let session_id = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        // Identity only: no `events.commit` marker, so no committed facts.
        CanonicalSessionIdentity::open_or_create(dir.path()).unwrap();

        let request = Request::builder()
            .uri(format!("/ringing/v2/sessions/{session_id}/bootstrap"))
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", client.client_session_id)
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["code"], "snapshot_missing");
    }

    /// V2-D3: a driver handover is a *reliable* control event, not just a
    /// bootstrap field. A client that reconnects with a pre-handover cursor
    /// must replay the `DriverChanged` fact.
    #[tokio::test]
    async fn v2_driver_handover_emits_reliable_control_event() {
        use qaqh_session::canonical::{
            CanonicalSessionIdentity, CanonicalSessionStore, WriterId, generate_ulid,
        };
        use qaqh_session::session_fact_v2::{
            DriverChanged, EventId, FactPayload, FactSchema, SessionCreated, SessionFact,
        };

        let state = test_state();
        let v2_hub = state.v2_hub.clone();
        let app = build_router(state);
        let a = open_v2_session(app.clone(), "ci-a").await;

        let sessions_dir = qaqh_types::platform::sessions_dir();
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let dir = tempfile::tempdir_in(&sessions_dir).unwrap();
        let session_id = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let identity = CanonicalSessionIdentity::open_or_create(dir.path()).unwrap();
        let now = 1_789_830_000_000;
        let mut store = CanonicalSessionStore::open(
            dir.path(),
            identity.session_id.clone(),
            identity.log_id.clone(),
        )
        .unwrap();
        let lease = store
            .acquire_writer(WriterId::new("v2-handover-test"), now, 600_000)
            .unwrap();
        let fact = |ts_ms: i64, payload: FactPayload| SessionFact {
            schema: FactSchema::v2(),
            session_id: identity.session_id.clone(),
            log_id: identity.log_id.clone(),
            fact_seq: 0,
            event_id: EventId::new(generate_ulid()),
            ts_ms,
            causation_id: None,
            turn_id: None,
            call_id: None,
            interaction_id: None,
            payload,
        };
        store
            .append(
                &lease,
                fact(
                    now,
                    FactPayload::SessionCreated(SessionCreated {
                        created_at_ms: now,
                        cwd: "/tmp".into(),
                        model: "test".into(),
                        parent_session_id: None,
                        schema_caps: Vec::new(),
                    }),
                ),
                now,
            )
            .unwrap();

        // Snapshot cursor taken *before* the handover.
        let bootstrap = Request::builder()
            .uri(format!("/ringing/v2/sessions/{session_id}/bootstrap"))
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", a.client_session_id.clone())
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(bootstrap).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(value["control"]["state"]["driver"]["holder"].is_null());
        assert_eq!(value["control"]["state"]["driver"]["driver_epoch"], 0);
        let cursor = value["snapshot_cursor"].as_str().unwrap().to_string();

        // `a` takes the seat: canonical DriverChanged + projection publish.
        let changed = store
            .append(
                &lease,
                fact(
                    now + 1,
                    FactPayload::DriverChanged(DriverChanged {
                        holder: Some(a.client_session_id.clone()),
                        driver_epoch: 1,
                        changed_at_ms: now + 1,
                    }),
                ),
                now + 1,
            )
            .unwrap();
        qaqh_session::projection::ProjectionSink::publish(
            v2_hub.as_ref(),
            dir.path(),
            &changed.fact,
            &changed.events,
        );

        // Reconnect from the pre-handover cursor: the reliable event must be
        // replayed on the control channel.
        let request = Request::builder()
            .uri(format!(
                "/ringing/v2/sessions/{session_id}/events?since_cursor={cursor}"
            ))
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", a.client_session_id.clone())
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        use futures_util::StreamExt as _;
        let mut stream = response.into_body().into_data_stream();
        let chunk = stream.next().await.unwrap().unwrap();
        let text = String::from_utf8_lossy(&chunk);
        assert!(text.contains("event: ringing.event"), "{text}");
        assert!(text.contains("\"delivery\":\"reliable\""), "{text}");
        assert!(text.contains("\"kind\":\"driver_changed\""), "{text}");
        assert!(text.contains("\"fact_seq\":2"), "{text}");
        assert!(text.contains("\"driver_epoch\":1"), "{text}");
    }

    /// V2-R1..R3: a pending interaction survives a reconnect as a stable
    /// `interaction_id` in bootstrap, for permission / ask / plan alike.
    #[tokio::test]
    async fn v2_pending_interactions_survive_reconnect() {
        use qaqh_session::canonical::{
            CanonicalSessionIdentity, CanonicalSessionStore, WriterId, generate_ulid,
            sha256_content_hash,
        };
        use qaqh_session::session_fact_v2::{
            ContentRef, EventId, FactPayload, FactSchema, InteractionId, InteractionKind,
            InteractionRequested, SessionCreated, SessionFact, ToolCallId, TurnId,
        };

        let app = build_router(test_state());
        let client = open_v2_session(app.clone(), "ci-reconnect").await;

        for (kind, expected_kind) in [
            (InteractionKind::Permission, "permission"),
            (InteractionKind::Ask, "ask"),
            (InteractionKind::Plan, "plan"),
        ] {
            let sessions_dir = qaqh_types::platform::sessions_dir();
            std::fs::create_dir_all(&sessions_dir).unwrap();
            let dir = tempfile::tempdir_in(&sessions_dir).unwrap();
            let session_id = dir
                .path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let identity = CanonicalSessionIdentity::open_or_create(dir.path()).unwrap();
            let now = 1_789_830_000_000;
            let mut store = CanonicalSessionStore::open(
                dir.path(),
                identity.session_id.clone(),
                identity.log_id.clone(),
            )
            .unwrap();
            let lease = store
                .acquire_writer(WriterId::new("v2-reconnect-test"), now, 60_000)
                .unwrap();
            let turn_id = TurnId::new(format!("turn_{}", generate_ulid()));
            let call_id = ToolCallId::new(format!("call_{}", generate_ulid()));
            let interaction_id = InteractionId::new(format!("int_{}", generate_ulid()));
            let envelope = |ts_ms: i64, payload: FactPayload| SessionFact {
                schema: FactSchema::v2(),
                session_id: identity.session_id.clone(),
                log_id: identity.log_id.clone(),
                fact_seq: 0,
                event_id: EventId::new(generate_ulid()),
                ts_ms,
                causation_id: None,
                turn_id: Some(turn_id.clone()),
                call_id: Some(call_id.clone()),
                interaction_id: Some(interaction_id.clone()),
                payload,
            };
            store
                .append(
                    &lease,
                    envelope(
                        now,
                        FactPayload::SessionCreated(SessionCreated {
                            created_at_ms: now,
                            cwd: "/tmp".into(),
                            model: "test".into(),
                            parent_session_id: None,
                            schema_caps: Vec::new(),
                        }),
                    ),
                    now,
                )
                .unwrap();
            store
                .append(
                    &lease,
                    envelope(
                        now + 1,
                        FactPayload::InteractionRequested(InteractionRequested {
                            interaction_id: interaction_id.clone(),
                            call_id: Some(call_id.clone()),
                            turn_id: turn_id.clone(),
                            kind,
                            request_ref: ContentRef::new(sha256_content_hash(b"pending")),
                            expires_at_ms: None,
                            requested_at_ms: now + 1,
                        }),
                    ),
                    now + 1,
                )
                .unwrap();

            let bootstrap = |app: axum::Router, session_id: String, client_session_id: String| async move {
                let request = Request::builder()
                    .uri(format!("/ringing/v2/sessions/{session_id}/bootstrap"))
                    .header("authorization", "Bearer test-token")
                    .header("x-qaqh-client-session-id", client_session_id)
                    .body(Body::empty())
                    .unwrap();
                let response = app.oneshot(request).await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
                    .await
                    .unwrap();
                let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
                value["control"]["state"]["interactions"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            };

            // Two bootstraps == subscribe / reconnect. The pending set must be
            // identical and carry the canonical interaction id.
            let first = bootstrap(
                app.clone(),
                session_id.clone(),
                client.client_session_id.clone(),
            )
            .await;
            let second = bootstrap(
                app.clone(),
                session_id.clone(),
                client.client_session_id.clone(),
            )
            .await;
            assert_eq!(first, second, "{kind:?} pending set must be stable");
            assert_eq!(
                first.len(),
                1,
                "{kind:?} must expose one pending interaction"
            );
            assert_eq!(first[0]["interaction_id"], interaction_id.as_str());
            assert_eq!(first[0]["kind"], expected_kind);
            assert_eq!(
                first[0]["request"]["kind"], "ref",
                "所有 pending interaction 都必须暴露 canonical 正文 ref"
            );

            // v2 approvals 端点：与旧 v1 端点同形，id 为 canonical 形态。
            let request = Request::builder()
                .uri(format!("/ringing/v2/sessions/{session_id}/approvals"))
                .header("authorization", "Bearer test-token")
                .header("x-qaqh-client-session-id", client.client_session_id.clone())
                .body(Body::empty())
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
                .await
                .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
            match kind {
                InteractionKind::Permission => {
                    assert_eq!(
                        value["pending_permission"]["tool_call_id"],
                        call_id.as_str(),
                        "permission 以 canonical call_id 暴露"
                    );
                    assert_eq!(
                        value["pending_permission"]["details_unavailable"], true,
                        "测试没往 content store 写正文，应标记详情不可用"
                    );
                    assert!(value["pending_interaction"].is_null());
                }
                InteractionKind::Ask | InteractionKind::Plan => {
                    assert_eq!(
                        value["pending_interaction"]["id"],
                        interaction_id.as_str(),
                        "ask/plan 以 canonical interaction_id 暴露"
                    );
                    // bootstrap 与 approvals 统一使用 wire 值 ask / plan。
                    let expected = if kind == InteractionKind::Ask {
                        "ask"
                    } else {
                        "plan"
                    };
                    assert_eq!(value["pending_interaction"]["kind"], expected);
                    assert!(
                        value["pending_interaction"]["details"].is_null(),
                        "测试没有写正文，details 应显式降级为 null"
                    );
                    assert!(value["pending_permission"].is_null());
                }
            }
        }
    }

    /// 纯 v2：approvals 的 v1 路径已硬切。
    #[tokio::test]
    async fn approvals_v1_route_is_hard_cut() {
        let state = test_state();
        let app = build_router(state);
        let req = Request::builder()
            .uri("/ringing/v1/sessions/seed-1/approvals")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", "cs-1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn timeline_events_success() {
        let state = test_state();
        state
            .leases
            .lock()
            .unwrap()
            .open("cs-1".into(), "ci-1".into());
        state
            .leases
            .lock()
            .unwrap()
            .attach_session("cs-1", "seed-1");
        let app = build_router(state);
        let req = Request::builder()
            .uri("/ringing/v2/sessions/seed-1/timeline/events")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", "cs-1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
    }

    /// SessionAttach：仅 lease attach，不触碰 actor。attach 后 owns_seed 放行
    /// timeline/频道读取；空 seed 被拒（missing_session_id）。
    #[tokio::test]
    async fn session_attach_grants_session_ownership_without_actor_side_effects() {
        let state = test_state();
        state
            .leases
            .lock()
            .unwrap()
            .open("cs-1".into(), "ci-1".into());
        let app = build_router(state.clone());
        let env = qaqh_ringing::RingingV2CommandEnvelope::new(
            "cmd-attach-1",
            "ci-1",
            qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::SessionAttach {
                session_id: "sub-seed-1".into(),
            }),
        )
        .with_client_session_id("cs-1")
        .with_session_id("sub-seed-1");
        let req = Request::builder()
            .method("POST")
            .uri("/ringing/v2/commands/control")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", "cs-1")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&env).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            state
                .leases
                .lock()
                .unwrap()
                .owns_session("cs-1", "sub-seed-1")
        );

        // 空 seed → Rejected missing_session_id，且不产生任何归属。
        let app = build_router(state.clone());
        let env_bad = qaqh_ringing::RingingV2CommandEnvelope::new(
            "cmd-attach-2",
            "ci-1",
            qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::SessionAttach {
                session_id: String::new(),
            }),
        )
        .with_client_session_id("cs-1");
        let req = Request::builder()
            .method("POST")
            .uri("/ringing/v2/commands/control")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", "cs-1")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&env_bad).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(!state.leases.lock().unwrap().owns_session("cs-1", ""));
    }

    /// todo CLI 路线：WRITE_SEEDED 调用在 lease 未 attach seed 时必须 401
    /// （拒绝发生在 dispatch 之前，不触碰磁盘）。
    #[tokio::test]
    async fn todo_set_requires_session_ownership() {
        let state = test_state();
        state
            .leases
            .lock()
            .unwrap()
            .open("cs-1".into(), "ci-1".into());
        let app = build_router(state);
        let req = Request::builder()
            .method("POST")
            .uri("/ringing/v2/service/todo.set")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", "cs-1")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"session_id": "seed-1", "id": "T1", "status": "completed"})
                    .to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// 纯 v2：open 不再带 attach_seed，归属由 `session_attach` 命令建立；
    /// attach 后同会话的 READ_SEEDED 调用放行（todo.list 只读，读不到即空表）。
    ///
    /// BETA-01 Phase D：服务面会话键写端为 `session_id`。
    #[tokio::test]
    async fn todo_list_allowed_after_session_attached() {
        let state = test_state();
        let app = build_router(state);
        let open_req = Request::builder()
            .method("POST")
            .uri("/ringing/v2/clients/open")
            .header("authorization", "Bearer test-token")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "schema": qaqh_ringing::protocol::RINGING_SCHEMA,
                    "version": qaqh_ringing::RINGING_V2_VERSION,
                    "client_instance_id": "ci-cli",
                })
                .to_string(),
            ))
            .unwrap();
        let resp = app.clone().oneshot(open_req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let open: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session_id = open["client_session_id"]
            .as_str()
            .expect("session id")
            .to_string();
        let attach_req = Request::builder()
            .method("POST")
            .uri("/ringing/v2/commands/control")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", &session_id)
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "schema": qaqh_ringing::protocol::RINGING_SCHEMA,
                    "version": qaqh_ringing::RINGING_V2_VERSION,
                    "channel": "control",
                    "command_id": "cli-attach-test",
                    "client_instance_id": "ci-cli",
                    "client_session_id": session_id,
                    "session_id": "seed-1",
                    "command": {"channel": "control", "type": "session_attach", "session_id": "seed-1"},
                })
                .to_string(),
            ))
            .unwrap();
        let resp = app.clone().oneshot(attach_req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let req = Request::builder()
            .method("POST")
            .uri("/ringing/v2/service/todo.list")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", &session_id)
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"session_id": "seed-1"}).to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn timeline_events_requires_session_ownership() {
        let state = test_state();
        state
            .leases
            .lock()
            .unwrap()
            .open("cs-1".into(), "ci-1".into());
        // not attached
        let app = build_router(state);
        let req = Request::builder()
            .uri("/ringing/v2/sessions/seed-1/timeline/events")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", "cs-1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// 普通 daemon Router 不挂载浏览器控制面（浏览器网关已随 Tauri 化移除）。
    #[tokio::test]
    async fn webui_routes_are_not_mounted() {
        let app = build_router(test_state());
        for path in [
            "/debug/",
            "/debug/__qaqh_bridge__.js",
            "/debug/__qaqh_token__",
            "/ui/",
            "/__gateway/bootstrap.js",
        ] {
            let req = Request::builder().uri(path).body(Body::empty()).unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            // S0 起鉴权中间件对所有非公开路径统一收口：未携带 Bearer 的请求在
            // 到达 fallback 前即 401，因此这里只断言「未被挂载」——绝不可能是
            // 200/2xx（有内容服务），且只可能是 401（未鉴权收口）或 404（fallback）。
            let status = resp.status();
            assert!(
                matches!(status, StatusCode::UNAUTHORIZED | StatusCode::NOT_FOUND),
                "{path} must not be mounted on the daemon (got {status})"
            );
        }
    }

    /// `/health` 不回显 `token_len`（凭据长度也是旁路信息）。
    #[tokio::test]
    async fn health_does_not_leak_token() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let txt = String::from_utf8_lossy(&body);
        assert!(txt.contains("ok epoch="), "{txt}");
        assert!(
            !txt.contains("token"),
            "/health must not mention token: {txt}"
        );
    }

    /// BUG-2026-09-13-18：`limit=0` 时 handler 曾把 0 直通给 `paginate_turns`，
    /// `end == start` → 空页，但 `start > 0` 仍报 `has_more=true`，按 has_more
    /// 驱动的客户端翻页永远拿不到行、也永远停不下来。修复要求 limit 经
    /// `max(1)` 钳制后仍返回有界页。
    #[tokio::test]
    async fn timeline_limit_zero_returns_bounded_page() {
        let state = test_state();
        state
            .leases
            .lock()
            .unwrap()
            .open("cs-1".into(), "ci-1".into());
        state
            .leases
            .lock()
            .unwrap()
            .attach_session("cs-1", "seed-1");
        for i in 1..=3 {
            state
                .hub
                .publish_timeline(
                    "seed-1",
                    qaqh_domain::TimelineIntent::TurnOpened {
                        turn_id: format!("t{i}"),
                        user_text: format!("q{i}"),
                    },
                )
                .expect("seed a timeline turn");
        }
        let app = build_router(state);
        let req = Request::builder()
            .uri("/ringing/v2/sessions/seed-1/timeline?limit=0")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", "cs-1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let turns = page["snapshot"]["turns"].as_array().expect("turns array");
        // limit=0 必须被钳到 1（而非返回空页 + has_more=true 的死循环）。
        assert_eq!(turns.len(), 1, "limit=0 must degrade to a bounded page");
        assert_eq!(page["total_turns"], serde_json::json!(3));
    }

    /// `?limit=N` 端到端生效（此前只有 `limit=0` 被钉过，正常页大小无人验证）。
    #[tokio::test]
    async fn timeline_honors_requested_page_size() {
        let state = test_state();
        state
            .leases
            .lock()
            .unwrap()
            .open("cs-1".into(), "ci-1".into());
        state
            .leases
            .lock()
            .unwrap()
            .attach_session("cs-1", "seed-1");
        for i in 1..=3 {
            state
                .hub
                .publish_timeline(
                    "seed-1",
                    qaqh_domain::TimelineIntent::TurnOpened {
                        turn_id: format!("t{i}"),
                        user_text: format!("q{i}"),
                    },
                )
                .expect("seed a timeline turn");
        }
        let app = build_router(state);
        let req = Request::builder()
            .uri("/ringing/v2/sessions/seed-1/timeline?limit=2")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", "cs-1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let turns = page["snapshot"]["turns"].as_array().expect("turns array");
        // 取的是最新两页端点：t3、t2（`before_index` 缺省 = 最新一页）。
        assert_eq!(turns.len(), 2, "limit=2 必须恰好给两条");
        assert_eq!(page["total_turns"], serde_json::json!(3));
        assert_eq!(turns[0]["turn_index"], serde_json::json!(1));
        assert_eq!(turns[1]["turn_index"], serde_json::json!(2));
        assert_eq!(page["has_more"], serde_json::json!(true), "还有 t1 可翻");
    }
}

#[cfg(test)]
mod command_entry_tests {
    //! `execute_command` 进程内命令入口的不变量测试（hub-fact-bus spec §5.2）。
    //!
    //! 只锁不变量：lease 归属校验在入口（不变量 4）、missing_session_id 拒绝、
    //! 幂等回执（同 payload 重放 accepted / 异 payload 冲突，不变量 3）。
    //! 不锁旧 HTTP 形状——这些路径曾经由 v1 handler 承载。

    use super::*;
    use crate::axum_server::axum_impl::command::execute_command;
    use axum::http::{HeaderMap, StatusCode};
    use qaqh_ringing::{RingingCommandAckStatus, RingingV2CommandEnvelope};

    const CALLER: &str = "cs-entry";

    fn entry_state() -> AppState {
        let leases = std::sync::Arc::new(std::sync::Mutex::new(
            qaqh_runtime::ringing::RingingLeaseStore::new(),
        ));
        leases
            .lock()
            .unwrap()
            .open(CALLER.into(), "ci-entry".into());
        let (shutdown, _) = tokio::sync::watch::channel(false);
        // SessionManager 是进程级单例，统一走 init_session_manager 守卫。
        super::init_session_manager();
        AppState {
            hub: std::sync::Arc::new(qaqh_runtime::RingingHub::new("entry-epoch")),
            v2_hub: std::sync::Arc::new(qaqh_runtime::ringing::V2ProjectionHub::new("entry-epoch")),
            leases,
            driver_watch: std::sync::Arc::new(std::sync::Mutex::new(
                qaqh_runtime::ringing::RingingDriverWatch::new(),
            )),
            pending: std::sync::Arc::new(std::sync::Mutex::new(
                qaqh_runtime::ringing::PendingCommandStore::new(),
            )),
            service: qaqh_runtime::QaqhService::init(qaqh_session::SessionManager::global())
                .clone(),
            admin_token: String::from("entry-token"),
            devices: std::sync::Arc::new(std::sync::Mutex::new(
                qaqh_runtime::ringing::DeviceRegistry::new(),
            )),
            pairings: std::sync::Arc::new(std::sync::Mutex::new(PairingTable::new())),
            tls_fingerprint: None,
            challenges: std::sync::Arc::new(ChallengeStore::default()),
            epoch: String::from("entry-epoch"),
            shutdown,
            test_hooks: std::sync::Arc::new(TestHooks::disabled()),
        }
    }

    fn caller_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-qaqh-client-session-id", CALLER.parse().unwrap());
        headers
    }

    fn attach_envelope(command_id: &str, target: &str) -> RingingV2CommandEnvelope {
        RingingV2CommandEnvelope::new(
            command_id,
            "ci-entry",
            qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::SessionAttach {
                session_id: target.to_string(),
            }),
        )
        .with_client_session_id(CALLER)
        .with_session_id(target)
    }

    #[tokio::test]
    async fn missing_lease_header_is_rejected_at_the_entry() {
        let state = entry_state();
        let envelope = attach_envelope("cmd-no-header", "seed-target");
        let (status, ack) =
            execute_command(&state, &HeaderMap::new(), &axum_impl::Identity::Admin, envelope).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(ack.status, RingingCommandAckStatus::Rejected);
        assert_eq!(ack.code.as_deref(), Some("lease_required"));
    }

    #[tokio::test]
    async fn inactive_lease_is_rejected_at_the_entry() {
        let state = entry_state();
        let mut headers = HeaderMap::new();
        headers.insert("x-qaqh-client-session-id", "cs-ghost".parse().unwrap());
        let envelope = attach_envelope("cmd-ghost", "seed-target");
        let (status, ack) =
            execute_command(&state, &headers, &axum_impl::Identity::Admin, envelope).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(ack.code.as_deref(), Some("lease_required"));
    }

    #[tokio::test]
    async fn missing_session_id_command_is_rejected() {
        let state = entry_state();
        // session.attach 之外的非 SessionCreate 命令缺 session_id 必须在入口拒绝。
        let envelope = RingingV2CommandEnvelope::new(
            "cmd-missing-seed",
            "ci-entry",
            qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::SessionResume {
                session_id: "seed-x".into(),
            }),
        )
        .with_client_session_id(CALLER);
        let (status, ack) = execute_command(&state, &caller_headers(), &axum_impl::Identity::Admin, envelope).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(ack.code.as_deref(), Some("missing_session_id"));
    }

    #[tokio::test]
    async fn duplicate_command_id_with_same_payload_replays_accepted() {
        let state = entry_state();
        let envelope = attach_envelope("cmd-dup", "seed-target");
        let (first_status, first_ack) =
            execute_command(&state, &caller_headers(), &axum_impl::Identity::Admin, envelope.clone()).await;
        assert_eq!(first_status, StatusCode::OK);
        assert_eq!(first_ack.status, RingingCommandAckStatus::Accepted);

        let (status, ack) = execute_command(&state, &caller_headers(), &axum_impl::Identity::Admin, envelope).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ack.status, RingingCommandAckStatus::Accepted);
        assert!(
            ack.message
                .as_deref()
                .is_some_and(|m| m.contains("duplicate command_id")),
            "replay must be answered from the receipt, not re-executed"
        );
    }

    #[tokio::test]
    async fn duplicate_command_id_with_other_payload_is_conflict() {
        let state = entry_state();
        let first = attach_envelope("cmd-clash", "seed-target");
        let (first_status, _) = execute_command(&state, &caller_headers(), &axum_impl::Identity::Admin, first).await;
        assert_eq!(first_status, StatusCode::OK);

        // 同 command_id、不同 payload：必须拒绝，不得重放旧回执。
        let second = attach_envelope("cmd-clash", "seed-other");
        let (status, ack) = execute_command(&state, &caller_headers(), &axum_impl::Identity::Admin, second).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(ack.code.as_deref(), Some("duplicate_command_mismatch"));
    }

    /// BUG-2026-09-29-01 回归锁（真实端到端路径：session.new 落盘 + worker
    /// spawn，同 scripts/smoke-g1.ps1 流程）。
    ///
    /// 不变量：SessionCreate 经 commands 通道必须「ack accepted + 新 seed 归属
    /// 发起命令的 lease」。回归前 `if let Some(session_id)` 遮蔽外层 lease 变量，
    /// `attach_session(&seed, &seed)` 恒 false——命令表面 200，会话实际处于
    /// 「无归属」孤儿态（后续所有该 seed 的命令 401）。
    #[tokio::test]
    async fn session_create_over_commands_channel_attaches_created_seed_to_the_lease() {
        let state = entry_state();
        let envelope = RingingV2CommandEnvelope::new(
            "cmd-session-create-attach",
            "ci-entry",
            qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::SessionCreate {
                close_current: false,
                cwd: None,
                tool_mode: None,
                custom_tools: Vec::new(),
            }),
        )
        .with_client_session_id(CALLER);
        let (status, ack) = execute_command(&state, &caller_headers(), &axum_impl::Identity::Admin, envelope).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ack.status, RingingCommandAckStatus::Accepted);

        // 归属不变量：本 lease 名下恰好出现（且仅出现）新建的那个 seed。
        let owned = state.leases.lock().unwrap().owned_sessions(CALLER);
        assert_eq!(
            owned.len(),
            1,
            "created session must be attached to the commanding lease, owned={owned:?}"
        );
        let created_seed = owned.into_iter().next().expect("owned set non-empty");
        assert!(
            state
                .leases
                .lock()
                .unwrap()
                .owns_session(CALLER, &created_seed),
            "lease must own the created seed after ack"
        );

        // 清理：close（join in-process worker）+ delete（删会话目录），
        // 与 command.rs SessionDelete op 同语义。
        let service = state.service.clone();
        let seed = created_seed.clone();
        let cleanup = tokio::task::spawn_blocking(move || {
            let _ = service.close_session(&seed, None);
            service.delete_session(&seed, None)
        })
        .await
        .unwrap_or_else(|e| Err(format!("cleanup join error: {e}")));
        if let Err(error) = cleanup {
            panic!("cleanup failed for {created_seed} (leaked session dir): {error}");
        }
    }

    /// BUG-2026-10-05-01 回归锁：SessionResume 必须恢复**命令声明的目标会话**，
    /// 而不是 header 的 client_session_id（租约标识）。
    ///
    /// 回归前的症状链：`session.resume` 收到租约 id → `.active_session` 被写成
    /// 租约 id → `register_root_agent` 对 `sessions/{cs}/` 物化孤儿 identity
    /// 目录 → actor resume 加载失败 → lifecycle 兜底静默再分配一个幽灵会话
    /// 目录。TUI 每次打开 tab（SessionCreate → SessionResume 序列）就多出
    /// 两个目录。不变量：resume 之后，磁盘上**不得**出现以租约 id 命名的会话
    /// 目录，`.active_session` 也不得指向租约 id。
    #[tokio::test]
    async fn session_resume_over_commands_channel_resumes_the_target_not_the_lease() {
        let state = entry_state();

        // 1. 先经 commands 通道真实建会话（同上方 create 回归锁的端到端路径）。
        let create = RingingV2CommandEnvelope::new(
            "cmd-resume-regression-create",
            "ci-entry",
            qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::SessionCreate {
                close_current: false,
                cwd: None,
                tool_mode: None,
                custom_tools: Vec::new(),
            }),
        )
        .with_client_session_id(CALLER);
        let (create_status, create_ack) =
            execute_command(&state, &caller_headers(), &axum_impl::Identity::Admin, create).await;
        assert_eq!(create_status, StatusCode::OK);
        assert_eq!(create_ack.status, RingingCommandAckStatus::Accepted);
        let created_seed = state
            .leases
            .lock()
            .unwrap()
            .owned_sessions(CALLER)
            .into_iter()
            .next()
            .expect("created session attached to lease");

        // 2. SessionResume：envelope session_id = 目标会话，header 仍是租约 id。
        let resume = RingingV2CommandEnvelope::new(
            "cmd-resume-regression",
            "ci-entry",
            qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::SessionResume {
                session_id: created_seed.clone(),
            }),
        )
        .with_client_session_id(CALLER)
        .with_session_id(&created_seed);
        let (status, ack) = execute_command(&state, &caller_headers(), &axum_impl::Identity::Admin, resume).await;
        assert_eq!(status, StatusCode::OK, "resume ack: {:?}", ack.message);
        assert_eq!(ack.status, RingingCommandAckStatus::Accepted);

        // 3. 不变量：租约 id 不得被当成会话物化/激活。
        let caller_dir = qaqh_types::platform::sessions_dir().join(CALLER);
        assert!(
            !caller_dir.exists(),
            "租约 id 被当成会话 id 物化了目录：{}",
            caller_dir.display()
        );
        assert!(
            qaqh_session::SessionManager::global().load_meta(CALLER).is_none(),
            "租约 id 不得解析出会话 meta"
        );
        let active = qaqh_session::SessionManager::global().active_session();
        assert_ne!(
            active.as_deref(),
            Some(CALLER),
            ".active_session 被写成租约 id（resume 传参错位）"
        );

        // 4. 清理：close（join in-process worker）+ delete（删会话目录）。
        let service = state.service.clone();
        let seed = created_seed.clone();
        let cleanup = tokio::task::spawn_blocking(move || {
            let _ = service.close_session(&seed, None);
            service.delete_session(&seed, None)
        })
        .await
        .unwrap_or_else(|e| Err(format!("cleanup join error: {e}")));
        if let Err(error) = cleanup {
            panic!("cleanup failed for {created_seed} (leaked session dir): {error}");
        }
    }
}
