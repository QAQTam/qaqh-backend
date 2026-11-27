//! 守卫①：**原子订阅 + 有界回放尾**（`ADR-0042` 铁律 4/5）。

use std::sync::Arc;
use std::time::Duration;

use qaqh_ringing_v3::{ControlRequest, ResyncReason, SessionEvent, SessionRequest};

use crate::v3_server::bus::SubscribeOutcome;
use crate::v3_server::harness::{self, BackendFactory};

pub(crate) async fn guard<F: BackendFactory>(factory: F) {
    let state = harness::state();
    let (client_transport, server_transport) = factory.pair().await;
    let (_server, _server_task) = harness::spawn_server(server_transport, Arc::clone(&state)).await;
    let client = harness::connect(client_transport, harness::owner_hello("guard-01")).await;

    let session = harness::session_for(&state, &harness::owner_device());
    let session_id = session.id.clone();
    client
        .call(ControlRequest::Session(SessionRequest::Attach {
            session_id: session_id.clone(),
            since_seq: 0,
        }))
        .await
        .expect("Attach 必须成功");

    // ── 甲：洪峰推进至少 200 帧后在洪峰中段 attach（真正覆盖回放 + 实时无缝交接） ──
    const FLOOD_FRAMES: u64 = 3000;
    let block_id = harness::block(1);
    let flood = {
        let session = Arc::clone(&session);
        let session_id = session_id.clone();
        let block_id = block_id.clone();
        tokio::spawn(async move {
            let mut published = String::new();
            for index in 0..FLOOD_FRAMES {
                let piece = format!("d{index:05};");
                published.push_str(&piece);
                session.emit(harness::answer(&session_id, &block_id, piece));
                if index % 32 == 0 {
                    tokio::task::yield_now().await;
                }
            }
            session.emit(SessionEvent::CatalogChanged {
                session_id: session_id.clone(),
            });
            published
        })
    };

    let wait_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while session.bus.last_seq() < 200 {
        assert!(
            tokio::time::Instant::now() < wait_deadline,
            "洪峰推进 5s 仍未到 200 帧（last_seq={}）",
            session.bus.last_seq()
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    let mut stream = client
        .attach_session(&session_id, 0)
        .await
        .expect("洪峰中 attach 必须成功");
    let attach_seq = stream.attach().seq;
    assert!(
        attach_seq >= 200,
        "洪峰中段 attach 必须带走大量回放尾（实得 attach_seq={attach_seq}）"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut answer_accum = String::new();
    let mut sentinel = false;
    while !sentinel {
        let frame = tokio::time::timeout_at(deadline, stream.next_frame())
            .await
            .expect("事件流 30s 无进展（订阅死锁或写侧卡住）")
            .expect("帧必须可读");
        match &frame.event {
            SessionEvent::AnswerDelta { block_id: bid, text, .. } if bid == &block_id => {
                answer_accum.push_str(text);
            }
            SessionEvent::CatalogChanged { .. } => sentinel = true,
            _ => {}
        }
    }
    let published = flood.await.expect("洪峰任务");
    let stats = stream.stats().clone();
    assert_eq!(
        stats.resync_cycles, 0,
        "洪峰期间 attach 触发了重同步（attach_seq={attach_seq}）"
    );
    assert_eq!(stats.ordering_violations, 0, "seq 出现倒退");
    assert_eq!(answer_accum, published, "回放/合帧吃掉或错排了字节");
    assert!(
        stats.frames <= FLOOD_FRAMES + 2,
        "收到的帧数 {} 竟然多于发布的 {FLOOD_FRAMES}",
        stats.frames
    );

    // ── 乙：游标跌出回放尾必须明确报缺口 ──
    let stale = harness::session_for(&state, &harness::owner_device());
    let stale_id = stale.id.clone();
    let stale_block = harness::block(2);
    let overflow = harness::config().ring_capacity as u64 + 512;
    for _ in 0..overflow {
        stale.emit(harness::answer(&stale_id, &stale_block, "x"));
    }
    assert!(stale.bus.last_seq() > harness::config().ring_capacity as u64);
    let mut gap_stream = client
        .attach_session(&stale_id, 0)
        .await
        .expect("陈旧游标也要能建流");
    let frame = gap_stream.next_frame().await.expect("缺口通知必须到达");
    match frame.event {
        SessionEvent::ResyncRequired {
            reason: ResyncReason::CursorGap,
            timeline_epoch,
            ..
        } => assert_eq!(timeline_epoch, stale.epoch()),
        other => panic!("必须是 CursorGap，实际 {other:?}"),
    }
    assert_eq!(gap_stream.stats().resync_cycles, 1);

    // ── 丙：破坏顺序必红（确定性复刻"先读环、后注册"的那条缝） ──
    let race = harness::session_for(&state, &harness::owner_device());
    let race_id = race.id.clone();
    let race_block = harness::block(3);
    race.emit(harness::answer(&race_id, &race_block, "before"));
    let since = race.bus.last_seq();
    let replay = race.bus.replay_after(since);
    assert!(replay.is_empty());
    race.emit(harness::answer(&race_id, &race_block, "in-seam"));
    let (mut broken_live, _broken_lag) = race.bus.register_only();
    let delivered = broken_live.try_recv();
    assert!(matches!(
        delivered,
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            | Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
    ));
    let lost = 1u64;

    let since = race.bus.last_seq();
    race.emit(harness::answer(&race_id, &race_block, "seam-2"));
    match race.bus.subscribe(since) {
        SubscribeOutcome::Attached {
            replay,
            mut live,
            ..
        } => {
            assert_eq!(replay.len(), 1);
            assert_eq!(replay[0].meta.seq, since + 1);
            assert!(live.try_recv().is_err());
        }
        SubscribeOutcome::CursorGap { .. } => panic!("原子订阅必须成功"),
    }

    println!(
        "[v3-guard-01/{}] 洪峰 {} 帧 → 收到 {} 帧；attach_seq={}；resync={}；字节守恒={} B；破序丢帧={} 帧",
        F::name(),
        FLOOD_FRAMES,
        stats.frames,
        attach_seq,
        stats.resync_cycles,
        stats.delta_bytes,
        lost
    );
}
