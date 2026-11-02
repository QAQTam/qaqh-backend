//! 新回合循环测试:Stop 回合端到端(输入 → 流式 → seal → 终态),
//! 全部状态/事件经 SessionOwner 的 SQLite 业务表与单流广播。

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use qaqh_ringing_contract::events::{SessionEvent, SessionEventFrame};
    use qaqh_ringing_contract::fact::InputPurpose;
    use qaqh_ringing_contract::state::TimelineEntry;
    use qaqh_session::db::{store, Db};
    use qaqh_session::owner::OwnerDirectory;

    use crate::provider_job::ProviderRequestSpec;
    use crate::session_loop::{LoopInput, SessionLoop};
    use crate::tool_job::{ToolPolicySnapshot, WorkspaceToolPort};

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

    fn spawn_sse(server: tiny_http::Server) {
        std::thread::spawn(move || {
            let header = tiny_http::Header::from_bytes(
                &b"Content-Type"[..],
                &b"text/event-stream"[..],
            )
            .expect("header");
            for request in server.incoming_requests() {
                let _ = request.respond(
                    tiny_http::Response::from_string(CHAT_SSE).with_header(header.clone()),
                );
            }
        });
    }

    async fn test_loop(
        dir: &std::path::Path,
    ) -> (Arc<SessionLoop>, Arc<OwnerDirectory>, tokio::sync::broadcast::Receiver<SessionEventFrame>) {
        let owners = Arc::new(
            OwnerDirectory::open(dir.join("qaqh.db")).expect("open owners db"),
        );
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
            None,
            owner.lifecycle(),
        ));
        let (_, base_url) = { let (server, addr) = sse_server(); spawn_sse(server); ((), addr) };
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
            .send_input(LoopInput {
                input_id: Some("IN1".into()),
                text: "打个招呼".into(),
                purpose: InputPurpose::TriggerTurn,
                as_system: false,
                attachments: vec![],
            })
            .expect("send input");

        // 事件流:TurnStarted → ActivityChanged → TimelineAppended(input)
        // → MessageDelta(流式)→ TimelineAppended(assistant seal)
        // → TurnEnded → ActivityChanged。
        assert!(matches!(recv_event(&mut rx).await, SessionEvent::TurnStarted { .. }));
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
}
