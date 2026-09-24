//! axum 迁移 P1.5：主干直切，已去 feature-gated。
//! P0 已完成 health；P1 补齐无状态 REST + 中间件/限流骨架；P1.5 起为默认 HTTP 栈。

mod axum_impl;

pub(crate) use axum_impl::reclaim_dead_driver_seats;
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
            .uri(format!("/ringing/v1/sessions/{SEED}/timeline/events"))
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

    /// `Scope::Any` 同时匹配 channel 与 timeline 两条流，但 token 仍然只能被消费
    /// 一次：第一条流拿到终止帧后，第二条流必须收到正常数据。
    #[tokio::test]
    async fn injected_stream_termination_any_scope_is_one_shot_across_stream_kinds() {
        let hub = std::sync::Arc::new(qaqh_runtime::RingingHub::new("lag-epoch"));
        let mut state = test_state_with_hub(hub.clone());
        state.test_hooks = std::sync::Arc::new(TestHooks::for_test_sse_terminate(
            "lagged",
            SseTerminateScope::Any,
            None,
        ));
        let app = build_router(state);

        let (status, mut channel_stream) = open_channel_sse(app.clone()).await;
        assert_eq!(status, StatusCode::OK);
        let (event, _) = next_sse_frame(&mut channel_stream, Duration::from_secs(5))
            .await
            .expect("channel stream must get the injected termination frame");
        assert_eq!(event, "ringing.stream_terminated");

        let (status, mut timeline_stream) = open_timeline_sse(app).await;
        assert_eq!(status, StatusCode::OK);
        hub.publish_timeline(
            SEED,
            qaqh_domain::TimelineIntent::TurnOpened {
                turn_id: "t-any-scope".into(),
                user_text: "must not be terminated".into(),
            },
        )
        .expect("publish timeline intent");
        let (event, data) = next_sse_frame(&mut timeline_stream, Duration::from_secs(5))
            .await
            .expect("timeline stream must still receive normal traffic");
        assert_ne!(
            event, "ringing.stream_terminated",
            "the one-shot token must not be consumable once per stream kind: {data}"
        );
        assert_eq!(event, "timeline.entry");
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
    async fn injected_timeline_gap_does_not_leak_foreign_seed_entries() {
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
        let seed = dir
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
            .uri(format!("/ringing/v2/sessions/{seed}/bootstrap"))
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
        assert_eq!(value["seed"], seed);
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
                "/ringing/v2/sessions/{seed}/events/control?since_cursor={snapshot_cursor}"
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
                seed: "seed-1".into(),
            }),
        )
        .with_client_session_id(open.client_session_id.clone())
        .with_seed("seed-1");
        let fingerprint = crate::axum_server::axum_impl::command_fingerprint(
            envelope.channel,
            envelope.seed.as_deref(),
            envelope.expected_revision,
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
                seed: "seed-1".into(),
            }),
        )
        .with_client_session_id(open.client_session_id.clone())
        .with_seed("seed-1");
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
        let seed = dir
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
        .with_seed(seed);
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
        seed: &str,
        action: &str,
        session_id: &str,
    ) -> serde_json::Value {
        let request = Request::builder()
            .method("POST")
            .uri(format!("/ringing/v2/sessions/{seed}/driver/{action}"))
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", session_id)
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
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
    fn seed_canonical_driver_session(
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
        let seed = dir
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
        (dir, seed)
    }

    #[tokio::test]
    async fn v2_driver_gate_rejects_non_driver_and_stale_epoch() {
        use qaqh_domain::ConversationCommand;
        use qaqh_ringing::{RingingCommand, RingingV2CommandEnvelope};

        let app = build_router(test_state());
        let a = open_v2_session(app.clone(), "ci-a").await;
        let b = open_v2_session(app.clone(), "ci-b").await;
        // Canonical seat held by `a`, whose lease is live.
        let (_dir, seed) = seed_canonical_driver_session(Some(&a.client_session_id), 1);

        let gated = |command_id: &str, session_id: &str| {
            RingingV2CommandEnvelope::new(
                command_id,
                "ci-v2",
                RingingCommand::Conversation(ConversationCommand::ConversationCancel {
                    turn_id: None,
                }),
            )
            .with_client_session_id(session_id)
            .with_seed(seed.clone())
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
    async fn v2_driver_busy_claim_is_rejected_before_dispatch() {
        let app = build_router(test_state());
        let a = open_v2_session(app.clone(), "ci-a").await;
        let b = open_v2_session(app.clone(), "ci-b").await;
        let (_dir, seed) = seed_canonical_driver_session(Some(&a.client_session_id), 1);

        let busy = post_driver(app.clone(), &seed, "claim", &b.client_session_id).await;
        assert_eq!(busy["accepted"], false);
        assert_eq!(busy["reason"], "driver_busy");
        assert_eq!(busy["holder"], a.client_session_id);
        assert_eq!(busy["driver_epoch"], 1);

        let already = post_driver(app.clone(), &seed, "claim", &a.client_session_id).await;
        assert_eq!(already["accepted"], true);
        assert_eq!(already["reason"], "already_holder");
        assert_eq!(already["driver_epoch"], 1);

        let not_driver = post_driver(app, &seed, "release", &b.client_session_id).await;
        assert_eq!(not_driver["accepted"], false);
        assert_eq!(not_driver["reason"], "not_driver");
    }

    #[tokio::test]
    async fn v2_bootstrap_reports_canonical_driver_state() {
        let app = build_router(test_state());
        let a = open_v2_session(app.clone(), "ci-a").await;
        let b = open_v2_session(app.clone(), "ci-b").await;
        let (_dir, seed) = seed_canonical_driver_session(Some(&a.client_session_id), 3);

        let bootstrap = |session_id: String| {
            let app = app.clone();
            let seed = seed.clone();
            async move {
                let request = Request::builder()
                    .uri(format!("/ringing/v2/sessions/{seed}/bootstrap"))
                    .header("authorization", "Bearer test-token")
                    .header("x-qaqh-client-session-id", session_id)
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
            (InteractionKind::Plan, "plan_review"),
        ] {
            let sessions_dir = qaqh_types::platform::sessions_dir();
            std::fs::create_dir_all(&sessions_dir).unwrap();
            let dir = tempfile::tempdir_in(&sessions_dir).unwrap();
            let seed = dir
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

            let bootstrap = |app: axum::Router, seed: String, session_id: String| async move {
                let request = Request::builder()
                    .uri(format!("/ringing/v2/sessions/{seed}/bootstrap"))
                    .header("authorization", "Bearer test-token")
                    .header("x-qaqh-client-session-id", session_id)
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
            let first =
                bootstrap(app.clone(), seed.clone(), client.client_session_id.clone()).await;
            let second = bootstrap(app.clone(), seed, client.client_session_id.clone()).await;
            assert_eq!(first, second, "{kind:?} pending set must be stable");
            assert_eq!(
                first.len(),
                1,
                "{kind:?} must expose one pending interaction"
            );
            assert_eq!(first[0]["interaction_id"], interaction_id.as_str());
            assert_eq!(first[0]["kind"], expected_kind);
        }
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
