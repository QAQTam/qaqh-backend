//! axum 迁移 P1.5：主干直切，已去 feature-gated。
//! P0 已完成 health；P1 补齐无状态 REST + 中间件/限流骨架；P1.5 起为默认 HTTP 栈。

mod axum_impl;

#[cfg(test)]
pub(crate) use axum_impl::test_hooks::SseTerminateScope;
pub(crate) use axum_impl::test_hooks::TestHooks;
pub use axum_impl::{AppState, build_router};

#[cfg(test)]
/// 进程级 SessionManager 初始化守卫。
///
/// `SessionManager::init` 内部是 `OnceLock::set().expect(...)`——同一测试
/// 二进制的多个用例共享进程，谁先谁后不确定，裸调 `init` 必然 `already
/// initialized` panic。所有测试模块统一走这里，只初始化一次。
#[cfg(test)]
fn init_session_manager() {
    static INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INIT.get_or_init(|| qaqh_session::SessionManager::init(qaqh_types::platform::data_dir()));
}

#[cfg(test)]
mod sse_tests {
    //! SSE 终止帧路径回归（BUG-2026-09-12-11 遗留 / issue #35）。
    //!
    //! 覆盖真实 handler 路径（`build_router` →
    //! `/ringing/v1/events/{channel}` 与
    //! `/ringing/v1/sessions/{seed}/timeline/events`）：
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
            g.attach_seed(SESSION, SEED);
        }
        let pending = std::sync::Arc::new(std::sync::Mutex::new(
            qaqh_runtime::ringing::PendingCommandStore::new(),
        ));
        let (shutdown, _) = tokio::sync::watch::channel(false);
        // 与 `axum_tests::test_state` 同源：SessionManager 是进程级单例
        // （`init` 用 `OnceLock::set`，重复调用会 panic）——测试二进制的多个
        // 用例共享同一进程，这里用与 `axum_tests::test_state` 相同的
        // `OnceLock::get_or_init` 形态保证只初始化一次。
        super::init_session_manager();
        AppState {
            hub,
            leases,
            pending,
            service: qaqh_runtime::QaqhService::init(qaqh_session::SessionManager::global())
                .clone(),
            token: TOKEN.into(),
            epoch: "lag-epoch".into(),
            shutdown,
            test_hooks: std::sync::Arc::new(TestHooks::disabled()),
        }
    }

    /// 打开一条真实 channel SSE：返回 (状态, 事件流)。
    async fn open_channel_sse(
        app: Router,
    ) -> (
        StatusCode,
        std::pin::Pin<
            Box<dyn futures_util::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Send>,
        >,
    ) {
        let req = Request::builder()
            .uri("/ringing/v1/events/conversation")
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("x-qaqh-client-session-id", SESSION)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let stream = axum::body::Body::into_data_stream(resp.into_body());
        (status, Box::pin(stream))
    }

    /// 从 SSE 字节流里取下一个 `event:`/`data:` 帧（超时返回 `None`）。
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
        let req = Request::builder()
            .uri(format!("/ringing/v1/sessions/{SEED}/timeline/events"))
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("x-qaqh-client-session-id", SESSION)
            .body(Body::empty())
            .unwrap();
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

    /// ① channel 流：慢消费者 → `ringing.stream_terminated` → 关流。
    ///
    /// 装置（确定性，不靠"真的慢"）：tokio broadcast 的语义是——
    ///  - 在 Sender **有积压历史之后**才 subscribe 的 receiver，看不到历史
    ///    （本测试与 tokio 实测一致：late subscriber 得到 `Empty`）；
    ///  - 而在灌满之前就存在的 receiver，若一直不排空，其**下一次** `recv()`
    ///    必然 `Lagged(capacity 之外的条数)`（环零保留）。
    ///
    /// 因此这里必须让 receiver **先于**溢出存在：handler 的
    /// `hub.subscribe(channel)` 只发生在请求进来之后，所以顺序是
    /// ① 起 handler 建立订阅 → ② 用 `channel_live_sender` 直接灌
    /// `capacity + 1` 条，把 handler 那个 receiver 挤出环 → ③ handler 的
    /// `recv()` 返回 `Lagged`，走终止帧分支。
    #[tokio::test]
    async fn channel_stream_lagged_sends_termination_frame_then_closes() {
        let hub = std::sync::Arc::new(qaqh_runtime::RingingHub::new("lag-epoch"));
        let state = test_state_with_hub(hub.clone());
        let app = build_router(state);
        let (status, mut stream) = open_channel_sse(app).await;
        assert_eq!(status, StatusCode::OK);

        // handler 的 receiver 已存在（`subscribe()` 在进入 SSE 流前完成）。
        let capacity =
            qaqh_runtime::RingingHub::live_capacity(qaqh_domain::RingingChannel::Conversation);
        hub.overflow_channel_live(
            qaqh_domain::RingingChannel::Conversation,
            SEED,
            1,
            capacity as u64 + 1,
        );

        let (event, data) = next_sse_frame(&mut stream, Duration::from_secs(5))
            .await
            .expect("termination frame must arrive");
        assert_eq!(event, "ringing.stream_terminated");
        let v: serde_json::Value = serde_json::from_str(&data).expect("valid json payload");
        assert_eq!(v["code"], "lagged");
        assert_eq!(v["channel"], "conversation");
        assert!(
            v["skipped"].as_u64().unwrap_or(0) > 0,
            "payload must carry the skipped count: {data}"
        );

        // 终止帧是**最后一帧**：发完即关流，不再有任何数据帧。
        let tail = next_sse_frame(&mut stream, Duration::from_millis(300)).await;
        assert!(
            tail.is_none(),
            "no frame may follow the termination frame: {tail:?}"
        );
    }

    #[tokio::test]
    async fn injected_stream_termination_has_stable_wire_shape_and_is_one_shot() {
        let hub = std::sync::Arc::new(qaqh_runtime::RingingHub::new("lag-epoch"));
        let mut state = test_state_with_hub(hub.clone());
        state.test_hooks = std::sync::Arc::new(TestHooks::for_test_sse_terminate(
            "lagged",
            SseTerminateScope::Channel,
            Some(qaqh_domain::RingingChannel::Conversation),
        ));

        let (status, mut first) = open_channel_sse(build_router(state)).await;
        assert_eq!(status, StatusCode::OK);
        let (event, data) = next_sse_frame(&mut first, Duration::from_secs(5))
            .await
            .expect("injected termination frame must arrive");
        assert_eq!(event, "ringing.stream_terminated");
        let value: serde_json::Value = serde_json::from_str(&data).expect("valid json payload");
        assert_eq!(value["code"], "lagged");
        assert_eq!(value["channel"], "conversation");
        assert_eq!(value["skipped"], 7);
        assert!(
            next_sse_frame(&mut first, Duration::from_millis(300))
                .await
                .is_none(),
            "injected termination must close the stream"
        );

        let state = test_state_with_hub(hub.clone());
        let (_, mut second) = open_channel_sse(build_router(state)).await;
        hub.publish(
            SEED,
            qaqh_domain::DomainEvent::Conversation(
                qaqh_domain::ConversationEvent::ConversationCancelled {
                    turn_id: Some("t-after-injection".into()),
                },
            ),
        );
        let (event, data) = next_sse_frame(&mut second, Duration::from_secs(5))
            .await
            .expect("second stream must receive normal live traffic");
        assert_ne!(event, "ringing.stream_terminated", "{data}");
        assert_eq!(event, "conversation_cancelled");
    }

    /// ② 终止后新订阅仍能正常收流（重连重定基不被破坏）。
    ///
    /// 客户端收到终止帧后的既定动作是**带 Last-Event-ID 重连**（新 SSE，
    /// 新 receiver）。这里断言终止帧没有把 hub 打坏：终止之后起的第二条
    /// SSE 依然能收到 live 事件。
    #[tokio::test]
    async fn a_new_subscription_still_receives_after_termination() {
        let hub = std::sync::Arc::new(qaqh_runtime::RingingHub::new("lag-epoch"));
        let capacity =
            qaqh_runtime::RingingHub::live_capacity(qaqh_domain::RingingChannel::Conversation);

        // 第一条流：被挤爆 → 终止帧。
        let state = test_state_with_hub(hub.clone());
        let (_, mut first) = open_channel_sse(build_router(state)).await;
        hub.overflow_channel_live(
            qaqh_domain::RingingChannel::Conversation,
            SEED,
            1,
            capacity as u64 + 1,
        );
        let (event, _) = next_sse_frame(&mut first, Duration::from_secs(5))
            .await
            .expect("first stream must terminate");
        assert_eq!(event, "ringing.stream_terminated");

        // 第二条流：新订阅，必须还能收到 live 事件。
        let state = test_state_with_hub(hub.clone());
        let (_, mut second) = open_channel_sse(build_router(state)).await;
        hub.overflow_channel_live(qaqh_domain::RingingChannel::Conversation, SEED, 10_000, 1);
        let (event, data) = next_sse_frame(&mut second, Duration::from_secs(5))
            .await
            .expect("a fresh subscription must still receive live events");
        assert_ne!(
            event, "ringing.stream_terminated",
            "a fresh stream must not be terminated by the previous overflow: {data}"
        );
        assert_eq!(event, "conversation_cancelled");
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
        assert_eq!(v["seed"], SEED);
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
    async fn injected_timeline_gap_emits_cursor_plus_two_once() {
        let hub = std::sync::Arc::new(qaqh_runtime::RingingHub::new("lag-epoch"));
        let mut state = test_state_with_hub(hub.clone());
        state.test_hooks = std::sync::Arc::new(TestHooks::for_test_timeline_gap());
        let (status, mut stream) = open_timeline_sse(build_router(state)).await;
        assert_eq!(status, StatusCode::OK);

        for turn in ["t-gap-1", "t-gap-2"] {
            hub.publish_timeline(
                SEED,
                qaqh_domain::TimelineIntent::TurnOpened {
                    turn_id: turn.into(),
                    user_text: "force a timeline gap".into(),
                },
            )
            .expect("publish timeline intent");
        }

        let (event, data) = next_sse_frame(&mut stream, Duration::from_secs(5))
            .await
            .expect("gap frame must arrive");
        assert_eq!(event, "timeline.entry");
        let value: serde_json::Value = serde_json::from_str(&data).expect("valid timeline frame");
        assert_eq!(value["entry"]["timeline_seq"], 2);
        assert!(
            next_sse_frame(&mut stream, Duration::from_millis(300))
                .await
                .is_none(),
            "gap injection must close the stream after forcing re-baseline"
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
    fn test_state() -> AppState {
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
            leases,
            pending,
            service,
            token: String::from("test-token"),
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

    #[tokio::test]
    async fn injected_session_404_short_circuits_timeline_snapshot() {
        let mut state = test_state();
        state.test_hooks = std::sync::Arc::new(TestHooks::for_test_session_404("missing"));
        let app = build_router(state);
        let req = Request::builder()
            .uri("/ringing/v1/sessions/missing/timeline")
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
            .uri("/ringing/v1/clients/open")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"schema":"qaqh.Ringing","version":1,"client_instance_id":"ci","capabilities":[]}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn open_success() {
        let app = build_router(test_state());
        let body = serde_json::json!({
            "schema":"qaqh.Ringing","version":1,"client_instance_id":"ci-1"
        });
        let req = Request::builder()
            .method("POST")
            .uri("/ringing/v1/clients/open")
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-token")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn events_requires_auth() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/ringing/v1/events/tool")
            .header("x-qaqh-client-session-id", "cs-1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn events_requires_lease() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/ringing/v1/events/tool")
            .header("authorization", "Bearer test-token")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn events_unknown_channel() {
        let state = test_state();
        state
            .leases
            .lock()
            .unwrap()
            .open("cs-1".into(), "ci-1".into());
        let app = build_router(state);
        let req = Request::builder()
            .uri("/ringing/v1/events/bogus")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", "cs-1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn events_success() {
        let state = test_state();
        state
            .leases
            .lock()
            .unwrap()
            .open("cs-1".into(), "ci-1".into());
        state.leases.lock().unwrap().attach_seed("cs-1", "seed-1");
        // publish an event for replay check (but new connection without cursor skips replay per design)
        let _ = state.hub.publish(
            "seed-1",
            qaqh_domain::DomainEvent::Tool(qaqh_domain::ToolEvent::ToolStarted {
                tool_call_id: "c1".into(),
                turn_id: "t1".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        let app = build_router(state);
        let req = Request::builder()
            .uri("/ringing/v1/events/tool")
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
        assert_eq!(resp.headers().get("cache-control").unwrap(), "no-cache");
    }

    #[tokio::test]
    async fn timeline_events_success() {
        let state = test_state();
        state
            .leases
            .lock()
            .unwrap()
            .open("cs-1".into(), "ci-1".into());
        state.leases.lock().unwrap().attach_seed("cs-1", "seed-1");
        let app = build_router(state);
        let req = Request::builder()
            .uri("/ringing/v1/sessions/seed-1/timeline/events")
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
    /// timeline/频道读取；空 seed 被拒（missing_seed）。
    #[tokio::test]
    async fn session_attach_grants_seed_ownership_without_actor_side_effects() {
        let state = test_state();
        state
            .leases
            .lock()
            .unwrap()
            .open("cs-1".into(), "ci-1".into());
        let app = build_router(state.clone());
        let env = qaqh_ringing::RingingCommandEnvelope::new(
            "cmd-attach-1",
            "ci-1",
            qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::SessionAttach {
                seed: "sub-seed-1".into(),
            }),
        )
        .with_client_session_id("cs-1")
        .with_seed("sub-seed-1");
        let req = Request::builder()
            .method("POST")
            .uri("/ringing/v1/commands/control")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", "cs-1")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&env).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(state.leases.lock().unwrap().owns_seed("cs-1", "sub-seed-1"));

        // 空 seed → Rejected missing_seed，且不产生任何归属。
        let app = build_router(state.clone());
        let env_bad = qaqh_ringing::RingingCommandEnvelope::new(
            "cmd-attach-2",
            "ci-1",
            qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::SessionAttach {
                seed: String::new(),
            }),
        )
        .with_client_session_id("cs-1");
        let req = Request::builder()
            .method("POST")
            .uri("/ringing/v1/commands/control")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", "cs-1")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&env_bad).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(!state.leases.lock().unwrap().owns_seed("cs-1", ""));
    }

    /// todo CLI 路线：WRITE_SEEDED 调用在 lease 未 attach seed 时必须 401
    /// （拒绝发生在 dispatch 之前，不触碰磁盘）。
    #[tokio::test]
    async fn todo_set_requires_seed_ownership() {
        let state = test_state();
        state
            .leases
            .lock()
            .unwrap()
            .open("cs-1".into(), "ci-1".into());
        let app = build_router(state);
        let req = Request::builder()
            .method("POST")
            .uri("/ringing/v1/service/todo.set")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", "cs-1")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"seed": "seed-1", "id": "T1", "status": "completed"})
                    .to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// open 握手声明 attach_seed 后，同 seed 的 READ_SEEDED 调用放行
    /// （todo.list 只读，读不到即空表，不落盘）。
    #[tokio::test]
    async fn todo_list_allowed_after_open_attached_seed() {
        let state = test_state();
        // 经 handle_open 真实路径建 lease 并 attach（覆盖 flatten 解析分支）
        let app = build_router(state);
        let open_req = Request::builder()
            .method("POST")
            .uri("/ringing/v1/clients/open")
            .header("authorization", "Bearer test-token")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "schema": qaqh_ringing::protocol::RINGING_SCHEMA,
                    "version": qaqh_ringing::protocol::RINGING_VERSION,
                    "client_instance_id": "ci-cli",
                    "attach_seed": "seed-1",
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
        let session_id = open["client_session_id"].as_str().expect("session id");
        let req = Request::builder()
            .method("POST")
            .uri("/ringing/v1/service/todo.list")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", session_id)
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"seed": "seed-1"}).to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn timeline_events_requires_seed_ownership() {
        let state = test_state();
        state
            .leases
            .lock()
            .unwrap()
            .open("cs-1".into(), "ci-1".into());
        // not attached
        let app = build_router(state);
        let req = Request::builder()
            .uri("/ringing/v1/sessions/seed-1/timeline/events")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", "cs-1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// 普通 daemon Router 不挂载浏览器控制面；WebUI 只能走独立 `webui` 网关。
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
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "{path} must not be mounted on the daemon"
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
        state.leases.lock().unwrap().attach_seed("cs-1", "seed-1");
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
            .uri("/ringing/v1/sessions/seed-1/timeline?limit=0")
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
}
