//! Ringing write-path smoke test: create a **fixture** session in an isolated
//! data root, send one edit-tool instruction into it, and observe the event
//! echo — exercising command/ack/command_status and the canonical v2 event
//! stream end to end.
//!
//! Usage (an isolated data root is mandatory, see [`isolated_data_root`]):
//!   $env:QAQH_DATA_DIR = "F:\QAQ-Harness\.qaqh-test-home\.qaqh"
//!   cargo run -p qaqh-client --example command
//!
//! V4 链路验证:主应用应显示工具胶囊/总结行/抽屉。
//!
//! 注意:本 example 会投递一次真实的文件编辑。因此它**只**跑在显式隔离的数据根上,
//! 且只驱动本次运行自己建的夹具会话——绝不借用既有会话(真实根里那就是用户正在
//! 用的那条),也不允许默认落回 `<USERPROFILE>\.qaqh`。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use qaqh_client::{
    Client, ClientHandlers, ClientOptions, CommandOptions, ControlCommand, ConversationCommand,
    QueryRequest, RingingCommand,
};

/// 隔离数据根守卫:`QAQH_DATA_DIR` 必须显式指向非真实根,否则拒绝启动。
///
/// 这条守卫管的是「客户端连到哪个 daemon」:`data_dir()`(platform.rs:57)与
/// `daemon_discovery_path()`(platform.rs:309)都以它为根,所以设好之后只会发现/
/// 拉起隔离根上的 daemon,日常那个真实 `<home>/.qaqh` 上的 daemon 不会被连上。
///
/// 之前的版本没有任何守卫,直接命中真实 daemon 并取 `session.list` 首条会话开枪。
fn isolated_data_root() -> PathBuf {
    let configured = std::env::var("QAQH_DATA_DIR")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let Some(configured) = configured else {
        eprintln!(
            "refusing to run: QAQH_DATA_DIR is unset, so this smoke would land on the real \
             data root and drive the live session list.\n\
             set it first, e.g.  $env:QAQH_DATA_DIR = \"F:\\QAQ-Harness\\.qaqh-test-home\\.qaqh\""
        );
        std::process::exit(2);
    };
    let root = PathBuf::from(&configured);
    let real = default_data_root();
    // 文本归一挡大小写/分隔符/尾斜杠,canonical 兜住同一目录的另一种写法
    // (`%USERPROFILE%\.qaqh\.`、`C:/Users/../Users/x/.qaqh`、链接目录)。
    let same_text = qaqh_types::platform::normalized_path_text(&root)
        == qaqh_types::platform::normalized_path_text(&real);
    let same_dir = match (std::fs::canonicalize(&root), std::fs::canonicalize(&real)) {
        (Ok(lhs), Ok(rhs)) => {
            qaqh_types::platform::normalized_path_text(&lhs)
                == qaqh_types::platform::normalized_path_text(&rhs)
        }
        _ => false,
    };
    if same_text || same_dir {
        eprintln!(
            "refusing to run: QAQH_DATA_DIR points at the real data root ({}).\n\
             This smoke edits a file and would pollute the live session list.",
            real.display()
        );
        std::process::exit(2);
    }
    // daemon 侧同一道守卫要求这个逃生口(platform.rs:147-166);在进程内设好,
    // `launch_daemon_if_missing` 拉起的 daemon 会继承它。
    //
    // SAFETY: 在 main 起点、任何线程与 tokio 运行时创建之前调用,此刻不存在
    // 并发读取环境变量的线程。
    unsafe { std::env::set_var("QAQH_ALLOW_TEST_DATA_ROOT", "1") };
    root
}

/// 不带 `QAQH_DATA_DIR` 覆写时的真实根——镜像 `data_dir()`(platform.rs:57-75)的
/// 默认分支。只用来把「这就是真实根」判出来,不参与路径解析。
fn default_data_root() -> PathBuf {
    let home = qaqh_types::platform::home_dir();
    if cfg!(windows) {
        home.join(".qaqh")
    } else {
        std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".config"))
            .join("qaqh")
    }
}

/// `session.list` 的 seed 集合(容忍数组 / `{ sessions: [...] }` 两种形状)。
async fn session_ids(client: &Client) -> Vec<String> {
    let listed = client
        .query(QueryRequest::SessionList)
        .await
        .expect("session.list");
    listed
        .as_array()
        .or_else(|| listed.get("sessions").and_then(serde_json::Value::as_array))
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.get("session_id").and_then(serde_json::Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// 建一个**本次运行专属**的夹具会话,返回它的 seed。
///
/// 用前后 diff 找新 seed,而不是取列表首条:隔离根里可能躺着上一次运行留下的
/// 夹具会话,取首条就会把指令投给那条(换成真实根,就是投给用户的会话)。
async fn create_fixture_session(client: &Client) -> String {
    let before = session_ids(client).await;
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
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(300)).await;
        if let Some(fresh) = session_ids(client)
            .await
            .into_iter()
            .find(|id| !before.contains(id))
        {
            return fresh;
        }
    }
    panic!("session_create did not surface a new seed within 6s — on the wrong data root?");
}

fn main() {
    let data_root = isolated_data_root();
    println!("[env] data root = {}", data_root.display());

    let handlers = ClientHandlers {
        on_liveness: std::sync::Arc::new(|| {}),
        on_v2_event: Arc::new(|session_id, event| {
            println!(
                "[event] seed={session_id} id={} delivery={:?}",
                event.event_id, event.delivery
            );
            if let Ok(v) = serde_json::to_value(&event.payload) {
                let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("?");
                println!("[event]   kind={kind}");
            }
        }),
        on_v2_reset: Arc::new(|session_id, reset| {
            println!("[reset] seed={session_id} reason={:?}", reset.reason);
        }),
        on_v2_status: Arc::new(|session_id, status| {
            println!("[status] seed={session_id} status={status:?}");
        }),
        ..Default::default()
    };

    let rt = qaqh_client::runtime_handle();
    rt.block_on(async {
        let client = Client::connect_async(ClientOptions {
            handlers,
            // 守卫已钉住隔离根,自动拉起也只会拉起隔离根上的 daemon,
            // 不会去碰真实根上的那个。
            launch_daemon_if_missing: true,
            ..Default::default()
        })
        .await
        .expect("connect");

        // 1. Fixture session. Never borrow an existing session as the subject.
        let session_id = create_fixture_session(&client).await;
        println!("[query] fixture seed = {session_id}");

        // 2. Attach the seed (Ringing v1: session_resume records ownership),
        //    then send a conversation message (conversation channel).
        client.attach(&session_id).await.expect("attach");
        println!("[cmd] attached session {session_id}");
        let command_id = uuid::Uuid::new_v4().to_string();
        let ack = client
            .send_command(
                Some(&session_id),
                RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
                    text: "请执行一次文件编辑（edit 工具）：在 ../qaqh-winui-app/apps/winui/src/diff_drawer.rs 的顶部模块文档注释里追加一行：//! V4 链路验证 #2：总结行修复后的真实 edit 工具事件。只做这一个改动，不要改其他文件。".to_string(),
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
                    expected_revision: None,
                    driver_epoch: None,
                },
            )
            .await
            .expect("send_message");
        println!("[cmd] send_message ack = {ack:?}");

        // 3. Observe the event echo briefly (agent turn runs after this CLI exits).
        println!("[wait] observing events for 6s...");
        tokio::time::sleep(Duration::from_secs(6)).await;

        // 4. Resolve uncertainty: command receipt (works even after success).
        let receipt = client.command_status(&command_id).await;
        println!("[status] command_status = {receipt:?}");

        println!("[done] write-path smoke complete");
    });
}
