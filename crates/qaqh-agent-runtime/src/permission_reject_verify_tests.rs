//! 权限拒绝语义的端到端裁决测试（沙箱开启 + WorkspaceWrite 档，Exec 必弹审批）。
//!
//! 验证两件事：
//! 1. 用户拒绝（`approved:false`）后，命令**绝不执行**，模型可见文本是
//!    "denied by user"，回合继续（非取消）——门禁语义的正面证据。
//! 2. 拒绝先于取消清扫到达时被静默吞掉、事实层记为 `cancelled` 的缺陷路径
//!    （session_loop.rs `request_permission` 的 select! 竞态）——前后不一的根因。

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use qaqh_ringing_contract::fact::InputPurpose;
    use qaqh_session::db::store;
    use qaqh_session::owner::OwnerDirectory;

    use crate::provider_job::ProviderRequestSpec;
    use crate::session_loop::{PermissionAnswer, SessionLoop};
    use crate::tool_job::{ToolPolicySnapshot, WorkspaceToolPort};
    use qaqh_ringing::ConversationSendMessage;

    const STOP_SSE: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],",
        "\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    );

    /// 依次吐出 `bodies`，耗尽后对后续请求一律回 STOP（回合自然收尾）。
    fn spawn_seq_sse(server: tiny_http::Server, bodies: Vec<String>) {
        std::thread::spawn(move || {
            let header =
                tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..])
                    .expect("header");
            let mut queue = bodies.into_iter();
            for request in server.incoming_requests() {
                let body = queue.next().unwrap_or_else(|| STOP_SSE.to_string());
                let _ = request
                    .respond(tiny_http::Response::from_string(body).with_header(header.clone()));
            }
        });
    }

    fn tool_calls_sse(call_id: &str, cmd: &str) -> String {
        let tool_call = serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {
                    "role": "assistant",
                    "tool_calls": [{
                        "index": 0,
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": "exec_command",
                            "arguments": serde_json::json!({
                                "cmd": cmd,
                                "with_escalated_permissions": true,
                                "justification": "test permission rejection"
                            }).to_string()
                        }
                    }]
                },
                "finish_reason": null
            }]
        });
        let done = serde_json::json!({
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }]
        });
        format!("data: {tool_call}\n\ndata: {done}\n\ndata: [DONE]\n\n")
    }

    /// 与 session_loop_tests::test_loop 同构：沙箱开启(workspace_write 快照
    /// `os_sandbox_enabled=true`)+ WorkspaceWrite 档 ⇒ exec_command 必须弹审批。
    async fn sandboxed_loop(
        dir: &std::path::Path,
        first_response: String,
    ) -> (Arc<SessionLoop>, Arc<OwnerDirectory>) {
        let owners = Arc::new(OwnerDirectory::open(dir.join("qaqh.db")).expect("open owners db"));
        let owner = owners
            .create_session(store::NewSession {
                session_id: "SES-REJECT".into(),
                parent_session_id: None,
                title: "reject-verify".into(),
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
        let manager = Arc::new(std::sync::Mutex::new(
            qaqh_workspace::registration::build_tool_manager(
                &[],
                std::sync::Arc::new(qaqh_workspace::todo::MemoryTodoHost::default()),
            ),
        ));
        let port = Arc::new(WorkspaceToolPort::new(
            "SES-REJECT",
            manager,
            ToolPolicySnapshot::workspace_write(dir.join("ws")),
            owner.lifecycle(),
            Arc::new(qaqh_process_tools::exec_session::ExecSessionManager::new()),
        ));
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
        let addr = format!("http://{}", server.server_addr());
        spawn_seq_sse(server, vec![first_response]);
        let mut provider = qaqh_gate::ProviderConfig::chat(
            &addr,
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
        let loop_ = SessionLoop::new("SES-REJECT", owner, port, spec, dir.to_path_buf());
        (loop_, owners)
    }

    async fn send_trigger(loop_: &Arc<SessionLoop>, text: &str) {
        loop_
            .send_input(ConversationSendMessage {
                message_id: Some(format!("IN-{text}")),
                text: text.to_string(),
                input_purpose: InputPurpose::TriggerTurn,
                as_system: false,
                attachments: None,
                images: vec![],
                inter_agent: None,
                subagent_terminal: None,
                widget_context: None,
            })
            .expect("send input");
    }

    /// 轮询直到 `predicate` 命中，返回最后一次快照（JSON）。
    async fn wait_for(
        owners: &OwnerDirectory,
        predicate: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let owner = owners.get_or_create("SES-REJECT");
        let mut last = serde_json::Value::Null;
        for _ in 0..500 {
            let state = owner.snapshot().await.expect("snapshot");
            let value = serde_json::to_value(state).expect("serialize snapshot");
            if predicate(&value) {
                return value;
            }
            last = value;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition not met within polling window: {last}");
    }

    async fn wait_permission_card(owners: &OwnerDirectory) {
        wait_for(owners, |v| {
            v["activity"] == "waiting_user"
                && v["interactions"]
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
        })
        .await;
    }

    /// 核心裁决:开着沙箱和权限,用户点拒绝 ⇒ 命令不执行、模型看到
    /// "denied by user"、回合继续(非取消)。
    #[tokio::test]
    async fn user_reject_denies_exec_and_the_command_never_runs() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 绝对路径落点:不依赖 exec 的 cwd 语义,执行与否由文件存在性直接裁决。
        let marker = dir.path().join("executed-marker.txt");
        let cmd = format!("echo marker-written > {}", marker.display());
        let (loop_, owners) =
            sandboxed_loop(dir.path(), tool_calls_sse("call-permission-reject", &cmd)).await;
        send_trigger(&loop_, "需要执行命令").await;

        wait_permission_card(&owners).await;

        // 前端"拒绝" → daemon → respond_permission(approved:false)。
        let delivered = loop_.respond_permission(
            "call-permission-reject",
            PermissionAnswer {
                approved: false,
                trust_folder: false,
                scope: qaqh_ringing_contract::PermissionGrantScope::Once,
            },
        );
        assert!(delivered, "pending 权限必须接住拒绝应答");

        let settled = wait_for(&owners, |v| {
            v["turns"].as_array().is_some_and(|turns| {
                turns
                    .first()
                    .and_then(|t| t["outcome"]["data"]["terminal"].as_str())
                    .is_some()
            })
        })
        .await;

        // 1) 工具结算 = Failed{permission_denied,"denied by user"},不是 cancelled。
        let outcome = &settled["tools"][0]["state"]["outcome"];
        assert_eq!(outcome["kind"], "failed", "拒绝必须按失败结算: {outcome}");
        assert_eq!(outcome["error"]["code"], "permission_denied");
        assert_eq!(outcome["error"]["message"], "denied by user");
        assert_ne!(
            settled["turns"][0]["outcome"]["data"]["terminal"], "cancelled",
            "用户拒绝 ≠ 回合取消"
        );

        // 2) 命令从未执行:侧效文件不存在。
        assert!(
            !marker.exists(),
            "拒绝后 exec_command 不得执行,但侧效文件出现了"
        );

        // 3) 模型可见文本 = "denied by user"(timeline ToolResult)。
        assert!(
            serde_json::to_string(&settled)
                .expect("serialize")
                .contains("denied by user"),
            "模型可见的工具结果必须如实说被拒绝"
        );

        // 4) 回合正常继续到下一轮(拿到 STOP 结尾),交互卡已解决。
        assert_eq!(settled["activity"], "idle");
        assert!(
            settled["interactions"]
                .as_array()
                .is_none_or(|items| items.is_empty()),
            "拒绝后交互不得残留 pending"
        );
    }

    /// 缺陷路径演示:拒绝与取消竞态、取消先落地 ⇒ 拒绝被静默吞掉,
    /// 事实层只留下 "cancelled"——模型看到"用户取消了操作",与前端
    /// "点了拒绝"前后不一(session_loop.rs request_permission 的 select! 竞态)。
    #[tokio::test]
    async fn reject_after_cancel_sweep_is_swallowed_and_recorded_as_cancelled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (loop_, owners) = sandboxed_loop(
            dir.path(),
            tool_calls_sse("call-permission-cancel", "echo should-not-run"),
        )
        .await;
        send_trigger(&loop_, "需要执行命令").await;
        wait_permission_card(&owners).await;

        let turn_id =
            wait_for(&owners, |v| v["current_turn_id"].as_str().is_some()).await["current_turn_id"]
                .as_str()
                .expect("active turn id")
                .to_string();

        // 取消先落地:清扫把挂起交互按 Cancelled 结算、丢弃 oneshot。
        assert!(loop_.cancel_turn(Some(&turn_id)));
        wait_for(&owners, |v| {
            v["tools"][0]["state"]["outcome"]["kind"] == "cancelled"
        })
        .await;

        // 用户此刻的拒绝到达:无人接,静默 false——daemon 侧仍 ACK 成功。
        let delivered = loop_.respond_permission(
            "call-permission-cancel",
            PermissionAnswer {
                approved: false,
                trust_folder: false,
                scope: qaqh_ringing_contract::PermissionGrantScope::Once,
            },
        );
        assert!(!delivered, "清扫后的拒绝应答应无处投递");

        let settled = wait_for(&owners, |v| {
            v["turns"][0]["outcome"]["data"]["terminal"] == "cancelled"
        })
        .await;
        // 事实层:只有"取消",没有"拒绝"的任何痕迹。
        assert_eq!(
            settled["tools"][0]["state"]["outcome"]["reason"], "turn_cancel",
            "拒绝被记成取消——前后不一的记录缺陷"
        );
        assert!(
            !serde_json::to_string(&settled)
                .expect("serialize")
                .contains("denied by user"),
            "拒绝语义彻底丢失,模型只能看到 cancelled"
        );
    }
}
