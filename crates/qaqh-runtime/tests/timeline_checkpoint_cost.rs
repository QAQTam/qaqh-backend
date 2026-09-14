//! 回归（BUG-2026-09-12-09 / issue #28）：BlockCheckpoint 写放大与 TurnSealed
//! 同步落盘。
//!
//! 改前必红的三条契约：
//!
//! 1. **增量交付**：`BlockCheckpoint` 不得把「全量块文本」塞进 SSE 载荷——
//!    单次发布成本会随块长度线性增长（实测 4 MiB 块 ≈ 3 ms，且每 64 token
//!    一次），并让快照体积达到 messages.jsonl 的 4 倍以上。
//! 2. **成本与块长度解耦**：4 MiB 块上的 checkpoint 单次发布必须落在固定
//!    预算内（不随块文本增长）。
//! 3. **TurnSealed 不得在发布会话线程上同步全量落盘**：seal 一次要重写整个
//!    快照（生产 17 MiB 快照实测 60 ms+），冻结发布方。
//!
//! 这组测试是**行为契约**，不依赖具体实现：
//! - 增量语义通过「消费者只用增量事件重建的文本 == 发布方物化文本」验证；
//! - 异步落盘通过「seal 发布耗时与快照体积解耦」+「flush 后仍可恢复」验证。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use qaqh_domain::{TimelineBlockKind, TimelineEvent, TimelineIntent, TimelineTurnState};
use qaqh_runtime::RingingHub;

fn temp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("qaqh-ckpt-cost-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp root");
    root
}

fn open_text_block(hub: &RingingHub, seed: &str, turn: &str) {
    hub.publish_timeline(
        seed,
        TimelineIntent::TurnOpened {
            turn_id: turn.into(),
            user_text: "probe".into(),
        },
    )
    .expect("turn opened");
    hub.publish_timeline(
        seed,
        TimelineIntent::BlockOpened {
            turn_id: turn.into(),
            round_num: 0,
            block_id: "b1".into(),
            kind: TimelineBlockKind::Text,
            tool: None,
        },
    )
    .expect("block opened");
}

fn snapshot_path(root: &Path, seed: &str) -> PathBuf {
    root.join("ringing-timeline").join(format!("{seed}.json"))
}

/// 从 SSE 流（live broadcast + 回放尾）忠实重建块文本的消费者。
///
/// 只认识协议里的事件形状：`TextDelta` 追加、`BlockCheckpoint { arg }` 追加
/// 增量 / `BlockCheckpoint { text }` 全量覆盖。这就是前端（单 transcript
/// reducer）的语义——若增量载荷不足以重建，说明协议自愈能力被破坏。
struct TranscriptConsumer {
    text: String,
    checkpoints: usize,
    incremental_bytes: usize,
    full_bytes: usize,
}

impl TranscriptConsumer {
    fn new() -> Self {
        Self {
            text: String::new(),
            checkpoints: 0,
            incremental_bytes: 0,
            full_bytes: 0,
        }
    }

    fn apply(&mut self, event: &TimelineEvent) {
        match event {
            TimelineEvent::TextDelta { delta, .. } => self.text.push_str(delta),
            TimelineEvent::BlockCheckpoint { arg: Some(arg), .. } => {
                self.checkpoints += 1;
                self.incremental_bytes += arg.len();
                self.text.push_str(arg);
            }
            TimelineEvent::BlockCheckpoint { text, .. } => {
                self.checkpoints += 1;
                self.full_bytes += text.len();
                self.text.clone_from(text);
            }
            _ => {}
        }
    }
}

/// 契约 1：checkpoint 载荷必须是增量。
///
/// 消费者从零开始（模拟丢过全部 TextDelta 的重连客户端）：只消费 checkpoint
/// 事件就必须重建出与发布方一致的全文；同时内容未变化时不得重复发余量。
#[test]
fn checkpoint_delivers_only_the_text_added_since_the_last_emit() {
    let root = temp_root("incremental");
    let hub = RingingHub::with_persistence("ckpt-epoch-incremental", &root);
    let seed = "seed-incremental";
    open_text_block(&hub, seed, "t1");

    let chunk = "a".repeat(4096);
    let mut published = String::new();
    let mut consumer = TranscriptConsumer::new();
    let mut rx = hub.subscribe_timeline();

    // 第 1 次（客户端从未见过该块）：必须全量覆盖，否则丢过 delta 的客户端
    // 无法补齐。此后每次只发增量。
    for index in 0..8 {
        published.push_str(&chunk);
        hub.publish_timeline(
            seed,
            TimelineIntent::BlockCheckpoint {
                turn_id: "t1".into(),
                round_num: 0,
                block_id: "b1".into(),
                text: published.clone(),
            },
        )
        .unwrap_or_else(|error| panic!("checkpoint {index}: {error}"));
    }

    // 内容未变化的重复 checkpoint：不得再发任何余量。
    hub.publish_timeline(
        seed,
        TimelineIntent::BlockCheckpoint {
            turn_id: "t1".into(),
            round_num: 0,
            block_id: "b1".into(),
            text: published.clone(),
        },
    )
    .expect("checkpoint");

    while let Ok(live) = rx.try_recv() {
        consumer.apply(&live.entry.event);
    }

    assert_eq!(
        consumer.checkpoints, 9,
        "every checkpoint must reach the transport"
    );
    assert_eq!(
        consumer.text, published,
        "incremental checkpoints must rebuild the full block text"
    );
    // 自愈：consumer 从零开始，只靠 checkpoint 序列也必须重建全文。
    // 且**永远不许出现全量覆盖**——每次载荷都只是"上次之后的余量"。
    assert_eq!(
        consumer.full_bytes, 0,
        "no checkpoint may fall back to a full overwrite on the append-only path"
    );
    assert_eq!(
        consumer.incremental_bytes,
        published.len(),
        "checkpoint payload must be the delta only, never the running full text"
    );

    let _ = std::fs::remove_dir_all(root);
}

/// 契约 1b：consumer 已追平的正常路径 + 非追加改写的降级自愈。
///
/// 协议不变量：SSE 帧严格递增（客户端 seq 不连续即 `TimelineGap` →
/// `recover_gap` 重基线），因此"正在应用第 N 帧"的 consumer 一定已应用全部
/// 更早的帧，其文本 = 写侧 `block.text`。在这个前提下增量载荷可无损重建；
/// 一旦文本不是追加式延伸（乱序/整流），写侧必须降级为全量覆盖 —— 这正是
/// 下面第二步验证的契约。
#[test]
fn checkpoint_self_heals_when_the_text_is_not_an_append() {
    let root = temp_root("self-heal");
    let hub = RingingHub::with_persistence("ckpt-epoch-self-heal", &root);
    let seed = "seed-self-heal";
    open_text_block(&hub, seed, "t1");
    let mut rx = hub.subscribe_timeline();

    // 1) 追平路径：deltas 逐条送达后，checkpoint 只补余量。
    for delta in ["hel", "lo ", "wor"] {
        hub.publish_timeline(
            seed,
            TimelineIntent::TextDelta {
                turn_id: "t1".into(),
                round_num: 0,
                block_id: "b1".into(),
                delta: delta.into(),
            },
        )
        .expect("delta");
    }
    let full = "hello world".to_string();
    hub.publish_timeline(
        seed,
        TimelineIntent::BlockCheckpoint {
            turn_id: "t1".into(),
            round_num: 0,
            block_id: "b1".into(),
            text: full.clone(),
        },
    )
    .expect("checkpoint");

    let mut consumer = TranscriptConsumer::new();
    while let Ok(live) = rx.try_recv() {
        consumer.apply(&live.entry.event);
    }
    assert_eq!(consumer.text, full);
    assert_eq!(
        consumer.incremental_bytes,
        "ld".len(),
        "a caught-up consumer needs only the tail"
    );
    assert_eq!(consumer.full_bytes, 0);

    // 2) 降级路径：文本被改成非追加式（乱序/整流）→ 必须全量覆盖，consumer 自愈。
    let rewritten = "rewritten from scratch".to_string();
    hub.publish_timeline(
        seed,
        TimelineIntent::BlockCheckpoint {
            turn_id: "t1".into(),
            round_num: 0,
            block_id: "b1".into(),
            text: rewritten.clone(),
        },
    )
    .expect("checkpoint");
    let full_before = consumer.full_bytes;
    while let Ok(live) = rx.try_recv() {
        consumer.apply(&live.entry.event);
    }
    assert!(
        consumer.full_bytes > full_before,
        "a non-append rewrite must fall back to a full overwrite"
    );
    assert_eq!(
        consumer.text, rewritten,
        "a consumer must recover from the full overwrite alone"
    );

    // 3) 降级后写侧重基线：下一次仍是增量。
    let incremental = format!("{rewritten}+tail");
    hub.publish_timeline(
        seed,
        TimelineIntent::BlockCheckpoint {
            turn_id: "t1".into(),
            round_num: 0,
            block_id: "b1".into(),
            text: incremental.clone(),
        },
    )
    .expect("checkpoint");
    let full_before = consumer.full_bytes;
    let incr_before = consumer.incremental_bytes;
    while let Ok(live) = rx.try_recv() {
        consumer.apply(&live.entry.event);
    }
    assert_eq!(consumer.full_bytes, full_before, "writer must re-baseline");
    assert_eq!(consumer.incremental_bytes - incr_before, "+tail".len());
    assert_eq!(consumer.text, incremental);

    let _ = std::fs::remove_dir_all(root);
}

/// 契约 2：checkpoint 的**写放大**与块长度解耦。
///
/// 通过「发布出去的条目 + 落盘 journal 的体积」衡量：一块 4 MiB 文本，发 32 次
/// 各 8 KiB 的增量后，传输/落盘体积必须 ~= 增量总和，而不是 32 × 4 MiB。
/// 改前每次 checkpoint 都携带全量块文本 → 放大 30 倍以上。
#[test]
fn checkpoint_write_amplification_does_not_grow_with_block_length() {
    let root = temp_root("amplification");
    let hub = RingingHub::with_persistence("ckpt-epoch-amp", &root);
    let seed = "seed-amp";
    open_text_block(&hub, seed, "t1");

    const CHUNK: usize = 8 * 1024;
    const STEPS: usize = 32;
    const FILL: usize = 4 * 1024 * 1024;

    let chunk = "x".repeat(CHUNK);
    let mut text = "f".repeat(FILL);
    let mut rx = hub.subscribe_timeline();

    // 先把块填到 4 MiB（首次 checkpoint 走"从空串追加"路径，仍是增量形态）。
    hub.publish_timeline(
        seed,
        TimelineIntent::BlockCheckpoint {
            turn_id: "t1".into(),
            round_num: 0,
            block_id: "b1".into(),
            text: text.clone(),
        },
    )
    .expect("fill checkpoint");
    while rx.try_recv().is_ok() {}

    let mut wire_bytes = 0usize;
    let mut publish = Duration::ZERO;
    for _ in 0..STEPS {
        text.push_str(&chunk);
        let owned = std::mem::take(&mut text);
        let started = Instant::now();
        hub.publish_timeline(
            seed,
            TimelineIntent::BlockCheckpoint {
                turn_id: "t1".into(),
                round_num: 0,
                block_id: "b1".into(),
                text: owned,
            },
        )
        .expect("checkpoint");
        publish += started.elapsed();
        text = hub
            .timeline_snapshot(seed)
            .and_then(|snapshot| {
                snapshot.turns.first().and_then(|turn| {
                    turn.rounds
                        .first()
                        .and_then(|round| round.blocks.first())
                        .map(|block| block.text.clone())
                })
            })
            .expect("block text");
    }
    while let Ok(live) = rx.try_recv() {
        if let TimelineEvent::BlockCheckpoint { arg, text, .. } = &live.entry.event {
            wire_bytes += arg.as_deref().map(str::len).unwrap_or(0) + text.len();
        }
    }
    let amplification = wire_bytes as f64 / (STEPS * CHUNK) as f64;
    println!(
        "[amp] 块 4 MiB，{STEPS} 次 8 KiB 增量：线上载荷 {:.1} KiB（放大 {amplification:.2}×），\
         平均发布 {:.3}ms/次",
        wire_bytes as f64 / 1024.0,
        publish.as_secs_f64() * 1000.0 / STEPS as f64
    );
    assert!(
        amplification < 1.5,
        "checkpoint payload must track the increment (4 MiB block): {amplification:.2}×"
    );

    let _ = std::fs::remove_dir_all(root);
}

/// 契约 3：TurnSealed 不得在发布会话线程上同步全量落盘。
#[test]
fn turn_sealed_does_not_flush_the_snapshot_on_the_publisher_thread() {
    let root = temp_root("async-seal");
    let hub = RingingHub::with_persistence("ckpt-epoch-async-seal", &root);
    let seed = "seed-async-seal";
    open_text_block(&hub, seed, "t1");

    // 灌到 ~2 MiB 块文本（快照同量级）。
    const CHUNK: usize = 64 * 1024;
    let chunk = "y".repeat(CHUNK);
    let mut text = String::new();
    for _ in 0..32 {
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
    // 先让异步 worker 把 2 MiB 快照写下去，确保 seal 时磁盘上已有大文件。
    hub.flush_timeline_persistence();
    let warm = std::fs::metadata(snapshot_path(&root, seed))
        .expect("snapshot after flush")
        .len();

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

    let started = Instant::now();
    hub.publish_timeline(
        seed,
        TimelineIntent::TurnSealed {
            turn_id: "t1".into(),
            state: TimelineTurnState::Completed,
            failure: None,
        },
    )
    .expect("turn sealed");
    let publish_cost = started.elapsed();
    println!(
        "[seal] TurnSealed 发布耗时 {:.2}ms（快照 {:.2} MiB）",
        publish_cost.as_secs_f64() * 1000.0,
        warm as f64 / 1048576.0
    );
    assert!(
        publish_cost < Duration::from_millis(10),
        "TurnSealed must not synchronously rewrite the snapshot on the publisher thread: \
         {publish_cost:?} (budget 10ms, snapshot {warm} B)"
    );

    // 崩溃一致性 / fail-closed：显式同步边界返回后，sealed turn 必须已落盘。
    hub.flush_timeline_persistence();
    let persisted: serde_json::Value = serde_json::from_slice(
        &std::fs::read(snapshot_path(&root, seed)).expect("snapshot persisted after flush"),
    )
    .expect("snapshot json");
    let turn = &persisted["snapshot"]["turns"][0];
    assert_eq!(turn["sealed"], serde_json::Value::Bool(true));
    assert_eq!(
        persisted["journal"].as_array().map(Vec::len),
        Some(0),
        "sealed turn must leave no replay tail on disk"
    );
    assert_eq!(
        turn["rounds"][0]["blocks"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .len(),
        text.len(),
        "full block text must survive the incremental checkpoint path"
    );

    let _ = std::fs::remove_dir_all(root);
}

/// 契约 4：快照体积不再放大（checkpoint 文本只物化一次）。
#[test]
fn snapshot_size_tracks_the_materialized_text() {
    let root = temp_root("snapshot-size");
    let hub = RingingHub::with_persistence("ckpt-epoch-size", &root);
    let seed = "seed-size";
    open_text_block(&hub, seed, "t1");

    const CHUNK: usize = 64 * 1024;
    const CHECKPOINTS: usize = 32; // 2 MiB 块文本
    let chunk = "z".repeat(CHUNK);
    let mut text = String::new();
    for _ in 0..CHECKPOINTS {
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
    hub.publish_timeline(
        seed,
        TimelineIntent::TurnSealed {
            turn_id: "t1".into(),
            state: TimelineTurnState::Completed,
            failure: None,
        },
    )
    .expect("turn sealed");
    hub.flush_timeline_persistence();

    let size = std::fs::metadata(snapshot_path(&root, seed))
        .expect("snapshot")
        .len();
    let ratio = size as f64 / text.len() as f64;
    println!(
        "[size] 快照 {:.2} MiB / 块文本 {:.2} MiB = {ratio:.2}×",
        size as f64 / 1048576.0,
        text.len() as f64 / 1048576.0
    );
    assert!(
        ratio < 2.0,
        "snapshot must materialize the block text once, not multiply it: {ratio:.2}×"
    );

    let _ = std::fs::remove_dir_all(root);
}

/// 契约 5：terminal 优先队列的有序性 —— 更晚的写永远更新。
///
/// 场景：TurnSealed 入队后立刻再打开一个新回合（t2）。worker 是单线程、每轮
/// 先 drain terminal 再处理软窗口，且每次落盘都取**当前**内存态，因此磁盘上
/// 不可能出现"旧快照盖过 terminal 边界"或"丢掉 terminal 之后的写"。
#[test]
fn terminal_persistence_stays_ordered_against_later_writes() {
    let root = temp_root("ordering");
    let hub = RingingHub::with_persistence("ckpt-epoch-ordering", &root);
    let seed = "seed-ordering";
    open_text_block(&hub, seed, "t1");
    for intent in [
        TimelineIntent::BlockSealed {
            turn_id: "t1".into(),
            round_num: 0,
            block_id: "b1".into(),
        },
        TimelineIntent::RoundSealed {
            turn_id: "t1".into(),
            round_num: 0,
            is_final: true,
        },
    ] {
        hub.publish_timeline(seed, intent).expect("seal round");
    }
    hub.publish_timeline(
        seed,
        TimelineIntent::TurnSealed {
            turn_id: "t1".into(),
            state: TimelineTurnState::Completed,
            failure: None,
        },
    )
    .expect("turn sealed");
    // terminal 之后立刻写下一个回合：必须一起落盘，且 t1 已 sealed。
    open_text_block(&hub, seed, "t2");
    hub.flush_timeline_persistence();

    let raw = std::fs::read(snapshot_path(&root, seed)).expect("snapshot");
    let persisted: serde_json::Value = serde_json::from_slice(&raw).expect("json");
    let turns = persisted["snapshot"]["turns"].as_array().expect("turns");
    assert_eq!(turns.len(), 2, "the later turn must not be lost");
    assert_eq!(turns[0]["turn_id"], "t1");
    assert_eq!(turns[0]["sealed"], serde_json::Value::Bool(true));
    assert_eq!(turns[1]["turn_id"], "t2");
    assert_eq!(turns[1]["sealed"], serde_json::Value::Bool(false));
    assert_eq!(
        persisted["snapshot"]["watermark"].as_u64(),
        Some(7),
        "watermark must cover every write, including the one after the terminal"
    );

    let _ = std::fs::remove_dir_all(root);
}

/// 契约 5b（reviewer 建议 B）：terminal 与**后续写**的真实竞争。
///
/// 上一个用例的断言实际由末尾的 `flush_timeline_persistence()` 兜住——`open_text_block`
/// 只发 `TurnOpened`/`BlockOpened`（非持久化触发事件，不进软窗口），因此它并未
/// 验证「terminal 入队后、后续写到达」时 worker 的 drain 顺序。
///
/// 本用例不调 flush：TurnSealed（terminal 队列）之后立刻发一个**会触发
/// `request_timeline_persistence`** 的写（t2 的 BlockCheckpoint，软窗口），
/// 然后**轮询**快照文件直到 t2 出现。断言磁盘上同时存在「已 sealed 的 t1」与
/// 「terminal 之后的 t2 写」——旧快照盖过 terminal 或丢掉后续写都会失败。
#[test]
fn terminal_then_later_write_both_survive_without_explicit_flush() {
    let root = temp_root("ordering-race");
    let hub = RingingHub::with_persistence("ckpt-epoch-race", &root);
    let seed = "seed-ordering-race";
    open_text_block(&hub, seed, "t1");
    for intent in [
        TimelineIntent::BlockSealed {
            turn_id: "t1".into(),
            round_num: 0,
            block_id: "b1".into(),
        },
        TimelineIntent::RoundSealed {
            turn_id: "t1".into(),
            round_num: 0,
            is_final: true,
        },
    ] {
        hub.publish_timeline(seed, intent).expect("seal round");
    }
    hub.publish_timeline(
        seed,
        TimelineIntent::TurnSealed {
            turn_id: "t1".into(),
            state: TimelineTurnState::Completed,
            failure: None,
        },
    )
    .expect("turn sealed");

    // terminal 入队后立刻写 t2：BlockCheckpoint 是持久化触发事件（软窗口），
    // 与 terminal 队列在同一 worker 上串行处理。
    open_text_block(&hub, seed, "t2");
    hub.publish_timeline(
        seed,
        TimelineIntent::BlockCheckpoint {
            turn_id: "t2".into(),
            round_num: 0,
            block_id: "b1".into(),
            text: "t2-payload".into(),
        },
    )
    .expect("t2 checkpoint");

    // 不调 flush：轮询等待 worker 自然落盘（软窗口 1s，给 10s 上限）。
    let path = snapshot_path(&root, seed);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = String::new();
    let persisted: serde_json::Value = loop {
        if let Ok(raw) = std::fs::read(&path) {
            last = String::from_utf8_lossy(&raw).into_owned();
            if last.contains("t2-payload") {
                break serde_json::from_slice(&raw).expect("json");
            }
        }
        assert!(
            Instant::now() < deadline,
            "worker did not persist the post-terminal write in time; last snapshot: {last}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };

    let turns = persisted["snapshot"]["turns"].as_array().expect("turns");
    assert_eq!(turns.len(), 2, "the post-terminal turn must not be lost");
    assert_eq!(turns[0]["turn_id"], "t1");
    assert_eq!(
        turns[0]["sealed"],
        serde_json::Value::Bool(true),
        "an older snapshot must not overwrite the sealed terminal boundary"
    );
    assert_eq!(turns[1]["turn_id"], "t2");

    let _ = std::fs::remove_dir_all(root);
}
