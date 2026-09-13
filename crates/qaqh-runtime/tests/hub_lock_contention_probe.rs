//! 并发回归 / 量化装置：`channels` per-(channel, seed) 锁表的分片效果。
//!
//! 背景（BUG-2026-09-13-33 / BUG-08 收尾）：跨会话内存态曾共享单一全局
//! `Mutex<HashMap<Channel, HashMap<Seed, State>>>`，persist=false 基线
//! 8 线程吞吐 448k → 276k（-38%）主要来自这里。本文件用**公开 API**
//! （`RingingHub::publish`）驱动真实热路径，量测「1 线程 vs 8 线程（8 个
//! 独立 seed）」的聚合吞吐与扩展比。
//!
//! `persist=false`：只测锁争用，不掺 journal I/O。
//!
//! 标记 `#[ignore]`：耗时随机器浮动，写死吞吐阈值会造成假红。需要基线时显式运行：
//!
//! ```bash
//! cargo test --release -p qaqh-runtime --test hub_lock_contention_probe -- --ignored --nocapture
//! ```
//!
//! 对照（报告 §3.4 基线 U(1)=110k、U(8)=72k，扩展比 0.65x）：
//! 修复前 8 线程显著低于 1 线程；修复后应转为正向扩展。
//!
//! 门禁版本（不依赖机器绝对吞吐，只断言「不退化」）在
//! `src/ringing/hub.rs::lock_sharding_tests` 内，随 `cargo test` 常态运行。

use std::sync::{Arc, Barrier};
use std::time::Instant;

use qaqh_domain::{ConversationEvent, DomainEvent};
use qaqh_runtime::RingingHub;

const THREADS: usize = 8;
const PER_THREAD: usize = 5_000;

fn delta(seq: u64) -> DomainEvent {
    DomainEvent::Conversation(ConversationEvent::RoundDelta {
        turn_id: "t1".into(),
        round_num: 1,
        kind: qaqh_domain::RoundDeltaKind::Answering,
        delta: format!("chunk-{seq}"),
    })
}

/// 单线程基线：顺序发 THREADS × PER_THREAD 条（无跨会话争用）。
fn measure_single_thread() -> f64 {
    let hub = Arc::new(RingingHub::new("lockbench-1"));
    let start = Instant::now();
    for index in 0..THREADS {
        let seed = format!("bench-{index}");
        for i in 0..PER_THREAD {
            hub.publish(&seed, delta(i as u64));
        }
    }
    let elapsed = start.elapsed();
    (THREADS * PER_THREAD) as f64 / elapsed.as_secs_f64()
}

/// 8 线程并发：每个线程一个独立 seed，同时起跑。
fn measure_multi_thread() -> f64 {
    let hub = Arc::new(RingingHub::new("lockbench-8"));
    let barrier = Arc::new(Barrier::new(THREADS));
    let mut joins = Vec::new();
    for index in 0..THREADS {
        let hub = Arc::clone(&hub);
        let barrier = Arc::clone(&barrier);
        joins.push(std::thread::spawn(move || {
            let seed = format!("bench-{index}");
            barrier.wait();
            for i in 0..PER_THREAD {
                hub.publish(&seed, delta(i as u64));
            }
        }));
    }
    let start = Instant::now();
    for join in joins {
        join.join().expect("publish thread must not panic");
    }
    let elapsed = start.elapsed();
    (THREADS * PER_THREAD) as f64 / elapsed.as_secs_f64()
}

#[test]
#[ignore = "量化装置：耗时随机器浮动，需显式 --ignored 运行"]
fn hub_publish_scaling_1_vs_8_threads() {
    let single = measure_single_thread();
    let multi = measure_multi_thread();
    println!(
        "[lockbench] 1 thread: {single:.0} ev/s | {THREADS} threads ({THREADS} seeds): {multi:.0} ev/s | scaling {:.2}x",
        multi / single
    );
    // 只断言「不显著退化」：修复前实测 0.49x（8 会话反而慢一半），
    // 修复后为正向扩展。此处留 0.9x 作为机器抖动余量。
    assert!(
        multi > single * 0.9,
        "8 independent sessions must not serialize on the shared channels lock \
         (1-thread={single:.0}/s, {THREADS}-thread={multi:.0}/s)"
    );
}
