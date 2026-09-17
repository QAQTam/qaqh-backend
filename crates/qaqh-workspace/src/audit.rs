//! Tool execution audit — append-only log of all tool calls.
//!
//! Each tool invocation produces an [`AuditEntry`] that is appended to
//! `<data_dir>/audit.csv`. The audit provides a tamper-evident trail
//! (via SHA-256 argument hashes) for security review.
//!
//! 增长有界（T-8-3① / 安全审查 P2 表）：单文件超过 [`MAX_AUDIT_BYTES`] 时
//! rotate 到 `audit.csv.1`（旧代顺移，最多 [`AUDIT_GENERATIONS`] 代）。
//! rotate 只做 **rename**，不 truncate —— 审计记录宁可多留不可丢。

use sha2::Digest;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 单个 `audit.csv` 的大小上限（字节）。达到后 rotate。
const MAX_AUDIT_BYTES: u64 = 4 * 1024 * 1024;

/// 保留的历史代数：`audit.csv.1` .. `audit.csv.{AUDIT_GENERATIONS}`。
const AUDIT_GENERATIONS: u32 = 3;

/// rotate 判定 + append 的进程内串行化。
///
/// 两个线程同时判定「超限」并 rotate，会让其中一个 rename 失败/把记录写进
/// 已被改名走的 inode；同进程内必须互斥。跨进程的安全性由 rename 语义保证
/// （见 [`rotate_if_needed`]）。
static AUDIT_APPEND_LOCK: Mutex<()> = Mutex::new(());

/// A single audit entry for a tool invocation.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub ts: String,
    pub user: String,
    pub tool: String,
    pub action: String,
    pub args_hash: String,
    pub result: String,
    pub elapsed_ms: u64,
    pub files: Vec<String>,
}

/// Path to the audit log file.
fn audit_path() -> std::path::PathBuf {
    let data_dir = qaqh_types::platform::data_dir();
    data_dir.join("audit.csv")
}

/// 第 `generation` 代 rotate 文件：`audit.csv` → `audit.csv.1` / `.2` / …。
fn rotated_path(path: &Path, generation: u32) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{generation}"));
    PathBuf::from(name)
}

/// 超限则 rotate：`audit.csv` → `audit.csv.1`，旧代顺移，最老一代删除。
///
/// **不丢记录**：只 `rename`，绝不 truncate。`rename` 在同一文件系统内是
/// 原子替换 ⇒ rotate 期间并发的 append（包括其它进程已持有 fd 的写）会落到
/// 被改名后的 inode（即 `.1`），记录仍在盘上；下一次 append 重新 open 即落到
/// 新的 `audit.csv`。唯一被丢弃的是超出 [`AUDIT_GENERATIONS`] 的最老一代——
/// 有界保留正是 rotation 的目的。
fn rotate_if_needed(path: &Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return; // 不存在：无需 rotate
    };
    if meta.len() < MAX_AUDIT_BYTES {
        return;
    }
    // 旧代顺移：从最老往新挪，避免覆盖。Windows 上 rename 到已存在目标会
    // 失败，故先删目标（即丢弃最老一代）。
    for generation in (1..AUDIT_GENERATIONS).rev() {
        let from = rotated_path(path, generation);
        if from.exists() {
            let to = rotated_path(path, generation + 1);
            let _ = std::fs::remove_file(&to);
            let _ = std::fs::rename(&from, &to);
        }
    }
    let first = rotated_path(path, 1);
    let _ = std::fs::remove_file(&first);
    if let Err(e) = std::fs::rename(path, &first) {
        // rename 失败（跨卷/权限/目标占用）：继续 append 到原文件——宁可
        // 继续增长，也不丢记录。
        log::error!("audit: rotate {} failed: {e}", path.display());
    }
}

/// Append a single audit entry to the CSV log.
pub fn append_audit(entry: &AuditEntry) {
    append_audit_to(&audit_path(), entry);
}

/// 落到显式路径的 append（`audit_path()` 之外的测试入口）。
fn append_audit_to(path: &Path, entry: &AuditEntry) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // rotate 判定与 append 在同一临界区：并发线程若在「判定超限 → rotate」
    // 与「open 旧 inode → write」之间交错，会把记录写进已被 rename 走的文件
    // （不丢但落错代），或与 rotate 竞争导致其失败后无限增长。
    let _guard = AUDIT_APPEND_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    rotate_if_needed(path);
    let mut file = match OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => f,
        Err(e) => {
            log::error!("audit: cannot open {}: {e}", path.display());
            return;
        }
    };
    // CSV row: ts,user,tool,action,args_hash,result,elapsed_ms,files
    let files_str = entry.files.join(";");
    let row = format!(
        "{},{},{},{},{},{},{},{}\n",
        csv_escape(&entry.ts),
        csv_escape(&entry.user),
        csv_escape(&entry.tool),
        csv_escape(&entry.action),
        csv_escape(&entry.args_hash),
        csv_escape(&entry.result),
        entry.elapsed_ms,
        csv_escape(&files_str),
    );
    if let Err(e) = file.write_all(row.as_bytes()) {
        log::error!("audit: write error: {e}");
    }
}

/// Compute SHA-256 hex digest of the serialized arguments.
pub fn hash_args(args: &serde_json::Value) -> String {
    let json_str = serde_json::to_string(args).unwrap_or_default();
    let hash = sha2::Sha256::digest(json_str.as_bytes());
    hex::encode(hash)
}

/// Escape a string for CSV: wrap in quotes if it contains comma, quote, or newline.
fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        let escaped = s.replace('"', "\"\"");
        format!("\"{}\"", escaped)
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(marker: &str) -> AuditEntry {
        AuditEntry {
            ts: "2026-09-17 12:00".to_string(),
            user: "seed".to_string(),
            tool: marker.to_string(),
            action: "run".to_string(),
            args_hash: "deadbeef".to_string(),
            result: "ok".to_string(),
            elapsed_ms: 1,
            files: Vec::new(),
        }
    }

    #[test]
    fn audit_csv_growth_bounded() {
        // T-8-3①：audit.csv 不再无界增长。预置一个超过上限的日志（模拟长期
        // append 后的体积），下一次 append 必须触发 rotate：活动文件体积回落
        // 到上限以下，旧内容与新记录都不丢（rename，不是 truncate）。
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.csv");
        let oversized = MAX_AUDIT_BYTES as usize + 64;
        std::fs::write(&path, "x".repeat(oversized)).expect("seed oversized audit");

        append_audit_to(&path, &entry("row-after-rotate"));

        let active = std::fs::read_to_string(&path).expect("read active");
        assert!(
            (active.len() as u64) < MAX_AUDIT_BYTES,
            "active audit.csv must shrink below the cap after rotate, got {} bytes",
            active.len()
        );
        assert!(
            active.contains("row-after-rotate"),
            "the triggering row must survive in the active file: {active:?}"
        );

        let rotated = std::fs::read_to_string(rotated_path(&path, 1)).expect("rotated .1 exists");
        assert_eq!(
            rotated.len(),
            oversized,
            "rotate must move (not truncate) the old file"
        );
        assert!(rotated.starts_with("xxx"));
    }

    #[test]
    fn audit_rotate_preserves_concurrent_rows() {
        // rotate 与并发 append 竞争时不得丢记录：8 线程 × 25 行，全部行必须
        // 能在活动文件或各代 rotate 文件中找到。
        let dir = tempfile::tempdir().expect("tempdir");
        let path = std::sync::Arc::new(dir.path().join("audit.csv"));
        std::fs::write(&*path, "x".repeat(MAX_AUDIT_BYTES as usize + 1)).expect("seed oversized");

        let mut handles = Vec::new();
        for t in 0..8u32 {
            let p = std::sync::Arc::clone(&path);
            handles.push(std::thread::spawn(move || {
                for i in 0..25u32 {
                    append_audit_to(&p, &entry(&format!("row-{t}-{i}")));
                }
            }));
        }
        for handle in handles {
            handle.join().expect("audit writer thread must not panic");
        }

        let mut all = std::fs::read_to_string(&*path).expect("read active");
        for generation in 1..=AUDIT_GENERATIONS {
            if let Ok(rotated) = std::fs::read_to_string(rotated_path(&path, generation)) {
                all.push_str(&rotated);
            }
        }
        for t in 0..8u32 {
            for i in 0..25u32 {
                let marker = format!("row-{t}-{i}");
                assert!(all.contains(&marker), "{marker} lost across rotate");
            }
        }
    }
}
