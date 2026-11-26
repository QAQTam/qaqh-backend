//! 新回合循环测试:Stop 回合端到端(输入 → 流式 → seal → 终态),
//! 全部状态/事件经 SessionOwner 的 SQLite 业务表与单流广播。

// 文件级（mod 外）测试的导入：ADR-0033 workspace 重解析用例在 mod 外直挂。
use std::sync::Arc;

use qaqh_session::owner::OwnerDirectory;

use crate::provider_job::ProviderRequestSpec;
use crate::session_loop::SessionLoop;
use crate::tool_job::{ToolPolicySnapshot, WorkspaceToolPort};

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use qaqh_ringing_contract::events::{SessionEvent, SessionEventFrame};
    use qaqh_ringing_contract::fact::InputPurpose;
    use qaqh_ringing_contract::state::TimelineEntry;
    use qaqh_session::db::store;
    use qaqh_session::owner::OwnerDirectory;

    use crate::provider_job::ProviderRequestSpec;
    use crate::session_loop::SessionLoop;
    use crate::tool_job::{ToolPolicySnapshot, WorkspaceToolPort};
    use qaqh_ringing::ConversationSendMessage;

    const CHAT_SSE: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hello from loop\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n",
        "data: [DONE]\n\n",
    );

    /// 图片用例专用串行锁：QAQH_DATA_DIR 是进程级环境，测试互斥期间不得
    /// 有其他图片用例并发改写。自旋获取（等待方不阻塞持锁方的推进）。
    static IMAGE_TEST_SERIAL: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    struct ImageTestSerialGuard;

    impl ImageTestSerialGuard {
        fn acquire() -> Self {
            while IMAGE_TEST_SERIAL.swap(true, std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Self
        }
    }

    impl Drop for ImageTestSerialGuard {
        fn drop(&mut self) {
            IMAGE_TEST_SERIAL.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn sse_server() -> (tiny_http::Server, String) {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
        let addr = format!("http://{}", server.server_addr());
        (server, addr)
    }

    fn spawn_sse_response(server: tiny_http::Server, body: String) {
        std::thread::spawn(move || {
            let header =
                tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..])
                    .expect("header");
            for request in server.incoming_requests() {
                let _ = request.respond(
                    tiny_http::Response::from_string(body.clone()).with_header(header.clone()),
                );
            }
        });
    }

    /// 逐请求回放脚本并捕获请求体：第 i 个请求回放 bodies[i]（越界回放最后
    /// 一份），请求体推入 captured 供断言。
    fn spawn_sse_script(
        server: tiny_http::Server,
        bodies: Vec<String>,
        captured: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        std::thread::spawn(move || {
            let header =
                tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..])
                    .expect("header");
            let mut served = 0usize;
            for mut request in server.incoming_requests() {
                let mut body = String::new();
                let _ = std::io::Read::read_to_string(request.as_reader(), &mut body);
                captured
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(body);
                let index = served.min(bodies.len().saturating_sub(1));
                let reply = bodies.get(index).cloned().unwrap_or_default();
                served += 1;
                let _ = request
                    .respond(tiny_http::Response::from_string(reply).with_header(header.clone()));
            }
        });
    }

    /// 最小合法 PNG（1×1，68 字节 → base64 92 字符，System.Drawing 验证可解码）。
    const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";

    fn tiny_png() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00,
            0x00, 0xB5, 0x1C, 0x0C, 0x02, 0x00, 0x00, 0x00, 0x0B, 0x49, 0x44, 0x41, 0x54, 0x78,
            0xDA, 0x63, 0x64, 0x60, 0x00, 0x00, 0x00, 0x06, 0x00, 0x02, 0x30, 0x81, 0xD0, 0x2F,
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ]
    }

    async fn test_loop(
        dir: &std::path::Path,
    ) -> (
        Arc<SessionLoop>,
        Arc<OwnerDirectory>,
        tokio::sync::broadcast::Receiver<SessionEventFrame>,
    ) {
        test_loop_with_response(dir, CHAT_SSE.to_string()).await
    }

    async fn test_loop_with_response(
        dir: &std::path::Path,
        response: String,
    ) -> (
        Arc<SessionLoop>,
        Arc<OwnerDirectory>,
        tokio::sync::broadcast::Receiver<SessionEventFrame>,
    ) {
        let owners = Arc::new(OwnerDirectory::open(dir.join("qaqh.db")).expect("open owners db"));
        let owner = owners
            .create_session(store::NewSession {
                session_id: "SES-LOOP".into(),
                parent_session_id: None,
                title: "loop".into(),
                title_source: Some("user".into()),
                cwd: None,
                model: None,
                tool_mode: None,
                model_selection: None,
                custom_tools: None,
                frozen_annotation: None,
                ephemeral: false,
            })
            .await
            .expect("create session");
        let rx = owner.subscribe();
        let manager = Arc::new(std::sync::Mutex::new(
            qaqh_workspace::registration::build_tool_manager(
                &[],
                std::sync::Arc::new(qaqh_workspace::todo::MemoryTodoHost::default()),
            ),
        ));
        let port = Arc::new(WorkspaceToolPort::new(
            "SES-LOOP",
            manager,
            ToolPolicySnapshot::workspace_write(dir.join("ws")),
            owner.lifecycle(),
            Arc::new(qaqh_process_tools::exec_session::ExecSessionManager::new()),
        ));
        let (_, base_url) = {
            let (server, addr) = sse_server();
            spawn_sse_response(server, response);
            ((), addr)
        };
        let mut provider = qaqh_gate::ProviderConfig::chat(
            &base_url,
            "sk-test",
            "test-model",
            None,
            None,
            qaqh_types::ThinkingParamMode::OpenAi,
            qaqh_types::CacheTokenField::PromptCacheHitTokens,
            true,
            None,
        );
        // 测试要确定性结算：空流重试（ADR-0018）默认 5 次退避会超出轮询窗。
        provider.retry = Some(qaqh_types::RetrySpec {
            max_retries: 1,
            ..Default::default()
        });
        let spec = ProviderRequestSpec::new(provider, 1024);
        let loop_ = SessionLoop::new("SES-LOOP", owner, port, spec, dir.to_path_buf());
        (loop_, owners, rx)
    }

    async fn recv_event(
        rx: &mut tokio::sync::broadcast::Receiver<SessionEventFrame>,
    ) -> SessionEvent {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Ok(frame)) => frame.event,
            Ok(Err(error)) => panic!("event channel: {error}"),
            Err(_) => panic!("event not delivered in time"),
        }
    }

    #[tokio::test]
    async fn loop_runs_a_stop_turn_end_to_end_through_owner() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (loop_, owners, mut rx) = test_loop(dir.path()).await;

        loop_
            .send_input(ConversationSendMessage {
                message_id: Some("IN1".into()),
                text: "打个招呼".into(),
                input_purpose: InputPurpose::TriggerTurn,
                as_system: false,
                attachments: None,
                images: vec![],
                inter_agent: None,
                subagent_terminal: None,
                widget_context: None,
            })
            .expect("send input");

        // 事件流:TurnStarted → TurnPhaseChanged → ActivityChanged → TimelineAppended(input)
        // → MessageDelta(流式)→ TimelineAppended(assistant seal)
        // → TurnEnded → ActivityChanged。
        assert!(matches!(
            recv_event(&mut rx).await,
            SessionEvent::TurnStarted { .. }
        ));
        assert!(matches!(
            recv_event(&mut rx).await,
            SessionEvent::TurnPhaseChanged {
                phase: qaqh_ringing_contract::state::TurnPhase::Running,
                ..
            }
        ));
        assert!(matches!(
            recv_event(&mut rx).await,
            SessionEvent::ActivityChanged { .. }
        ));
        assert!(matches!(
            recv_event(&mut rx).await,
            SessionEvent::TimelineAppended { .. }
        ));
        let mut saw_delta = false;
        loop {
            match recv_event(&mut rx).await {
                SessionEvent::MessageDelta { text, .. } => {
                    assert_eq!(text, "hello from loop");
                    saw_delta = true;
                }
                SessionEvent::TimelineAppended { .. } => break,
                other => panic!("unexpected event {other:?}"),
            }
        }
        assert!(saw_delta, "流式增量必须先于 seal");
        match recv_event(&mut rx).await {
            SessionEvent::TurnEnded { outcome, .. } => {
                assert!(matches!(
                    outcome,
                    qaqh_ringing_contract::state::TurnOutcome::Finished { .. }
                ));
            }
            other => panic!("expected turn ended, got {other:?}"),
        }

        // 回合收尾后快照:turn finished、timeline 齐、无工具。
        let owner = owners.get_or_create("SES-LOOP");
        let state = owner.snapshot().await.expect("snapshot");
        assert_eq!(state.turns.len(), 1);
        assert!(state.turns[0].outcome.is_some());
        assert_eq!(state.timeline.len(), 2);
        assert!(matches!(&state.timeline[0], TimelineEntry::Input { .. }));
        assert!(matches!(
            &state.timeline[1],
            TimelineEntry::AssistantBlock { .. }
        ));
        assert!(state.tools.is_empty());
        assert_eq!(
            state.activity,
            qaqh_ringing_contract::fact::ActivityState::Idle
        );
        let _ = owners;
    }

    /// 回归 2026-11-17-01：read_image 的图片必须真正到达 provider 请求与
    /// 时间线重放。旧实现里图片止步于 tool_job 边界（execute 只回传文本），
    /// 模型永远收不到像素——zhipuai 计费侧图片 token 恒为 0。
    #[tokio::test]
    async fn tool_image_reaches_provider_request_and_replay() {
        let _serial = ImageTestSerialGuard::acquire();
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).expect("mkdir data dir");
        // SAFETY: 测试独占进程环境；串行锁内成对设置/恢复。
        unsafe { std::env::set_var("QAQH_DATA_DIR", &data_dir) };

        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).expect("mkdir ws");
        let png = tiny_png();
        std::fs::write(ws.join("img.png"), &png).expect("write png");

        let tool_call_sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_img\",",
            "\"type\":\"function\",\"function\":{\"name\":\"read_image\",\"arguments\":\"{\\\"path\\\":\\\"img.png\\\"}\"}}]},",
            "\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n",
            "data: [DONE]\n\n",
        )
        .to_string();
        let (server, base_url) = sse_server();
        let captured: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        spawn_sse_script(
            server,
            vec![tool_call_sse, CHAT_SSE.to_string()],
            std::sync::Arc::clone(&captured),
        );

        let owners =
            Arc::new(OwnerDirectory::open(dir.path().join("qaqh.db")).expect("open owners db"));
        let owner = owners
            .create_session(store::NewSession {
                session_id: "SES-LOOP".into(),
                parent_session_id: None,
                title: "loop".into(),
                title_source: Some("user".into()),
                cwd: None,
                model: None,
                tool_mode: None,
                model_selection: None,
                custom_tools: None,
                frozen_annotation: None,
                ephemeral: false,
            })
            .await
            .expect("create session");
        let mut rx = owner.subscribe();
        let manager = Arc::new(std::sync::Mutex::new(
            qaqh_workspace::registration::build_tool_manager(
                &[],
                std::sync::Arc::new(qaqh_workspace::todo::MemoryTodoHost::default()),
            ),
        ));
        let port = Arc::new(WorkspaceToolPort::new(
            "SES-LOOP",
            manager,
            ToolPolicySnapshot::workspace_write(dir.path().join("ws")),
            owner.lifecycle(),
            Arc::new(qaqh_process_tools::exec_session::ExecSessionManager::new()),
        ));
        let mut provider = qaqh_gate::ProviderConfig::chat(
            &base_url,
            "sk-test",
            "test-model",
            None,
            None,
            qaqh_types::ThinkingParamMode::OpenAi,
            qaqh_types::CacheTokenField::PromptCacheHitTokens,
            true,
            None,
        );
        provider.retry = Some(qaqh_types::RetrySpec {
            max_retries: 1,
            ..Default::default()
        });
        let spec = ProviderRequestSpec::new(provider, 1024);
        let loop_ = SessionLoop::new("SES-LOOP", owner, port, spec, dir.path().to_path_buf());

        loop_
            .send_input(ConversationSendMessage {
                message_id: Some("IN-IMG".into()),
                text: "读一下这张图".into(),
                input_purpose: InputPurpose::TriggerTurn,
                as_system: false,
                attachments: None,
                images: vec![],
                inter_agent: None,
                subagent_terminal: None,
                widget_context: None,
            })
            .expect("send input");
        loop {
            match recv_event(&mut rx).await {
                SessionEvent::TurnEnded { .. } => break,
                _ => {}
            }
        }

        // 第二次请求（工具结果回传轮）必须携带图片像素。
        let captured = captured.lock().unwrap_or_else(|error| error.into_inner());
        assert!(
            captured.len() >= 2,
            "工具轮必须触发第二次请求，实际 {} 次",
            captured.len()
        );
        // 回归 2026-11-17-01 断言：第二次请求（工具结果回传轮）必须携带图片像素。
        let body = &captured[1];
        let tail: String = body
            .chars()
            .skip(body.chars().count().saturating_sub(1500))
            .collect();
        assert!(
            body.contains("iVBORw0KGgo"),
            "第二次请求必须携带 read_image 的像素（PNG base64 前缀）\n请求体（尾 1500 字）: {tail}"
        );
        drop(captured);

        // 时间线持久化外置引用；重放组装出兄弟 ImageRef 块。
        let state = owners
            .get_or_create("SES-LOOP")
            .snapshot()
            .await
            .expect("snapshot");
        let refs = state
            .timeline
            .iter()
            .find_map(|entry| match entry {
                TimelineEntry::ToolResult { images, .. } if !images.is_empty() => {
                    Some(images.clone())
                }
                _ => None,
            })
            .expect("timeline must carry tool image refs");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].bytes_len, 92);
        assert_eq!(refs[0].sha256.len(), 64);
        let messages = loop_.assemble_context().await.expect("assemble context");
        let tool_msg = messages
            .iter()
            .find(|message| message.role == "tool")
            .expect("tool message");
        assert!(matches!(
            &tool_msg.content[1],
            qaqh_types::ContentBlock::ImageRef { bytes_len, .. } if *bytes_len == 92
        ));

        // SAFETY: 与开头的 set_var 成对恢复。
        unsafe { std::env::remove_var("QAQH_DATA_DIR") };
    }

    /// 上传图片注册（read_image(image_index) 的真相源）：输入图按序登记，
    /// gate 的 [Image #N] 占位符编号与注册表索引对齐。
    #[tokio::test]
    async fn input_images_are_registered_for_read_image_index() {
        let _serial = ImageTestSerialGuard::acquire();
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).expect("mkdir data dir");
        // SAFETY: 测试独占进程环境；串行锁内成对设置/恢复。
        unsafe { std::env::set_var("QAQH_DATA_DIR", &data_dir) };

        let (loop_, _owners, mut rx) = test_loop(dir.path()).await;
        loop_
            .send_input(ConversationSendMessage {
                message_id: Some("IN-UPLOAD".into()),
                text: "看这张图".into(),
                input_purpose: InputPurpose::TriggerTurn,
                as_system: false,
                attachments: None,
                images: vec![qaqh_ringing_contract::commands::ImageBlock {
                    mime_type: "image/png".into(),
                    data: "aW1hZ2U=".into(),
                }],
                inter_agent: None,
                subagent_terminal: None,
                widget_context: None,
            })
            .expect("send input");
        loop {
            match recv_event(&mut rx).await {
                SessionEvent::TurnEnded { .. } => break,
                _ => {}
            }
        }

        let messages = loop_.assemble_context().await.expect("assemble context");
        let user = messages
            .iter()
            .find(|message| message.role == "user")
            .expect("user message");
        assert!(
            user.content.iter().any(|block| matches!(
                block,
                qaqh_types::ContentBlock::Image { mime_type, data }
                    if mime_type == "image/png" && data == "aW1hZ2U="
            )),
            "上传图必须以 Image 块进入模型上下文（gate 投影为占位符）"
        );
        assert_eq!(
            qaqh_workspace::read_image::peek_image("SES-LOOP", 0),
            Some(("image/png".to_string(), "aW1hZ2U=".to_string())),
            "read_image(image_index=0) 必须能取回上传图"
        );

        // SAFETY: 与开头的 set_var 成对恢复。
        unsafe { std::env::remove_var("QAQH_DATA_DIR") };
    }

    /// 取消契约:stale/错目标的 turn_id 不取消任何东西;无目标取消生效。
    #[tokio::test]
    async fn cancel_turn_validates_the_target_turn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (loop_, _owners, _rx) = test_loop(dir.path()).await;

        // 空闲:任何取消都不生效。
        assert!(!loop_.cancel_turn(None));
        assert!(!loop_.cancel_turn(Some("turn_stale")));

        loop_
            .send_input(ConversationSendMessage {
                message_id: Some("IN-CANCEL".into()),
                text: "长回合".into(),
                input_purpose: InputPurpose::TriggerTurn,
                as_system: false,
                attachments: None,
                images: vec![],
                inter_agent: None,
                subagent_terminal: None,
                widget_context: None,
            })
            .expect("send input");
        // 等回合出现(sse mock 立即完成,给 spawn 一点时间)。
        for _ in 0..100 {
            if loop_.active_turn().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let turn = loop_.active_turn().expect("active turn");
        assert!(!loop_.cancel_turn(Some("turn_stale")), "错目标不生效");
        assert!(loop_.cancel_turn(Some(&turn)), "匹配目标生效");
    }

    #[tokio::test]
    async fn turn_cancel_during_permission_cancels_interaction_and_tool_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tool_args = serde_json::json!({
            "cmd": "echo permission-cancel-smoke",
            "with_escalated_permissions": true,
            "justification": "test turn cancel during permission wait"
        });
        let tool_call = serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {
                    "role": "assistant",
                    "tool_calls": [{
                        "index": 0,
                        "id": "call-permission-cancel",
                        "type": "function",
                        "function": {
                            "name": "exec_command",
                            "arguments": tool_args.to_string()
                        }
                    }]
                },
                "finish_reason": null
            }]
        });
        let tool_calls_done = serde_json::json!({
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }]
        });
        let response = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            tool_call, tool_calls_done
        );
        let (loop_, owners, _rx) = test_loop_with_response(dir.path(), response).await;
        loop_
            .send_input(ConversationSendMessage {
                message_id: Some("IN-PERMISSION-CANCEL".into()),
                text: "触发权限等待".into(),
                input_purpose: InputPurpose::TriggerTurn,
                as_system: false,
                attachments: None,
                images: vec![],
                inter_agent: None,
                subagent_terminal: None,
                widget_context: None,
            })
            .expect("send input");

        let owner = owners.get_or_create("SES-LOOP");
        let mut waiting = None;
        for _ in 0..500 {
            let state = owner.snapshot().await.expect("snapshot");
            let value = serde_json::to_value(state).expect("serialize snapshot");
            if value["activity"] == "waiting_user"
                && value["interactions"]
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
            {
                waiting = Some(value);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let waiting = waiting.expect("exec permission should be waiting for the user");
        assert_eq!(waiting["interactions"][0]["kind"], "permission");
        assert_eq!(waiting["tools"][0]["state"]["kind"], "awaiting_interaction");
        let turn_id = waiting["current_turn_id"].as_str().expect("active turn id");

        assert!(loop_.cancel_turn(Some(turn_id)));
        let mut settled = None;
        let mut last_state = serde_json::Value::Null;
        for _ in 0..500 {
            let state = owner.snapshot().await.expect("snapshot");
            let value = serde_json::to_value(state).expect("serialize snapshot");
            last_state = value.clone();
            let turn_cancelled = value["turns"][0]["outcome"]["data"]["terminal"] == "cancelled";
            let call_cancelled = value["tools"][0]["state"]["outcome"]["kind"] == "cancelled"
                && value["tools"][0]["state"]["outcome"]["reason"] == "turn_cancel";
            if turn_cancelled && call_cancelled {
                settled = Some(value);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let settled = settled.unwrap_or_else(|| {
            panic!("turn and waiting tool call should settle as cancelled: {last_state}")
        });
        assert_eq!(settled["activity"], "idle");
        assert!(settled["exec_sessions"].as_array().unwrap().is_empty());
        assert_eq!(settled["tools"][0]["state"]["kind"], "finished");
    }

    /// ADR-0018：零内容且无终态帧的流 = 空流掐断——gate 整请求重试（此处
    /// 禁重试）后带真实原因失败，runtime 结算为 provider_failed（旧分类
    /// provider_incomplete 留给「有终态但不可用」的响应）。
    #[tokio::test]
    async fn provider_empty_stream_finishes_turn_as_failed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (loop_, owners, _rx) =
            test_loop_with_response(dir.path(), "data: [DONE]\n\n".to_string()).await;
        loop_
            .send_input(ConversationSendMessage {
                message_id: Some("IN-PROVIDER-FAILURE".into()),
                text: "触发 provider 失败".into(),
                input_purpose: InputPurpose::TriggerTurn,
                as_system: false,
                attachments: None,
                images: vec![],
                inter_agent: None,
                subagent_terminal: None,
                widget_context: None,
            })
            .expect("send input");

        let owner = owners.get_or_create("SES-LOOP");
        let mut failed = None;
        for _ in 0..500 {
            let state = owner.snapshot().await.expect("snapshot");
            let value = serde_json::to_value(state).expect("serialize snapshot");
            if value["turns"][0]["outcome"]["data"]["terminal"] == "failed" {
                failed = Some(value);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let failed = failed.expect("provider failure should finish the turn as failed");
        assert_eq!(failed["activity"], "idle");
        assert_eq!(failed["turns"][0]["outcome"]["kind"], "finished");
        assert_eq!(
            failed["turns"][0]["outcome"]["data"]["error"]["code"],
            "provider_failed"
        );
        assert!(
            failed["turns"][0]["outcome"]["data"]["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("upstream closed stream")),
            "失败必须携带真实原因: {}",
            failed["turns"][0]["outcome"]["data"]["error"]["message"]
        );
        assert_eq!(
            failed["turns"][0]["outcome"]["data"]["error"]["retryable"],
            false
        );
    }

    #[tokio::test]
    async fn input_metadata_and_images_reach_timeline_and_model_context() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (loop_, owners, _rx) = test_loop(dir.path()).await;
        let inter_agent = qaqh_ringing_contract::commands::InterAgentEnvelope {
            message_id: "msg-1".into(),
            root_session_id: "SES-ROOT".into(),
            author: "/root".into(),
            recipient: "/root/child".into(),
            other_recipients: vec![],
            task_id: Some("task-1".into()),
            reply_to: None,
            causation_id: None,
            delivery: qaqh_ringing_contract::fact::InterAgentDelivery::Queue,
            created_at_ms: 1,
        };
        let terminal = qaqh_ringing_contract::commands::SubagentTerminalNotification {
            child_session_id: "SES-CHILD".into(),
            parent_call_id: "call-1".into(),
            terminal: qaqh_ringing_contract::commands::SubagentTerminalKind::Completed,
        };
        loop_
            .send_input(ConversationSendMessage {
                message_id: Some("msg-1".into()),
                text: "result from child".into(),
                input_purpose: InputPurpose::TriggerTurn,
                as_system: true,
                attachments: None,
                images: vec![qaqh_ringing_contract::commands::ImageBlock {
                    mime_type: "image/png".into(),
                    data: "aW1hZ2U=".into(),
                }],
                inter_agent: Some(inter_agent.clone()),
                subagent_terminal: Some(terminal.clone()),
                widget_context: None,
            })
            .expect("send input");

        let owner = owners.get_or_create("SES-LOOP");
        for _ in 0..100 {
            if !owner
                .snapshot()
                .await
                .expect("snapshot")
                .timeline
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let state = owner.snapshot().await.expect("snapshot");
        assert!(!state.timeline.is_empty(), "input must be persisted");
        let TimelineEntry::Input {
            content,
            images,
            inter_agent: stored_inter_agent,
            subagent_terminal: stored_terminal,
            actor,
            ..
        } = &state.timeline[0]
        else {
            panic!("first timeline entry must be input");
        };
        assert_eq!(
            content,
            &qaqh_ringing_contract::fact::ContentValue::Inline {
                text: "result from child".into(),
            }
        );
        assert_eq!(images.len(), 1);
        assert_eq!(stored_inter_agent.as_ref(), Some(&inter_agent));
        assert_eq!(stored_terminal.as_ref(), Some(&terminal));
        assert_eq!(actor.kind, qaqh_ringing_contract::fact::ActorKind::System);
        assert_eq!(actor.id, "system");

        let messages = loop_.assemble_context().await.expect("assemble context");
        let system_input = messages
            .iter()
            .find(|message| message.role == "user")
            .expect("system actor 输入必须以 user 角色到达 provider 上下文");
        assert!(matches!(
            system_input.content.as_slice(),
            [
                qaqh_types::ContentBlock::Text { text },
                qaqh_types::ContentBlock::Image { mime_type, data }
            ] if text == "result from child" && mime_type == "image/png" && data == "aW1hZ2U="
        ));
    }

    /// 出站不变量（钉死）：≥2 次请求的 durable 历史只能是 append——A+B+C
    /// 每个字节逐次原样重现，任何 B'/A'（折叠、摘要、改写、重排）都不允许。
    /// 出站前拼装的消息向量与服务端实际收到的 wire 逐条相等；工具调用、
    /// 上下文窗口提醒、goal 注入这些侵入消息只追加、不改写历史前缀。
    #[tokio::test]
    async fn provider_requests_are_pure_append_with_no_tampering() {
        // 哨兵串刻意取得独一无二：一旦被压缩/摘要/加前缀尾注，contains 仍为真
        // 但 exact == 1 与「不得作为更长串子串」两条会同时抓到。
        const A: &str = "ALPHA-SENTINEL-SEQ01";
        const B: &str = "BRAVO-SENTINEL-SEQ02";
        const C: &str = "CHARLIE-SENTINEL-SEQ03";
        const TOOL: &str = "TOOLCALL-INTRUSIVE-SEQ04";
        const REMINDER: &str = "CTXWINDOW-INTRUSIVE-SEQ05";
        const GOAL: &str = "GOALINJECT-INTRUSIVE-SEQ06";

        fn text_msg(role: &str, text: &str) -> qaqh_types::Message {
            qaqh_types::Message {
                msg_id: None,
                role: role.into(),
                name: None,
                content: vec![qaqh_types::ContentBlock::Text { text: text.into() }],
            }
        }

        /// Chat SDK 的 user 投影是 `{"role":"user","content":"..."}`；
        /// assistant/tool 是 `content:[{...}]`。两种情况都取回可见文本。
        fn wire_text(message: &serde_json::Value) -> String {
            match message.get("content") {
                Some(serde_json::Value::String(text)) => text.clone(),
                Some(serde_json::Value::Array(parts)) => parts
                    .iter()
                    .filter_map(|part| part.get("text").and_then(|text| text.as_str()))
                    .collect(),
                _ => String::new(),
            }
        }

        fn collect_strings(value: &serde_json::Value, out: &mut Vec<String>) {
            match value {
                serde_json::Value::String(text) => out.push(text.clone()),
                serde_json::Value::Array(items) => {
                    for item in items {
                        collect_strings(item, out);
                    }
                }
                serde_json::Value::Object(map) => {
                    for (_, item) in map {
                        collect_strings(item, out);
                    }
                }
                _ => {}
            }
        }

        let (server, base_url) = sse_server();
        let captured: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        spawn_sse_script(
            server,
            vec![CHAT_SSE.to_string(), CHAT_SSE.to_string()],
            Arc::clone(&captured),
        );

        let mut provider = qaqh_gate::ProviderConfig::chat(
            &base_url,
            "sk-test",
            "test-model",
            None,
            None,
            qaqh_types::ThinkingParamMode::OpenAi,
            qaqh_types::CacheTokenField::PromptCacheHitTokens,
            true,
            None,
        );
        provider.retry = Some(qaqh_types::RetrySpec {
            max_retries: 1,
            ..Default::default()
        });
        let spec = ProviderRequestSpec::new(provider, 1024);
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

        // ── 出站前拼装：A+B+C，逐字节送出门 ──
        let first: Vec<qaqh_types::Message> = vec![
            text_msg("user", A),
            text_msg("user", B),
            text_msg("user", C),
        ];
        let outcome1 = crate::provider_job::run_round(
            spec.clone(),
            first.clone(),
            "SES-APPEND".into(),
            cancel_rx.clone(),
            None,
            None,
        )
        .await;
        assert!(
            outcome1.error.is_none(),
            "第一次请求必须真实出发: {:?}",
            outcome1.error
        );

        // ── 侵入消息：工具调用 + 上下文窗口提醒 + goal 注入，纯追加 ──
        let mut second = first.clone();
        second.push(qaqh_types::Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![qaqh_types::ContentBlock::ToolUse {
                id: "call-append".into(),
                name: "read_image".into(),
                input: serde_json::json!({ "path": TOOL }),
            }],
        });
        // 真实回执：网关不再补 "missing tool result"，wire 逐条等于出站前拼装。
        second.push(qaqh_types::Message {
            msg_id: None,
            role: "tool".into(),
            name: None,
            content: vec![qaqh_types::ContentBlock::ToolResult {
                tool_use_id: "call-append".into(),
                result: qaqh_types::ToolResult::ok(TOOL),
            }],
        });
        second.push(text_msg("user", REMINDER));
        second.push(text_msg("user", GOAL));

        let outcome2 = crate::provider_job::run_round(
            spec,
            second.clone(),
            "SES-APPEND".into(),
            cancel_rx,
            None,
            None,
        )
        .await;
        assert!(
            outcome2.error.is_none(),
            "第二次请求必须真实出发: {:?}",
            outcome2.error
        );

        let bodies = captured
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        assert_eq!(
            bodies.len(),
            2,
            "必须恰好两次出站请求，实际 {} 次",
            bodies.len()
        );
        let parsed: Vec<serde_json::Value> = bodies
            .iter()
            .map(|body| serde_json::from_str(body).expect("请求体必须是 JSON"))
            .collect();
        let messages: Vec<&Vec<serde_json::Value>> = parsed
            .iter()
            .map(|body| {
                body.get("messages")
                    .and_then(|value| value.as_array())
                    .expect("请求体必须含 messages 数组")
            })
            .collect();

        // 出站后 == 出站前：第一条请求 wire 逐条等于拼装的 A / B / C。
        assert_eq!(
            messages[0].len(),
            3,
            "第一次请求不应夹带其他消息: {}",
            bodies[0]
        );
        for (index, sentinel) in [A, B, C].iter().enumerate() {
            assert_eq!(messages[0][index]["role"], "user", "第 {index} 条角色被改写");
            assert_eq!(
                &wire_text(&messages[0][index]),
                sentinel,
                "第 {index} 条消息出站后被改写（A'/B'）: {}",
                bodies[0]
            );
        }

        // ≥2 次请求纯 append：第二次的前缀逐字节等于第一次整体。
        assert!(
            messages[1].len() > messages[0].len(),
            "第二次请求必须严格追加，实际长度 {} -> {}",
            messages[0].len(),
            messages[1].len()
        );
        assert_eq!(
            &messages[1][..messages[0].len()],
            messages[0].as_slice(),
            "第二次请求的前缀（A+B+C）被篡改：{}",
            bodies[1]
        );

        // 不允许任何 A' / B'：哨兵必须原样出现恰好一次，且不得作为更长串的子串。
        for (round, body) in parsed.iter().enumerate() {
            let mut strings = Vec::new();
            collect_strings(body, &mut strings);
            for sentinel in [A, B, C] {
                let exact = strings.iter().filter(|text| *text == sentinel).count();
                assert_eq!(
                    exact, 1,
                    "第 {} 次请求：{sentinel} 必须原样出现恰好一次，实际 {exact}",
                    round + 1
                );
                let mutated: Vec<&String> = strings
                    .iter()
                    .filter(|text| text.contains(sentinel) && *text != sentinel)
                    .collect();
                assert!(
                    mutated.is_empty(),
                    "第 {} 次请求：发现 {sentinel} 的篡改变体（A'/B'）: {mutated:?}",
                    round + 1
                );
            }
        }

        // 侵入消息原样追加进第二次请求。user 提醒/goal 是裸文本串；工具参数与
        // 回执会被网关合法包裹（function.arguments 的 JSON、<qaqh_tool_result>
        // 信封），只核对哨兵逐字节出现在其中，不冒充「整串相等」。
        let mut second_strings = Vec::new();
        collect_strings(&parsed[1], &mut second_strings);
        for sentinel in [REMINDER, GOAL] {
            assert_eq!(
                second_strings.iter().filter(|text| *text == sentinel).count(),
                1,
                "侵入消息 {sentinel} 必须原样追加且不被改写: {}",
                bodies[1]
            );
        }
        assert!(
            second_strings.iter().any(|text| text.contains(TOOL)),
            "工具调用/回执必须把 {TOOL} 逐字节带上: {}",
            bodies[1]
        );
    }
}

#[cfg(test)]
mod handoff_tests {
    //! 行为级测试：交接真的终止回合、参数不全会响亮报错并继续推进、
    //! 超限错误真的改写分母、硬压力真的折叠且留下可见痕迹。
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use qaqh_ringing::ConversationSendMessage;
    use qaqh_ringing_contract::fact::InputPurpose;
    use qaqh_session::db::store;
    use qaqh_session::owner::OwnerDirectory;

    use crate::handoff::{self, FOLD_NOTE};
    use crate::provider_job::ProviderRequestSpec;
    use crate::session_loop::SessionLoop;
    use crate::tool_job::{ToolPolicySnapshot, WorkspaceToolPort};

    /// 改写 `QAQH_DATA_DIR` 的用例都要拿这把锁：镜像落点是进程级环境变量，
    /// 并行跑时串行化，防止互相把镜像写进对方的 tempdir。
    static QAQH_DATA_DIR_LOCK: Mutex<()> = Mutex::new(());

    struct Harness {
        loop_: Arc<SessionLoop>,
        owners: Arc<OwnerDirectory>,
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    fn tool_call_body(
        name: &str,
        args: &serde_json::Value,
        id: &str,
        usage: &serde_json::Value,
    ) -> String {
        let call = serde_json::json!({"choices":[{"index":0,"delta":{"role":"assistant",
            "tool_calls":[{"index":0,"id":id,"type":"function",
            "function":{"name":name,"arguments":args.to_string()}}]},"finish_reason":null}]});
        let done = serde_json::json!({"choices":[{"index":0,"delta":{},
            "finish_reason":"tool_calls"}],"usage": usage});
        format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n", call, done)
    }

    fn text_body(text: &str) -> String {
        let head = serde_json::json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]});
        let done = serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
        format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n", head, done)
    }

    /// 口径样本：裸 `prompt_tokens`=60，真实总输入 = 命中 900 + 未命中 60 = 960。
    fn cache_usage() -> serde_json::Value {
        serde_json::json!({"prompt_tokens":60,"completion_tokens":5,"total_tokens":65,
            "prompt_cache_hit_tokens":900,"prompt_cache_miss_tokens":60})
    }

    async fn harness(
        dir: &std::path::Path,
        script: Vec<(u16, String)>,
        tools: Vec<qaqh_types::ToolDef>,
        window: u32,
        threshold: f64,
    ) -> Harness {
        harness_titled(
            dir,
            script,
            tools,
            window,
            threshold,
            "handoff",
            Some("user"),
        )
        .await
    }

    async fn harness_titled(
        dir: &std::path::Path,
        script: Vec<(u16, String)>,
        tools: Vec<qaqh_types::ToolDef>,
        window: u32,
        threshold: f64,
        title: &str,
        title_source: Option<&str>,
    ) -> Harness {
        let owners = Arc::new(OwnerDirectory::open(dir.join("qaqh.db")).expect("open owners db"));
        let owner = owners
            .create_session(store::NewSession {
                session_id: "SES-HANDOFF".into(),
                parent_session_id: None,
                title: title.into(),
                title_source: title_source.map(str::to_string),
                cwd: None,
                model: None,
                tool_mode: None,
                model_selection: None,
                custom_tools: None,
                frozen_annotation: None,
                ephemeral: false,
            })
            .await
            .expect("create session");
        let manager = Arc::new(Mutex::new(
            qaqh_workspace::registration::build_tool_manager(
                &[],
                std::sync::Arc::new(qaqh_workspace::todo::MemoryTodoHost::default()),
            ),
        ));
        let port = Arc::new(WorkspaceToolPort::new(
            "SES-HANDOFF",
            manager,
            ToolPolicySnapshot::workspace_write(dir.join("ws")),
            owner.lifecycle(),
            Arc::new(qaqh_process_tools::exec_session::ExecSessionManager::new()),
        ));
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
        let base_url = format!("http://{}", server.server_addr());
        let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let script = Arc::new(Mutex::new(script));
            let cursor = Arc::new(Mutex::new(0usize));
            let requests = Arc::clone(&requests);
            std::thread::spawn(move || {
                let header =
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..])
                        .expect("header");
                for request in server.incoming_requests() {
                    let mut request = request;
                    let mut body = String::new();
                    let _ = request.as_reader().read_to_string(&mut body);
                    requests.lock().unwrap().push(body);
                    // 末尾一条重复供应：SDK 重试不该让测试假失败。
                    let (status, payload) = {
                        let script = script.lock().unwrap();
                        let mut cursor = cursor.lock().unwrap();
                        let index = (*cursor).min(script.len() - 1);
                        *cursor += 1;
                        script[index].clone()
                    };
                    let _ = request.respond(
                        tiny_http::Response::from_string(payload)
                            .with_status_code(status)
                            .with_header(header.clone()),
                    );
                }
            });
        }
        let provider = qaqh_gate::ProviderConfig::chat(
            &base_url,
            "sk-test",
            "test-model",
            None,
            None,
            qaqh_types::ThinkingParamMode::OpenAi,
            qaqh_types::CacheTokenField::PromptCacheHitTokens,
            true,
            None,
        );
        let spec = ProviderRequestSpec::new(provider, 1024)
            .with_context(window, threshold)
            .with_tools(tools);
        let loop_ = SessionLoop::new("SES-HANDOFF", owner, port, spec, dir.to_path_buf());
        Harness {
            loop_,
            owners,
            base_url,
            requests,
        }
    }

    fn input(text: &str) -> ConversationSendMessage {
        ConversationSendMessage {
            text: text.into(),
            images: vec![],
            attachments: None,
            message_id: Some(format!("in-{text}")),
            input_purpose: InputPurpose::TriggerTurn,
            as_system: false,
            inter_agent: None,
            subagent_terminal: None,
            widget_context: None,
        }
    }

    async fn wait_until(
        owners: &OwnerDirectory,
        pred: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let owner = owners.get_or_create("SES-HANDOFF");
        for _ in 0..800 {
            let value = serde_json::to_value(owner.snapshot().await.expect("snapshot"))
                .expect("serialize snapshot");
            if pred(&value) {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("条件未在时限内成立");
    }

    fn idle_with_turns(value: &serde_json::Value) -> bool {
        value["activity"] == "idle"
            && value["turns"]
                .as_array()
                .is_some_and(|turns| !turns.is_empty())
    }

    fn timeline_tool_result(value: &serde_json::Value) -> String {
        value["timeline"]
            .as_array()
            .expect("timeline")
            .iter()
            .filter(|entry| entry["kind"] == "tool_result")
            .map(|entry| {
                entry["data"]["content"]["data"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 交接 = 终局：一次请求、completed 终态、结果入库、镜像落盘。
    #[tokio::test]
    async fn handoff_ends_the_turn_and_persists_the_durable_fact() {
        let _env_guard = QAQH_DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let previous = std::env::var_os("QAQH_DATA_DIR");
        // 镜像落点由 data_dir() 决定，把根指到临时目录才能断言文件真的写出来。
        // SAFETY: 本用例在锁内独占 QAQH_DATA_DIR 的改写并在断言后还原
        // （与幽灵回合反证用例共享同一把锁），tempdir 路径互不重叠。
        unsafe { std::env::set_var("QAQH_DATA_DIR", dir.path().join("root")) };

        let doc = serde_json::json!({
            "handoff_doc": "## 待决\n- 下一步查 SIWC 窗口来源",
            "work_summary": "交接测试"
        });
        let h = harness(
            dir.path(),
            vec![(
                200,
                tool_call_body("handoff", &doc, "call-h1", &cache_usage()),
            )],
            vec![handoff::tool_def()],
            10_000,
            0.5,
        )
        .await;
        h.loop_.send_input(input("开始干活")).expect("send input");

        let state = wait_until(&h.owners, idle_with_turns).await;
        assert_eq!(state["turns"][0]["outcome"]["kind"], "finished");
        assert_eq!(
            state["turns"][0]["outcome"]["data"]["terminal"], "completed",
            "TurnTerminal 冻结三值，交接只能报 completed"
        );
        assert_eq!(state["tools"][0]["state"]["kind"], "finished");
        let stored = timeline_tool_result(&state);
        assert!(
            stored.contains("\"handed_off\":true"),
            "机器面必须入库: {stored}"
        );
        assert_eq!(
            h.requests.lock().unwrap().len(),
            1,
            "交接后不得再向模型回传"
        );

        let mirror = dir
            .path()
            .join("root")
            .join("sessions")
            .join("SES-HANDOFF")
            .join("handoff");
        let written: Vec<_> = std::fs::read_dir(&mirror)
            .expect("镜像目录必须存在")
            .flatten()
            .collect();
        assert_eq!(written.len(), 1, "一次交接只写一份镜像");
        let content = std::fs::read_to_string(written[0].path()).expect("read mirror");
        assert!(content.contains("交接测试"));
        assert!(content.contains("查 SIWC 窗口来源"));
        assert!(content.contains("上下文占用"));

        match previous {
            // SAFETY: 与用例开头的 set_var 配对，本用例独占该变量。
            Some(value) => unsafe { std::env::set_var("QAQH_DATA_DIR", value) },
            // SAFETY: 该变量原本未设置，移除即还原环境。
            None => unsafe { std::env::remove_var("QAQH_DATA_DIR") },
        }
    }

    /// 缺交接文档 = 工具失败并回给模型，回合继续推进（不静默、不终止）。
    #[tokio::test]
    async fn handoff_with_missing_doc_fails_loudly_and_keeps_the_loop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let usage = serde_json::json!({"prompt_tokens":10,"completion_tokens":1,"total_tokens":11});
        let bad = tool_call_body(
            "handoff",
            &serde_json::json!({ "work_summary": "只写了一句话" }),
            "call-h2",
            &usage,
        );
        let h = harness(
            dir.path(),
            vec![(200, bad), (200, text_body("明白，我不交空文档"))],
            vec![handoff::tool_def()],
            10_000,
            0.95,
        )
        .await;
        h.loop_
            .send_input(input("交接但忘了正文"))
            .expect("send input");

        let state = wait_until(&h.owners, idle_with_turns).await;
        let stored = timeline_tool_result(&state);
        assert!(
            stored.contains("缺少 `handoff_doc`"),
            "缺字段必须响亮报错并入库: {stored}"
        );
        assert_eq!(h.requests.lock().unwrap().len(), 2, "报错后必须继续推进");
        assert!(
            serde_json::to_string(&state)
                .unwrap()
                .contains("明白，我不交空文档"),
            "第二轮的回答要留在会话里"
        );
    }

    /// 反证（规则 18）：封箱前排队的输入不得在封箱后的死线上开幽灵回合。
    /// QueueOnly 在空闲时入队不出回合（`send_input` 的 QueueOnly 分支），
    /// 给出确定性的「封箱前已在队内」窗口，不靠时序赛跑。
    /// 修复前该用例必红：`finish_active` 排水会让幽灵回合吃掉脚本第 2 条。
    #[tokio::test]
    async fn handoff_seal_drops_queued_inputs_instead_of_spawning_ghost_turns() {
        let _env_guard = QAQH_DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let previous = std::env::var_os("QAQH_DATA_DIR");
        // SAFETY: 锁内独占改写并在断言后还原（与镜像落点用例共享同一把锁）。
        unsafe { std::env::set_var("QAQH_DATA_DIR", dir.path().join("root")) };

        let doc = serde_json::json!({
            "handoff_doc": "## 待决\n- 幽灵回合反证",
            "work_summary": "封箱闭门"
        });
        // 脚本第 2 条只有幽灵回合会请求到：修复后 provider 请求数恒为 1。
        let h = harness(
            dir.path(),
            vec![
                (
                    200,
                    tool_call_body("handoff", &doc, "call-ghost", &cache_usage()),
                ),
                (200, text_body("幽灵回合不该跑到我")),
            ],
            vec![handoff::tool_def()],
            10_000,
            0.5,
        )
        .await;
        // 空闲态 QueueOnly：只入队，不触发回合。
        h.loop_
            .send_input(ConversationSendMessage {
                text: "封箱前排队的输入".into(),
                images: vec![],
                attachments: None,
                message_id: Some("in-queued-before-seal".into()),
                input_purpose: InputPurpose::QueueOnly,
                as_system: false,
                inter_agent: None,
                subagent_terminal: None,
                widget_context: None,
            })
            .expect("queue before first turn");
        h.loop_.send_input(input("触发交接")).expect("send input");

        let _ = wait_until(&h.owners, idle_with_turns).await;
        // 幽灵回合（若存在）在 Idle 事件后的同一任务里同步 spawn，再跑一整轮
        // SSE——留出余量让它现形，再冻结快照断言。
        tokio::time::sleep(Duration::from_millis(500)).await;
        let owner = h.owners.get_or_create("SES-HANDOFF");
        let state = serde_json::to_value(owner.snapshot().await.expect("snapshot"))
            .expect("serialize snapshot");
        assert_eq!(
            state["turns"].as_array().map(Vec::len),
            Some(1),
            "封箱后不得在死线开幽灵回合: {state}"
        );
        assert_eq!(
            h.requests.lock().unwrap().len(),
            1,
            "封箱闭门后不得再向模型发请求"
        );

        match previous {
            // SAFETY: 与用例开头的 set_var 配对，锁内独占。
            Some(value) => unsafe { std::env::set_var("QAQH_DATA_DIR", value) },
            // SAFETY: 该变量原本未设置，移除即还原环境。
            None => unsafe { std::env::remove_var("QAQH_DATA_DIR") },
        }
    }

    #[tokio::test]
    async fn session_title_sets_business_row_and_stays_declared_for_revision() {
        // 模型自命名:业务行落 title + title_source=auto,快照带新题;
        // 工具留在声明面——任务需求变化时可重新生成(零契约新增)。
        let dir = tempfile::tempdir().expect("tempdir");
        let doc = serde_json::json!({ "title": "daemon 启动崩溃排查" });
        let h = harness_titled(
            dir.path(),
            vec![
                (
                    200,
                    tool_call_body("session_title", &doc, "call-t1", &cache_usage()),
                ),
                (200, text_body("命名完成，继续干活")),
            ],
            vec![crate::session_title::tool_def()],
            10_000,
            0.0,
            "",
            None,
        )
        .await;
        h.loop_
            .send_input(input("排查 daemon 崩溃"))
            .expect("send input");

        let state = wait_until(&h.owners, |value| {
            value["activity"] == "idle" && value["title"] == "daemon 启动崩溃排查"
        })
        .await;
        assert!(
            timeline_tool_result(&state).contains("daemon 启动崩溃排查"),
            "回执必须入库: {state}"
        );
        // title_source 是业务列不进快照:直读原始行验来源。
        let source: String = h
            .owners
            .db()
            .read_blocking(|conn| {
                conn.query_row(
                    "SELECT title_source FROM sessions WHERE session_id = 'SES-HANDOFF'",
                    [],
                    |row| row.get(0),
                )
            })
            .expect("read title_source");
        assert_eq!(source, "auto");
        // 第二轮请求体的**声明面**仍含 session_title(auto 标题可修订);
        // 消息历史里的 tool_call 回显是合法内容,不参与断言。
        let requests = h.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        let declared = |body: &str| -> Vec<String> {
            serde_json::from_str::<serde_json::Value>(body)
                .expect("request body json")
                .get("tools")
                .and_then(|tools| tools.as_array())
                .map(|tools| {
                    tools
                        .iter()
                        .map(|tool| tool["function"]["name"].as_str().unwrap_or("").to_string())
                        .collect()
                })
                .unwrap_or_default()
        };
        assert!(declared(&requests[0]).contains(&"session_title".to_string()));
        assert!(
            declared(&requests[1]).contains(&"session_title".to_string()),
            "auto 标题落笔后工具必须留在声明面以供修订: {:?}",
            declared(&requests[1])
        );
    }

    #[tokio::test]
    async fn session_title_regenerates_when_task_changes() {
        // 任务需求变化:第二次调用覆盖旧题,title_source 仍为 auto,
        // 每一轮请求体都声明命名工具。
        let dir = tempfile::tempdir().expect("tempdir");
        let old = serde_json::json!({ "title": "旧任务排查" });
        let new = serde_json::json!({ "title": "新任务排查" });
        let h = harness_titled(
            dir.path(),
            vec![
                (
                    200,
                    tool_call_body("session_title", &old, "call-t1", &cache_usage()),
                ),
                (
                    200,
                    tool_call_body("session_title", &new, "call-t2", &cache_usage()),
                ),
                (200, text_body("改题完成")),
            ],
            vec![crate::session_title::tool_def()],
            10_000,
            0.0,
            "",
            None,
        )
        .await;
        h.loop_.send_input(input("任务变了")).expect("send input");

        let state = wait_until(&h.owners, |value| {
            value["activity"] == "idle" && value["title"] == "新任务排查"
        })
        .await;
        assert!(
            timeline_tool_result(&state).contains("新任务排查"),
            "修订回执必须入库: {state}"
        );
        let source: String = h
            .owners
            .db()
            .read_blocking(|conn| {
                conn.query_row(
                    "SELECT title_source FROM sessions WHERE session_id = 'SES-HANDOFF'",
                    [],
                    |row| row.get(0),
                )
            })
            .expect("read title_source");
        assert_eq!(source, "auto");
        let requests = h.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 3);
        for body in &requests {
            let names: Vec<String> = serde_json::from_str::<serde_json::Value>(body)
                .expect("request body json")
                .get("tools")
                .and_then(|tools| tools.as_array())
                .map(|tools| {
                    tools
                        .iter()
                        .map(|tool| tool["function"]["name"].as_str().unwrap_or("").to_string())
                        .collect()
                })
                .unwrap_or_default();
            assert!(
                names.contains(&"session_title".to_string()),
                "每轮都应声明: {names:?}"
            );
        }
    }

    #[tokio::test]
    async fn session_title_refuses_to_overwrite_user_named_session() {
        // 用户命名的会话(harness 默认 "handoff"/user):即使工具被预声明,
        // 写口守卫也必须拒绝覆盖,title 与 title_source 原样不动。
        let dir = tempfile::tempdir().expect("tempdir");
        let doc = serde_json::json!({ "title": "模型想覆盖" });
        let h = harness(
            dir.path(),
            vec![
                (
                    200,
                    tool_call_body("session_title", &doc, "call-t1", &cache_usage()),
                ),
                (200, text_body("明白，不覆盖用户命名")),
            ],
            vec![crate::session_title::tool_def()],
            10_000,
            0.0,
        )
        .await;
        h.loop_.send_input(input("给会话改名")).expect("send input");

        let state = wait_until(&h.owners, |value| {
            timeline_tool_result(value).contains("用户命名")
        })
        .await;
        assert_eq!(state["title"], "handoff", "用户标题不得被覆盖: {state}");
        let source: String = h
            .owners
            .db()
            .read_blocking(|conn| {
                conn.query_row(
                    "SELECT title_source FROM sessions WHERE session_id = 'SES-HANDOFF'",
                    [],
                    |row| row.get(0),
                )
            })
            .expect("read title_source");
        assert_eq!(source, "user");
    }

    /// OpenAI chat SSE:一段增量 + 一条 error 尾帧。增量必须在前——有产出
    /// 时 gate 才走「掐流即失败」(Fatal)而不是零产出重试,后者会消耗脚本
    /// 第二条响应,让 turn 级重发的断言失真。
    fn mid_stream_error_body(code: &str) -> String {
        let delta =
            serde_json::json!({"choices":[{"index":0,"delta":{"content":"半截"},"finish_reason":null}]});
        let usage = serde_json::json!({"choices":[],"usage":{"prompt_tokens":60,"completion_tokens":5,"total_tokens":65}});
        let error = serde_json::json!({"error":{"code":code,"message":"upstream exploded"}});
        format!("data: {delta}\n\ndata: {usage}\n\ndata: {error}\n\n")
    }

    /// 纯文本截断:一段增量 + error 尾帧(ProviderInternal 瞬态)。
    fn truncated_text_error_body(prefix: &str) -> String {
        let delta = serde_json::json!({"choices":[{"index":0,"delta":{"content":prefix},"finish_reason":null}]});
        let error = serde_json::json!({"error":{"code":"server_error","message":"upstream exploded"}});
        format!("data: {delta}\n\ndata: {error}\n\n")
    }

    /// 工具参数截断:text 前缀 + 半截 args 的 tool_calls delta + error 尾帧。
    fn truncated_tool_error_body() -> String {
        let delta = serde_json::json!({"choices":[{"index":0,"delta":{"content":"我先读取配置"},"finish_reason":null}]});
        let tool = serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_x1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"conf"}}]}}]});
        let error = serde_json::json!({"error":{"code":"server_error","message":"upstream exploded"}});
        format!("data: {delta}\n\ndata: {tool}\n\ndata: {error}\n\n")
    }

    /// 终态响应携带参数截断的 ToolUse：finish_reason=tool_calls + 半截 args，
    /// 没有 error 尾帧。gate finalize 落 Null input → classify → Incomplete。
    /// 这是与流中断相邻、此前漏掉重试的那条边界。
    fn corrupt_tool_terminal_body() -> String {
        let call = serde_json::json!({"choices":[{"index":0,"delta":{"content":"我先读取配置",
            "tool_calls":[{"index":0,"id":"call_x1","type":"function",
            "function":{"name":"read_file","arguments":"{\"path\":\"conf"}}]},"finish_reason":null}]});
        let done = serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],
            "usage":cache_usage()});
        format!("data: {call}\n\ndata: {done}\n\ndata: [DONE]\n\n")
    }

    /// 可重试的 mid-stream error 尾帧(未识别 token → ProviderInternal 瞬态),
    /// attempt 残段为纯文本 → A 路(prefill+seed):attempt-2 的块 = seed +
    /// 续写段,一条块承载完整文本;失败 attempt 已见的 usage 不丢。
    #[tokio::test]
    async fn turn_retries_retryable_mid_stream_error_and_completes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = harness(
            dir.path(),
            vec![
                (200, mid_stream_error_body("server_error")),
                (200, text_body("恢复后完成")),
            ],
            vec![],
            10_000,
            0.0,
        )
        .await;
        h.loop_
            .send_input(input("瞬态失败重发"))
            .expect("send input");

        let state = wait_until(&h.owners, |value| {
            value["activity"] == "idle"
                && serde_json::to_string(&value["timeline"])
                    .map_or(false, |timeline| timeline.contains("恢复后完成"))
        })
        .await;
        assert_eq!(
            h.requests.lock().unwrap().len(),
            2,
            "同一 turn 必须恰好两次请求:原始 + 重发"
        );
        let outcome = &state["turns"][0]["outcome"]["data"];
        assert!(
            outcome["error"].is_null(),
            "重发成功后不得留下失败事实: {outcome}"
        );
        let timeline = serde_json::to_string(&state["timeline"]).expect("timeline");
        assert!(
            timeline.contains("半截恢复后完成"),
            "A 路续写:attempt-2 的块 = seed+续写段,一条完整块: {timeline}"
        );
        // 失败 attempt 的已见 usage(65)并入最终口径,不因失败被丢弃。
        assert_eq!(
            state["turns"][0]["outcome"]["data"]["usage"]["total_tokens"],
            serde_json::json!(65),
            "失败 attempt 的 usage 必须计入: {outcome}"
        );
    }

    /// 不可重试类别(content_filter token → ContentFiltered):不重发,按
    /// provider_failed 结算,真实原因随失败事实落盘。
    #[tokio::test]
    async fn turn_does_not_retry_non_retryable_mid_stream_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = harness(
            dir.path(),
            vec![(200, mid_stream_error_body("content_filter"))],
            vec![],
            10_000,
            0.0,
        )
        .await;
        h.loop_
            .send_input(input("内容审查不重发"))
            .expect("send input");

        let state = wait_until(&h.owners, |value| {
            value["activity"] == "idle"
                && value["turns"][0]["outcome"]["data"]["error"]["code"] == "provider_failed"
        })
        .await;
        assert_eq!(
            h.requests.lock().unwrap().len(),
            1,
            "不可重试类别不得发起第二次请求"
        );
        let error = &state["turns"][0]["outcome"]["data"]["error"];
        assert_eq!(error["code"], "provider_failed", "结算形态: {error}");
        assert!(
            error["message"]
                .as_str()
                .is_some_and(|m| m.contains("provider stream error")),
            "失败必须携带真实原因: {error}"
        );
    }

    /// 重发预算耗尽:第二次 attempt 仍瞬态失败 → 不再重发,按失败结算。
    #[tokio::test]
    async fn turn_retry_budget_exhausted_settles_failed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = harness(
            dir.path(),
            vec![
                (200, mid_stream_error_body("server_error")),
                (200, mid_stream_error_body("server_error")),
            ],
            vec![],
            10_000,
            0.0,
        )
        .await;
        h.loop_
            .send_input(input("两次瞬态失败"))
            .expect("send input");

        let state = wait_until(&h.owners, |value| {
            value["activity"] == "idle"
                && value["turns"][0]["outcome"]["data"]["error"]["code"] == "provider_failed"
        })
        .await;
        assert_eq!(
            h.requests.lock().unwrap().len(),
            2,
            "预算 1 次:原始 + 一次重发,之后必须收口"
        );
        let error = &state["turns"][0]["outcome"]["data"]["error"];
        assert_eq!(error["code"], "provider_failed", "结算形态: {error}");
    }

    /// B 路:截断的工具调用 → 注入失败 ToolResult。前缀独立落盘;第二次
    /// 请求的 body 里 assistant 消息带 tool_use(合法化 input)与失败
    /// tool_result,模型据此重调;失败事实作为 durable fact 入库。
    #[tokio::test]
    async fn turn_retry_injects_failed_tool_result_for_truncated_calls() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = harness(
            dir.path(),
            vec![
                (200, truncated_tool_error_body()),
                (200, text_body("收到，将重新调用")),
            ],
            vec![],
            10_000,
            0.0,
        )
        .await;
        h.loop_
            .send_input(input("工具参数截断重发"))
            .expect("send input");

        let state = wait_until(&h.owners, |value| {
            value["activity"] == "idle"
                && timeline_tool_result(value).contains("cut off")
        })
        .await;
        assert_eq!(
            h.requests.lock().unwrap().len(),
            2,
            "注入失败结果后 attempt-2 完成回合"
        );
        let second = &h.requests.lock().unwrap()[1];
        assert!(second.contains("call_x1"), "失败注入必须携带原 call_id");
        assert!(
            second.contains("cut off"),
            "失败 tool_result 必须随请求回传: {second}"
        );
        assert!(
            second.contains("我先读取配置"),
            "text 前缀必须随 assistant 消息回传"
        );
        let outcome = &state["turns"][0]["outcome"]["data"];
        assert!(outcome["error"].is_null(), "B 路完成后无失败事实: {outcome}");
        // 前缀独立落盘成块(它不在 attempt-2 的消息里)。
        let timeline = serde_json::to_string(&state["timeline"]).expect("timeline");
        assert!(
            timeline.contains("我先读取配置"),
            "B 路前缀必须作为独立块落盘: {timeline}"
        );
    }

    /// 终态 Incomplete + 损坏 ToolUse 与流中断同律：也必须走 B 路重试，
    /// 而不是直接 provider_incomplete 判死（相邻边界漏重试的回归）。
    #[tokio::test]
    async fn terminal_corrupt_tool_retries_instead_of_failing_turn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = harness(
            dir.path(),
            vec![
                (200, corrupt_tool_terminal_body()),
                (200, text_body("收到，将重新调用")),
            ],
            vec![],
            10_000,
            0.0,
        )
        .await;
        h.loop_
            .send_input(input("终态坏工具重发"))
            .expect("send input");

        let state = wait_until(&h.owners, |value| {
            value["activity"] == "idle" && timeline_tool_result(value).contains("cut off")
        })
        .await;
        assert_eq!(
            h.requests.lock().unwrap().len(),
            2,
            "终态损坏工具也必须重发一次，不得直接判死"
        );
        let second = &h.requests.lock().unwrap()[1];
        assert!(
            second.contains("call_x1"),
            "失败注入必须携带原 call_id: {second}"
        );
        let outcome = &state["turns"][0]["outcome"]["data"];
        assert!(outcome["error"].is_null(), "B 路完成后无失败事实: {outcome}");
    }

    /// A 路:纯文本截断 → prefill 前缀 + seed 续写。第二次请求带尾部
    /// assistant 消息;attempt-2 的块 = seed + 续写段,一条块承载完整文本,
    /// 历史不残留 prefill 临时消息。
    #[tokio::test]
    async fn turn_retry_continues_from_truncated_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = harness(
            dir.path(),
            vec![
                (200, truncated_text_error_body("今天天气")),
                (200, text_body("很好，适合出门")),
            ],
            vec![],
            10_000,
            0.0,
        )
        .await;
        h.loop_
            .send_input(input("文本截断续写"))
            .expect("send input");

        let state = wait_until(&h.owners, |value| {
            value["activity"] == "idle"
                && serde_json::to_string(&value["timeline"]).map_or(false, |timeline| {
                    timeline.contains("今天天气很好，适合出门")
                })
        })
        .await;
        assert_eq!(h.requests.lock().unwrap().len(), 2);
        let second = &h.requests.lock().unwrap()[1];
        assert!(
            second.contains("今天天气"),
            "prefill 前缀必须随第二次请求回传: {second}"
        );
        let outcome = &state["turns"][0]["outcome"]["data"];
        assert!(outcome["error"].is_null(), "续写完成后无失败事实: {outcome}");
        let timeline = serde_json::to_string(&state["timeline"]).expect("timeline");
        assert!(
            !timeline.contains("今天天气今天天气"),
            "seed 不得重复拼接: {timeline}"
        );
    }

    /// 占用页脚 append 进工具结果正文：落库与发送同一份字节，消息即历史。
    ///
    /// 回归：页脚只进模型投影时，下一回合重建读回的是不带页脚的原文，
    /// 前缀缓存从页脚位置起整段击穿（2026-11-17 复盘）。现在页脚在 dispatch
    /// 后即拼进结果 text，落库与 tool_result_message 共用它，零字节反转。
    #[tokio::test]
    async fn context_footer_appends_into_tool_result_and_is_durable() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 第一轮：handoff 缺正文 → 工具失败，回合继续；读数 960/1000 → 96%。
        // 第二轮：直接收尾。第二条请求里那条工具结果应带页脚。
        let h = harness(
            dir.path(),
            vec![
                (
                    200,
                    tool_call_body(
                        "handoff",
                        &serde_json::json!({ "work_summary": "只写了一句话" }),
                        "call-h3",
                        &cache_usage(),
                    ),
                ),
                (200, text_body("收到，继续")),
            ],
            vec![handoff::tool_def()],
            1000,
            0.0,
        )
        .await;
        h.loop_
            .send_input(input("交接但忘了正文"))
            .expect("send input");

        let state = wait_until(&h.owners, idle_with_turns).await;
        let requests = h.requests.lock().unwrap();
        assert!(requests.len() >= 2, "工具失败后回合必须继续推进");
        assert!(
            !requests[0].contains("[#ctx 96%"),
            "首轮请求还没有任何读数，不得出现真实占用页脚"
        );
        assert!(
            requests[1].contains("[#ctx 96% · 0k/1k"),
            "第二条请求的最后一条工具结果应带占用页脚（读数 960/1000）"
        );
        // 消息即历史：页脚已拼进结果正文并落库，重放读到的必须带同一份页脚。
        assert!(
            timeline_tool_result(&state).contains("[#ctx 96% · 0k/1k"),
            "历史必须与发往模型的字节一致（零字节反转）"
        );
    }

    /// widget 快照作为 durable Input 落库：首轮发出、次轮从历史逐字节重建。
    /// 回归（2026-11-18）：旧形态只进请求瞬态尾，下一回合重建丢失该字节，
    /// 前缀缓存从快照位置击穿。
    #[tokio::test]
    async fn widget_snapshot_is_durable_and_rebuilt_byte_identical() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = harness(
            dir.path(),
            vec![(200, text_body("one")), (200, text_body("two"))],
            vec![],
            10_000,
            0.0,
        )
        .await;
        let mut first = input("首轮带快照");
        first.widget_context = Some(qaqh_ringing_contract::commands::WidgetContext {
            session_id: "SES-HANDOFF".into(),
            widget: qaqh_ringing_contract::commands::WidgetRef {
                path: "/w/.qaqh/widgets/SES-HANDOFF/chart.html".into(),
                title: Some("chart".into()),
            },
            model_content: serde_json::json!({"score": 42}),
        });
        h.loop_.send_input(first).expect("send input");
        let state = wait_until(&h.owners, idle_with_turns).await;

        // 首轮请求带快照，且快照已落库为 actor=widget-state 的 Input。
        {
            let requests = h.requests.lock().unwrap();
            assert!(
                requests[0].contains("<widget-state>"),
                "首轮请求必须带 widget 快照"
            );
        }
        let timeline = serde_json::to_string(&state["timeline"]).expect("timeline");
        assert!(
            timeline.contains("widget-state") && timeline.contains("score"),
            "快照必须落库为 durable Input: {timeline}"
        );

        // 第二轮不带 widget_context，但重建请求仍逐字节带上同一份快照。
        h.loop_.send_input(input("次轮不带快照")).expect("send input");
        wait_until(&h.owners, |v| {
            v["activity"] == "idle"
                && v["turns"].as_array().is_some_and(|t| t.len() >= 2)
        })
        .await;
        let requests = h.requests.lock().unwrap();
        assert!(
            requests[1].contains("<widget-state>") && requests[1].contains("score"),
            "次轮重建必须逐字节带上落库的快照（消息即历史）: {}",
            &requests[1][..requests[1].len().min(600)]
        );
    }

    /// 超限错误：分母立刻按声明值一半保守收敛，且失败码不再伪装成一般 provider 失败。
    #[tokio::test]
    async fn context_overflow_error_lowers_the_denominator() {
        let dir = tempfile::tempdir().expect("tempdir");
        let overflow = serde_json::json!({"error":{"message":
            "This model's maximum context length is 1200 tokens. However, your request is too long.",
            "type":"invalid_request_error","code":"context_length_exceeded"}})
        .to_string();
        let h = harness(
            dir.path(),
            vec![(400, overflow)],
            vec![handoff::tool_def()],
            2000,
            0.5,
        )
        .await;
        h.loop_.send_input(input("推超限")).expect("send input");

        let state = wait_until(&h.owners, idle_with_turns).await;
        assert_eq!(
            state["turns"][0]["outcome"]["data"]["error"]["code"], "context_overflow",
            "超限不得伪装成一般 provider 失败"
        );
        assert_eq!(
            handoff::effective_window(&h.base_url, "test-model", 2000),
            1000,
            "无实测记录时按声明值一半保守收敛"
        );
    }

    /// 单一折叠阈值收口：达到用户配置的 `auto_compact_threshold` 时折叠工具结果、
    /// 只落一条严格英文交接指令（不再双发软/硬两版），且 `[Environment]` 携带该阈值。
    #[tokio::test]
    async fn hard_pressure_folds_tool_results_and_leaves_visible_inputs() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 800/1000 = 80%（hit 740 + miss 60），用户阈值 75%（低于旧硬阈值 95%）：
        // 必须按用户阈值直接折叠并只注入一条英文强制指令。
        let usage_80 = serde_json::json!({"prompt_tokens":60,"completion_tokens":5,"total_tokens":65,
            "prompt_cache_hit_tokens":740,"prompt_cache_miss_tokens":60});
        let bad = tool_call_body(
            "handoff",
            &serde_json::json!({ "handoff_doc": "  ", "work_summary": "空正文" }),
            "call-h3",
            &usage_80,
        );
        let h = harness(
            dir.path(),
            vec![(200, bad), (200, text_body("收到，这就写文档"))],
            vec![handoff::tool_def()],
            1000,
            0.75,
        )
        .await;
        h.loop_.send_input(input("压力大")).expect("send input");

        let state = wait_until(&h.owners, idle_with_turns).await;
        let requests = h.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[0].contains("context_fold_threshold: 75%"),
            "系统提示 [Environment] 必须宣告本会话折叠阈值"
        );
        assert!(!requests[0].contains(FOLD_NOTE), "第一轮没有任何东西可折叠");
        assert!(requests[1].contains(FOLD_NOTE), "第二轮必须折叠工具结果");
        assert!(
            !requests[1].contains("不能为空"),
            "折叠后原文不该再进模型上下文"
        );
        assert!(
            requests[1].contains("[SYSTEM · HANDOFF REQUIRED] Context usage reached 80% (800 / 1000 tokens; threshold 75%)"),
            "占用必须按 hit+miss 口径、使用用户配置阈值并输出严厉英文指令: {}",
            requests[1]
        );
        assert!(requests[1].contains("MANDATORY ACTION: Call `handoff` NOW"));
        assert!(
            timeline_tool_result(&state).contains("不能为空"),
            "折叠只动模型投影，落库事实必须原样"
        );
        let watchdog = state["timeline"]
            .as_array()
            .expect("timeline")
            .iter()
            .filter(|entry| {
                entry["kind"] == "input"
                    && entry["data"]["purpose"] == "interject"
                    && entry["data"]["actor"]["kind"] == "api"
            })
            .count();
        assert_eq!(watchdog, 1, "软硬合一后同轮只允许注入一条交接指令");
    }

    /// 次回合前缀缓存回归：
    /// 1. `[Environment]`（含 `context_fold_threshold` 与 `tools`）在首回合与次回合逐字节一致；
    /// 2. 纯工具调用轮（无正文/无思考/无 opaque）在次回合 `assemble_context` 中不得被丢弃。
    #[tokio::test]
    async fn turn_two_preserves_pure_tool_call_round_and_system_prefix_byte_identically() {
        let dir = tempfile::tempdir().expect("tempdir");
        let usage_low = serde_json::json!({"prompt_tokens":100,"completion_tokens":5,"total_tokens":105,
            "prompt_cache_hit_tokens":80,"prompt_cache_miss_tokens":20});
        let pure_tool = tool_call_body(
            "handoff",
            &serde_json::json!({ "handoff_doc": "  ", "work_summary": "纯工具轮" }),
            "call-pure-1",
            &usage_low,
        );
        let h = harness(
            dir.path(),
            vec![
                (200, pure_tool),
                (200, text_body("turn 1 done")),
                (200, text_body("turn 2 done")),
            ],
            vec![handoff::tool_def()],
            10_000,
            0.75,
        )
        .await;
        h.loop_.send_input(input("第一回合")).expect("send turn 1");
        wait_until(&h.owners, idle_with_turns).await;

        h.loop_.send_input(input("第二回合")).expect("send turn 2");
        wait_until(&h.owners, |v| {
            v["activity"] == "idle"
                && v["turns"].as_array().is_some_and(|t| t.len() >= 2)
        })
        .await;

        let requests = h.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 3, "首回合两轮 + 次回合一轮");
        let req_t1_r2: serde_json::Value = serde_json::from_str(&requests[1]).expect("t1 r2 json");
        let req_t2_r1: serde_json::Value = serde_json::from_str(&requests[2]).expect("t2 r1 json");
        let t1_msgs = req_t1_r2["messages"].as_array().expect("t1 messages");
        let t2_msgs = req_t2_r1["messages"].as_array().expect("t2 messages");
        assert!(
            t2_msgs.len() > t1_msgs.len(),
            "次回合应包含首回合全部前缀 + 首回合终答 + 次回合用户输入"
        );
        assert_eq!(
            &t2_msgs[..t1_msgs.len()],
            t1_msgs.as_slice(),
            "次回合前缀必须与首回合末轮逐字节一致（不得丢纯工具轮或改写 system 环境块）"
        );
        let system_text = t2_msgs[0]["content"].as_str().unwrap_or_default();
        assert!(system_text.contains("context_fold_threshold: 75%"));
    }

    /// Responses 线次回合前缀缓存回归：`provider_job` 把 `ResponseOutputItem` 拆进
    /// `outcome.opaque_items` 后，首回合内存 `messages` 必须同步回填，使首回合次轮
    /// 与次回合 `assemble_context` 发出的 `input` 前缀逐字节一致（不得在次回合突然
    /// 出现 `encrypted_content`/原生 item 击穿前缀缓存）。
    #[tokio::test]
    async fn responses_opaque_items_stay_byte_identical_across_turns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r1_tool = concat!(
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"status\":\"completed\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"plan\"}],\"encrypted_content\":\"enc_abc\"}}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"status\":\"completed\",\"call_id\":\"call_r1\",\"name\":\"handoff\",\"arguments\":\"{\\\"handoff_doc\\\":\\\"  \\\",\\\"work_summary\\\":\\\"x\\\"}\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"model\":\"test-model\",\"output\":[{\"id\":\"rs_1\",\"type\":\"reasoning\",\"status\":\"completed\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"plan\"}],\"encrypted_content\":\"enc_abc\"},{\"id\":\"fc_1\",\"type\":\"function_call\",\"status\":\"completed\",\"call_id\":\"call_r1\",\"name\":\"handoff\",\"arguments\":\"{\\\"handoff_doc\\\":\\\"  \\\",\\\"work_summary\\\":\\\"x\\\"}\"}],\"usage\":{\"input_tokens\":100,\"output_tokens\":10,\"total_tokens\":110}}}\n\n",
            "data: [DONE]\n\n"
        )
        .to_string();
        let r_text = |id: &str, msg_id: &str, text: &str| {
            format!(
                "event: response.output_text.delta\n\
                 data: {{\"type\":\"response.output_text.delta\",\"item_id\":\"{msg_id}\",\"output_index\":0,\"content_index\":0,\"delta\":\"{text}\"}}\n\n\
                 event: response.output_item.done\n\
                 data: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{{\"id\":\"{msg_id}\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"{text}\",\"annotations\":[]}}]}}}}\n\n\
                 event: response.completed\n\
                 data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"{id}\",\"status\":\"completed\",\"model\":\"test-model\",\"usage\":{{\"input_tokens\":120,\"output_tokens\":5,\"total_tokens\":125}}}}}}\n\n\
                 data: [DONE]\n\n"
            )
        };
        let script = vec![
            (200u16, r1_tool),
            (200, r_text("resp_2", "msg_1", "turn 1 done")),
            (200, r_text("resp_3", "msg_2", "turn 2 done")),
        ];
        let owners = Arc::new(OwnerDirectory::open(dir.path().join("qaqh.db")).expect("db"));
        let owner = owners
            .create_session(store::NewSession {
                session_id: "SES-HANDOFF".into(),
                parent_session_id: None,
                title: "responses-cache".into(),
                title_source: Some("user".into()),
                cwd: None,
                model: None,
                tool_mode: None,
                model_selection: None,
                custom_tools: None,
                frozen_annotation: None,
                ephemeral: false,
            })
            .await
            .expect("create session");
        let manager = Arc::new(Mutex::new(
            qaqh_workspace::registration::build_tool_manager(
                &[],
                std::sync::Arc::new(qaqh_workspace::todo::MemoryTodoHost::default()),
            ),
        ));
        let port = Arc::new(WorkspaceToolPort::new(
            "SES-HANDOFF",
            manager,
            ToolPolicySnapshot::workspace_write(dir.path().join("ws")),
            owner.lifecycle(),
            Arc::new(qaqh_process_tools::exec_session::ExecSessionManager::new()),
        ));
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
        let base_url = format!("http://{}", server.server_addr());
        let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let script = Arc::new(Mutex::new(script));
            let cursor = Arc::new(Mutex::new(0usize));
            let requests = Arc::clone(&requests);
            std::thread::spawn(move || {
                let header =
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..])
                        .expect("header");
                for mut request in server.incoming_requests() {
                    let mut body = String::new();
                    let _ = request.as_reader().read_to_string(&mut body);
                    requests.lock().unwrap().push(body);
                    let (status, payload) = {
                        let script = script.lock().unwrap();
                        let mut cursor = cursor.lock().unwrap();
                        let index = (*cursor).min(script.len() - 1);
                        *cursor += 1;
                        script[index].clone()
                    };
                    let _ = request.respond(
                        tiny_http::Response::from_string(payload)
                            .with_status_code(status)
                            .with_header(header.clone()),
                    );
                }
            });
        }
        let provider = qaqh_gate::ProviderConfig::responses(&base_url, "sk-test", "test-model", None);
        let spec = ProviderRequestSpec::new(provider, 1024)
            .with_context(10_000, 0.75)
            .with_tools(vec![handoff::tool_def()]);
        let loop_ = SessionLoop::new("SES-HANDOFF", owner, port, spec, dir.path().to_path_buf());

        loop_.send_input(input("第一回合")).expect("send turn 1");
        wait_until(&owners, idle_with_turns).await;
        loop_.send_input(input("第二回合")).expect("send turn 2");
        wait_until(&owners, |v| {
            v["activity"] == "idle"
                && v["turns"].as_array().is_some_and(|t| t.len() >= 2)
        })
        .await;

        let requests = requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 3);
        assert!(
            requests[1].contains("enc_abc"),
            "首回合次轮必须已携带原生 ResponseOutputItem（不得等到次回合才冒出）: {}",
            requests[1]
        );
        let req_t1_r2: serde_json::Value = serde_json::from_str(&requests[1]).expect("t1 r2 json");
        let req_t2_r1: serde_json::Value = serde_json::from_str(&requests[2]).expect("t2 r1 json");
        assert_eq!(
            req_t1_r2["prompt_cache_key"], "SES-HANDOFF",
            "Responses 线生产路径必须自动绑定会话级 prompt_cache_key"
        );
        assert_eq!(
            req_t2_r1["prompt_cache_key"], "SES-HANDOFF",
            "次回合 prompt_cache_key 必须与首回合一致"
        );
        assert_eq!(
            req_t2_r1["instructions"], req_t1_r2["instructions"],
            "首回合与次回合 instructions 必须逐字节一致"
        );
        let t1_input = req_t1_r2["input"].as_array().expect("t1 input");
        let t2_input = req_t2_r1["input"].as_array().expect("t2 input");
        assert!(t2_input.len() > t1_input.len());
        assert_eq!(
            &t2_input[..t1_input.len()],
            t1_input.as_slice(),
            "次回合 Responses input 前缀必须与首回合末轮逐字节一致"
        );
    }

    // ── 出站口延迟 + 流式信息完整度 / 顺序回归 ────────────────────────────
    //
    // 复现现场（多端实测）：一句完整回答 "好的，我这就开始展开调查" 之后跟一次
    // 工具调用，前端却呈现 "好的，" → 工具卡片 → "好的，我这就开始展开调查"。
    //
    // 关键前提：**上游时间顺序完全正常**。服务商按 token 分片是常态，一句话会
    // 拆成 N 个 content delta，且全部在 tool_calls 之前到达；本 fixture 正是
    // 连续 3 片文本 → 1 片 tool_calls → finish_reason=tool_calls，没有任何交错。
    // 半句不是上游给的，也不是前端拆的，是内核自己造的：
    //
    //   provider_job.rs:259-267 ContentDelta 按 PROGRESS_INTERVAL=40ms 合并窗
    //     发「累积前缀」→ 只有第 1 片触发 due() 首发，其余 N-1 片落在同一窗内
    //     被吞，出站口只看到第 1 片的前缀（半句）。
    //   provider_job.rs:282-305 ToolCallProgress 是流式帧，参数一闭合就抢发
    //     ToolArgumentsDelta（mark_complete() 短路，不等节流窗）。
    //   provider_job.rs:189-198 stream_once 整体返回后，run_round 才用
    //     outcome.partial_text 补发完整文本（"final values always published"）。
    //
    // ⇒ 帧序：MessageDelta(半句) → ToolArgumentsDelta → MessageDelta(全句)。
    // 文本不丢（信息完整度 OK），但工具展示帧把一句话劈成两半；前端按到达
    // 顺序把两类帧渲染进不同区域，就成了「话没说完先调工具、事后补全」。
    //
    // 观测点 = owner 单流 broadcast，即 daemon `handle_events_v2` 的
    // `attach.receiver` 拿到的同一批帧（出站口排队前的最后一站；不含 axum
    // mpsc(64) 序列化与网络，故这是延迟**下界**，不是 TCP 到屏时间）。
    //
    // 已修复并转为回归守卫：工具帧落地前先把正文全文冲刷到同一个 answer
    // block（provider_job::flush_text_before_tool），流末兜底按已发布长度
    // 判重，不再在工具帧之后补发一帧全文。上游正常顺序下 answer 的
    // MessageDelta 序列必须连续。

    use qaqh_ringing_contract::events::{SessionEvent, SessionEventFrame};
    use qaqh_ringing_contract::state::TimelineEntry;

    /// 服务商按 token 分片的正常顺序：3 片连续文本，全部在工具调用之前。
    const ANSWER_CHUNKS: [&str; 3] = ["好的，", "我这就开始", "展开调查"];

    /// 上游时间顺序完全正常；不构造任何 content/tool_calls 交错。
    fn normal_order_answer_then_tool_sse() -> String {
        let mut frames = Vec::new();
        for text in ANSWER_CHUNKS {
            frames.push(
                serde_json::json!({
                    "choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]
                })
                .to_string(),
            );
        }
        frames.push(
            serde_json::json!({
                "choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{
                    "index":0,"id":"call_order_1","type":"function",
                    "function":{"name":"__probe_unknown__","arguments":"{\"path\":\"x\"}"}
                }]},"finish_reason":null}]
            })
            .to_string(),
        );
        frames.push(
            serde_json::json!({
                "choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],
                "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
            })
            .to_string(),
        );
        let mut body = String::new();
        for frame in frames {
            body.push_str("data: ");
            body.push_str(&frame);
            body.push_str("\n\n");
        }
        body.push_str("data: [DONE]\n\n");
        body
    }

    fn stop_sse() -> String {
        concat!(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        )
        .to_string()
    }

    struct OutboundProbe {
        loop_: Arc<SessionLoop>,
        rx: tokio::sync::broadcast::Receiver<SessionEventFrame>,
        upstream_written_at: Arc<Mutex<Option<std::time::Instant>>>,
        _owners: Arc<OwnerDirectory>,
    }

    async fn outbound_probe(dir: &std::path::Path) -> OutboundProbe {
        let owners = Arc::new(OwnerDirectory::open(dir.join("qaqh.db")).expect("open owners db"));
        let owner = owners
            .create_session(store::NewSession {
                session_id: "SES-OUTBOUND".into(),
                parent_session_id: None,
                title: "outbound".into(),
                title_source: Some("user".into()),
                cwd: None,
                model: None,
                tool_mode: None,
                model_selection: None,
                custom_tools: None,
                frozen_annotation: None,
                ephemeral: false,
            })
            .await
            .expect("create session");
        let rx = owner.subscribe();
        let manager = Arc::new(Mutex::new(
            qaqh_workspace::registration::build_tool_manager(
                &[],
                std::sync::Arc::new(qaqh_workspace::todo::MemoryTodoHost::default()),
            ),
        ));
        let port = Arc::new(WorkspaceToolPort::new(
            "SES-OUTBOUND",
            manager,
            ToolPolicySnapshot::workspace_write(dir.join("ws")),
            owner.lifecycle(),
            Arc::new(qaqh_process_tools::exec_session::ExecSessionManager::new()),
        ));
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
        let base_url = format!("http://{}", server.server_addr());
        let upstream_written_at: Arc<Mutex<Option<std::time::Instant>>> =
            Arc::new(Mutex::new(None));
        {
            let t0 = Arc::clone(&upstream_written_at);
            std::thread::spawn(move || {
                let header =
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..])
                        .expect("header");
                let mut n = 0usize;
                for request in server.incoming_requests() {
                    let payload = if n == 0 {
                        normal_order_answer_then_tool_sse()
                    } else {
                        stop_sse()
                    };
                    n += 1;
                    // 只记第一次 respond：第二次是工具结果后的收尾请求，不算上游进门。
                    if n == 1 {
                        *t0.lock().unwrap() = Some(std::time::Instant::now());
                    }
                    let _ = request.respond(
                        tiny_http::Response::from_string(payload).with_header(header.clone()),
                    );
                }
            });
        }
        let mut provider = qaqh_gate::ProviderConfig::chat(
            &base_url,
            "sk-test",
            "test-model",
            None,
            None,
            qaqh_types::ThinkingParamMode::OpenAi,
            qaqh_types::CacheTokenField::PromptCacheHitTokens,
            true,
            None,
        );
        provider.retry = Some(qaqh_types::RetrySpec {
            max_retries: 1,
            ..Default::default()
        });
        let spec = ProviderRequestSpec::new(provider, 1024);
        let loop_ = SessionLoop::new("SES-OUTBOUND", owner, port, spec, dir.to_path_buf());
        OutboundProbe {
            loop_,
            rx,
            upstream_written_at,
            _owners: owners,
        }
    }

    fn brief(event: &SessionEvent) -> String {
        let debug = format!("{event:?}");
        debug.chars().take(150).collect()
    }

    #[tokio::test]
    async fn upstream_chunk_to_outbound_latency_and_stream_completeness() {
        use qaqh_ringing_contract::fact::{AssistantBlockKind, ContentValue};

        let dir = tempfile::tempdir().expect("tempdir");
        let mut probe = outbound_probe(dir.path()).await;
        probe
            .loop_
            .send_input(input("开始调查"))
            .expect("send input");

        let mut frames: Vec<(SessionEventFrame, std::time::Instant)> = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(8), probe.rx.recv()).await {
                Ok(Ok(frame)) => {
                    let ended = matches!(frame.event, SessionEvent::TurnEnded { .. });
                    frames.push((frame, std::time::Instant::now()));
                    if ended {
                        break;
                    }
                }
                Ok(Err(error)) => panic!("event channel: {error}"),
                Err(_) => panic!("出站帧未在 8s 内收齐（已收 {} 帧）", frames.len()),
            }
        }

        let t0 = probe
            .upstream_written_at
            .lock()
            .unwrap()
            .expect("上游未写出 SSE");
        eprintln!(
            "=== 出站口观测：t0 = 上游首个 SSE respond（共 {} 帧）===",
            frames.len()
        );
        for (frame, at) in &frames {
            eprintln!(
                "seq={:<3} +{:<5}ms {}",
                frame.seq,
                at.saturating_duration_since(t0).as_millis(),
                brief(&frame.event)
            );
        }

        // ① 延迟：上游内容驱动的首帧 / 回合收口末帧。
        let first = frames
            .iter()
            .find(|(f, _)| {
                matches!(
                    f.event,
                    SessionEvent::MessageDelta { .. } | SessionEvent::ToolArgumentsDelta { .. }
                )
            })
            .expect("上游内容没有驱动任何出站帧");
        let first_ms = first.1.saturating_duration_since(t0).as_millis();
        let last_ms = frames
            .last()
            .unwrap()
            .1
            .saturating_duration_since(t0)
            .as_millis();
        eprintln!("[延迟] 首帧 {first_ms}ms / 回合收口 {last_ms}ms（下界，不含 axum+网络）");
        assert!(
            first_ms < 5_000,
            "上游内容到出站口首帧超过 5s：{first_ms}ms"
        );
        assert!(last_ms < 8_000, "回合收口超过 8s：{last_ms}ms");

        // ② 信息完整度：区分「上游丢信息」与「流式通道丢信息」。
        eprintln!(
            "[上游] content 分片顺序 = {ANSWER_CHUNKS:?}（全部在 tool_calls 之前，时间顺序正常）"
        );
        let expected: String = ANSWER_CHUNKS.concat();
        let seal_text = frames.iter().find_map(|(f, _)| match &f.event {
            SessionEvent::TimelineAppended {
                entry:
                    TimelineEntry::AssistantBlock {
                        block_kind,
                        content,
                        ..
                    },
            } if *block_kind == AssistantBlockKind::Answer => match content {
                ContentValue::Inline { text } => Some(text.as_str()),
                _ => None,
            },
            _ => None,
        });
        assert_eq!(
            seal_text,
            Some(expected.as_str()),
            "上游全文在 seal 里必须完整；若这里就缺，是上游/解析丢信息，不是展示通道问题"
        );

        let answer_deltas: Vec<(u64, &str)> = frames
            .iter()
            .filter_map(|(f, _)| match &f.event {
                SessionEvent::MessageDelta {
                    block_kind, text, ..
                } if *block_kind == AssistantBlockKind::Answer => Some((f.seq, text.as_str())),
                _ => None,
            })
            .collect();
        let tool_declared_seq = frames.iter().find_map(|(f, _)| match &f.event {
            SessionEvent::ToolDeclared { .. } => Some(f.seq),
            _ => None,
        });
        eprintln!("[完整度] answer MessageDelta 序列 = {answer_deltas:?}");
        eprintln!(
            "[完整度] seal 全文 = {seal_text:?} / 首个 ToolDeclared seq = {tool_declared_seq:?}"
        );

        // 替换语义的终值必须等于上游完整回答，不能停在半句等 seal 事后补。
        assert_eq!(
            answer_deltas.last().map(|(_, t)| *t),
            Some(expected.as_str()),
            "MessageDelta 终值停在半句 → 前端先显示半句、工具卡片插队、完整文本事后才补"
        );
        if let Some(seq) = tool_declared_seq {
            assert!(
                answer_deltas
                    .iter()
                    .any(|(s, t)| *t == expected && *s < seq),
                "完整回答帧必须先于工具声明（seq {seq}）到达出站口"
            );
        }

        // ③ 展示顺序：answer 的 MessageDelta 序列必须连续，中间不得夹工具展示帧。
        let first_answer_seq = answer_deltas.first().map(|(s, _)| *s);
        let last_answer_seq = answer_deltas.last().map(|(s, _)| *s);
        let interleaved: Vec<u64> = frames
            .iter()
            .filter_map(|(f, _)| {
                let is_tool_frame = matches!(
                    f.event,
                    SessionEvent::ToolArgumentsDelta { .. }
                        | SessionEvent::ToolDeclared { .. }
                        | SessionEvent::ToolStateChanged { .. }
                        | SessionEvent::ToolFinished { .. }
                );
                let within = Some(f.seq) > first_answer_seq && Some(f.seq) < last_answer_seq;
                (is_tool_frame && within).then_some(f.seq)
            })
            .collect();
        eprintln!(
            "[顺序] answer 区间 {first_answer_seq:?}..={last_answer_seq:?} 内夹的工具帧 seq = {interleaved:?}"
        );
        assert!(
            interleaved.is_empty(),
            "工具展示帧插在 answer 的两帧 MessageDelta 之间 → 前端呈现『半句 → 工具卡片 → 事后补全』"
        );
    }
}

/// ADR-0033：回合边界 workspace 重解析——resolver 落新根时端口快照与环境块
/// cwd 一并热切换（沙箱 writable roots 跟随）；None / 同根解析保持现状。
#[test]
fn effective_workspace_follows_resolver_at_turn_boundary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ws = dir.path().join("ws");
    let owners =
        Arc::new(OwnerDirectory::open(dir.path().join("qaqh.db")).expect("open owners db"));
    let owner = owners.get_or_create("SES-WSWAP");
    let port = Arc::new(WorkspaceToolPort::new(
        "SES-WSWAP",
        Arc::new(std::sync::Mutex::new(
            qaqh_workspace::registration::build_tool_manager(
                &[],
                std::sync::Arc::new(qaqh_workspace::todo::MemoryTodoHost::default()),
            ),
        )),
        ToolPolicySnapshot::workspace_write(ws.clone()),
        owner.lifecycle(),
        Arc::new(qaqh_process_tools::exec_session::ExecSessionManager::new()),
    ));
    let spec = ProviderRequestSpec::new(
        qaqh_gate::ProviderConfig::responses("https://example.com", "", "m", None),
        1024,
    );
    let loop_ = SessionLoop::new("SES-WSWAP", owner, port.clone(), spec, ws.clone());

    // 未注入钩子：维持现状。
    assert_eq!(loop_.effective_workspace(), ws);
    assert_eq!(port.workspace_root(), ws);

    // resolver 落 worktree 根：快照 + cwd 一并切换，沙箱 writable roots 跟随。
    let wt = ws.join(".qaqh").join("worktree").join("feature-wt");
    std::fs::create_dir_all(&wt).expect("create worktree dir");
    let resolved = wt.clone();
    loop_.set_workspace_resolver(Arc::new(move |_| Some(resolved.clone())));
    assert_eq!(loop_.effective_workspace(), wt);
    assert_eq!(port.workspace_root(), wt, "端口快照必须跟随新根");
    assert!(
        port.sandbox_spec().writable_roots.contains(&wt),
        "沙箱 writable roots 必须跟随新根（否则 exec 锚定旧根）"
    );

    // 同根解析与 None 解析都保持现状（不抖动、不回退到失效路径）。
    assert_eq!(loop_.effective_workspace(), wt);
    loop_.set_workspace_resolver(Arc::new(|_| None));
    assert_eq!(loop_.effective_workspace(), wt);
    assert_eq!(port.workspace_root(), wt);
}

#[cfg(test)]
mod disk_full_fail_closed_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use qaqh_ringing_contract::events::SessionEvent;
    use qaqh_ringing_contract::fact::InputPurpose;
    use qaqh_ringing_contract::state::TimelineEntry;
    use qaqh_session::db::store;
    use qaqh_session::owner::OwnerDirectory;

    use crate::provider_job::ProviderRequestSpec;
    use crate::session_loop::SessionLoop;
    use crate::tool_job::{ToolPolicySnapshot, WorkspaceToolPort};
    use qaqh_ringing::ConversationSendMessage;

    fn sse_server() -> (tiny_http::Server, String) {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
        let addr = format!("http://{}", server.server_addr());
        (server, addr)
    }

    /// 盘满故障注入式 SSE 响应:请求到达后先等 `release` 翻真再回放,
    /// 给测试留出「回合已启动 → 注入盘满 → 放行响应」的确定性窗口。
    fn spawn_gated_sse_response(
        server: tiny_http::Server,
        body: String,
        release: Arc<std::sync::atomic::AtomicBool>,
    ) {
        std::thread::spawn(move || {
            let header =
                tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..])
                    .expect("header");
            for request in server.incoming_requests() {
                while !release.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                let _ = request.respond(
                    tiny_http::Response::from_string(body.clone()).with_header(header.clone()),
                );
            }
        });
    }

    /// v4.5.1 任务1(P0)红→绿反证:盘满(SQLITE_FULL)时答案块 seal 写库失败。
    /// 旧实现 `.await.ok()` 吞错并广播 `TurnTerminal::Completed`(必红);新实现
    /// 中止回合、事件流广播 `TurnTerminal::Failed`,错误码 `response_persist_failed`
    /// 且盘满错误文本明确提示「磁盘空间不足，上下文未保存」,库内无 assistant 块。
    #[tokio::test]
    async fn disk_full_seal_fails_closed_instead_of_lying_completed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let owners =
            Arc::new(OwnerDirectory::open(dir.path().join("qaqh.db")).expect("open"));
        let owner = owners
            .create_session(store::NewSession {
                session_id: "SES-DISKFULL".into(),
                parent_session_id: None,
                title: "diskfull".into(),
                title_source: Some("user".into()),
                cwd: None,
                model: None,
                tool_mode: None,
                model_selection: None,
                custom_tools: None,
                frozen_annotation: None,
                ephemeral: false,
            })
            .await
            .expect("create session");
        let mut rx = owner.subscribe();
        let manager = Arc::new(std::sync::Mutex::new(
            qaqh_workspace::registration::build_tool_manager(
                &[],
                std::sync::Arc::new(qaqh_workspace::todo::MemoryTodoHost::default()),
            ),
        ));
        let port = Arc::new(WorkspaceToolPort::new(
            "SES-DISKFULL",
            manager,
            ToolPolicySnapshot::workspace_write(dir.path().join("ws")),
            owner.lifecycle(),
            Arc::new(qaqh_process_tools::exec_session::ExecSessionManager::new()),
        ));

        // 2 MiB 答案:答案块的溢出页需求远超任何小行,是撞 `max_page_count`
        // 天花板的确定性素材(小行不会撞,2 MiB 必撞)。
        let big_answer = "x".repeat(2 * 1024 * 1024);
        let sse_body = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{big_answer}\"}},\"finish_reason\":null}}]}}\n\n\
             data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}}}\n\n\
             data: [DONE]\n\n"
        );
        let (server, base_url) = sse_server();
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        spawn_gated_sse_response(server, sse_body, Arc::clone(&release));

        let mut provider = qaqh_gate::ProviderConfig::chat(
            &base_url,
            "sk-test",
            "test-model",
            None,
            None,
            qaqh_types::ThinkingParamMode::OpenAi,
            qaqh_types::CacheTokenField::PromptCacheHitTokens,
            true,
            None,
        );
        provider.retry = Some(qaqh_types::RetrySpec {
            max_retries: 1,
            ..Default::default()
        });
        let spec = ProviderRequestSpec::new(provider, 1024);
        let loop_ = SessionLoop::new("SES-DISKFULL", owner, port, spec, dir.path().to_path_buf());

        loop_
            .send_input(ConversationSendMessage {
                message_id: Some("IN-DISK".into()),
                text: "写一篇长文".into(),
                input_purpose: InputPurpose::TriggerTurn,
                as_system: false,
                attachments: None,
                images: vec![],
                inter_agent: None,
                subagent_terminal: None,
                widget_context: None,
            })
            .expect("send input");
        for _ in 0..200 {
            if loop_.active_turn().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(loop_.active_turn().is_some(), "回合必须已启动");

        // 盘满故障注入:写连接的页数上限钉在当前页数 + 4 页。小行(输入块、turn 行)
        // 照常落库,2 MiB 答案块的溢出页必然撞墙 → SQLITE_FULL → DbError::DiskFull。
        owners
            .db()
            .write_blocking(|conn| {
                let pages: i64 = conn
                    .query_row("PRAGMA page_count", [], |row| row.get(0))
                    .map_err(qaqh_session::db::DbError::from)?;
                conn.execute_batch(&format!("PRAGMA max_page_count = {}", pages + 4))
                    .map_err(qaqh_session::db::DbError::from)?;
                Ok::<(), qaqh_session::db::DbError>(())
            })
            .expect("squeeze max_page_count");
        release.store(true, std::sync::atomic::Ordering::SeqCst);

        // 事件流收集到 TurnEnded 为止;盘满时严禁出现 assistant 块落库广播。
        let outcome = loop {
            match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
                Ok(Ok(frame)) => match frame.event {
                    SessionEvent::TimelineAppended { entry } => {
                        assert!(
                            !matches!(entry, TimelineEntry::AssistantBlock { .. }),
                            "盘满时答案块 seal 失败,不得广播 assistant 块(旧实现假成功,必红)"
                        );
                    }
                    SessionEvent::TurnEnded { outcome, .. } => break outcome,
                    _ => {}
                },
                Ok(Err(error)) => panic!("event channel: {error}"),
                Err(_) => panic!("盘满回合必须在时限内收到终态事件"),
            }
        };
        let qaqh_ringing_contract::state::TurnOutcome::Finished {
            terminal,
            error: turn_error,
            ..
        } = &outcome
        else {
            panic!("expected finished outcome, got {outcome:?}");
        };
        assert!(
            matches!(terminal, qaqh_ringing_contract::fact::TurnTerminal::Failed),
            "盘满必须广播 Failed,严禁假报 Completed(旧实现必红): {terminal:?}"
        );
        let turn_error = turn_error.as_ref().expect("failed turn must carry error");
        assert_eq!(turn_error.code, "response_persist_failed");
        assert!(
            turn_error.message.contains("磁盘空间不足，上下文未保存"),
            "盘满错误文本必须明确提示上下文未保存: {}",
            turn_error.message
        );

        // 库内直查:turn 按 failed 收口,assistant 块从未落盘。
        owners
            .db()
            .read_blocking(|conn| {
                let assistant: Option<String> = conn
                    .query_row(
                        "SELECT kind FROM blocks WHERE session_id = 'SES-DISKFULL' AND kind = 'assistant'",
                        [],
                        |row| row.get(0),
                    )
                    .map_err(qaqh_session::db::DbError::from)
                    .unwrap_or(None);
                assert!(
                    assistant.is_none(),
                    "盘满时答案块不得落盘(seal 失败必须如实判死,不是假成功)"
                );
                let outcome_json: String = conn
                    .query_row(
                        "SELECT outcome_json FROM turns WHERE session_id = 'SES-DISKFULL' LIMIT 1",
                        [],
                        |row| row.get(0),
                    )
                    .map_err(qaqh_session::db::DbError::from)?;
                assert!(
                    outcome_json.contains("response_persist_failed"),
                    "turn 终态必须携带持久化失败码: {outcome_json}"
                );
                Ok::<(), qaqh_session::db::DbError>(())
            })
            .expect("verify db state");
    }

    /// v4.5.1 PR #45 Blocker 1 + Blocker 2 端到端红→绿反证:
    /// 当 `seal_response` 因 SQLITE_FULL 失败进入 `finish_persist_failed -> finish_failed`,
    /// 且 `owner.finish_turn` 自身也写库失败(由 `turns` BEFORE UPDATE 触发器模拟真 0 字节
    /// 盘满):
    /// 1. 旧实现直接跳过广播导致前端永久挂在 Running 超时卡死(必红),新实现经
    ///    `emit_turn_persist_failed` 必在时限内收到 `TurnEnded(Failed)`(带
    ///    `response_persist_failed` 与盘满提示)以及 `ActivityChanged(Idle)`;
    /// 2. 旧实现会触发 `notify_terminal` **2 次**(必红),新实现 `terminal_port`
    ///    回调计数精确等于 `1`。
    #[tokio::test]
    async fn finish_turn_persist_failure_unlocks_ui_and_notifies_terminal_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Mutex;
        use qaqh_ringing_contract::fact::{ActivityState, TurnTerminal};
        use crate::session_loop::TurnTerminalNotice;

        let dir = tempfile::tempdir().expect("tempdir");
        let owners =
            Arc::new(OwnerDirectory::open(dir.path().join("qaqh.db")).expect("open"));
        let owner = owners
            .create_session(store::NewSession {
                session_id: "SES-FINISH-FAIL".into(),
                parent_session_id: None,
                title: "finish-fail".into(),
                title_source: Some("user".into()),
                cwd: None,
                model: None,
                tool_mode: None,
                model_selection: None,
                custom_tools: None,
                frozen_annotation: None,
                ephemeral: false,
            })
            .await
            .expect("create session");
        let mut rx = owner.subscribe();
        let manager = Arc::new(Mutex::new(
            qaqh_workspace::registration::build_tool_manager(
                &[],
                Arc::new(qaqh_workspace::todo::MemoryTodoHost::default()),
            ),
        ));
        let port = Arc::new(WorkspaceToolPort::new(
            "SES-FINISH-FAIL",
            manager,
            ToolPolicySnapshot::workspace_write(dir.path().join("ws")),
            owner.lifecycle(),
            Arc::new(qaqh_process_tools::exec_session::ExecSessionManager::new()),
        ));

        let big_answer = "x".repeat(2 * 1024 * 1024);
        let sse_body = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{big_answer}\"}},\"finish_reason\":null}}]}}\n\n\
             data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}}}\n\n\
             data: [DONE]\n\n"
        );
        let (server, base_url) = sse_server();
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        spawn_gated_sse_response(server, sse_body, Arc::clone(&release));

        let mut provider = qaqh_gate::ProviderConfig::chat(
            &base_url,
            "sk-test",
            "test-model",
            None,
            None,
            qaqh_types::ThinkingParamMode::OpenAi,
            qaqh_types::CacheTokenField::PromptCacheHitTokens,
            true,
            None,
        );
        provider.retry = Some(qaqh_types::RetrySpec {
            max_retries: 1,
            ..Default::default()
        });
        let spec = ProviderRequestSpec::new(provider, 1024);
        let loop_ = SessionLoop::new("SES-FINISH-FAIL", owner, port, spec, dir.path().to_path_buf());

        let notice_count = Arc::new(AtomicUsize::new(0));
        let notices: Arc<Mutex<Vec<TurnTerminalNotice>>> = Arc::new(Mutex::new(Vec::new()));
        let count_hook = Arc::clone(&notice_count);
        let notices_hook = Arc::clone(&notices);
        loop_.set_terminal_port(Arc::new(move |notice| {
            count_hook.fetch_add(1, Ordering::SeqCst);
            notices_hook.lock().expect("lock").push(notice);
        }));

        loop_
            .send_input(ConversationSendMessage {
                message_id: Some("IN-FINISH-FAIL".into()),
                text: "触发双失败".into(),
                input_purpose: InputPurpose::TriggerTurn,
                as_system: false,
                attachments: None,
                images: vec![],
                inter_agent: None,
                subagent_terminal: None,
                widget_context: None,
            })
            .expect("send input");
        for _ in 0..200 {
            if loop_.active_turn().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(loop_.active_turn().is_some(), "回合必须已启动");

        // 双重故障注入:1) max_page_count 让 2 MiB seal_response 撞 SQLITE_FULL;
        // 2) turns BEFORE UPDATE 触发器让随后的 owner.finish_turn 本身也落库失败。
        owners
            .db()
            .write_blocking(|conn| {
                let pages: i64 = conn
                    .query_row("PRAGMA page_count", [], |row| row.get(0))
                    .map_err(qaqh_session::db::DbError::from)?;
                conn.execute_batch(&format!(
                    "PRAGMA max_page_count = {};\
                     CREATE TRIGGER block_turn_update BEFORE UPDATE ON turns \
                     BEGIN SELECT RAISE(ABORT, 'simulated finish_turn failure'); END;",
                    pages + 4
                ))
                .map_err(qaqh_session::db::DbError::from)?;
                Ok::<(), qaqh_session::db::DbError>(())
            })
            .expect("inject dual failure");
        release.store(true, Ordering::SeqCst);

        let mut terminal_outcome = None;
        let mut got_idle = false;
        while terminal_outcome.is_none() || !got_idle {
            match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
                Ok(Ok(frame)) => match frame.event {
                    SessionEvent::TurnEnded { outcome, .. } => {
                        terminal_outcome = Some(outcome);
                    }
                    SessionEvent::ActivityChanged {
                        activity: ActivityState::Idle,
                        current_turn_id: None,
                    } => {
                        got_idle = true;
                    }
                    _ => {}
                },
                Ok(Err(error)) => panic!("event channel: {error}"),
                Err(_) => panic!(
                    "finish_turn 失败后前端必须收到 TurnEnded(Failed) 与 ActivityChanged(Idle)(旧实现永久挂死)"
                ),
            }
        }

        let outcome = terminal_outcome.expect("terminal outcome");
        let qaqh_ringing_contract::state::TurnOutcome::Finished {
            terminal,
            error: turn_error,
            ..
        } = &outcome
        else {
            panic!("expected finished outcome, got {outcome:?}");
        };
        assert!(
            matches!(terminal, TurnTerminal::Failed),
            "必须广播 Failed: {terminal:?}"
        );
        let turn_error = turn_error.as_ref().expect("failed turn must carry error");
        assert_eq!(turn_error.code, "response_persist_failed");
        assert!(
            turn_error.message.contains("磁盘空间不足，上下文未保存"),
            "必须保留盘满上下文未保存提示: {}",
            turn_error.message
        );
        assert!(
            turn_error.message.contains("simulated finish_turn failure"),
            "必须合并 finish_turn 落库失败原因: {}",
            turn_error.message
        );

        // 等待回合槽位清零,确保 settle_terminal 与 notify_terminal 已全部收尾。
        for _ in 0..200 {
            if loop_.active_turn().is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(loop_.active_turn().is_none(), "active turn must clear");
        assert_eq!(
            notice_count.load(Ordering::SeqCst),
            1,
            "terminal_port 必须精确触发 1 次(旧实现会触发 2 次)"
        );
        let notices = notices.lock().expect("lock");
        assert!(matches!(notices[0].terminal, TurnTerminal::Failed));
    }

    /// v4.5.1 PR #45 Blocker 2 补充反证:`seal_response` 正常落库,但
    /// `finish_completed` 内 `owner.finish_turn` 自身写库失败时,事件流仍必须
    /// 收到 `TurnEnded(Failed)`(`response_persist_failed`)与 `ActivityChanged(Idle)`,
    /// 且 `terminal_port` 仅触发 1 次 `Failed`(绝不假报 Completed,也不永久挂死)。
    #[tokio::test]
    async fn finish_completed_persist_failure_emits_failed_and_idle() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Mutex;
        use qaqh_ringing_contract::fact::{ActivityState, TurnTerminal};
        use crate::session_loop::TurnTerminalNotice;

        let dir = tempfile::tempdir().expect("tempdir");
        let owners =
            Arc::new(OwnerDirectory::open(dir.path().join("qaqh.db")).expect("open"));
        let owner = owners
            .create_session(store::NewSession {
                session_id: "SES-COMP-FAIL".into(),
                parent_session_id: None,
                title: "comp-fail".into(),
                title_source: Some("user".into()),
                cwd: None,
                model: None,
                tool_mode: None,
                model_selection: None,
                custom_tools: None,
                frozen_annotation: None,
                ephemeral: false,
            })
            .await
            .expect("create session");
        let mut rx = owner.subscribe();
        let manager = Arc::new(Mutex::new(
            qaqh_workspace::registration::build_tool_manager(
                &[],
                Arc::new(qaqh_workspace::todo::MemoryTodoHost::default()),
            ),
        ));
        let port = Arc::new(WorkspaceToolPort::new(
            "SES-COMP-FAIL",
            manager,
            ToolPolicySnapshot::workspace_write(dir.path().join("ws")),
            owner.lifecycle(),
            Arc::new(qaqh_process_tools::exec_session::ExecSessionManager::new()),
        ));

        let sse_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"short answer\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n",
            "data: [DONE]\n\n",
        ).to_string();
        let (server, base_url) = sse_server();
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        spawn_gated_sse_response(server, sse_body, Arc::clone(&release));

        let provider = qaqh_gate::ProviderConfig::chat(
            &base_url,
            "sk-test",
            "test-model",
            None,
            None,
            qaqh_types::ThinkingParamMode::OpenAi,
            qaqh_types::CacheTokenField::PromptCacheHitTokens,
            true,
            None,
        );
        let spec = ProviderRequestSpec::new(provider, 1024);
        let loop_ = SessionLoop::new("SES-COMP-FAIL", owner, port, spec, dir.path().to_path_buf());

        let notice_count = Arc::new(AtomicUsize::new(0));
        let notices: Arc<Mutex<Vec<TurnTerminalNotice>>> = Arc::new(Mutex::new(Vec::new()));
        let count_hook = Arc::clone(&notice_count);
        let notices_hook = Arc::clone(&notices);
        loop_.set_terminal_port(Arc::new(move |notice| {
            count_hook.fetch_add(1, Ordering::SeqCst);
            notices_hook.lock().expect("lock").push(notice);
        }));

        loop_
            .send_input(ConversationSendMessage {
                message_id: Some("IN-COMP-FAIL".into()),
                text: "触发 finish_completed 落库失败".into(),
                input_purpose: InputPurpose::TriggerTurn,
                as_system: false,
                attachments: None,
                images: vec![],
                inter_agent: None,
                subagent_terminal: None,
                widget_context: None,
            })
            .expect("send input");
        for _ in 0..200 {
            if loop_.active_turn().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(loop_.active_turn().is_some(), "回合必须已启动");

        owners
            .db()
            .write_blocking(|conn| {
                conn.execute_batch(
                    "CREATE TRIGGER block_turn_update BEFORE UPDATE ON turns \
                     BEGIN SELECT RAISE(ABORT, 'turns update failed'); END;",
                )
                .map_err(qaqh_session::db::DbError::from)
            })
            .expect("inject trigger");
        release.store(true, Ordering::SeqCst);

        let mut terminal_outcome = None;
        let mut got_idle = false;
        while terminal_outcome.is_none() || !got_idle {
            match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
                Ok(Ok(frame)) => match frame.event {
                    SessionEvent::TurnEnded { outcome, .. } => {
                        terminal_outcome = Some(outcome);
                    }
                    SessionEvent::ActivityChanged {
                        activity: ActivityState::Idle,
                        current_turn_id: None,
                    } => {
                        got_idle = true;
                    }
                    _ => {}
                },
                Ok(Err(error)) => panic!("event channel: {error}"),
                Err(_) => panic!("finish_completed 失败后前端必须收到 TurnEnded(Failed) 与 Idle"),
            }
        }

        let qaqh_ringing_contract::state::TurnOutcome::Finished {
            terminal: TurnTerminal::Failed,
            error: Some(turn_error),
            ..
        } = terminal_outcome.expect("terminal outcome")
        else {
            panic!("expected TurnTerminal::Failed with TurnError");
        };
        assert_eq!(turn_error.code, "response_persist_failed");
        assert!(turn_error.message.contains("turns update failed"));

        for _ in 0..200 {
            if loop_.active_turn().is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(notice_count.load(Ordering::SeqCst), 1);
        let notices = notices.lock().expect("lock");
        assert!(matches!(notices[0].terminal, TurnTerminal::Failed));
    }
}
