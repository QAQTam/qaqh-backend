//! 新回合循环测试:Stop 回合端到端(输入 → 流式 → seal → 终态),
//! 全部状态/事件经 SessionOwner 的 SQLite 业务表与单流广播。

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
            qaqh_workspace::registration::build_tool_manager(&[]),
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
        let tool_args = serde_json::json!({ "cmd": "echo permission-cancel-smoke" });
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

    #[tokio::test]
    async fn provider_incomplete_response_finishes_turn_as_failed() {
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
            "provider_incomplete"
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
            .find(|message| message.role == "system")
            .expect("system input must reach provider context");
        assert!(matches!(
            system_input.content.as_slice(),
            [
                qaqh_types::ContentBlock::Text { text },
                qaqh_types::ContentBlock::Image { mime_type, data }
            ] if text == "result from child" && mime_type == "image/png" && data == "aW1hZ2U="
        ));
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

    use crate::handoff::{FOLD_NOTE, self};
    use crate::provider_job::ProviderRequestSpec;
    use crate::session_loop::SessionLoop;
    use crate::tool_job::{ToolPolicySnapshot, WorkspaceToolPort};

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
        let head =
            serde_json::json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]});
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
        let owners = Arc::new(OwnerDirectory::open(dir.join("qaqh.db")).expect("open owners db"));
        let owner = owners
            .create_session(store::NewSession {
                session_id: "SES-HANDOFF".into(),
                parent_session_id: None,
                title: "handoff".into(),
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
            qaqh_workspace::registration::build_tool_manager(&[]),
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
        let dir = tempfile::tempdir().expect("tempdir");
        let previous = std::env::var_os("QAQH_DATA_DIR");
        // 镜像落点由 data_dir() 决定，把根指到临时目录才能断言文件真的写出来。
        // SAFETY: 本用例独占 QAQH_DATA_DIR 的改写并在断言后还原；同 crate 内只有
        // 这一个用例碰它，tempdir 路径互不重叠。
        unsafe { std::env::set_var("QAQH_DATA_DIR", dir.path().join("root")) };

        let doc = serde_json::json!({
            "handoff_doc": "## 待决\n- 下一步查 SIWC 窗口来源",
            "work_summary": "交接测试"
        });
        let h = harness(
            dir.path(),
            vec![(200, tool_call_body("handoff", &doc, "call-h1", &cache_usage()))],
            vec![handoff::tool_def()],
            10_000,
            0.5,
        )
        .await;
        h.loop_
            .send_input(input("开始干活"))
            .expect("send input");

        let state = wait_until(&h.owners, idle_with_turns).await;
        assert_eq!(state["turns"][0]["outcome"]["kind"], "finished");
        assert_eq!(
            state["turns"][0]["outcome"]["data"]["terminal"],
            "completed",
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

    #[tokio::test]
    async fn session_title_sets_business_row_and_prunes_the_surface() {
        // 模型自命名:业务行落 title + title_source=auto,快照带新题,
        // 工具在下一轮请求体里被摘除(handoff 同款控制面纹理,零契约新增)。
        let dir = tempfile::tempdir().expect("tempdir");
        let doc = serde_json::json!({ "title": "daemon 启动崩溃排查" });
        let h = harness(
            dir.path(),
            vec![
                (200, tool_call_body("session_title", &doc, "call-t1", &cache_usage())),
                (200, text_body("命名完成，继续干活")),
            ],
            vec![crate::session_title::tool_def()],
            10_000,
            0.0,
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
        // 第二轮请求体的**声明面**不再含 session_title(摘面当回合生效);
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
                        .map(|tool| {
                            tool["function"]["name"].as_str().unwrap_or("").to_string()
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        assert!(declared(&requests[0]).contains(&"session_title".to_string()));
        assert!(
            !declared(&requests[1]).contains(&"session_title".to_string()),
            "落笔后不得再向模型声明命名工具: {:?}",
            declared(&requests[1])
        );
    }

    /// 占用页脚挂在本轮最后一条工具结果的信封正文尾部：实时遥测，不进历史事实。
    ///
    /// 回归：页脚挂载按 `ContentBlock::Text` 匹配，而工具消息的正文块是
    /// `ToolResult`——永远落空，读数从未真正到达模型。
    #[tokio::test]
    async fn context_footer_rides_the_last_tool_result_of_the_round() {
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
        // 实时遥测不进 durable timeline：重放读到的仍是不带页脚的原文。
        assert!(
            !timeline_tool_result(&state).contains("[#ctx"),
            "历史事实不得携带页脚"
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
            state["turns"][0]["outcome"]["data"]["error"]["code"],
            "context_overflow",
            "超限不得伪装成一般 provider 失败"
        );
        assert_eq!(
            handoff::effective_window(&h.base_url, "test-model", 2000),
            1000,
            "无实测记录时按声明值一半保守收敛"
        );
    }

    /// 硬压力：折叠只动模型投影、提醒以可见事实入库、占用按 hit+miss 口径。
    #[tokio::test]
    async fn hard_pressure_folds_tool_results_and_leaves_visible_inputs() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 960/1000 = 96%：软提醒（>=50%）与硬折叠（>=95%）同轮触发。
        let bad = tool_call_body(
            "handoff",
            &serde_json::json!({ "handoff_doc": "  ", "work_summary": "空正文" }),
            "call-h3",
            &cache_usage(),
        );
        let h = harness(
            dir.path(),
            vec![(200, bad), (200, text_body("收到，这就写文档"))],
            vec![handoff::tool_def()],
            1000,
            0.5,
        )
        .await;
        h.loop_.send_input(input("压力大")).expect("send input");

        let state = wait_until(&h.owners, idle_with_turns).await;
        let requests = h.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert!(
            !requests[0].contains(FOLD_NOTE),
            "第一轮没有任何东西可折叠"
        );
        assert!(
            requests[1].contains(FOLD_NOTE),
            "第二轮必须折叠工具结果"
        );
        assert!(
            !requests[1].contains("不能为空"),
            "折叠后原文不该再进模型上下文"
        );
        assert!(
            requests[1].contains("[系统] 上下文占用 96%"),
            "占用必须按 hit+miss 口径，而不是裸 prompt_tokens 的 6%"
        );
        assert!(requests[1].contains("本轮唯一任务：调用 handoff"));
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
        assert_eq!(watchdog, 2, "提醒与折叠各留一条可见事实");
    }
}
