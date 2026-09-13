//! 回归测试：tool_outbox 的锁粒度与 fsync 批量化（issue #30 / BUG-2026-09-12-14）。
//!
//! 缺陷机制（pre-fix，`crates/qaqh-runtime/src/agent/tool_outbox.rs`）：
//! - `static OUTBOX_LOCK: Mutex<()>`（进程级）：**不同会话**的并发工具线程也
//!   完全串行——下游缓慢的 `fsync` 把锁持有时间放大并传播到所有会话；
//! - 每条记录 `open→write→flush→sync_all`：单条成本被磁盘延迟钉死，
//!   1→8 线程聚合吞吐恒定（零扩展）。
//!
//! 本文件是**行为级**回归（不做机器相关的耗时断言）：
//! 1. `flush_does_not_serialize_on_a_slow_session`——会话 A 的批量化 fsync
//!    挂起 400 ms 时，另一会话的追加不得被阻塞（fsync 已在写路径之外；
//!    pre-fix 会被互斥锁钉住 → 红）；
//! 2. `flush_is_joined_before_returning`——显式 flush 是同步屏障：返回时
//!    该会话的 fsync 已发生（持久化语义不牺牲）；
//! 3. `concurrent_sessions_scale_end_to_end`——8 会话并发追加的墙钟时间必须
//!    远低于串行算术和（无跨会话互斥时可并行，pre-fix → 红）。
//!
//! 装置：注入**独立于 outbox 实现**的 `FaultFsyncHook`，按路径让 `sync_all`
//! 挂起/计时。它把「单条成本」与「并发阻塞」解耦，因此其存在本身就证明追加
//! 路径不再同步 fsync。钩子由 `set_fsync_hook` 全局安装；`cargo test` 每个
//! 测试二进制为独立进程，本文件独占该进程。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use qaqh_runtime::agent::tool_outbox;

/// fsync 故障注入：命中的路径要么延迟 `delay`、要么计时返回。
struct FaultFsyncHook {
    /// 目标文件绝对路径（`tool_outbox.wal`）。
    target: PathBuf,
    /// `Some(d)`: 命中时阻塞 d；`None`: 命中时只计数并返回。
    delay: Option<Duration>,
    /// 累计延迟次数（诊断用）。
    hits: AtomicUsize,
}

/// 安装钩子并返回它；同一时刻只允许一个（测试内串行使用）。
fn install_hook(target: PathBuf, delay: Option<Duration>) -> Arc<FaultFsyncHook> {
    let hook = Arc::new(FaultFsyncHook {
        target,
        delay,
        hits: AtomicUsize::new(0),
    });
    let weak = Arc::downgrade(&hook);
    tool_outbox::set_fsync_hook(Some(Box::new(move |path: &Path| {
        let Some(hook) = weak.upgrade() else { return };
        if path != hook.target {
            return;
        }
        hook.hits.fetch_add(1, Ordering::SeqCst);
        if let Some(delay) = hook.delay {
            std::thread::sleep(delay);
        }
    })));
    hook
}

fn clear_hook() {
    tool_outbox::set_fsync_hook(None);
}

fn temp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("qaqh-tool-outbox-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp root");
    root
}

/// 1. 慢会话的批量化 fsync 不得阻塞其它会话的追加。
///
/// pre-fix：追加路径同步 `sync_all` 且全程持进程级锁 → 线程 A 的 fsync
/// 挂起 400 ms 期间，线程 B 的 `record_in` 至少被阻塞 ~400 ms → 红。
#[test]
fn flush_does_not_serialize_on_a_slow_session() {
    let root = temp_root("slow-session");
    let dir_a = root.join("aaaa1111");
    let dir_b = root.join("bbbb2222");
    std::fs::create_dir_all(&dir_a).expect("dir a");
    std::fs::create_dir_all(&dir_b).expect("dir b");

    // 会话 A 的 fsync 挂起 400 ms；会话 B 的文件不受影响。
    let hook = install_hook(
        tool_outbox::outbox_path(&dir_a),
        Some(Duration::from_millis(400)),
    );

    // A 先写入并在后台触发其批量化 fsync（显式 flush = 同步屏障）。
    tool_outbox::record_in(&dir_a, "a-1", "bash", true);
    let a_dir = dir_a.clone();
    let slow_a = std::thread::spawn(move || tool_outbox::flush_in(&a_dir));

    // 给 A 一点时间真正进入 fsync 后再测 B（避免调度假阴性）。
    let deadline = Instant::now() + Duration::from_secs(2);
    while hook.hits.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        hook.hits.load(Ordering::SeqCst) > 0,
        "会话 A 的 fsync 未被触发——批量化/屏障语义未生效"
    );

    let started = Instant::now();
    tool_outbox::record_in(&dir_b, "b-1", "grep", true);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(150),
        "会话 B 的追加被会话 A 的慢 fsync 阻塞了 {elapsed:?}（期望 < 150ms）——\
         锁仍是进程级或 fsync 仍在写路径内"
    );

    slow_a.join().expect("flush thread");
    clear_hook();
    assert_eq!(tool_outbox::read_records(&dir_b).len(), 1);
}

/// 2. 显式 flush 之后，fsync 确已对本会话的记录发生（持久化屏障不退化）。
#[test]
fn flush_is_joined_before_returning() {
    let root = temp_root("flush-join");
    let dir = root.join("cccc3333");
    std::fs::create_dir_all(&dir).expect("dir");
    let hook = install_hook(tool_outbox::outbox_path(&dir), None);

    for i in 0..8 {
        tool_outbox::record_in(&dir, &format!("c-{i}"), "bash", true);
    }
    let before = hook.hits.load(Ordering::SeqCst);
    tool_outbox::flush_in(&dir);
    let after = hook.hits.load(Ordering::SeqCst);

    assert!(
        after > before,
        "flush 返回前该会话的 fsync 必须已发生（before={before}, after={after}）"
    );
    assert_eq!(tool_outbox::read_records(&dir).len(), 8);
    clear_hook();
}

/// 3. 8 会话并发追加必须可扩展：墙钟远低于「单会话成本 × 会话数」。
///
/// pre-fix：跨会话互斥 + 每条 8 ms 的同步 fsync ⇒ 墙钟 ≈ 64 × 8 ms ≈ 512 ms；
/// post-fix：追加路径无 fsync（只在 flush 时一次）⇒ 墙钟 ~ 毫秒级。
#[test]
fn concurrent_sessions_scale_end_to_end() {
    const SESSIONS: usize = 8;
    const PER_SESSION: usize = 64;
    const FSYNC_COST: Duration = Duration::from_millis(8);

    let root = temp_root("scale");
    let dirs: Vec<PathBuf> = (0..SESSIONS)
        .map(|i| {
            let dir = root.join(format!("s{i:08x}"));
            std::fs::create_dir_all(&dir).expect("dir");
            dir
        })
        .collect();
    // 所有会话共用一个耗时钩子：命中的 fsync 各睡 8 ms。
    let hook = Arc::new(FaultFsyncHook {
        target: PathBuf::new(),
        delay: Some(FSYNC_COST),
        hits: AtomicUsize::new(0),
    });
    let weak = Arc::downgrade(&hook);
    tool_outbox::set_fsync_hook(Some(Box::new(move |_path: &Path| {
        let Some(hook) = weak.upgrade() else { return };
        hook.hits.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(FSYNC_COST);
    })));

    let started = Instant::now();
    let handles: Vec<_> = dirs
        .iter()
        .map(|dir| {
            let dir = dir.clone();
            std::thread::spawn(move || {
                for i in 0..PER_SESSION {
                    tool_outbox::record_in(&dir, &format!("call-{i}"), "bash", true);
                }
                tool_outbox::flush_in(&dir);
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("append thread");
    }
    let elapsed = started.elapsed();
    clear_hook();

    let serial_floor = FSYNC_COST * (PER_SESSION as u32);
    assert!(
        elapsed < serial_floor / 4,
        "8 会话并发追加耗时 {elapsed:?}，未见扩展（串行下限 {serial_floor:?}）——\
         锁分片或 fsync 批量化未生效"
    );

    for dir in &dirs {
        assert_eq!(
            tool_outbox::read_records(dir).len(),
            PER_SESSION,
            "flush 后每个会话的记录都必须完整落盘"
        );
    }
}
