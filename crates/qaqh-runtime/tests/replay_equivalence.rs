//! 重放等价契约（v1 表征测试）——Ringing v2.0 R1/R2 的迁移判据。
//!
//! 为什么需要（v2.0 重构前置）：v2 计划把 `journal×3 + checkpoint 目录`
//! 合并为单一 canonical `events.jsonl`，并把重连语义从
//! `Last-Event-ID` 迁移到 `since_seq` + 原子订阅（见
//! `docs/plan/2026-09-19-qaqh-v2.0-前瞻设计-plan.md` §2.5 R1/R2）。
//! 动刀前必须先把**现状重放语义**钉住——本文件是 R1/R2 的行为契约，
//! 覆盖缺口见 `docs/report/2026-09-19-测试缺口盘点-v2重构前-report.md` §2.1。
//!
//! 锁定的现状语义：
//! 1. **reliable 等价**：live 订阅序列 == `replay_since(0)`（同 event_id、
//!    按 stream_seq 升序）；
//! 2. **断点续传严格大于**：`replay_since(cursor)` 对 reliable 只返回
//!    `stream_seq > cursor`（无重复、无缺口）；
//! 3. **replaceable 只保当前值**：live 收全量增量，重放只给每个 identity 的
//!    最新值；且当前值**不受 cursor 过滤**（seq ≤ cursor 仍会重放）——
//!    客户端必须幂等（v2 D2 决策的现状基线）；
//! 4. **重启等价**：flush 后同 persistence root 的新 hub，重放出的 reliable
//!    序列与重启前 live 序列逐 event_id 一致；
//! 5. **频道级合并**：`replay_channel_since` 跨 seed 按 stream_seq 升序合并；
//! 6. **cursor 过期**：可靠 journal 窗口淘汰后 `replay_since` 返回
//!    `CursorExpired`（客户端走 reset→snapshot 路径）。
//!
//! 与 hub.rs 内联测试的分工：内联测试逐机制验证（router 覆盖、reset 信号、
//! 落盘存活）；本文件在**公开 API 面**上锁"live 与重放两条路径等价"这一
//! R2 核心性质，供 v2 迁移做对照验收。

use std::path::PathBuf;

use qaqh_domain::{Delivery, DomainEvent, RingingChannel, ToolEvent};
use qaqh_ringing::{RingingEvent, RingingEventEnvelope};
use qaqh_runtime::RingingHub;
use qaqh_runtime::ringing::hub::PublishOutcome;
use tokio::sync::broadcast;

fn temp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "qaqh-replay-equivalence-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp root");
    root
}

fn tool_started(call_id: &str) -> DomainEvent {
    DomainEvent::Tool(ToolEvent::ToolStarted {
        tool_call_id: call_id.to_string(),
        turn_id: "t1".to_string(),
        round_num: 0,
        name: "exec".to_string(),
    })
}

fn tool_prepared(call_id: &str, args_so_far: &str) -> DomainEvent {
    DomainEvent::Tool(ToolEvent::ToolCallPrepared {
        tool_call_id: call_id.to_string(),
        turn_id: "t1".to_string(),
        round_num: 0,
        name: "exec".to_string(),
        args_so_far: args_so_far.to_string(),
    })
}

/// 发布并返回信封（断言必须真的 Published）。
fn publish(hub: &RingingHub, seed: &str, event: DomainEvent) -> RingingEventEnvelope {
    match hub.publish(seed, event) {
        PublishOutcome::Published { envelope } => envelope,
        other => panic!("publish must succeed for {seed}, got {other:?}"),
    }
}

/// 排空 live 订阅（广播环内已排队的事件）。
fn drain(live: &mut broadcast::Receiver<RingingEventEnvelope>) -> Vec<RingingEventEnvelope> {
    let mut out = Vec::new();
    while let Ok(envelope) = live.try_recv() {
        out.push(envelope);
    }
    out
}

fn reliable_ids(events: &[RingingEventEnvelope]) -> Vec<String> {
    events
        .iter()
        .filter(|envelope| envelope.delivery == Delivery::Reliable)
        .map(|envelope| envelope.event_id.clone())
        .collect()
}

/// 1. reliable 事件：live 订阅序列 == 重放序列（同 event_id、升序）。
#[test]
fn live_and_replay_agree_on_reliable_events() {
    let hub = RingingHub::new("epoch-1");
    let mut live = hub.subscribe(RingingChannel::Tool, "s1");

    let mut published = Vec::new();
    for index in 1..=3 {
        published.push(publish(&hub, "s1", tool_started(&format!("c{index}"))).event_id);
    }

    let live_events = drain(&mut live);
    assert_eq!(
        reliable_ids(&live_events),
        published,
        "live 订阅必须按发布序收到全部 reliable 事件"
    );

    let replayed = hub
        .replay_since(RingingChannel::Tool, "s1", 0)
        .expect("in-window replay");
    assert_eq!(
        reliable_ids(&replayed),
        published,
        "重放序列必须与 live 序列逐 event_id 一致（R2 等价基线）"
    );

    let seqs: Vec<u64> = replayed.iter().map(|e| e.stream_seq).collect();
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "重放必须按 stream_seq 升序：{seqs:?}"
    );
}

/// 2. 断点续传：`replay_since(k)` 对 reliable 严格返回 seq > k。
#[test]
fn replay_since_cursor_is_strictly_greater_for_reliable() {
    let hub = RingingHub::new("epoch-1");
    let mut live = hub.subscribe(RingingChannel::Tool, "s1");
    for index in 1..=5 {
        publish(&hub, "s1", tool_started(&format!("c{index}")));
    }
    let live_ids = reliable_ids(&drain(&mut live));
    assert_eq!(live_ids.len(), 5);

    for cursor in 0..=5u64 {
        let replayed = hub
            .replay_since(RingingChannel::Tool, "s1", cursor)
            .expect("in-window replay");
        let ids = reliable_ids(&replayed);
        assert_eq!(
            ids,
            live_ids[cursor as usize..].to_vec(),
            "cursor={cursor} 必须严格返回 seq > cursor 的可靠事件（无重复、无缺口）"
        );
        assert!(
            replayed
                .iter()
                .filter(|e| e.delivery == Delivery::Reliable)
                .all(|e| e.stream_seq > cursor),
            "cursor={cursor} 不得回放已确认的可靠事件"
        );
    }
}

/// 3. replaceable：live 全量增量，重放只给当前值；当前值不受 cursor 过滤。
#[test]
fn replaceable_replay_returns_current_value_unfiltered_by_cursor() {
    let hub = RingingHub::new("epoch-1");
    let mut live = hub.subscribe(RingingChannel::Tool, "s1");
    publish(&hub, "s1", tool_prepared("c1", "a"));
    publish(&hub, "s1", tool_prepared("c1", "ab"));
    publish(&hub, "s1", tool_prepared("c1", "abc"));

    let live_events = drain(&mut live);
    assert_eq!(
        live_events.len(),
        3,
        "live 订阅必须收到全部 replaceable 增量（流式渲染依赖）"
    );

    let replayed = hub
        .replay_since(RingingChannel::Tool, "s1", 0)
        .expect("in-window replay");
    let prepared: Vec<&RingingEventEnvelope> = replayed
        .iter()
        .filter(|e| {
            matches!(
                e.event,
                RingingEvent::Tool(ToolEvent::ToolCallPrepared { .. })
            )
        })
        .collect();
    assert_eq!(
        prepared.len(),
        1,
        "重放只保留每个 identity 的当前值（v1 现状；v2 D2 决策基线）"
    );
    assert!(
        matches!(
            &prepared[0].event,
            RingingEvent::Tool(ToolEvent::ToolCallPrepared { args_so_far, .. })
                if args_so_far == "abc"
        ),
        "当前值必须是最后一次发布的增量：{:?}",
        prepared[0].event
    );

    // cursor 越过该 replaceable 的 stream_seq 之后，当前值仍会重放
    // （router 的 replaceable 不过滤 cursor）——客户端必须幂等。
    let cursor = prepared[0].stream_seq;
    let after = hub
        .replay_since(RingingChannel::Tool, "s1", cursor)
        .expect("in-window replay");
    assert!(
        after
            .iter()
            .any(|e| e.delivery != Delivery::Reliable && e.event_id == prepared[0].event_id),
        "replaceable 当前值不受 cursor 过滤（seq={cursor} 仍重放）——这是 v1 现状"
    );
}

/// 4. 重启等价：flush 后同 persistence root 的新 hub，重放的 reliable 序列
///    与重启前 live 序列逐 event_id 一致。
#[test]
fn restart_preserves_reliable_sequence() {
    let root = temp_root("restart");
    let live_ids = {
        let hub = RingingHub::with_persistence("epoch-1", &root);
        let mut live = hub.subscribe(RingingChannel::Tool, "s1");
        for index in 1..=3 {
            publish(&hub, "s1", tool_started(&format!("c{index}")));
        }
        let ids = reliable_ids(&drain(&mut live));
        assert_eq!(ids.len(), 3);
        hub.flush_journal_persistence();
        ids
    };

    let hub = RingingHub::with_persistence("epoch-2", &root);
    let replayed = hub
        .replay_since(RingingChannel::Tool, "s1", 0)
        .expect("replay after restart");
    assert_eq!(
        reliable_ids(&replayed),
        live_ids,
        "重启后重放的 reliable 序列必须与重启前 live 序列一致"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// 5. 频道级重放：跨 seed 按 stream_seq 升序合并（SSE 重连的频道视图）。
#[test]
fn channel_replay_merges_seeds_in_stream_order() {
    let hub = RingingHub::new("epoch-1");
    publish(&hub, "s1", tool_started("a1"));
    publish(&hub, "s2", tool_started("b1"));
    publish(&hub, "s1", tool_started("a2"));
    publish(&hub, "s2", tool_started("b2"));

    let replay = hub.replay_channel_since(RingingChannel::Tool, 0, false);
    assert!(replay.resets.is_empty(), "窗口内不得出现 reset 信号");
    let seeds: Vec<&str> = replay.events.iter().map(|e| e.seed.as_str()).collect();
    assert_eq!(
        seeds,
        vec!["s1", "s2", "s1", "s2"],
        "频道级重放必须按发布序（stream_seq 升序）跨 seed 合并"
    );
    let seqs: Vec<u64> = replay.events.iter().map(|e| e.stream_seq).collect();
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "合并后必须严格升序：{seqs:?}"
    );
}

/// 6. cursor 超出可靠窗口 → `CursorExpired`（客户端必须改走 reset→snapshot）。
#[test]
fn cursor_expired_signals_reset_path() {
    let hub = RingingHub::new("epoch-1");
    // 可靠 journal 容量 8192：灌满后再追加一条，最早的序号被淘汰。
    for index in 0..=8192 {
        publish(&hub, "s1", tool_started(&format!("c{index}")));
    }

    let expired = hub
        .replay_since(RingingChannel::Tool, "s1", 0)
        .expect_err("cursor 0 必须超出窗口");
    assert!(
        expired.earliest_available_seq > 0,
        "过期响应必须携带最早可回放序号（客户端据此 reset）：{expired:?}"
    );

    // 频道级路径给出 reset 信号而非静默半截回放。
    let channel = hub.replay_channel_since(RingingChannel::Tool, 0, false);
    assert!(
        channel.resets.iter().any(|reset| reset.seed == "s1"),
        "频道级重放必须对该 seed 发出 RingingResetRequired"
    );
}

/// 7. 无 cursor 的新连接（`skip_reliable = true`）：只回放 replaceable 当前值，
///    可靠历史交给 bootstrap 快照（防止幽灵 running turn 先于快照到达）。
///
/// 同时锁定一处**路径不对称**（v2 必须显式决策）：
/// - per-seed `replay_since`：replaceable 当前值**不受 cursor 过滤**（见用例 3）；
/// - 频道级 `replay_channel_since`：replaceable 仍按 `stream_seq > cursor` 过滤。
#[test]
fn fresh_connection_skips_reliable_history() {
    let hub = RingingHub::new("epoch-1");
    publish(&hub, "s1", tool_started("c1")); // reliable
    let prepared = publish(&hub, "s1", tool_prepared("c2", "abc")); // replaceable

    let fresh = hub.replay_channel_since(RingingChannel::Tool, 0, true);
    assert!(
        fresh
            .events
            .iter()
            .all(|e| e.delivery != Delivery::Reliable),
        "新连接不得回放可靠历史（由 bootstrap 快照承担）"
    );
    assert!(
        fresh.events.iter().any(|e| matches!(
            e.event,
            RingingEvent::Tool(ToolEvent::ToolCallPrepared { .. })
        )),
        "replaceable 当前值仍必须回放"
    );

    // 路径不对称：频道级在 cursor 越过当前值 seq 后不再回放它，
    // 而 per-seed 路径仍会回放（用例 3 已断言）。
    let after = hub.replay_channel_since(RingingChannel::Tool, prepared.stream_seq, false);
    assert!(
        after
            .events
            .iter()
            .all(|e| e.stream_seq > prepared.stream_seq),
        "频道级重放对 replaceable 仍按 seq 过滤：{:?}",
        after
            .events
            .iter()
            .map(|e| e.stream_seq)
            .collect::<Vec<_>>()
    );
}
