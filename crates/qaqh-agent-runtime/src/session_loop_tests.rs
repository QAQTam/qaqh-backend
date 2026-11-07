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
                effort: None,
                profile: None,
                mode: None,
                tool_mode: None,
                model_selection: None,
                custom_tools: None,
                skills: None,
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
