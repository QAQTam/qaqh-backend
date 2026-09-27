//! Low-level JSONL I/O for session persistence.
//!
//! Each session directory contains:
//!   meta.json      — session metadata (small, atomic replace-write)
//!   messages.jsonl — one JSON line per Message, append-only
//!
//! A central `index.json` in the sessions root enables fast listing
//! without scanning every session directory.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use qaqh_types::{Message, SessionMeta};

pub mod bounded_read;

// ── Meta ──

/// Write session metadata to `meta.json` atomically (write to temp, rename).
pub fn write_meta(session_dir: &Path, meta: &SessionMeta) -> Result<(), String> {
    let tmp = session_dir.join(".meta.tmp");
    let dst = session_dir.join("meta.json");
    let json = serde_json::to_string_pretty(meta).map_err(|e| format!("serialize meta: {e}"))?;
    {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("create meta tmp: {e}"))?;
        f.write_all(json.as_bytes())
            .map_err(|e| format!("write meta tmp: {e}"))?;
        f.flush().map_err(|e| format!("flush meta tmp: {e}"))?;
        f.sync_all().map_err(|e| format!("sync meta tmp: {e}"))?;
    }
    fs::rename(&tmp, &dst).map_err(|e| format!("rename meta: {e}"))?;
    Ok(())
}

/// Read session metadata from `meta.json`.
pub fn read_meta(session_dir: &Path) -> Option<SessionMeta> {
    let path = session_dir.join("meta.json");
    let data = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&data).ok()
}

// ── Messages (JSONL) ──

/// Append a single message as a JSON line to `messages.jsonl`.
/// Used for immediate per-message persistence.
pub fn append_one(session_dir: &Path, msg: &Message) -> Result<(), String> {
    let path = session_dir.join("messages.jsonl");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open messages.jsonl: {e}"))?;
    let line = serde_json::to_string(msg).map_err(|e| format!("serialize message: {e}"))?;
    writeln!(file, "{line}").map_err(|e| format!("write message: {e}"))?;
    file.flush().map_err(|e| format!("flush: {e}"))?;
    file.sync_all().map_err(|e| format!("sync: {e}"))?;
    Ok(())
}

/// Append messages as JSON lines to `messages.jsonl`.
/// Creates the file if it doesn't exist.
pub fn append_messages(session_dir: &Path, messages: &[Message]) -> Result<(), String> {
    let path = session_dir.join("messages.jsonl");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open messages.jsonl: {e}"))?;
    for msg in messages {
        let line = serde_json::to_string(msg).map_err(|e| format!("serialize message: {e}"))?;
        writeln!(file, "{line}").map_err(|e| format!("write message: {e}"))?;
    }
    file.flush().map_err(|e| format!("flush messages: {e}"))?;
    file.sync_all().map_err(|e| format!("sync messages: {e}"))?;
    Ok(())
}

/// Rewrite the entire messages.jsonl with the given messages.
/// Used after undo or compact.
pub fn rewrite_messages(session_dir: &Path, messages: &[Message]) -> Result<(), String> {
    let tmp = session_dir.join(".messages.tmp");
    let dst = session_dir.join("messages.jsonl");
    {
        let mut file = fs::File::create(&tmp).map_err(|e| format!("create tmp: {e}"))?;
        for msg in messages {
            let line = serde_json::to_string(msg).map_err(|e| format!("serialize: {e}"))?;
            writeln!(file, "{line}").map_err(|e| format!("write: {e}"))?;
        }
        file.flush().map_err(|e| format!("flush: {e}"))?;
        file.sync_all().map_err(|e| format!("sync: {e}"))?;
    }
    fs::rename(&tmp, &dst).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

/// Max persisted `msg_id` in messages.jsonl (0 when the archive is empty
/// or the messages carry no ids). Scans the file once without materializing
/// messages (BUG-2026-09-12-07: used by `save_append` to make appends
/// idempotent against WAL replay double-writes).
///
/// 这是**权威判据**（每次真读盘）。热路径请用 [`watermark_msg_id`]：
/// 后者是同一量的增量视图，语义等价但 amortized O(批次)。
pub fn max_msg_id(session_dir: &Path) -> u64 {
    max_msg_id_full_scan(session_dir)
}

fn max_msg_id_full_scan(session_dir: &Path) -> u64 {
    let path = session_dir.join("messages.jsonl");
    note_full_scan(session_dir);
    let Ok(content) = fs::read_to_string(&path) else {
        return 0;
    };
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Message>(line).ok())
        .filter_map(|message| message.msg_id)
        .max()
        .unwrap_or(0)
}

// ── msg_id 水位（BUG-2026-09-13-32，性能族）──

/// 归档身份判据：`(字节长度, 修改时间)` 变化即作废水位缓存。
///
/// 不变量：写入路径上所有调用方**先落盘、后推进水位**（`note_watermark`
/// 只在 append_messages 返回 Ok 之后调用），因此「身份与缓存建立时不一致」
/// 只可能来自外部 writer / save_full 重写 / 目录重建。这类情况下本轮走
/// 全量扫描并把新水位记入缓存（harness：判据不失效时才要求零扫描）。
type ArchiveIdentity = (u64, Option<std::time::SystemTime>);

struct WatermarkEntry {
    identity: ArchiveIdentity,
    max_msg_id: u64,
}

fn watermarks()
-> &'static std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, WatermarkEntry>> {
    static WATERMARKS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, WatermarkEntry>>,
    > = std::sync::OnceLock::new();
    WATERMARKS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 归档当前身份（不存在视为 `(0, None)`）。
fn archive_identity(path: &Path) -> ArchiveIdentity {
    match fs::metadata(path) {
        Ok(meta) => (meta.len(), meta.modified().ok()),
        Err(_) => (0, None),
    }
}

/// 归档内最大 `msg_id` 的**增量水位**。
///
/// 缓存命中（身份未变）→ 只看一次 `stat`，不再读归档。身份变化（外部
/// writer、save_full 重写、会话重建）→ 重建：优先反向尾读（O(尾部)），
/// 尾窗无 id 才回落全量扫描（旧归档尾部可能全是无 id 消息）。
///
/// 与 [`max_msg_id`] 语义等价：两者都返回归档内最大 `msg_id`（无则 0）。
pub fn watermark_msg_id(session_dir: &Path) -> u64 {
    let path = session_dir.join("messages.jsonl");
    let identity = archive_identity(&path);
    if let Some(entry) = watermarks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_dir)
        && entry.identity == identity
    {
        return entry.max_msg_id;
    }
    let rebuilt = rebuild_watermark(&path, identity);
    let max_msg_id = rebuilt.max_msg_id;
    watermarks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(session_dir.to_path_buf(), rebuilt);
    max_msg_id
}

/// 尾读窗口（行数）：`read_last_lines` 以 64 KiB 块反向读，凑够即停。
const TAIL_WINDOW_LINES: usize = 4096;

/// 重建水位（不做缓存读写）。
fn rebuild_watermark(path: &Path, identity: ArchiveIdentity) -> WatermarkEntry {
    if identity.0 == 0 {
        return WatermarkEntry {
            identity,
            max_msg_id: 0,
        };
    }
    // 只有「窗口内一条带 id 的消息都没有且头部还有未读内容」（旧归档 /
    // 全无 id 消息）才回落全量扫描——与修复前同阶，但每个归档只付一次。
    let (lines, truncated) =
        bounded_read::read_last_lines(path, TAIL_WINDOW_LINES).unwrap_or_default();
    let mut tail_max = 0u64;
    for line in &lines {
        if let Ok(message) = serde_json::from_str::<Message>(line)
            && let Some(id) = message.msg_id
        {
            tail_max = tail_max.max(id);
        }
    }
    let max_msg_id = if tail_max > 0 || !truncated {
        tail_max
    } else {
        max_msg_id_full_scan(path.parent().unwrap_or(path))
    };
    WatermarkEntry {
        identity,
        max_msg_id,
    }
}

/// 归档被改写后强制重建水位（`save_full` 全量重写、`delete` 目录销毁）。
/// 缓存按目录路径键控，与目录是否存在无关。
pub fn invalidate_watermark(session_dir: &Path) {
    watermarks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(session_dir);
}

/// 清空全部水位缓存（测试隔离用）。
#[doc(hidden)]
pub fn reset_watermarks() {
    watermarks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// 全量扫描计数器（测试用规模判据：缓存命中路径必须不递增）。
#[doc(hidden)]
pub fn reset_scan_counter() {
    scans().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// 指定会话目录的累计全量扫描次数（测试用）。
#[doc(hidden)]
pub fn scan_count(session_dir: &Path) -> usize {
    scans()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_dir)
        .copied()
        .unwrap_or(0)
}

#[doc(hidden)]
pub fn note_full_scan(session_dir: &Path) {
    *scans()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(session_dir.to_path_buf())
        .or_insert(0) += 1;
}

fn scans() -> &'static std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, usize>> {
    static SCANS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, usize>>,
    > = std::sync::OnceLock::new();
    SCANS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 落盘成功后推进水位（只增不减；写入路径的唯一入口）。
///
/// 身份取当前文件的 `(len, mtime)`：写路径结束于此，所以这一步同时把
/// 缓存标记为「与盘面一致」。水位低于既有缓存值时不回退（msg_id 会话
/// 单调；save_full 的重基走 [`invalidate_watermark`]）。
pub fn note_watermark(session_dir: &Path, max_msg_id: u64) {
    let path = session_dir.join("messages.jsonl");
    let identity = archive_identity(&path);
    let mut cache = watermarks().lock().unwrap_or_else(|e| e.into_inner());
    let entry = cache
        .entry(session_dir.to_path_buf())
        .or_insert(WatermarkEntry {
            identity,
            max_msg_id,
        });
    entry.max_msg_id = entry.max_msg_id.max(max_msg_id);
    entry.identity = identity;
}

/// Count lines in messages.jsonl (fast, reads line-by-line without parsing JSON).
pub fn count_message_lines(session_dir: &Path) -> Result<usize, String> {
    let path = session_dir.join("messages.jsonl");
    if !path.exists() {
        return Ok(0);
    }
    let file = fs::File::open(&path).map_err(|e| format!("open: {e}"))?;
    let reader = BufReader::new(file);
    Ok(reader.lines().count())
}

// ── Index ──

/// Phase 3（无 SQLite 方案）：append-only 增量索引。
///
/// 旧 `index.json` 每次 upsert 都要 读全量→改内存→全量重写+fsync（O(N)
/// 写放大，IndexLock 忙等放大锁竞争）。新格式 `index.jsonl` 每行一个
/// 操作（对标 Kafka log compaction / Codex 的 JSONL-权威思路）：
///
/// - `upsert`：追加一行（O(1)，无锁竞争放大，同 key 后行胜）；
/// - `remove`：追加 tombstone（O(1)）；
/// - 读取时一次性重放（内存 map 归并）；
/// - 行数超过 archive 阈值时启动/定期 compact 成紧凑全量（保留每个
///   seed 的最新 upsert，丢弃 tombstone）。
///
/// 兼容：首次读取若无 `index.jsonl` 但有旧 `index.json`，一次性迁移
/// （生成 jsonl 后删除旧文件，格式破坏性变更已获 owner 批准）。
//
/// 单行操作（wire: 每行一个 JSON 对象）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum IndexOp {
    Upsert { meta: Box<SessionMeta> },
    Remove { session_id: String },
}

/// 索引日志的 compact 阈值（行数）：超过后整文件重写为每 seed 一行。
const INDEX_COMPACT_LINES: usize = 1024;

fn index_log_path(sessions_dir: &Path) -> std::path::PathBuf {
    sessions_dir.join("index.jsonl")
}

fn legacy_index_path(sessions_dir: &Path) -> std::path::PathBuf {
    sessions_dir.join("index.json")
}

/// 读取并归并索引（含旧格式一次性迁移）。
fn read_merged_index(sessions_dir: &Path) -> Vec<SessionMeta> {
    let path = index_log_path(sessions_dir);
    if !path.exists()
        && let Some(migrated) = migrate_legacy_index_if_present(sessions_dir)
    {
        return migrated;
    }
    let Ok(data) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut by_session: std::collections::HashMap<String, SessionMeta> =
        std::collections::HashMap::new();
    let mut lines = 0usize;
    for line in data.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        lines += 1;
        match serde_json::from_str::<IndexOp>(trimmed) {
            Ok(IndexOp::Upsert { meta }) => {
                by_session.insert(meta.session_id.clone(), *meta);
            }
            Ok(IndexOp::Remove { session_id }) => {
                by_session.remove(&session_id);
            }
            Err(_) => continue, // 损坏行跳过（日志尾部中断可容忍）
        }
    }
    // 阈值外不做写放大：compact 只在读取（列表页）路径顺带执行。
    if lines > INDEX_COMPACT_LINES {
        compact_index(sessions_dir, by_session.values());
    }
    by_session.into_values().collect()
}

/// 旧 `index.json` → 新 `index.jsonl`（一次性；迁移后删除旧文件）。
fn migrate_legacy_index_if_present(sessions_dir: &Path) -> Option<Vec<SessionMeta>> {
    let legacy = legacy_index_path(sessions_dir);
    let data = fs::read_to_string(&legacy).ok()?;
    let metas: Vec<SessionMeta> = serde_json::from_str(&data).unwrap_or_default();
    rewrite_index_log(sessions_dir, metas.iter());
    let _ = fs::remove_file(&legacy);
    log::info!(
        "[index] migrated legacy index.json ({} sessions) to index.jsonl",
        metas.len()
    );
    Some(metas)
}

/// 整文件重写为紧凑全量（每个 seed 最新一行，无 tombstone）。
fn rewrite_index_log<'a>(sessions_dir: &Path, metas: impl Iterator<Item = &'a SessionMeta>) {
    let tmp = sessions_dir.join(".index.jsonl.tmp");
    let dst = index_log_path(sessions_dir);
    let mut body = String::new();
    for meta in metas {
        let op = IndexOp::Upsert {
            meta: Box::new(meta.clone()),
        };
        if let Ok(line) = serde_json::to_string(&op) {
            body.push_str(&line);
            body.push('\n');
        }
    }
    if fs::write(&tmp, body).is_err() {
        return;
    }
    let _ = fs::rename(&tmp, &dst);
}

fn compact_index<'a>(sessions_dir: &Path, metas: impl Iterator<Item = &'a SessionMeta>) {
    rewrite_index_log(sessions_dir, metas);
}

/// Read the central session index.
pub fn read_index(sessions_dir: &Path) -> Vec<SessionMeta> {
    read_merged_index(sessions_dir)
}

/// 追加一行索引操作（进程内串行由调用方保证；跨进程依赖 daemon 单实例
/// 假设——`.qaqh/daemon.lock` 已排除双 daemon，append 单行窗口极小）。
fn append_index_op(sessions_dir: &Path, op: &IndexOp) {
    let Ok(line) = serde_json::to_string(op) else {
        return;
    };
    let path = index_log_path(sessions_dir);
    let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) else {
        log::warn!("[index] append failed: cannot open {}", path.display());
        return;
    };
    let _ = file.write_all(line.as_bytes());
    let _ = file.write_all(b"\n");
    let _ = file.flush();
}

/// Upsert a single session meta into the index：O(1) append，无全量重写。
/// 同 seed 后行胜（读侧归并语义），启动后首次列表读取超阈值时自动 compact。
pub fn upsert_index(sessions_dir: &Path, meta: &SessionMeta) {
    append_index_op(
        sessions_dir,
        &IndexOp::Upsert {
            meta: Box::new(meta.clone()),
        },
    );
}

/// Remove a session from the index：追加 tombstone，读侧归并时丢弃。
/// tombstone 随下次 compact 消失，日志不会无界增长。
pub fn remove_from_index(sessions_dir: &Path, session_id: &str) {
    append_index_op(
        sessions_dir,
        &IndexOp::Remove {
            session_id: session_id.to_string(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_sessions_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "qaqh-index-{}-{}-{}",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn meta(session_id: &str, updated: u64) -> SessionMeta {
        SessionMeta {
            session_id: session_id.to_string(),
            updated_at: updated,
            ..SessionMeta::default()
        }
    }

    #[test]
    fn legacy_index_json_migrates_to_jsonl_once() {
        let dir = temp_sessions_dir("migrate");
        let legacy = vec![meta("a", 1), meta("b", 2)];
        fs::write(
            legacy_index_path(&dir),
            serde_json::to_string_pretty(&legacy).unwrap(),
        )
        .unwrap();

        let read = read_index(&dir);
        assert_eq!(read.len(), 2, "legacy entries must survive migration");
        assert!(index_log_path(&dir).is_file(), "jsonl created");
        assert!(!legacy_index_path(&dir).exists(), "legacy file removed");

        // 二次读取稳定（迁移不重复执行）。
        assert_eq!(read_index(&dir).len(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn upsert_and_remove_merge_last_write_wins() {
        let dir = temp_sessions_dir("merge");
        upsert_index(&dir, &meta("a", 1));
        upsert_index(&dir, &meta("b", 2));
        upsert_index(&dir, &meta("a", 3)); // 同 session_id 后行胜
        assert_eq!(read_index(&dir).len(), 2);
        assert_eq!(
            read_index(&dir)
                .iter()
                .find(|m| m.session_id == "a")
                .unwrap()
                .updated_at,
            3
        );
        remove_from_index(&dir, "a");
        let after = read_index(&dir);
        assert_eq!(after.len(), 1, "tombstone removes the seed");
        assert_eq!(after[0].session_id, "b");
        // tombstone 后再 upsert 复活。
        upsert_index(&dir, &meta("a", 9));
        assert_eq!(read_index(&dir).len(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn compact_drops_tombstones_and_dedupes() {
        let dir = temp_sessions_dir("compact");
        // 超过阈值（1024）的 upsert 行：同 seed 重复 600 次 + tombstone。
        for i in 0..1200 {
            upsert_index(&dir, &meta("hot", i));
        }
        upsert_index(&dir, &meta("cold", 1));
        remove_from_index(&dir, "cold");
        let read = read_index(&dir);
        assert_eq!(read.len(), 1, "merge collapses to latest per seed");
        assert_eq!(read[0].session_id, "hot");
        assert_eq!(read[0].updated_at, 1199);
        // compact 后日志行数收缩（每 seed ≤ 1 行 + 可能的后续 append）。
        let lines = fs::read_to_string(index_log_path(&dir))
            .unwrap()
            .lines()
            .count();
        assert!(lines <= 3, "log must be compacted, got {lines} lines");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn torn_tail_line_is_tolerated() {
        // 崩溃写一半：最后一行非法 JSON，读侧跳过、不 panic。
        let dir = temp_sessions_dir("torn");
        upsert_index(&dir, &meta("a", 1));
        let path = index_log_path(&dir);
        let mut body = fs::read_to_string(&path).unwrap();
        body.push_str("{\"op\":\"upsert\",\"meta\":{\"seed\":\"b");
        fs::write(&path, body).unwrap();
        let read = read_index(&dir);
        assert_eq!(read.len(), 1, "torn tail must be skipped");
        assert_eq!(read[0].session_id, "a");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
