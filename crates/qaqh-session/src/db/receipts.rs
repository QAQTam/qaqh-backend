//! `command_receipts` 的行级原语(v4.4 P1-3,MUST 铁闸 2)。
//!
//! 取代 `<data_dir>/ringing-command-receipts.json` 的全量覆写(L-06)。
//! TTL 与容量裁剪都是 SQL:`delete_expired` + `prune_oldest_over` 在同一
//! 写事务里与预留 INSERT 合并,不存在「先全量读进内存再全量写回」的窗口。

use rusqlite::{Connection, OptionalExtension};

use super::DbError;

#[derive(Debug, Clone)]
pub struct ReceiptRow {
    pub command_id: String,
    pub fingerprint: String,
    pub client_session_id: Option<String>,
    /// snake_case 词表:accepted | running | succeeded | failed | rejected。
    pub state: String,
    pub terminal_event_id: Option<String>,
    pub error_code: Option<String>,
    pub result_json: Option<String>,
    pub sync_result_json: Option<String>,
    pub accepted_at_ms: i64,
}

const COLUMNS: &str = "command_id, fingerprint, client_session_id, state, terminal_event_id, \
                        error_code, result_json, sync_result_json, accepted_at_ms";

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReceiptRow> {
    Ok(ReceiptRow {
        command_id: row.get(0)?,
        fingerprint: row.get(1)?,
        client_session_id: row.get(2)?,
        state: row.get(3)?,
        terminal_event_id: row.get(4)?,
        error_code: row.get(5)?,
        result_json: row.get(6)?,
        sync_result_json: row.get(7)?,
        accepted_at_ms: row.get(8)?,
    })
}

pub fn get(conn: &Connection, command_id: &str) -> Result<Option<ReceiptRow>, DbError> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM command_receipts WHERE command_id = ?1"),
            [command_id],
            map_row,
        )
        .optional()?)
}

/// 预留/重放落库:同 ID 覆盖(过期行由调用方在同一事务里先行判定)。
pub fn upsert(conn: &Connection, row: &ReceiptRow) -> Result<(), DbError> {
    conn.execute(
        "INSERT OR REPLACE INTO command_receipts \
         (command_id, fingerprint, client_session_id, state, terminal_event_id, \
          error_code, result_json, sync_result_json, accepted_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            row.command_id,
            row.fingerprint,
            row.client_session_id,
            row.state,
            row.terminal_event_id,
            row.error_code,
            row.result_json,
            row.sync_result_json,
            row.accepted_at_ms,
        ],
    )?;
    Ok(())
}

/// 只迁状态与终态字段,不动 fingerprint/owner/accepted_at(保持首次接受年龄)。
#[allow(clippy::too_many_arguments)]
pub fn update_state(
    conn: &Connection,
    command_id: &str,
    state: &str,
    terminal_event_id: Option<&str>,
    error_code: Option<&str>,
    result_json: Option<&str>,
    sync_result_json: Option<&str>,
) -> Result<usize, DbError> {
    let changed = conn.execute(
        "UPDATE command_receipts \
         SET state = ?2, terminal_event_id = ?3, error_code = ?4, \
             result_json = ?5, sync_result_json = ?6 \
         WHERE command_id = ?1",
        rusqlite::params![command_id, state, terminal_event_id, error_code, result_json, sync_result_json],
    )?;
    Ok(changed)
}

pub fn delete(conn: &Connection, command_id: &str) -> Result<(), DbError> {
    conn.execute("DELETE FROM command_receipts WHERE command_id = ?1", [command_id])?;
    Ok(())
}

/// 物理删除已超 TTL 的行(巡检与预留路径同事务调用,幂等表不再认它们)。
pub fn delete_expired(conn: &Connection, cutoff_ms: i64) -> Result<usize, DbError> {
    Ok(conn.execute(
        "DELETE FROM command_receipts WHERE accepted_at_ms < ?1",
        [cutoff_ms],
    )?)
}

/// 容量上限裁剪:按 accepted_at_ms 升序删超出 max_entries 的最老行。
pub fn prune_oldest_over(conn: &Connection, max_entries: usize) -> Result<usize, DbError> {
    Ok(conn.execute(
        "DELETE FROM command_receipts WHERE command_id IN (\
           SELECT command_id FROM command_receipts ORDER BY accepted_at_ms ASC \
           LIMIT MAX((SELECT COUNT(*) FROM command_receipts) - ?1, 0)\
         )",
        [max_entries as i64],
    )?)
}

/// Accepted/Running 且未过期的行(冻结告警巡检的候选集)。
pub fn list_unsettled(conn: &Connection, cutoff_ms: i64) -> Result<Vec<ReceiptRow>, DbError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM command_receipts \
         WHERE state IN ('accepted', 'running') AND accepted_at_ms >= ?1 \
         ORDER BY accepted_at_ms ASC"
    ))?;
    let rows = stmt
        .query_map([cutoff_ms], map_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}
