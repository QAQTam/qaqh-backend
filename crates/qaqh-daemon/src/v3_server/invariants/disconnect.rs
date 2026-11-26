//! 守卫③：**断联不杀进程 + 幂等回执**（`ADR-0042` 铁律 4）。
//!
//! ## 断联必须只断"看的人"
//!
//! 连接断了，shell 句柄、输出水位、回放历史都必须在；否则"手机切个后台，
//! 长跑命令就没了"——那是 V2 时代最难解释的一类事故。
//!
//! ## 幂等回执必须逐字节相同
//!
//! 断线重放同一个 `command_id`，服务端必须返回**原回执**，且**不得再执行一次**。
//! 这里用 `encode_frame` 比较两次回执的线缆字节：任何一处字段漂移都抓得到。

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use qaqh_core_vocab::InputId;
use qaqh_ringing_v3::{
    CommandRequest, ControlRequest, ControlResponse, SessionRequest, encode_frame,
};
use qaqh_v3_transport::MuxTransport;

use crate::v3_server::harness;

pub(crate) async fn guard<B: MuxTransport>(pair: fn() -> (B, B)) {
    let state = harness::state();

    // ── 第一个连接 ──
    let (client_transport, server_transport) = pair();
    let (_server, _server_task) =
        harness::spawn_server(server_transport, Arc::clone(&state)).await;
    let client_a = harness::connect(client_transport, harness::owner_hello("guard-03")).await;

    let session = harness::session_for(&state, &harness::owner_device());
    let session_id = session.id.clone();
    client_a
        .call(ControlRequest::Session(SessionRequest::Attach {
            session_id: session_id.clone(),
            since_seq: 0,
        }))
        .await
        .expect("Attach");
    let shell_id = harness::open_shell(&client_a).await;
    let shell = state.shells.get(&shell_id).expect("shell 句柄");

    let request = ControlRequest::Command(CommandRequest::SendPrompt {
        session_id: session_id.clone(),
        input_id: InputId::new("guard-03-input").expect("input id"),
        text: "只执行一次的回合".to_string(),
        attachments: Vec::new(),
    });
    let first = client_a
        .call_idempotent("guard-03-idem", request.clone())
        .await
        .expect("首次 SendPrompt");
    assert!(matches!(first, ControlResponse::Command(_)), "回执形状");
    let first_bytes = encode_frame(&first).expect("编码回执");
    assert_eq!(
        session.metrics.turns_started.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "首次投递必须只开一个回合"
    );

    // ── 断联 ──
    drop(client_a);
    // 等驱动权释放（这是"服务端确实看到了断联"的可观测证据，比 sleep 可靠）。
    for _ in 0..300 {
        if session.driver().is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        session.driver().is_none(),
        "断联后驱动权必须释放（否则重连的实例永远投递不了输入）"
    );
    assert!(
        state.shells.get(&shell_id).is_some(),
        "断联把 shell 杀了（断联不杀进程）"
    );
    // 断联之后服务端继续产出字节（真实场景：长跑命令还在输出）。
    shell.push_output(Bytes::from_static(b"after-disconnect\n"));

    // ── 第二个连接：重放同一个 command_id ──
    let (client_transport, server_transport) = pair();
    let (_server2, _server_task2) =
        harness::spawn_server(server_transport, Arc::clone(&state)).await;
    let client_b = harness::connect(client_transport, harness::owner_hello("guard-03")).await;
    client_b
        .call(ControlRequest::Session(SessionRequest::Attach {
            session_id: session_id.clone(),
            since_seq: 0,
        }))
        .await
        .expect("重连后 Attach");
    let replay = client_b
        .call_idempotent("guard-03-idem", request)
        .await
        .expect("重放必须返回回执");
    assert_eq!(
        encode_frame(&replay).expect("编码回执"),
        first_bytes,
        "重放必须返回**逐字节相同**的原回执"
    );
    assert_eq!(
        session.metrics.turns_started.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "重放不得再执行一次回合（幂等不是「再跑一遍但返回旧结果」）"
    );

    // ── 断联后的字节还在：从偏移 0 重拉必须能看到 ──
    let mut pty = client_b
        .attach_shell(&shell_id, 0)
        .await
        .expect("重连后 attach shell");
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !seen
        .windows(b"after-disconnect\n".len())
        .any(|window| window == b"after-disconnect\n")
    {
        let frame = tokio::time::timeout_at(deadline, pty.next_frame())
            .await
            .expect("断联后的字节必须还在回放历史里")
            .expect("PTY 帧");
        seen.extend_from_slice(&frame.bytes);
        assert!(seen.len() <= 64 * 1024, "回放历史里没有目标字节");
    }
    assert_eq!(
        pty.stats().gaps,
        0,
        "从偏移 0 重拉不该出现任何偏移断裂"
    );
    assert_eq!(pty.stats().bytes, seen.len() as u64);

    println!(
        "[v3-guard-03] 断联后 shell 存活；重放回执 {} B 逐字节相同；回合数=1；断联后回放 {} B（gap=0）",
        first_bytes.len(),
        seen.len()
    );
}
