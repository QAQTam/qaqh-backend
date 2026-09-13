//! 测量（非断言型回归）：流式高频输出下 daemon 热路径的实际耗时。
//!
//! 背景：事故报告「多 session 并行 + 高频输出 → 前端视觉变慢 + 切会话被拒（401）」
//! 需要一个可复现的量化基线。本文件用**公开 API**（`RingingHub`）复现生产负载形状
//! 的形状，逐事件计时，并测「切换 session」所依赖的冷装载与同步落盘成本。
//!
//! 刻意标记 `#[ignore]`：这是测量装置而非门禁——耗时随机器浮动，写死阈值会造成
//! 假红。需要基线时显式运行：
//!
//! ```powershell
//! cargo test --release -p qaqh-runtime --test timeline_load_latency_probe `
//!     -- --ignored --nocapture --test-threads=1
//! ```
//!
//! 数据根固定在独立临时目录，不触碰真实 `~/.qaqh`。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use qaqh_domain::{
    ConversationEvent, DomainEvent, RoundDeltaKind, TimelineIntent, TimelineTurnState,
};
use qaqh_runtime::RingingHub;
use qaqh_session::SessionManager;

/// 进程级共享 data root（`SessionManager` 是单例，`init` 只能调用一次）。
fn shared_root() -> PathBuf {
    use std::sync::OnceLock;
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root =
            std::env::temp_dir().join(format!("qaqh-timeline-load-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create temp root");
        SessionManager::init(root.clone());
        root
    })
    .clone()
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn snapshot_bytes(root: &std::path::Path, seed: &str) -> u64 {
    std::fs::metadata(root.join("ringing-timeline").join(format!("{seed}.json")))
        .map(|meta| meta.len())
        .unwrap_or(0)
}

/// 按生产形状灌入一个「长回合」：块文本随 checkpoint 累积增长
/// （`BlockCheckpoint` 携带**全量块文本**，这是快照体积的真正来源）。
fn build_long_turn(hub: &RingingHub, seed: &str, checkpoints: usize, chunk_kib: usize) -> String {
    hub.publish_timeline(
        seed,
        TimelineIntent::TurnOpened {
            turn_id: "t1".into(),
            user_text: "probe".into(),
        },
    )
    .expect("turn opened");
    hub.publish_timeline(
        seed,
        TimelineIntent::BlockOpened {
            turn_id: "t1".into(),
            round_num: 0,
            block_id: "b1".into(),
            kind: qaqh_domain::TimelineBlockKind::Text,
            tool: None,
        },
    )
    .expect("block opened");
    let chunk = "x".repeat(chunk_kib * 1024);
    let mut text = String::new();
    for _ in 0..checkpoints {
        text.push_str(&chunk);
        hub.publish_timeline(
            seed,
            TimelineIntent::BlockCheckpoint {
                turn_id: "t1".into(),
                round_num: 0,
                block_id: "b1".into(),
                text: text.clone(),
            },
        )
        .expect("checkpoint");
    }
    text
}

#[test]
#[ignore = "measurement harness; run explicitly with --ignored --nocapture"]
fn hot_path_latency_under_fast_streaming() {
    let root = shared_root();
    let seed = "probe-latency";
    let hub = RingingHub::with_persistence("probe-epoch-1", &root);

    // ── A. 每个流式 checkpoint 的发布成本（累积文本 → 线性增长） ──────────
    const CHECKPOINTS: usize = 600;
    let t0 = Instant::now();
    let text = build_long_turn(&hub, seed, CHECKPOINTS, 4);
    let build_total = t0.elapsed();
    println!(
        "[A] {CHECKPOINTS} 次 BlockCheckpoint（累积文本 {:.2} MiB）合计 {:.1}ms，平均 {:.3}ms/次",
        text.len() as f64 / 1048576.0,
        ms(build_total),
        ms(build_total) / CHECKPOINTS as f64
    );

    // ── B. conversation 频道 RoundDelta：Reliable → 每事件一次锁内落盘 ─────
    hub.publish(
        seed,
        DomainEvent::Conversation(ConversationEvent::TurnStarted {
            turn_id: "t1".into(),
            user_text: "probe".into(),
        }),
    );
    let chunk = "x".repeat(4096);
    const DELTAS: usize = 4000;
    let mut worst = Duration::ZERO;
    let mut over_20ms = 0usize;
    let journal_path = root
        .join("journal")
        .join("conversation")
        .join(format!("{seed}.jsonl"));
    let before = std::fs::metadata(&journal_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let t0 = Instant::now();
    for _ in 0..DELTAS {
        let t = Instant::now();
        hub.publish(
            seed,
            DomainEvent::Conversation(ConversationEvent::RoundDelta {
                turn_id: "t1".into(),
                round_num: 0,
                kind: RoundDeltaKind::Answering,
                delta: chunk.clone(),
            }),
        );
        let d = t.elapsed();
        if d > worst {
            worst = d;
        }
        if ms(d) > 20.0 {
            over_20ms += 1;
        }
    }
    let sink_total = t0.elapsed();
    let after = std::fs::metadata(&journal_path)
        .map(|m| m.len())
        .unwrap_or(0);
    println!(
        "[B] {DELTAS} 次 RoundDelta（4 KiB）合计 {:.1}ms，平均 {:.3}ms/次，\
         最慢 {:.1}ms，>20ms 的 {over_20ms} 次",
        ms(sink_total),
        ms(sink_total) / DELTAS as f64,
        ms(worst)
    );
    println!(
        "[B] journal 增长 {:.2} MiB（{:.0} B/事件，含 4 MiB 全量重写）",
        (after - before) as f64 / 1048576.0,
        (after - before) as f64 / DELTAS as f64
    );

    // ── C. TurnSealed：同步全量落盘（在发布会话线程上） ────────────────────
    hub.publish_timeline(
        seed,
        TimelineIntent::BlockSealed {
            turn_id: "t1".into(),
            round_num: 0,
            block_id: "b1".into(),
        },
    )
    .expect("block sealed");
    hub.publish_timeline(
        seed,
        TimelineIntent::RoundSealed {
            turn_id: "t1".into(),
            round_num: 0,
            is_final: true,
        },
    )
    .expect("round sealed");
    let t0 = Instant::now();
    hub.publish_timeline(
        seed,
        TimelineIntent::TurnSealed {
            turn_id: "t1".into(),
            state: TimelineTurnState::Completed,
            failure: None,
        },
    )
    .expect("turn sealed");
    let sync_persist = t0.elapsed();
    let size = snapshot_bytes(&root, seed);
    println!(
        "[C] TurnSealed 同步落盘 {:.1}ms（快照 {:.2} MiB）",
        ms(sync_persist),
        size as f64 / 1048576.0
    );

    // ── D. 冷装载：新 hub 首次访问该 seed（= 切换 session / 重连的路径） ────
    drop(hub);
    let hub2 = RingingHub::with_persistence("probe-epoch-2", &root);
    let t0 = Instant::now();
    let snapshot = hub2.timeline_snapshot(seed);
    let cold_load = t0.elapsed();
    let turns = snapshot.as_ref().map(|s| s.turns.len()).unwrap_or(0);
    println!(
        "[D] 冷装载 timeline_snapshot（{} 回合 / {:.2} MiB 快照）耗时 {:.1}ms",
        turns,
        size as f64 / 1048576.0,
        ms(cold_load)
    );
    assert!(turns > 0, "cold load must return the persisted turn");
}

/// 生产规模（快照 ~12–17 MiB）下的同步落盘与冷装载成本。
///
/// 生产证据：`%USERPROFILE%\.qaqh\ringing\ringing-timeline\a6227e42.json` = 17.07 MiB。
/// 本用例按同量级构造，测量「切换 session」真正要付的两笔账：
/// ① 上一个回合 seal 时的同步全量重写（发生在发布会话线程上）；
/// ② 新会话/重连首次访问该 seed 时的冷装载。
#[test]
#[ignore = "measurement harness; run explicitly with --ignored --nocapture"]
fn production_scale_snapshot_persist_and_cold_load() {
    let root = shared_root();
    let seed = "probe-production-scale";
    let hub = RingingHub::with_persistence("probe-epoch-3", &root);

    // 8 个回合，每回合块文本累积到 ~2 MiB → 快照同量级于生产 17 MiB。
    const TURNS: usize = 8;
    const CHECKPOINTS: usize = 512;
    let mut last_seal = Duration::ZERO;
    for index in 1..=TURNS {
        let turn_id = format!("t{index}");
        hub.publish_timeline(
            seed,
            TimelineIntent::TurnOpened {
                turn_id: turn_id.clone(),
                user_text: format!("probe turn {index}"),
            },
        )
        .expect("turn opened");
        hub.publish_timeline(
            seed,
            TimelineIntent::BlockOpened {
                turn_id: turn_id.clone(),
                round_num: 0,
                block_id: "b1".into(),
                kind: qaqh_domain::TimelineBlockKind::Text,
                tool: None,
            },
        )
        .expect("block opened");
        let chunk = "y".repeat(4096);
        let mut text = String::new();
        for _ in 0..CHECKPOINTS {
            text.push_str(&chunk);
            hub.publish_timeline(
                seed,
                TimelineIntent::BlockCheckpoint {
                    turn_id: turn_id.clone(),
                    round_num: 0,
                    block_id: "b1".into(),
                    text: text.clone(),
                },
            )
            .expect("checkpoint");
        }
        hub.publish_timeline(
            seed,
            TimelineIntent::BlockSealed {
                turn_id: turn_id.clone(),
                round_num: 0,
                block_id: "b1".into(),
            },
        )
        .expect("block sealed");
        hub.publish_timeline(
            seed,
            TimelineIntent::RoundSealed {
                turn_id: turn_id.clone(),
                round_num: 0,
                is_final: true,
            },
        )
        .expect("round sealed");
        let before = snapshot_bytes(&root, seed);
        let t0 = Instant::now();
        hub.publish_timeline(
            seed,
            TimelineIntent::TurnSealed {
                turn_id: turn_id.clone(),
                state: TimelineTurnState::Completed,
                failure: None,
            },
        )
        .expect("turn sealed");
        last_seal = t0.elapsed();
        let after = snapshot_bytes(&root, seed);
        println!(
            "[E] turn {turn_id} seal：同步落盘 {:.1}ms，快照 {:.2} MiB → {:.2} MiB",
            ms(last_seal),
            before as f64 / 1048576.0,
            after as f64 / 1048576.0
        );
    }
    let size = snapshot_bytes(&root, seed);

    drop(hub);
    let hub2 = RingingHub::with_persistence("probe-epoch-4", &root);
    let t0 = Instant::now();
    let snapshot = hub2.timeline_snapshot(seed);
    let cold_load = t0.elapsed();
    let turns = snapshot.as_ref().map(|s| s.turns.len()).unwrap_or(0);
    println!(
        "[F] 生产规模冷装载：{turns} 回合 / {:.2} MiB 快照，耗时 {:.1}ms（最后一次 seal {:.1}ms）",
        size as f64 / 1048576.0,
        ms(cold_load),
        ms(last_seal)
    );
    assert_eq!(turns, TURNS, "all sealed turns must survive the round trip");
}
