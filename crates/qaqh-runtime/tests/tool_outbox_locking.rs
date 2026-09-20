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
//! 3. `concurrent_sessions_scale_end_to_end`——8 会话 × 64 条并发追加的
//!    **fsync 次数**必须远低于记录数（批量化：每会话每轮至多 1 次；
//!    pre-fix 每条一次 = 512 次 → 红）。判据不依赖墙钟（D-1 修复）。
//!
//! 装置：注入**独立于 outbox 实现**的 `FaultFsyncHook`，按路径让 `sync_all`
//! 挂起/计时。它把「单条成本」与「并发阻塞」解耦，因此其存在本身就证明追加
//! 路径不再同步 fsync。钩子由 `set_fsync_hook` 全局安装；`cargo test` 每个
//! 测试二进制为独立进程，本文件独占该进程。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use qaqh_runtime::agent::tool_outbox::{self, FsyncPhase};

/// fsync 故障注入：命中的路径要么延迟 `delay`、要么计时返回。
struct FaultFsyncHook {
    /// 目标文件绝对路径（`tool_outbox.wal`）。
    target: PathBuf,
    /// `Some(d)`: 命中时阻塞 d；`None`: 命中时只计数并返回。
    delay: Option<Duration>,
    /// 累计延迟次数（诊断用）。
    hits: AtomicUsize,
    /// 显式 flush 路径命中次数。
    explicit_hits: AtomicUsize,
    /// 后台 flusher 路径命中次数。
    background_hits: AtomicUsize,
}

/// 闸门式 fsync 钩子：命中后挂起在钩子内，直到测试显式放行。
///
/// 用于把「B 会话的追加是否被 A 的 fsync 阻塞」变成**相对判据**：
/// A 在闸门内期间，B 必须能完成追加；pre-fix（进程级锁）下 B 会一直等到
/// 看门狗放行才返回 → 断言打红。全程无墙钟阈值（看门狗只在失败路径兜底）。
struct GatedFsyncHook {
    target: PathBuf,
    entered: AtomicUsize,
    released: std::sync::atomic::AtomicBool,
    hits: AtomicUsize,
}

/// 同时观察显式/后台两条 fsync 路径；后台命中时停在闸门内，显式命中时检查
/// 目标文件是否已经包含调用方刚追加的记录。
struct ExplicitBarrierHook {
    background_path: PathBuf,
    explicit_path: PathBuf,
    background_entered: std::sync::atomic::AtomicBool,
    explicit_entered: std::sync::atomic::AtomicBool,
    release_background: std::sync::atomic::AtomicBool,
    saw_explicit_record: std::sync::atomic::AtomicBool,
}

impl GatedFsyncHook {
    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
    }

    /// 等待钩子被命中（A 已进入 fsync）。返回是否在超时前进入。
    fn wait_until_entered(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.entered.load(Ordering::SeqCst) > 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    }
}

fn install_gated_hook(target: PathBuf) -> Arc<GatedFsyncHook> {
    let hook = Arc::new(GatedFsyncHook {
        target,
        entered: AtomicUsize::new(0),
        released: std::sync::atomic::AtomicBool::new(false),
        hits: AtomicUsize::new(0),
    });
    let weak = Arc::downgrade(&hook);
    tool_outbox::set_fsync_hook(Some(Arc::new(move |path: &Path, phase: FsyncPhase| {
        let Some(hook) = weak.upgrade() else { return };
        if phase != FsyncPhase::Explicit || path != hook.target {
            return;
        }
        hook.hits.fetch_add(1, Ordering::SeqCst);
        hook.entered.fetch_add(1, Ordering::SeqCst);
        while !hook.released.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
    })));
    hook
}

/// 安装钩子并返回它；同一时刻只允许一个（测试内串行使用）。
fn install_hook(target: PathBuf, delay: Option<Duration>) -> Arc<FaultFsyncHook> {
    let hook = Arc::new(FaultFsyncHook {
        target,
        delay,
        hits: AtomicUsize::new(0),
        explicit_hits: AtomicUsize::new(0),
        background_hits: AtomicUsize::new(0),
    });
    let weak = Arc::downgrade(&hook);
    tool_outbox::set_fsync_hook(Some(Arc::new(move |path: &Path, phase: FsyncPhase| {
        let Some(hook) = weak.upgrade() else { return };
        if path != hook.target {
            return;
        }
        hook.hits.fetch_add(1, Ordering::SeqCst);
        match phase {
            FsyncPhase::Explicit => hook.explicit_hits.fetch_add(1, Ordering::SeqCst),
            FsyncPhase::Background => hook.background_hits.fetch_add(1, Ordering::SeqCst),
        };
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

fn shard_index_for_test(path: &Path) -> usize {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.as_os_str().to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash as usize) % 16
}

fn distinct_shard_dirs(root: &Path) -> (PathBuf, PathBuf) {
    let first = root.join("shard-a");
    let first_index = shard_index_for_test(&tool_outbox::outbox_path(&first));
    for index in 0..128 {
        let candidate = root.join(format!("shard-b-{index}"));
        if shard_index_for_test(&tool_outbox::outbox_path(&candidate)) != first_index {
            return (first, candidate);
        }
    }
    panic!("failed to find two outbox paths in distinct shards");
}

/// 1. 慢会话的批量化 fsync 不得阻塞其它会话的追加（相对判据，无墙钟阈值）。
///
/// pre-fix：追加路径同步 `sync_all` 且全程持进程级锁 → A 的 fsync 挂起期间
/// B 的 `record_in` 也会被阻塞，直到看门狗放行 → 断言打红。
#[test]
fn flush_does_not_serialize_on_a_slow_session() {
    let root = temp_root("slow-session");
    let dir_a = root.join("aaaa1111");
    let dir_b = root.join("bbbb2222");
    std::fs::create_dir_all(&dir_a).expect("dir a");
    std::fs::create_dir_all(&dir_b).expect("dir b");

    // 会话 A 的 fsync 被闸门挂起；会话 B 的文件不受影响。
    let hook = install_gated_hook(tool_outbox::outbox_path(&dir_a));

    // A 先写入并在后台触发其批量化 fsync（显式 flush = 同步屏障）。
    tool_outbox::record_in(&dir_a, "a-1", "bash", true);
    let a_dir = dir_a.clone();
    let slow_a = std::thread::spawn(move || tool_outbox::flush_in(&a_dir));

    assert!(
        hook.wait_until_entered(Duration::from_secs(2)),
        "会话 A 的 fsync 未被触发——批量化/屏障语义未生效"
    );

    // 看门狗：2s 后强制放行，防止 pre-fix 下 B 被阻塞导致测试挂死
    //（放行前 B 返回 = 通过；放行后才返回 = 打红）。正常路径下测试立即
    // 发信号让看门狗退出，不引入额外等待。
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let watchdog_hook = Arc::clone(&hook);
    let watchdog = std::thread::spawn(move || {
        if done_rx.recv_timeout(Duration::from_secs(2)).is_err() {
            watchdog_hook.release();
        }
    });

    // A 仍被闸门挂在 fsync 内：B 的追加必须现在就能完成。
    tool_outbox::record_in(&dir_b, "b-1", "grep", true);
    let blocked = hook.released.load(Ordering::SeqCst);
    let _ = done_tx.send(());
    hook.release();
    slow_a.join().expect("flush thread");
    watchdog.join().expect("watchdog thread");
    clear_hook();

    assert!(
        !blocked,
        "会话 B 的追加被会话 A 的挂起 fsync 阻塞（直到看门狗放行才返回）——\
         锁仍是进程级或 fsync 仍在写路径内"
    );
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
    let before = hook.explicit_hits.load(Ordering::SeqCst);
    tool_outbox::flush_in(&dir);
    let after = hook.explicit_hits.load(Ordering::SeqCst);

    assert!(
        after > before,
        "flush 返回前该会话的显式 fsync 必须已发生（before={before}, after={after}）"
    );
    assert_eq!(tool_outbox::read_records(&dir).len(), 8);
    clear_hook();
}

/// 3. 显式 flush 必须携带显式 phase，并且调用 hook 时记录已经完成 append；
/// 后台 flusher 的命中不能被当成显式屏障。
#[test]
fn explicit_flush_is_distinct_from_background_flusher() {
    let root = temp_root("explicit-phase");
    let (dir_background, dir_explicit) = distinct_shard_dirs(&root);
    std::fs::create_dir_all(&dir_background).expect("background dir");
    std::fs::create_dir_all(&dir_explicit).expect("explicit dir");

    let background_path = tool_outbox::outbox_path(&dir_background);
    let explicit_path = tool_outbox::outbox_path(&dir_explicit);
    let hook = Arc::new(ExplicitBarrierHook {
        background_path: background_path.clone(),
        explicit_path: explicit_path.clone(),
        background_entered: std::sync::atomic::AtomicBool::new(false),
        explicit_entered: std::sync::atomic::AtomicBool::new(false),
        release_background: std::sync::atomic::AtomicBool::new(false),
        saw_explicit_record: std::sync::atomic::AtomicBool::new(false),
    });
    let weak = Arc::downgrade(&hook);
    tool_outbox::set_fsync_hook(Some(Arc::new(move |path: &Path, phase: FsyncPhase| {
        let Some(hook) = weak.upgrade() else { return };
        match phase {
            FsyncPhase::Background if path == hook.background_path => {
                hook.background_entered.store(true, Ordering::SeqCst);
                while !hook.release_background.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            FsyncPhase::Explicit if path == hook.explicit_path => {
                let bytes = std::fs::read(path).unwrap_or_default();
                let expected = b"explicit-record";
                hook.saw_explicit_record.store(
                    bytes
                        .windows(expected.len())
                        .any(|window| window == expected),
                    Ordering::SeqCst,
                );
                hook.explicit_entered.store(true, Ordering::SeqCst);
            }
            _ => {}
        }
    })));

    tool_outbox::record_in(&dir_background, "background-record", "bash", true);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !hook.background_entered.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    let background_entered = hook.background_entered.load(Ordering::SeqCst);
    if !background_entered {
        hook.release_background.store(true, Ordering::SeqCst);
        clear_hook();
        panic!("后台 flusher 未进入 fsync 钩子");
    }

    tool_outbox::record_in(&dir_explicit, "explicit-record", "grep", true);
    let explicit_dir = dir_explicit.clone();
    let explicit_flush = std::thread::spawn(move || tool_outbox::flush_in(&explicit_dir));

    let deadline = Instant::now() + Duration::from_secs(2);
    while !hook.explicit_entered.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    let explicit_entered = hook.explicit_entered.load(Ordering::SeqCst);
    let saw_explicit_record = hook.saw_explicit_record.load(Ordering::SeqCst);

    hook.release_background.store(true, Ordering::SeqCst);
    explicit_flush.join().expect("explicit flush thread");
    clear_hook();

    assert!(
        explicit_entered,
        "flush_in 未进入显式 fsync 路径（后台 flusher 被误当成显式屏障）"
    );
    assert!(
        saw_explicit_record,
        "显式 fsync 时记录尚未完成 append——FS 追加顺序契约被破坏"
    );
}

/// 4. 8 会话并发追加的 fsync 次数必须远低于记录数（批量化判据，无墙钟）。
///
/// pre-fix：每条记录同步 fsync ⇒ 次数 = SESSIONS × PER_SESSION = 512 → 红；
/// post-fix：fsync 只在 flush 轮次发生（显式 flush 每会话 ≤1 次 = 8，加上
/// 后台 flusher 每 100ms 一轮、每轮每会话 ≤1 次）。判据给足余量：
/// `hits <= SESSIONS * 8`（64）——要 8 轮后台 flush（≈800ms 窗口）才可能触及，
/// 而回归值 512 是它的 8 倍。
#[test]
fn concurrent_sessions_scale_end_to_end() {
    const SESSIONS: usize = 8;
    const PER_SESSION: usize = 64;

    let root = temp_root("scale");
    let dirs: Vec<PathBuf> = (0..SESSIONS)
        .map(|i| {
            let dir = root.join(format!("s{i:08x}"));
            std::fs::create_dir_all(&dir).expect("dir");
            dir
        })
        .collect();
    // 计数钩子：命中即记一次 fsync（不引入延迟，避免把判据变成时序）。
    let hook = Arc::new(FaultFsyncHook {
        target: PathBuf::new(),
        delay: None,
        hits: AtomicUsize::new(0),
        explicit_hits: AtomicUsize::new(0),
        background_hits: AtomicUsize::new(0),
    });
    let weak = Arc::downgrade(&hook);
    tool_outbox::set_fsync_hook(Some(Arc::new(move |_path: &Path, _phase: FsyncPhase| {
        let Some(hook) = weak.upgrade() else { return };
        hook.hits.fetch_add(1, Ordering::SeqCst);
    })));

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
    let hits = hook.hits.load(Ordering::SeqCst);
    clear_hook();

    let records = SESSIONS * PER_SESSION;
    assert!(
        hits <= SESSIONS * 8,
        "fsync 次数 {hits} 过高（记录数 {records}；批量化后应为每会话每轮 ≤1 次，\
         pre-fix 每条一次 = {records}）——fsync 批量化或锁分片未生效"
    );
    // 反向保险：至少要真的发生过 flush（否则上界会被"零次"平凡满足）。
    assert!(
        hits >= SESSIONS,
        "fsync 次数 {hits} 少于会话数 {SESSIONS}——显式 flush 未对每个会话生效"
    );

    for dir in &dirs {
        assert_eq!(
            tool_outbox::read_records(dir).len(),
            PER_SESSION,
            "flush 后每个会话的记录都必须完整落盘"
        );
    }
}
