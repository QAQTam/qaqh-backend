//! Low-level JSONL I/O for session persistence.
//!
//! Session metadata lives in the fact journal (`crate::meta`); this module keeps
//! only the listing index and the still-used JSONL helpers.
//!
//! A central `index.jsonl` in the sessions root is a rebuildable cache that
//! enables fast listing without replaying every journal.

use std::fs;
use std::io::Write;
use std::path::Path;

use qaqh_types::SessionMeta;

// ── Meta ──

/// Commit a full metadata state as a single journal patch.
///
/// `meta.json` no longer exists: the journal is the only durable truth, and
/// this function is the write adapter the legacy `SessionManager` closures use.
/// A caller that already knows exactly which fields changed should prefer
/// [`crate::meta::patch`] with a narrow patch instead of this full-state form.
pub fn write_meta(session_dir: &Path, meta: &SessionMeta) -> Result<(), String> {
    crate::meta::patch(
        session_dir,
        crate::session_fact_v2::SessionMetadataPatch {
            cwd: meta.cwd.clone(),
            model: Some(meta.model.clone()),
            archived: Some(meta.archived),
            profile: Some(meta.profile.clone().unwrap_or_default()),
            effort: Some(meta.effort.clone().unwrap_or_default()),
            mode: Some(meta.mode),
            tool_mode: Some(meta.tool_mode.clone()),
            custom_tools: Some(meta.custom_tools.clone()),
            skills: Some(meta.skills.clone()),
            frozen_annotation: meta.frozen_annotation.clone(),
            ephemeral: Some(meta.ephemeral),
            usage: Some(crate::session_fact_v2::UsageTotals {
                totals: meta.usage_totals.clone(),
                requests: meta.usage_requests,
                cache_reported_requests: meta.cache_reported_requests,
            }),
            ..Default::default()
        },
    )
}

/// Read session metadata by replaying the committed facts.
pub fn read_meta(session_dir: &Path) -> Option<SessionMeta> {
    crate::meta::read(session_dir)
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

/// 读取并归并索引。
fn read_merged_index(sessions_dir: &Path) -> Vec<SessionMeta> {
    let path = index_log_path(sessions_dir);
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
