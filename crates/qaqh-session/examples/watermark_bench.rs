//! max_msg_id 水位 A/B 基准（BUG-2026-09-13-32）。
//!
//! 用法：
//!   cargo run --release -p qaqh-session --example watermark_bench -- <sessions_dir>
//!
//! 口径与 issue 报告一致：单线程墙钟，每档「灌满归档 → 反复小批 append」，
//! 报告稳态最小值。A 档（每批全量扫描）= 修复前，B 档（增量水位）= 修复后。
//!
//! 本基准不改动被测归档之外的磁盘状态；`--keep` 保留临时目录。

use qaqh_session::{SessionManager, store};
use qaqh_types::Message;
use std::path::PathBuf;
use std::time::Instant;

/// 各档归档目标体积（字节）：约 100 KiB / 1 MiB / 4 MiB / 10 MiB。
const SIZES: [(&str, usize); 4] = [
    ("101 KiB", 100 * 1024),
    ("1026 KiB", 1024 * 1024),
    ("4097 KiB", 4 * 1024 * 1024),
    ("10241 KiB", 10 * 1024 * 1024),
];

const BATCHES: usize = 20;

fn message(id: u64) -> Message {
    let mut msg = Message::user("0123456789abcdef0123456789abcdef");
    msg.msg_id = Some(id);
    msg
}

fn main() {
    let root: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("qaqh-watermark-bench"));
    let sessions_dir = root.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("create sessions dir");

    println!("sessions_dir = {}", sessions_dir.display());
    println!("{:<12} {:>12} {:>12} {:>9}", "archive", "A full-scan", "B watermark", "speedup");

    for (label, target) in SIZES {
        let seed = format!("bench-{target}");
        let dir = sessions_dir.join(&seed);
        SessionManager::init_for_test(root.clone());
        let sm = SessionManager::new_for_test(sessions_dir.clone(), root.join(".active_session"));

        // 灌满：按行大小估算条数（行 = JSON 序列化后的字节数 + 换行）。
        let line_bytes = serde_json::to_string(&message(1)).expect("serialize").len() + 1;
        let lines = target / line_bytes;
        let batch: Vec<Message> = (1..=lines as u64).map(message).collect();
        sm.save_append(&seed, &batch, "m", None, 0, 1);
        let actual = std::fs::metadata(dir.join("messages.jsonl"))
            .map(|m| m.len())
            .unwrap_or(0);
        let next_id = lines as u64 + 1;

        // A 档：每批都真全量扫描（修复前语义）。稳态最小值（首轮含页缓存
        // 冷读，取 min 与 issue 报告口径一致）。
        let mut best_a = f64::MAX;
        for _ in 0..BATCHES {
            let started = Instant::now();
            let _ = store::max_msg_id(&dir);
            best_a = best_a.min(started.elapsed().as_secs_f64() * 1e3);
        }

        // B 档：增量水位（修复后热路径）。
        store::reset_watermarks();
        let _ = store::watermark_msg_id(&dir); // 冷启动重建一次（不计入）
        let mut best_b = f64::MAX;
        for _ in 0..BATCHES {
            let started = Instant::now();
            let _ = store::watermark_msg_id(&dir);
            best_b = best_b.min(started.elapsed().as_secs_f64() * 1e3);
        }

        // C 档：修复后的端到端 save_append（含读水位 + 落盘 + fsync），
        // 与 A 档同口径对比——issue 关心的「每批 append 耗时」。
        store::reset_watermarks();
        let _ = store::watermark_msg_id(&dir);
        let mut best_c = f64::MAX;
        for _ in 0..BATCHES {
            let started = Instant::now();
            sm.save_append(&seed, &[message(next_id)], "m", None, 0, 2);
            best_c = best_c.min(started.elapsed().as_secs_f64() * 1e3);
        }
        assert_eq!(store::max_msg_id(&dir), next_id, "水位必须与权威扫描一致");

        println!(
            "{label:<12} {:>10.3} ms {:>10.3} ms {:>8.1}×  ({} KiB)  append e2e {:.3} ms",
            best_a,
            best_b,
            best_a / best_b,
            actual / 1024,
            best_c
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    if !std::env::args().any(|a| a == "--keep") {
        let _ = std::fs::remove_dir_all(&root);
    }
}
