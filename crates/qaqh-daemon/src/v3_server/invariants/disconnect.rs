//! 守卫③：**断联不杀进程 + 幂等回执 + 非零偏移续读无误报缺口**（`ADR-0042` 铁律 4/5）。

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use qaqh_core_vocab::InputId;
use qaqh_ringing_v3::{
    CommandRequest, ControlRequest, ControlResponse, SessionRequest, encode_frame,
};

use crate::v3_server::harness::{self, BackendFactory};

pub(crate) async fn guard<F: BackendFactory>(factory: F) {
    let state = harness::state();

    // ── 第一个连接 ──
    let (client_transport, server_transport) = factory.pair().await;
    let (_server, _server_task) = harness::spawn_server(server_transport, Arc::clone(&state)).await;
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
    let shell_id = harness::open_shell(&client_a, &session_id).await;
    let shell = state.shells.get(&shell_id).expect("shell 句柄");
    shell.push_output(Bytes::from_static(b"before-disconnect\n"));
    let pre_disconnect_offset = shell.output_offset();

    let request = ControlRequest::Command(CommandRequest::SendPrompt {
        session_id: session_id.clone(),
        input_id: InputId::new("guard-03-input").expect("input id"),
        text: "只执行一次的回合".to_string(),
        attachments: Vec::new(),
        images: Vec::new(),
    });
    let first = client_a
        .call_idempotent("guard-03-idem", request.clone())
        .await
        .expect("首次 SendPrompt");
    assert!(matches!(first, ControlResponse::Command(_)), "回执形状");
    let first_bytes = encode_frame(&first).expect("编码回执");
    assert_eq!(
        session
            .metrics
            .turns_started
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
        "首次投递必须只开一个回合"
    );

    // ── 断联 ──
    drop(client_a);
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
    shell.push_output(Bytes::from_static(b"after-disconnect\n"));

    // ── 第二个连接：重放同一个 command_id ──
    let (client_transport, server_transport) = factory.pair().await;
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
        "重放必须返回逐字节相同的原回执"
    );
    assert_eq!(
        session
            .metrics
            .turns_started
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
        "重放不得再执行一次回合"
    );

    // ── 非零偏移续读：只拉断联之后的新字节，gap 必须为 0 ──
    let mut pty = client_b
        .attach_shell(&shell_id, pre_disconnect_offset)
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
        "从非零偏移 {pre_disconnect_offset} 续读不该误报任何缺口"
    );
    assert_eq!(seen, b"after-disconnect\n");

    println!(
        "[v3-guard-03/{}] 断联后 shell 存活；重放回执 {} B 逐字节相同；回合数=1；从偏移 {} 续读 {} B（gap=0）",
        F::name(),
        first_bytes.len(),
        pre_disconnect_offset,
        seen.len()
    );
}
