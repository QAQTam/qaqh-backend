//! v4.5.1 任务2/3:存储体积卫生维护原语(修复单 §二.2/§二.3,宪法 #8 独立子模块)。
//!
//! 三个职责,全部作用于既有 schema 13——零迁移、零新磁盘格式:
//! 1. [`configure_journal_size_limit`]:`journal_size_limit = 16 MiB`,封顶 `-wal` 长期膨胀;
//! 2. [`checkpoint_truncate`] / [`reclaim_if_fragmented`]:删除会话树后的 WAL 截断与
//!    高碎片阈值空间回收(小删除不达阈值直接跳过,避免全库 VACUUM 卡顿);
//! 3. [`collect_dangling_candidates`] / [`delete_orphan_session_contents`]:
//!    `truncate_from_turn` 撤回回合后清理孤儿 `session_contents` BLOB。

use std::collections::HashSet;

use rusqlite::{Connection, Transaction};

use super::{DbError, store};

/// WAL 高水位上限(修复单 §二.3.1):16 MiB。
pub const WAL_SIZE_LIMIT_BYTES: i64 = 16 * 1024 * 1024;

/// 可回收空闲页阈值(修复单 §二.3.2):`freelist_count >= 1024`(4 KiB 页制下
/// ≥ 4 MiB)且 `freelist_count * 100 / page_count >= 30` 才执行空间回收。
const RECLAIM_MIN_FREELIST_PAGES: i64 = 1024;
const RECLAIM_MIN_FREELIST_PERCENT: i64 = 30;

/// 删除路径执行 `TRUNCATE` checkpoint 的 WAL 帧数下限(64 页 = 256 KiB):
/// 小 WAL 的删除只做不阻塞的 PASSIVE 合帧,不为小会话付出截断与 busy 等待。
const CHECKPOINT_TRUNCATE_MIN_WAL_PAGES: i64 = 64;

/// 设置并读回确认 `journal_size_limit`(修复单 §二.3.1;读回校验 fail-closed)。
pub fn configure_journal_size_limit(conn: &Connection) -> Result<(), DbError> {
    conn.pragma_update(None, "journal_size_limit", WAL_SIZE_LIMIT_BYTES)?;
    let actual: i64 = conn.query_row("PRAGMA journal_size_limit", [], |row| row.get(0))?;
    if actual != WAL_SIZE_LIMIT_BYTES {
        return Err(DbError::JournalSizeLimit {
            actual,
            expected: WAL_SIZE_LIMIT_BYTES,
        });
    }
    Ok(())
}

/// `PRAGMA wal_checkpoint(TRUNCATE)`:安全截断已同步 WAL(修复单 §二.3.2)。
///
/// 有并发读者钉住 WAL 时返回 `busy = 1`、本次不截断——这是良性竞态,不算错误,
/// 下一个维护窗口重试;其余错误如实上抛,严禁吞错。
pub fn checkpoint_truncate(conn: &mut Connection) -> Result<(), DbError> {
    let (busy, _log_pages, _checkpointed_pages): (i64, i64, i64) = conn.query_row(
        "PRAGMA wal_checkpoint(TRUNCATE)",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if busy != 0 {
        log::debug!("[db:maintenance] wal_checkpoint(TRUNCATE) busy; wal kept for next window");
    }
    Ok(())
}

/// 高碎片时回收主库空间(修复单 §二.3.2)。返回是否执行了 VACUUM。
///
/// **盘满安全取舍**(修复单 §二.3.2 明示):VACUUM 需要创建临时副本,盘满时
/// SQLite 返回 `SQLITE_FULL`——其语义保证 VACUUM 中途失败**不损坏主库**(原
/// 文件原样保留,临时文件清理)。这里如实上抛错误,由调用方响亮记日志;
/// 严禁静默吞掉,也严禁在主库可能受损的假设下继续。
pub fn reclaim_if_fragmented(conn: &mut Connection) -> Result<bool, DbError> {
    let page_count: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let freelist: i64 = conn.query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
    if page_count <= 0
        || freelist < RECLAIM_MIN_FREELIST_PAGES
        || freelist * 100 / page_count < RECLAIM_MIN_FREELIST_PERCENT
    {
        return Ok(false);
    }
    conn.execute("VACUUM", [])?;
    Ok(true)
}

/// 删除会话树后的维护收口(修复单 §二.3.3):先截断 WAL,再按碎片阈值回收。
/// 不达阈值自动跳过全库 VACUUM(删除小会话不卡顿)。维护失败响亮记日志、
/// 不上抛——调用点(删除事务)已经提交成功,不得用维护错误掩盖删除结果。
///
/// **小 WAL 快速路径**:`TRUNCATE` checkpoint 在有并发读者时会按 busy_timeout
/// 等待,且每次删除都全量截断代价不值。先用 `PASSIVE`(不阻塞、不等读者)
/// 探明当前 WAL 帧数:低于 [`CHECKPOINT_TRUNCATE_MIN_WAL_PAGES`] 时跳过
/// `TRUNCATE`(PASSIVE 已顺手把已提交帧合入主库);回收后仍按原样补一次
/// TRUNCATE——VACUUM 重建的紧凑库必须落回主库文件,否则体积只涨不跌。
pub(crate) fn checkpoint_after_delete(conn: &mut Connection, session_id: &str) {
    let (busy, wal_frames, _checkpointed): (i64, i64, i64) =
        match conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        }) {
            Ok(value) => value,
            Err(error) => {
                log::error!(
                    "[db:maintenance] wal probe after delete {session_id}: {error}"
                );
                return;
            }
        };
    let _ = busy;
    if wal_frames >= CHECKPOINT_TRUNCATE_MIN_WAL_PAGES
        && let Err(error) = checkpoint_truncate(conn)
    {
        log::error!("[db:maintenance] wal checkpoint after delete {session_id}: {error}");
    }
    match reclaim_if_fragmented(conn) {
        Ok(true) => {
            log::info!("[db:maintenance] reclaimed fragmented pages after deleting {session_id}");
            if let Err(error) = checkpoint_truncate(conn) {
                log::error!(
                    "[db:maintenance] post-reclaim wal checkpoint for {session_id}: {error}"
                );
            }
        }
        Ok(false) => {}
        Err(error) => {
            log::error!("[db:maintenance] space reclaim after delete {session_id}: {error}")
        }
    }
}

/// 撤回前收集被删行引用的 BLOB 候选(修复单 §二.2.2):被撤回合的
/// `tool_calls.output_ref` / `tool_calls.diff_ref` 与被撤回合的
/// `exec_sessions.output_content_id`,经 [`store::bare_content_id`] 归一为
/// 裸 64-hex 并去重。**必须在删除三表之前调用**(删除后引用就查不到了)。
pub(crate) fn collect_dangling_candidates(
    tx: &Transaction<'_>,
    session_id: &str,
    target_seq: i64,
) -> Result<Vec<String>, DbError> {
    let doomed = "(SELECT turn_id FROM turns WHERE session_id = ?1 AND seq >= ?2)";
    let mut candidates: HashSet<String> = HashSet::new();
    {
        let mut stmt = tx.prepare(&format!(
            "SELECT output_ref, diff_ref FROM tool_calls
             WHERE session_id = ?1 AND turn_id IN {doomed}"
        ))?;
        let rows = stmt.query_map(rusqlite::params![session_id, target_seq], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        })?;
        for row in rows {
            let (output_ref, diff_ref) = row?;
            if let Some(reference) = output_ref {
                candidates.insert(store::bare_content_id(&reference).to_string());
            }
            if let Some(reference) = diff_ref {
                candidates.insert(store::bare_content_id(&reference).to_string());
            }
        }
    }
    {
        let mut stmt = tx.prepare(&format!(
            "SELECT output_content_id FROM exec_sessions
             WHERE session_id = ?1 AND origin_turn_id IN {doomed}"
        ))?;
        let rows = stmt.query_map(rusqlite::params![session_id, target_seq], |row| {
            row.get::<_, Option<String>>(0)
        })?;
        for row in rows {
            if let Some(reference) = row? {
                candidates.insert(store::bare_content_id(&reference).to_string());
            }
        }
    }
    Ok(candidates.into_iter().collect())
}

/// 撤回删除 `tool_calls` / `exec_sessions` / `blocks` / `turns` 之后、同事务内
/// 清理孤儿 BLOB(修复单 §二.2.2):候选集合内、且**不再被**该会话剩余
/// `tool_calls.output_ref` / `tool_calls.diff_ref` / `exec_sessions.output_content_id`
/// 引用的行才删除——保留回合与被撤回合共用的同一 `content_id` 绝不误删。
/// 返回删除的行数。
pub(crate) fn delete_orphan_session_contents(
    tx: &Transaction<'_>,
    session_id: &str,
    candidates: &[String],
) -> Result<usize, DbError> {
    // refs 库内可能是 wire 形态(`sha256:<hex>`),与裸 hex 主键比较前剥前缀
    // (schema 6→7 迁移同款口径);NULL 引用比较恒为 NULL,NOT EXISTS 自然跳过。
    const BARE_REF: &str =
        "CASE WHEN {col} LIKE 'sha256:%' THEN substr({col}, 8) ELSE {col} END";
    let mut deleted = 0usize;
    for content_id in candidates {
        deleted += tx.execute(
            &format!(
                "DELETE FROM session_contents WHERE session_id = ?1 AND content_id = ?2
                 AND NOT EXISTS (
                     SELECT 1 FROM tool_calls tc
                     WHERE tc.session_id = ?1
                       AND ({out} = ?2 OR {diff} = ?2)
                 )
                 AND NOT EXISTS (
                     SELECT 1 FROM exec_sessions es
                     WHERE es.session_id = ?1 AND {content} = ?2
                 )",
                out = BARE_REF.replace("{col}", "tc.output_ref"),
                diff = BARE_REF.replace("{col}", "tc.diff_ref"),
                content = BARE_REF.replace("{col}", "es.output_content_id"),
            ),
            rusqlite::params![session_id, content_id],
        )?;
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::super::Db;
    use super::*;
    use crate::owner::OwnerDirectory;
    use std::path::Path;

    /// 修复单 §四 任务3 反证1:`Db::open` 后读回 `journal_size_limit` 等于
    /// 16 MiB(16777216)。旧实现不设置该 PRAGMA(读回 0),必红。
    #[test]
    fn journal_size_limit_is_capped_at_16mib_after_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open(dir.path().join("qaqh.db")).expect("open");
        let actual: i64 = db
            .read_blocking(|conn| {
                conn.query_row("PRAGMA journal_size_limit", [], |row| row.get(0))
                    .map_err(super::DbError::from)
            })
            .expect("pragma read");
        assert_eq!(actual, WAL_SIZE_LIMIT_BYTES);
    }

    /// 修复单 §四 任务3 反证2:向会话写入 5 MiB 大 BLOB 后删除该会话树,
    /// `qaqh.db` 物理体积必须显著回落、碎片页必须已回收。旧实现既不 checkpoint
    /// 也不回收(文件体积与高水位 freelist 基本不回落),必红。
    #[test]
    fn deleting_a_blob_heavy_session_reclaims_disk_space() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("qaqh.db");
        let owners = OwnerDirectory::open(&path).expect("open owners db");
        owners
            .ensure_session("SES-BIG", None, false)
            .expect("ensure session");
        let blob = vec![0xABu8; 5 * 1024 * 1024];
        let blob_len = blob.len();
        owners
            .db()
            .write_blocking(move |conn| {
                store::put_session_content(
                    conn,
                    "SES-BIG",
                    &"a".repeat(64),
                    "application/octet-stream",
                    &blob,
                    false,
                )
            })
            .expect("put 5MiB blob");
        let size_before = file_len(&path);
        assert!(size_before >= blob_len as u64, "大 BLOB 必须已写入主库");

        owners.delete_session_tree("SES-BIG").expect("delete tree");

        let size_after = file_len(&path);
        assert!(
            size_after < size_before,
            "删除大 BLOB 会话后主库必须物理回落(旧实现残留空闲页不回落): \
             {size_before} -> {size_after}"
        );
        let freelist: i64 = owners
            .db()
            .read_blocking(|conn| {
                conn.query_row("PRAGMA freelist_count", [], |row| row.get(0))
                    .map_err(super::DbError::from)
            })
            .expect("freelist read");
        assert!(
            freelist < RECLAIM_MIN_FREELIST_PAGES,
            "高碎片必须已被阈值回收:剩余 freelist = {freelist}"
        );
    }

    /// `checkpoint_truncate` 把非空 `-wal` 截断回 0 字节(修复单 §二.3.2)。
    #[test]
    fn checkpoint_truncate_zeroes_the_wal_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("qaqh.db");
        let db = Db::open(&path).expect("open");
        // 制造非空 WAL:一次写事务。
        db.write_blocking(|conn| {
            conn.execute(
                "INSERT INTO workspaces (workspace_id, path, title, order_index)
                 VALUES ('w1', '/tmp/w1', 'w1', 1)",
                [],
            )
            .map_err(super::DbError::from)
        })
        .expect("write");
        let wal_path = format!("{}-wal", path.display());
        let wal = Path::new(&wal_path);
        assert!(
            wal.metadata().expect("wal must exist").len() > 0,
            "前置:写事务后 -wal 必须非空"
        );
        db.write_blocking(|conn| checkpoint_truncate(conn))
            .expect("checkpoint");
        assert_eq!(
            wal.metadata().expect("wal must exist").len(),
            0,
            "TRUNCATE checkpoint 必须把 -wal 截断回 0 字节"
        );
    }

    /// 阈值守卫:新鲜库(无碎片)不触发 VACUUM——删除小会话不得卡顿。
    #[test]
    fn reclaim_skips_when_fragmentation_is_below_threshold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut conn = rusqlite::Connection::open(dir.path().join("qaqh.db")).expect("open");
        conn.execute("CREATE TABLE t (x TEXT)", []).expect("table");
        conn.execute("INSERT INTO t VALUES ('hello')", [])
            .expect("row");
        let reclaimed = reclaim_if_fragmented(&mut conn).expect("reclaim");
        assert!(!reclaimed, "碎片低于阈值必须跳过回收");
    }

    fn file_len(path: &Path) -> u64 {
        std::fs::metadata(path).expect("db file").len()
    }
}
