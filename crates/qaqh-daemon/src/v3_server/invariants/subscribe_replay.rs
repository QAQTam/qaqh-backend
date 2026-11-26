//! 守卫①：**原子订阅 + 有界回放尾**（`ADR-0042` 铁律 4）。
//!
//! 三件事，一件都不能少：
//!
//! 1. 洪峰期间 attach：`resync_cycles == 0`、字节守恒（合帧不吃字节、不错排）；
//! 2. 游标跌出回放尾：必须**明确**给 `ResyncRequired{CursorGap}`，绝不"从最老的接着给"；
//! 3. **破坏顺序必红**：把"读环 + 注册"拆成两步、在缝里插一条发布，那条帧必然消失；
//!    同样场景下原子订阅一帧不丢。这是本守卫的反证部分，缺了它就只剩"看起来对"。

use std::sync::Arc;
use std::time::Duration;

use qaqh_ringing_v3::{ControlRequest, ResyncReason, SessionEvent, SessionRequest};
use qaqh_v3_transport::MuxTransport;

use crate::v3_server::bus::SubscribeOutcome;
use crate::v3_server::harness;

pub(crate) async fn guard<B: MuxTransport>(pair: fn() -> (B, B)) {
    let state = harness::state();
    let (client_transport, server_transport) = pair();
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

    // ── 甲：洪峰已经开始之后再 attach（要的正是"洪峰中"这个窗口） ──
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
            // 哨兵：看到它就说明前面的全到了（单流单 seq，定序有物理载体）。
            session.emit(SessionEvent::CatalogChanged {
                session_id: session_id.clone(),
            });
            published
        })
    };

    let mut stream = client
        .attach_session(&session_id, 0)
        .await
        .expect("洪峰中 attach 必须成功");
    let attach_seq = stream.attach().seq;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut sentinel = false;
    while !sentinel {
        let frame = tokio::time::timeout_at(deadline, stream.next_frame())
            .await
            .expect("事件流 30s 无进展（订阅死锁或写侧卡住）")
            .expect("帧必须可读");
        if matches!(frame.event, SessionEvent::CatalogChanged { .. }) {
            sentinel = true;
        }
    }
    let published = flood.await.expect("洪峰任务");
    let stats = stream.stats().clone();
    assert_eq!(
        stats.resync_cycles, 0,
        "洪峰期间 attach 触发了重同步（attach_seq={attach_seq}）"
    );
    assert_eq!(stats.ordering_violations, 0, "seq 出现倒退");
    assert_eq!(
        stream.answer_text(&block_id),
        published,
        "回放/合帧吃掉或错排了字节"
    );
    assert!(
        stats.frames <= FLOOD_FRAMES + 1,
        "收到的帧数 {stats_delta} 竟然多于发布的 {FLOOD_FRAMES}（重复投递？）",
        stats_delta = stats.frames
    );

    // ── 乙：游标跌出回放尾必须明确报缺口 ──
    let stale = harness::session_for(&state, &harness::owner_device());
    let stale_id = stale.id.clone();
    let stale_block = harness::block(2);
    let overflow = harness::config().ring_capacity as u64 + 512;
    for _ in 0..overflow {
        stale.emit(harness::answer(&stale_id, &stale_block, "x"));
    }
    assert!(
        stale.bus.last_seq() > harness::config().ring_capacity as u64,
        "回放环必须已经绕过去（否则这一半没被压到）"
    );
    let mut gap_stream = client
        .attach_session(&stale_id, 0)
        .await
        .expect("陈旧游标也要能建流（答复里说清缺口）");
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
    assert!(replay.is_empty(), "since 之后此刻不该有帧");
    // 缝：这一帧发生在"读完环"与"注册 receiver"之间。
    race.emit(harness::answer(&race_id, &race_block, "in-seam"));
    let (mut broken_live, _broken_lag) = race.bus.register_only();
    let delivered = broken_live.try_recv();
    assert!(
        matches!(
            delivered,
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                | Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ),
        "破序写法竟然把缝隙里的帧送达了（反证失效，必须重写）"
    );
    assert_eq!(
        race.bus.last_seq(),
        since + 1,
        "服务端确实发布了那一帧，但破序订阅端拿不到它"
    );
    let lost = 1u64;

    // 同一场景、原子订阅：缝隙里的那一帧必须被回放出来，一帧不丢。
    let since = race.bus.last_seq();
    race.emit(harness::answer(&race_id, &race_block, "seam-2"));
    match race.bus.subscribe(since) {
        SubscribeOutcome::Attached {
            replay,
            mut live,
            ..
        } => {
            assert_eq!(replay.len(), 1, "原子订阅必须把缝隙里的那一帧回放出来");
            assert_eq!(replay[0].meta.seq, since + 1);
            let live_first = live.try_recv().expect_err("没有更多帧了");
            assert!(
                matches!(
                    live_first,
                    tokio::sync::mpsc::error::TryRecvError::Empty
                        | tokio::sync::mpsc::error::TryRecvError::Disconnected
                ),
                "原子订阅不该把同一帧再发一遍"
            );
        }
        other => panic!(
            "原子订阅必须成功，实际 {}",
            match other {
                SubscribeOutcome::CursorGap { requested_since, oldest_available, newest } =>
                    format!("CursorGap({requested_since}, {oldest_available}..{newest})"),
                SubscribeOutcome::Attached { .. } => unreachable!(),
            }
        ),
    }

    println!(
        "[v3-guard-01] 洪峰 {} 帧 → 收到 {} 帧；attach_seq={}；resync={}；字节守恒={} B；破序丢帧={} 帧",
        FLOOD_FRAMES,
        stats.frames,
        attach_seq,
        stats.resync_cycles,
        stats.delta_bytes,
        lost
    );
}
