//! Ringing write-path smoke test: create/open a session, send a message,
//! and observe the event echo — exercising command/ack/command_status and
//! the canonical v2 event stream end to end.
//!
//! Usage (against the parallel dev daemon):
//!   $env:QAQH_DATA_DIR = "F:\QAQ-Harness\.qaqh-test-home\.qaqh"
//!   cargo run -p qaqh-client --example command
//!
//! V4 链路验证：默认连真实数据目录（%USERPROFILE%\.qaqh），向当前会话
//! 发一条「执行 edit 工具」指令，主应用应显示工具胶囊/总结行/抽屉。

use std::sync::Arc;
use std::time::Duration;

use qaqh_client::{
    Client, ClientHandlers, ClientOptions, CommandOptions, ControlCommand, ConversationCommand,
    QueryRequest, RingingCommand,
};

fn main() {
    let handlers = ClientHandlers {
        on_liveness: std::sync::Arc::new(|| {}),
        on_v2_event: Arc::new(|seed, event| {
            println!(
                "[event] seed={seed} id={} delivery={:?}",
                event.event_id, event.delivery
            );
            if let Ok(v) = serde_json::to_value(&event.payload) {
                let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("?");
                println!("[event]   kind={kind}");
            }
        }),
        on_v2_reset: Arc::new(|seed, reset| {
            println!("[reset] seed={seed} reason={:?}", reset.reason);
        }),
        on_v2_status: Arc::new(|seed, status| {
            println!("[status] seed={seed} status={status:?}");
        }),
        ..Default::default()
    };

    let rt = qaqh_client::runtime_handle();
    rt.block_on(async {
        let client = Client::connect_async(ClientOptions {
            handlers,
            launch_daemon_if_missing: true,
            ..Default::default()
        })
        .await
        .expect("connect");

        // 1. Session discovery. `session.list` returns a top-level array of
        //    sessions (or `{ sessions: [...] }` from some daemon versions).
        let sessions = client
            .query(QueryRequest::SessionList)
            .await
            .expect("session.list");
        println!("[query] session.list = {sessions}");
        let first_seed = |v: &serde_json::Value| -> Option<String> {
            let arr = v
                .as_array()
                .or_else(|| v.get("sessions").and_then(|s| s.as_array()))?;
            arr.first()
                .and_then(|s| s.get("seed"))
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
        };
        let mut seed = first_seed(&sessions);

        // 2. Create a session when none exists (control channel command).
        if seed.is_none() {
            let ack = client
                .send_command(
                    None,
                    RingingCommand::Control(ControlCommand::SessionCreate {
                        close_current: false,
                        cwd: None,
                        tool_mode: None,
                        custom_tools: Vec::new(),
                    }),
                    CommandOptions::default(),
                )
                .await
                .expect("session_create");
            println!("[cmd] session_create ack = {ack:?}");
            // Creation is confirmed through the event stream; poll session.list.
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let sessions = client
                    .query(QueryRequest::SessionList)
                    .await
                    .expect("session.list re-query");
                if let Some(s) = first_seed(&sessions) {
                    seed = Some(s);
                    break;
                }
            }
        }
        let seed = seed.expect("session seed");

        // 3. Attach the seed (Ringing v1: session_resume records ownership),
        //    then send a conversation message (conversation channel).
        client.attach(&seed).await.expect("attach");
        println!("[cmd] attached session {seed}");
        let command_id = uuid::Uuid::new_v4().to_string();
        let ack = client
            .send_command(
                Some(&seed),
                RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
                    text: "请执行一次文件编辑（edit 工具）：在 ../qaqh-winui-app/apps/winui/src/diff_drawer.rs 的顶部模块文档注释里追加一行：//! V4 链路验证 #2：总结行修复后的真实 edit 工具事件。只做这一个改动，不要改其他文件。".to_string(),
                    images: vec![],
                    attachments: None,
                    message_id: None,
                    input_purpose: qaqh_domain::ConversationInputPurpose::TriggerTurn,
                    as_system: false,
                    subagent_terminal: None,
                }),
                CommandOptions {
                    command_id: Some(command_id.clone()),
                    expected_revision: None,
                    driver_epoch: None,
                },
            )
            .await
            .expect("send_message");
        println!("[cmd] send_message ack = {ack:?}");

        // 4. Observe the event echo briefly (agent turn runs after this CLI exits).
        println!("[wait] observing events for 6s...");
        tokio::time::sleep(Duration::from_secs(6)).await;

        // 5. Resolve uncertainty: command receipt (works even after success).
        let receipt = client.command_status(&command_id).await;
        println!("[status] command_status = {receipt:?}");

        println!("[done] write-path smoke complete");
    });
}
