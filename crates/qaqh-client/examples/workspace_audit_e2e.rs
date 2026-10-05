//! 真实链路 e2e：把一个会话建在指定工作区，让**真实模型**用 exec 跑一个会改文件
//! 的脚本，然后把事件流打出来。用于人工核对 workspace-audit 注入在真 daemon 里
//! 确实生效（进程内测试已覆盖逻辑，这里覆盖传输/装配层与真实耗时）。
//!
//! 用法（先备好隔离数据根与 config，见 docs/plan-workspace-diff-injection.md）：
//!   $env:USERPROFILE = '<隔离 home>'
//!   cargo run -p qaqh-client --example workspace_audit_e2e -- <workspaceDir> [seconds]
//!
//! 提示词写死为「跑 python cleanup.py 并汇报」，因为它要触发的正是 exec 盲区：
//! 脚本自报成功、却把 app.py 清空了。

use std::sync::Arc;
use std::time::Duration;

use qaqh_client::{
    Client, ClientHandlers, ClientOptions, CommandOptions, ControlCommand, ConversationCommand,
    QueryRequest, RingingCommand,
};

const PROMPT: &str = concat!(
    "当前目录下有个 cleanup.py。用 exec 工具运行它（python cleanup.py），",
    "然后只根据工具给你的信息告诉我 app.py 现在是什么状态。",
    "不要使用 read 工具，不要读任何文件。",
);

fn main() {
    let Some(workspace) = std::env::args().nth(1) else {
        eprintln!("usage: workspace_audit_e2e <workspaceDir> [seconds]");
        std::process::exit(2);
    };
    let seconds: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(90);

    let handlers = ClientHandlers {
        on_liveness: Arc::new(|| {}),
        on_v2_event: Arc::new(|session_id, event| {
            if let Ok(v) = serde_json::to_value(&event.payload) {
                let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("?");
                println!("[event] seed={session_id} kind={kind}");
            }
        }),
        on_timeline_entry: Arc::new(|seed, entry| {
            if let Ok(v) = serde_json::to_value(&entry) {
                let t = v
                    .get("event")
                    .and_then(|e| e.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("?");
                println!("[timeline] seed={seed} {t}");
            }
        }),
        ..Default::default()
    };

    let rt = qaqh_client::runtime_handle();
    rt.block_on(async {
        let client = Client::connect_async(ClientOptions {
            handlers,
            // 由外部先把 daemon 起起来，确保跑的是我们刚构建的那个二进制。
            launch_daemon_if_missing: false,
            ..Default::default()
        })
        .await
        .expect("connect to daemon");

        let ack = client
            .send_command(
                None,
                RingingCommand::Control(ControlCommand::SessionCreate {
                    close_current: false,
                    cwd: Some(workspace.clone()),
                    tool_mode: None,
                    custom_tools: Vec::new(),
                }),
                CommandOptions::default(),
            )
            .await
            .expect("session_create");
        println!("[cmd] session_create ack = {ack:?}");

        let mut session_id = None;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let sessions = client
                .query(QueryRequest::SessionList)
                .await
                .expect("session.list");
            let arr = sessions
                .as_array()
                .cloned()
                .or_else(|| sessions.get("sessions").and_then(|s| s.as_array()).cloned())
                .unwrap_or_default();
            if let Some(first) = arr
                .iter()
                .rev()
                .find_map(|s| s.get("session_id").and_then(|v| v.as_str()).map(String::from))
            {
                session_id = Some(first);
                break;
            }
        }
        let session_id = session_id.expect("daemon did not report a session");
        println!("[session] {session_id}");

        client.attach(&session_id).await.expect("attach");
        let command_id = uuid::Uuid::new_v4().to_string();
        let ack = client
            .send_command(
                Some(&session_id),
                RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
                    text: PROMPT.to_string(),
                    images: vec![],
                    attachments: None,
                    message_id: None,
                    input_purpose: qaqh_domain::ConversationInputPurpose::TriggerTurn,
                    as_system: false,
                    inter_agent: None,
                    subagent_terminal: None,
                }),
                CommandOptions {
                    command_id: Some(command_id.clone()),
                    ..Default::default()
                },
            )
            .await
            .expect("send_message");
        println!("[cmd] send_message ack = {ack:?}");

        println!("[wait] observing {seconds}s...");
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        let receipt = client.command_status(&command_id).await;
        println!("[status] command_status = {receipt:?}");
        println!("[done] session={session_id}");
    });
}
