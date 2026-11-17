//! 用户 shell 终端的**真后端**端到端探针（`--features shell-stream`）。
//!
//! 与 crate 内的定向测试分工不同：那边用假 ws 服务端锁住帧与重连语义，这里跑
//! **真 daemon + 真 ConPTY + 真 axum ws**，把 ADR-0028 §切片 5 的行为验收逐条
//! 打在机器上（不是人肉点一次就完）：
//!
//! | 验收项 | 本探针里的动作 | 判据 |
//! |---|---|---|
//! | ① 开终端立刻看到提示符 | `shell.open` 后订阅，等下行裸字节 | 拿到非空字节 |
//! | ② 打字有回显 | `stdin("echo …\\r\n")` | 累积输出里出现 marker |
//! | ③ resize 后几何跟着变 | `resize(120,40)` + `shell.list` | cols/rows == 120/40 |
//! | ④ 输入 exit 收到 `closed` | `stdin("exit\r\n")` | `closed{reason:"child_exited"}` |
//! | ⑤ 能力位而不是版本猜测 | 连接后读 `capabilities.shell_stream` | 为 true 才继续 |
//!
//! 用法（**不要**拿用户正在用的 daemon 跑，用独立 data dir 起一个）：
//!
//! ```sh
//! QAQH_DATA_DIR=$TMP/qaqh-shell-probe cargo run -p qaqh-daemon -- run   # 另一个终端
//! QAQH_DATA_DIR=$TMP/qaqh-shell-probe cargo run -p qaqh-client --features shell-stream \
//!   --example shell_stream_e2e
//! ```
//!
//! 退出码：全中 0；任何一条不中非 0 并打印缺的是哪一条。

use std::path::PathBuf;
use std::time::Duration;

use qaqh_client::shell_stream::{ShellStream, ShellStreamHandlers, ShellStreamStatus};
use qaqh_client::{Client, ClientOptions, RemoteEndpoint};
use serde_json::{Value, json};
use tokio::sync::mpsc;

const MARKER: &str = "qaqh-shell-probe-ok";

fn discovery_path() -> PathBuf {
    // 与 daemon 同一个根：QAQH_DATA_DIR 指到哪，探针就读哪份 daemon.json。
    qaqh_client::discovery::data_dir().join("daemon.json")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let record: Value = serde_json::from_str(&std::fs::read_to_string(discovery_path())?)?;
    let base_url = record["endpoint"].as_str().expect("daemon.json endpoint").to_string();
    let token = record["token"].as_str().expect("daemon.json token").to_string();
    // 只覆写 `remote`：探针直连指定 data dir 里那份 daemon.json 报的端点。
    let client = Client::connect_async(ClientOptions {
        remote: Some(RemoteEndpoint { base_url, token, tls_fp: None }),
        ..Default::default()
    })
    .await?;

    // ⑤ 能力位：这条流存在与否是 daemon 显式声明的，探针不接受「版本看起来够新」。
    let capabilities = client
        .session_state()
        .await
        .expect("lease 已协商")
        .capabilities;
    assert!(capabilities.shell_stream, "daemon 没声明 shell_stream 能力");

    let (bytes_tx, mut bytes_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (closed_tx, mut closed_rx) = mpsc::unbounded_channel::<String>();
    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<ShellStreamStatus>();
    let handlers = ShellStreamHandlers {
        on_bytes: std::sync::Arc::new(move |_shell_id, bytes| {
            let _ = bytes_tx.send(bytes);
        }),
        on_event: std::sync::Arc::new(move |_shell_id, event| {
            let qaqh_ringing::ShellStreamEvent::Closed { reason } = event;
            let _ = closed_tx.send(reason);
        }),
        on_status: std::sync::Arc::new(move |_shell_id, status| {
            let _ = status_tx.send(status);
        }),
    };

    // ① 开 shell：owner 由 daemon 从身份反查，params 里只有几何与目录（D4）。
    let opened: Value = client
        .service_call("shell.open", json!({ "cols": 100, "rows": 30 }))
        .await?;
    let shell_id = opened["id"].as_str().expect("ShellView.id").to_string();
    println!("[probe] opened {shell_id} pid={} cols={} rows={}",
        opened["pid"].as_i64().unwrap_or_default(),
        opened["cols"].as_i64().unwrap_or_default(),
        opened["rows"].as_i64().unwrap_or_default());

    let stream = ShellStream::start(&shell_id, &client, handlers).await?;
    let mut output = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut saw_prompt = false;
    while tokio::time::Instant::now() < deadline {
        let Ok(bytes) = tokio::time::timeout(Duration::from_millis(400), bytes_rx.recv()).await else {
            continue;
        };
        let Some(bytes) = bytes else { continue };
        output.push_str(&String::from_utf8_lossy(&bytes));
        if !output.is_empty() {
            saw_prompt = true;
            println!("[probe] ① 下行裸字节到达（{} 字节）", output.len());
            break;
        }
    }
    assert!(saw_prompt, "① 失败：订阅后一个字节都没收到");

    // ② 打字有回显。一次 stdin 帧：base64 由 SDK 生成，这里只给原始键入。
    stream.stdin(format!("echo {MARKER}\r\n").as_bytes()).await?;
    let mut saw_marker = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(400), bytes_rx.recv()).await {
            Ok(Some(bytes)) => output.push_str(&String::from_utf8_lossy(&bytes)),
            _ => continue,
        }
        if output.contains(MARKER) {
            saw_marker = true;
            break;
        }
    }
    assert!(saw_marker, "② 失败：输出里找不到 marker");
    println!("[probe] ② stdin 回显确认（累计 {} 字节）", output.len());

    // ③ 几何真值读 shell.list：这条流上没有 ack 帧，resize 的结果只能从这里核实。
    stream.resize(120, 40).await?;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let listed: Value = client.service_call("shell.list", json!({})).await?;
    let mine = listed["shells"]
        .as_array()
        .expect("ShellListResult.shells")
        .iter()
        .find(|entry| entry["id"].as_str() == Some(shell_id.as_str()))
        .expect("shell.list 里没有刚开的那条");
    assert_eq!(mine["cols"].as_i64(), Some(120), "③ 失败：cols 没跟上");
    assert_eq!(mine["rows"].as_i64(), Some(40), "③ 失败：rows 没跟上");
    println!("[probe] ③ resize 已生效（shell.list: 120x40）");

    // ④ 子进程自己退出：退出观察线程必须把 `child_exited` 落到 closed 帧上。
    stream.stdin(b"exit\r\n").await?;
    let reason = tokio::time::timeout(Duration::from_secs(20), closed_rx.recv())
        .await
        .map_err(|_| "④ 失败：等不到 closed 帧")?
        .ok_or("④ 失败：closed 通道断了")?;
    assert_eq!(reason, "child_exited", "④ 失败：终止原因不是 child_exited");
    println!("[probe] ④ closed{{reason:\"{reason}\"}} 已到");

    // 终止之后不该再有第三次重连：状态机必须自己收摊。
    let mut terminal_status = None;
    while let Ok(status) = status_rx.try_recv() {
        if let ShellStreamStatus::Closed { reason } = status {
            terminal_status = Some(reason);
        }
    }
    assert_eq!(terminal_status.as_deref(), Some("child_exited"),
        "传输状态没跟着协议事实落到 Closed");
    // 断言用 match 而不是 unwrap_err：examples 面上 clippy::unwrap_used 是 -D，
    // 而这里要的本来就是「必须失败」这条负向事实。
    match stream.stdin(b"ls\r\n").await {
        Ok(()) => panic!("流已终止却还接受控制帧"),
        Err(error) => assert!(error.to_string().contains("已终止"), "{error}"),
    }
    println!("[probe] 全部通过");
    Ok(())
}
