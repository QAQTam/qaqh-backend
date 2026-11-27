//! 守卫⑥：**前端渲染预算保护**（宪法 #5"初心守卫"）。

use std::sync::Arc;
use std::time::Duration;

use qaqh_core_vocab::{ContentRef, InputId, InteractionId, TurnId, TurnTerminal, UsageInfo};
use qaqh_ringing_v3::{
    CommandRequest, CompactionSummary, ControlRequest, ControlResponse, ErrorCode, SessionEvent,
    SessionRequest, SessionResponse,
};

use crate::v3_server::facade::session::permission_interaction;
use crate::v3_server::harness::{self, BackendFactory};

const DELTAS: usize = 2000;
const INTERACTIONS: usize = 20;
const BIG_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

pub(crate) async fn guard<F: BackendFactory>(factory: F) {
    let state = harness::state();
    let (client_transport, server_transport) = factory.pair().await;
    let (_server, _server_task) = harness::spawn_server(server_transport, Arc::clone(&state)).await;
    let mut client = harness::connect(client_transport, harness::owner_hello("guard-06")).await;
    let mut blobs = client.take_blob_inbox().expect("blob 收件箱");

    let session = harness::session_for(&state, &harness::owner_device());
    let session_id = session.id.clone();
    client
        .call(ControlRequest::Session(SessionRequest::Attach {
            session_id: session_id.clone(),
            since_seq: 0,
        }))
        .await
        .expect("Attach");
    let mut stream = client.attach_session(&session_id, 0).await.expect("事件流");

    // ── 甲：Replaceable 洪峰 + 交错的 Reliable 审批 + 最后一条 Reliable 终态 ──
    let block_id = harness::block(201);
    let mut expected = String::new();
    let interaction_every = DELTAS / INTERACTIONS;
    for index in 0..DELTAS {
        let piece = format!("t{index:05},");
        expected.push_str(&piece);
        session.emit(harness::answer(&session_id, &block_id, piece));
        if index % 256 == 255 {
            // 有界订阅队列刻意为 512；每 256 帧让步一次防单线程紧密循环在调度极忙时
            // 尚未让出执行权就先挤爆队列触发慢消费者断流，同时仍保持 256 帧级批量合帧压力。
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        if index % interaction_every == 0 {
            let interaction_id =
                InteractionId::new(format!("guard-ia-{index:04}")).expect("interaction id");
            session.emit(SessionEvent::InteractionRequested {
                session_id: session_id.clone(),
                interaction: permission_interaction(
                    interaction_id,
                    "允许执行？",
                    format!("guard-ia-{index:04}"),
                ),
            });
        }
    }
    session.emit(SessionEvent::TurnFinished {
        session_id: session_id.clone(),
        turn_id: TurnId::new("guard-turn-1").expect("turn id"),
        terminal: TurnTerminal::Completed,
        usage: UsageInfo::default(),
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut at_terminal: Option<String> = None;
    let mut answer_accum = String::new();
    let mut interactions = 0usize;
    while at_terminal.is_none() {
        let frame = tokio::time::timeout_at(deadline, stream.next_frame())
            .await
            .expect("事件流 30s 无进展")
            .expect("事件帧");
        match &frame.event {
            SessionEvent::AnswerDelta {
                block_id: bid,
                text,
                ..
            } if bid == &block_id => {
                answer_accum.push_str(text);
            }
            SessionEvent::InteractionRequested { .. } => interactions += 1,
            SessionEvent::TurnFinished { .. } => {
                at_terminal = Some(answer_accum.clone());
            }
            _ => {}
        }
    }
    let at_terminal = at_terminal.expect("终态必须到达");
    assert_eq!(
        at_terminal, expected,
        "终态到达时增量还没齐：`Reliable` 抢到了已合并增量的前面"
    );
    let stats = stream.stats().clone();
    assert_eq!(answer_accum, expected, "合帧吃掉或错排了字节");
    assert_eq!(stats.ordering_violations, 0, "seq 出现倒退");
    assert_eq!(interactions, INTERACTIONS);
    let published = DELTAS + INTERACTIONS + 1;
    assert!((stats.frames as usize) < published);

    // ── 乙：大输出必须外置（8 MiB 正文绝不内联） ──
    let big = "z".repeat(BIG_OUTPUT_BYTES);
    client
        .call(ControlRequest::Command(CommandRequest::SendPrompt {
            session_id: session_id.clone(),
            input_id: InputId::new("guard-06-big").expect("input id"),
            text: big,
            attachments: Vec::new(),
            images: Vec::new(),
        }))
        .await
        .expect("大输出回合必须受理");

    let mut spilled_preview = 0usize;
    let mut blob_ref: Option<(qaqh_core_vocab::ContentHash, u64)> = None;
    while blob_ref.is_none() || spilled_preview == 0 {
        let frame = tokio::time::timeout_at(deadline, stream.next_frame())
            .await
            .expect("等外置引用超时")
            .expect("事件帧");
        match &frame.event {
            SessionEvent::ExecOutputDelta { spilled, text, .. } if *spilled => {
                spilled_preview = text.len();
            }
            SessionEvent::ExecSessionUpdated { exec, .. } => {
                if let Some(ContentRef::Blob { hash, len }) = &exec.output {
                    blob_ref = Some((*hash, *len));
                }
            }
            _ => {}
        }
    }
    let (hash, len) = blob_ref.expect("外置引用");
    assert_eq!(len, BIG_OUTPUT_BYTES as u64);
    assert!(spilled_preview > 0 && spilled_preview <= 1024);
    let assembled = tokio::time::timeout(Duration::from_secs(30), blobs.next())
        .await
        .expect("blob 必须在 30s 内到达")
        .expect("blob 收件箱不该关闭");
    assert_eq!(assembled.hash, hash);
    assert_eq!(assembled.bytes.len(), BIG_OUTPUT_BYTES);

    // ── 丙：固定压缩独立于 turns 窗口 + 翻页代际校验 ──
    session.compact(CompactionSummary {
        pinned_at_ms: 1_700_000_000_000,
        blocks_compacted: 42,
        summary: "固定摘要（独立于 turns 窗口）".to_string(),
    });
    let snapshot = match client
        .call(ControlRequest::Session(
            SessionRequest::GetSessionSnapshot {
                session_id: session_id.clone(),
                tail_blocks: 1,
                tail_turns: 32,
            },
        ))
        .await
        .expect("快照")
    {
        ControlResponse::Session(SessionResponse::Snapshot(snapshot)) => snapshot,
        other => panic!("快照回执形状不符：{other:?}"),
    };
    assert!(snapshot.pinned_compaction.is_some());
    assert!(snapshot.tail_blocks.len() <= 1);
    assert!(!snapshot.exec_sessions.is_empty());
    let stale = client
        .call(ControlRequest::Session(SessionRequest::TranscriptPage {
            session_id: session_id.clone(),
            before_ord: 0,
            limit: 10,
            timeline_epoch: snapshot.timeline_epoch.saturating_sub(1),
        }))
        .await
        .expect_err("陈旧代际必须拒绝");
    assert_eq!(stale.code(), Some(ErrorCode::StaleCursor));
    let page = match client
        .call(ControlRequest::Session(SessionRequest::TranscriptPage {
            session_id: session_id.clone(),
            before_ord: 0,
            limit: 10,
            timeline_epoch: snapshot.timeline_epoch,
        }))
        .await
        .expect("当代际必须能翻页")
    {
        ControlResponse::Session(SessionResponse::Page(page)) => page,
        other => panic!("翻页回执形状不符：{other:?}"),
    };
    assert_eq!(page.timeline_epoch, snapshot.timeline_epoch);
    assert!(!page.blocks.is_empty());
    for block in &page.blocks {
        if let Some(tool_call_id) = &block.tool_call_id {
            assert!(
                page.tool_calls
                    .iter()
                    .any(|call| &call.tool_call_id == tool_call_id)
            );
        }
    }

    println!(
        "[v3-guard-06/{}] 合帧：发布 {} 帧 → 收到 {} 帧（合掉 {}）；增量字节 {} B 全对齐；外置 {} MiB（预览 {} B）；代际 {} → StaleCursor 生效",
        F::name(),
        published,
        stats.frames,
        published as u64 - stats.frames,
        stats.delta_bytes,
        BIG_OUTPUT_BYTES / (1024 * 1024),
        spilled_preview,
        snapshot.timeline_epoch
    );
}
