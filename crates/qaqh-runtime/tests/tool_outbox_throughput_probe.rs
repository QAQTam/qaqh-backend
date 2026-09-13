//! 吞吐基准（测量装置，非门禁）：tool_outbox 在 1→8 会话并发下的聚合吞吐。
//!
//! `#[ignore]`：耗时随机器/文件系统浮动，写死阈值会造成假红。需要基线时显式运行：
//!
//! ```bash
//! cargo test --release -p qaqh-runtime --test tool_outbox_throughput_probe \
//!     -- --ignored --nocapture --test-threads=1
//! ```
//!
//! 对照组 = pre-fix 逐行复刻（进程级 `static Mutex` + 每条 `sync_all`），见
//! `legacy_record_in`。两组共用同一临时文件系统与同一批会话数，因此「扩展性
//! 形状」的对比是有效的。
//!
//! 记录持久化核对：每组结束后对新 worker（=新 writer 缓存）调用
//! `read_records`，断言条数完整——批量化 fsync 不得丢记录。

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use qaqh_runtime::agent::tool_outbox;

const SESSIONS: usize = 8;
const PER_THREAD: usize = 300;

/// pre-fix 逐行复刻：进程级 static 锁 + 每条 open→write→flush→sync_all。
static LEGACY_LOCK: Mutex<()> = Mutex::new(());

fn legacy_record_in(session_dir: &Path, call_id: &str, name: &str, success: bool) {
    let line = format!(
        "{{\"call_id\":\"{call_id}\",\"name\":\"{name}\",\"status\":\"{}\",\"ts\":0}}",
        if success { "ok" } else { "error" }
    );
    let _guard = LEGACY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ = File::options()
        .create(true)
        .append(true)
        .open(session_dir.join("tool_outbox.wal"))
        .and_then(|mut file| {
            file.write_all(line.as_bytes())?;
            file.write_all(b"\n")?;
            file.flush()?;
            file.sync_all()
        });
}

fn temp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("qaqh-outbox-bench-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp root");
    root
}

fn dirs(root: &Path, count: usize) -> Vec<PathBuf> {
    (0..count)
        .map(|i| {
            let dir = root.join(format!("s{i:08x}"));
            std::fs::create_dir_all(&dir).expect("dir");
            dir
        })
        .collect()
}

/// 跑一轮：`threads` 个会话各自在一个线程里追加 `PER_THREAD` 条。
/// 返回（墙钟耗时, p50 单条延迟微秒）。
fn run_round(
    dirs: &[PathBuf],
    threads: usize,
    record: impl Fn(&Path, usize, usize) + Copy + Send + Sync + 'static,
) -> (Duration, f64) {
    let write: std::sync::Arc<dyn Fn(&Path, usize, usize) + Send + Sync> =
        std::sync::Arc::new(record);
    let started = Instant::now();
    let handles: Vec<_> = dirs[..threads]
        .iter()
        .enumerate()
        .map(|(worker, dir)| {
            let dir = dir.clone();
            let write = std::sync::Arc::clone(&write);
            std::thread::spawn(move || {
                let mut samples: Vec<Duration> = Vec::with_capacity(PER_THREAD);
                for i in 0..PER_THREAD {
                    let t = Instant::now();
                    write(&dir, worker, i);
                    samples.push(t.elapsed());
                }
                samples
            })
        })
        .collect();
    let mut all = Vec::new();
    for handle in handles {
        all.extend(handle.join().expect("bench thread"));
    }
    let elapsed = started.elapsed();
    all.sort_unstable();
    let p50 = all[all.len() / 2].as_secs_f64() * 1e6;
    (elapsed, p50)
}

fn report(label: &str, threads: usize, elapsed: Duration, p50: f64) {
    let records = (threads * PER_THREAD) as f64;
    println!(
        "{label} {threads:>2} 线程：{:.0} 条/s（p50 {:.1}µs，墙钟 {:.1}ms）",
        records / elapsed.as_secs_f64(),
        p50,
        elapsed.as_secs_f64() * 1000.0
    );
}

/// pre-fix 复刻的「去掉 fsync」变体：只留**进程级 static Mutex + 每条 open/write**。
/// 用于把「锁粒度」这一半缺陷从「每条 fsync」中隔离出来——fsync 免费时 pre-fix
/// 吞吐仍应随线程下降（全局锁争用），post-fix 则应上升（分片锁并行）。
fn legacy_record_in_no_fsync(session_dir: &Path, call_id: &str, name: &str) {
    let line =
        format!("{{\"call_id\":\"{call_id}\",\"name\":\"{name}\",\"status\":\"ok\",\"ts\":0}}");
    let _guard = LEGACY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ = File::options()
        .create(true)
        .append(true)
        .open(session_dir.join("tool_outbox.wal"))
        .and_then(|mut file| {
            file.write_all(line.as_bytes())?;
            file.write_all(b"\n")?;
            file.flush()
        });
}

/// 纯锁效应对照（fsync 成本已从两边都移除或摊薄）：形状比绝对值重要。
#[test]
#[ignore = "measurement harness; run explicitly with --ignored --nocapture"]
fn outbox_lock_effect_without_disk_cost() {
    println!("\n== 纯锁效应对照（{SESSIONS} 会话 × {PER_THREAD} 条，无 fsync）==");
    for threads in [1usize, 2, 4, 8] {
        let root = temp_root(&format!("legacy-nofsync-{threads}"));
        let round_dirs = dirs(&root, SESSIONS);
        let (elapsed, p50) = run_round(&round_dirs, threads, |dir, worker, i| {
            legacy_record_in_no_fsync(dir, &format!("w{worker}-{i}"), "bash");
        });
        report("pre-fix ", threads, elapsed, p50);
    }
    for threads in [1usize, 2, 4, 8] {
        let root = temp_root(&format!("fixed-nofsync-{threads}"));
        let round_dirs = dirs(&root, SESSIONS);
        let (elapsed, p50) = run_round(&round_dirs, threads, |dir, worker, i| {
            tool_outbox::record_in(dir, &format!("w{worker}-{i}"), "bash", true);
        });
        report("post-fix", threads, elapsed, p50);
    }
}

#[test]
#[ignore = "measurement harness; run explicitly with --ignored --nocapture"]
fn outbox_throughput_scales_with_threads() {
    println!("\n== tool_outbox 聚合吞吐（{SESSIONS} 会话 × {PER_THREAD} 条，per-thread 并发）==");

    // 每轮用**全新目录**（每线程每轮恰好 PER_THREAD 条，便于落盘条数核对）。
    // ── pre-fix 基线（进程级锁 + 每条 fsync）────────────────────────────
    for threads in [1usize, 2, 4, 8] {
        let root = temp_root(&format!("legacy-{threads}"));
        let round_dirs = dirs(&root, SESSIONS);
        let (elapsed, p50) = run_round(&round_dirs, threads, |dir, worker, i| {
            legacy_record_in(dir, &format!("w{worker}-{i}"), "bash", true);
        });
        report("pre-fix ", threads, elapsed, p50);
    }

    // ── post-fix（分片锁 + 批量化 fsync）───────────────────────────────
    let mut last = Duration::ZERO;
    for threads in [1usize, 2, 4, 8] {
        let root = temp_root(&format!("fixed-{threads}"));
        let round_dirs = dirs(&root, SESSIONS);
        let (elapsed, p50) = run_round(&round_dirs, threads, |dir, worker, i| {
            tool_outbox::record_in(dir, &format!("w{worker}-{i}"), "bash", true);
        });
        report("post-fix", threads, elapsed, p50);
        last = elapsed;

        // ── 持久化语义核对：flush 后每个会话（含未参与的会话）记录完整 ──
        for dir in &round_dirs {
            tool_outbox::flush_in(dir);
            let expected = if round_dirs.iter().position(|d| d == dir).unwrap() < threads {
                PER_THREAD
            } else {
                0
            };
            let count = tool_outbox::read_records(dir).len();
            assert_eq!(
                count,
                expected,
                "{} 记录条数不符（flush 后）",
                dir.display()
            );
        }
    }
    println!(
        "记录核对：每轮 flush 后 8 会话记录条数全部相符 ✅（8 线程墙钟 {:.1}ms）",
        last.as_secs_f64() * 1000.0
    );
}
