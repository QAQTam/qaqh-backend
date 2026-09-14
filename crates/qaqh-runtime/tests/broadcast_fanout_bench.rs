//! 扇出基准（非断言型测量装置，issue #31 / D-5）。
//!
//! 量化「live broadcast 按 (channel, seed) 分片」相对分片前（频道单环）
//! 的两项收益：
//!
//! 1. **隔离性**：S 个会话并发高频发布时，旁观会话连接是否被推向 `Lagged`；
//! 2. **扇出成本**：每事件在 N 个订阅连接上的投递总成本（分片后与连接数解耦）。
//!
//! 同时给出回放过滤的租约锁持有时长对比（锁内逐事件 `owns_seed` → 锁内取
//! 一次归属快照 + 锁外过滤），对应 `sse.rs::filter_replay_for_session`。
//!
//! 刻意标记 `#[ignore]`：测量装置而非门禁，耗时随机器浮动。
//!
//! ```bash
//! cargo test --release -p qaqh-runtime --test broadcast_fanout_bench \
//!     -- --ignored --nocapture --test-threads=1
//! ```

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use qaqh_domain::{ConversationEvent, DomainEvent, RingingChannel};
use qaqh_runtime::RingingHub;
use qaqh_runtime::ringing::RingingLeaseStore;

fn round_delta(tag: &str, seq: u64) -> DomainEvent {
    DomainEvent::Conversation(ConversationEvent::RoundDelta {
        turn_id: tag.into(),
        round_num: 0,
        kind: qaqh_domain::RoundDeltaKind::Thinking,
        delta: format!("chunk-{seq}"),
    })
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// 场景 1：S 个会话风暴 → 旁观连接的 Lagged 计数。
///
/// 分片前：频道单环 1024，任一会话灌满即全体 Lagged（旁观连接丢事件）。
/// 分片后：每 seed 独立环，旁观连接零丢失。
#[test]
#[ignore = "measurement harness; run explicitly with --ignored --nocapture"]
fn fanout_isolation_bystanders_lag_under_session_storm() {
    const SESSIONS: usize = 8;
    const PER_SESSION: u64 = 2_000;

    let hub = RingingHub::new("bench-fanout");
    // 每个会话一个连接 + 一个「旁观」连接（订阅空闲会话 idle-0）。
    let mut storm_rx: Vec<_> = (0..SESSIONS)
        .map(|i| hub.subscribe(RingingChannel::Conversation, &format!("s{i}")))
        .collect();
    let mut idle_rx = hub.subscribe(RingingChannel::Conversation, "idle-0");

    let t0 = Instant::now();
    for seq in 1..=PER_SESSION {
        for i in 0..SESSIONS {
            let _ = hub.publish(&format!("s{i}"), round_delta("t", seq));
        }
    }
    let publish = t0.elapsed();
    let total = SESSIONS as u64 * PER_SESSION;
    println!(
        "── 扇出隔离 ──\n{SESSIONS} 会话 × {PER_SESSION} 事件 = {total} 次 publish，合计 {:.1}ms（{:.2} µs/事件）",
        ms(publish),
        publish.as_secs_f64() * 1e6 / total as f64
    );

    // 风暴连接（故意不排空）统计自己的 Lagged。
    let mut storm_lagged = 0usize;
    for rx in storm_rx.iter_mut() {
        if let Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) = rx.try_recv() {
            storm_lagged += n as usize;
        }
    }
    // 旁观连接发布一条自己的事件后必须原样收到。
    let _ = hub.publish("idle-0", round_delta("idle", 1));
    let idle_ok = idle_rx.try_recv().is_ok();
    println!(
        "风暴连接自身 Lagged 事件数 = {storm_lagged}（预期 > 0：风暴会话确实被压垮）\n\
         旁观连接收到自己的事件 = {idle_ok}（预期 true：验收标准 1）"
    );
    assert!(idle_ok, "旁观连接不得因他人风暴而丢事件");
}

/// 场景 2：扇出投递成本 vs 订阅连接数（分片后每个连接只从自己的分片取事件）。
#[test]
#[ignore = "measurement harness; run explicitly with --ignored --nocapture"]
fn fanout_delivery_cost_scales_with_own_shard_only() {
    const SESSIONS: usize = 32;
    const EVENTS: u64 = 2_000;

    // 生产者与消费者跑在同一 hub 上；hub 用 Arc 共享（RingingHub 非 Clone）。
    let hub = Arc::new(RingingHub::new("bench-cost"));
    // 每个会话 1 个连接（分片订阅），全部活跃排空（模拟健康消费者）。
    let mut rxs: Vec<_> = (0..SESSIONS)
        .map(|i| hub.subscribe(RingingChannel::Conversation, &format!("c{i}")))
        .collect();

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let producer = std::thread::spawn({
        let hub = Arc::clone(&hub);
        move || {
            let t0 = Instant::now();
            for seq in 1..=EVENTS {
                for i in 0..SESSIONS {
                    let _ = hub.publish(&format!("c{i}"), round_delta("t", seq));
                }
            }
            t0.elapsed()
        }
    });

    // 主线程持续排空，模拟活跃前端。
    let mut received = 0usize;
    while !producer.is_finished() {
        for rx in rxs.iter_mut() {
            while rx.try_recv().is_ok() {
                received += 1;
            }
        }
        std::thread::yield_now();
    }
    for rx in rxs.iter_mut() {
        while rx.try_recv().is_ok() {
            received += 1;
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let publish = producer.join().expect("bench thread");
    let total = SESSIONS as u64 * EVENTS;
    println!(
        "── 扇出成本 ──\n{SESSIONS} 连接 × {EVENTS} 事件 = {total} 次 publish，\
         合计 {:.1}ms（{:.2} µs/事件），收到 {received} 条（{:.1}%）",
        ms(publish),
        publish.as_secs_f64() * 1e6 / total as f64,
        received as f64 * 100.0 / total as f64
    );
}

/// 场景 3：回放过滤的租约锁持有时长（修复前 vs 修复后）。
///
/// 修复前：一次全局租约锁内做 R 次 `owns_seed`（O(L) 命中查询 + O(1) 索引）。
/// 修复后：锁内取一次归属快照（O(1) clone），锁外 O(R) `HashSet` 命中。
#[test]
#[ignore = "measurement harness; run explicitly with --ignored --nocapture"]
fn replay_filter_lease_lock_hold_time() {
    const LEASES: usize = 200;
    const REPLAY: usize = 32_768;

    let leases = Arc::new(Mutex::new(RingingLeaseStore::new()));
    {
        let mut g = leases.lock().unwrap();
        for i in 0..LEASES {
            let cs = format!("cs-{i}");
            g.open(cs.clone(), format!("ci-{i}"));
            g.attach_seed(&cs, &format!("seed-{i}"));
        }
    }
    let seeds: Vec<String> = (0..REPLAY)
        .map(|i| format!("seed-{}", i % LEASES))
        .collect();

    // 修复前：锁内逐事件 owns_seed。
    let t0 = Instant::now();
    {
        let mut g = leases.lock().unwrap();
        let kept = seeds.iter().filter(|s| g.owns_seed("cs-0", s)).count();
        let _ = kept;
    }
    let before = t0.elapsed();

    // 修复后：锁内取快照，锁外过滤。
    let t1 = Instant::now();
    let owned = {
        let g = leases.lock().unwrap();
        g.owned_seeds("cs-0")
    };
    let lock_hold = t1.elapsed();
    let kept = seeds.iter().filter(|s| owned.contains(*s)).count();
    let total_after = t1.elapsed();
    let _ = kept;

    println!(
        "── 回放过滤租约锁（L={LEASES} 租约，R={REPLAY} 事件）──\n\
         修复前：单次持锁 {:.1}ms（锁内 R 次 owns_seed）\n\
         修复后：持锁 {:.3}ms + 锁外过滤，合计 {:.1}ms\n\
         持锁时间下降 {:.0}×",
        ms(before),
        ms(lock_hold),
        ms(total_after),
        ms(before) / ms(lock_hold).max(1e-9)
    );
}

/// 场景 1'：对照组——**频道聚合**订阅（等价于分片前的频道单环行为）在
/// 同一会话风暴下的旁观 Lagged 计数。
///
/// 该订阅视图今天仍在（`subscribe_channel`，供本来就要看全部会话的观察者
/// 使用），因此可以同场对照：同一个风暴下，分片订阅零丢失，聚合订阅被
/// 推向 `Lagged`。
#[test]
#[ignore = "measurement harness; run explicitly with --ignored --nocapture"]
fn aggregate_view_costs_lag_where_shard_does_not() {
    const SESSIONS: usize = 8;
    const PER_SESSION: u64 = 2_000;

    let hub = RingingHub::new("bench-fanout-ctl");
    // 聚合视图连接（不排空，模拟慢消费者/旁观者）。
    let mut aggregate_rx = hub.subscribe_channel(RingingChannel::Conversation);
    // 8 个会话风暴（另一个空闲会话 idle-0 的**分片**连接）。
    let mut idle_rx = hub.subscribe(RingingChannel::Conversation, "idle-0");

    for seq in 1..=PER_SESSION {
        for i in 0..SESSIONS {
            let _ = hub.publish(&format!("s{i}"), round_delta("t", seq));
        }
    }

    let mut aggregate_lagged = 0usize;
    loop {
        match aggregate_rx.try_recv() {
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                aggregate_lagged += n as usize;
                break;
            }
            Err(_) => break,
        }
    }
    let _ = hub.publish("idle-0", round_delta("idle", 1));
    let idle_ok = idle_rx.try_recv().is_ok();
    println!(
        "── 对照（同一风暴，两种订阅视图）──\n\
         频道聚合视图（= 分片前的频道单环行为）Lagged 事件数 = {aggregate_lagged}\n\
         分片视图（idle-0）收到自己的事件 = {idle_ok}"
    );
    assert!(aggregate_lagged > 0, "聚合视图在风暴下应被推向 Lagged");
}
