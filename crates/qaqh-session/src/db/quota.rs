//! `quota_reservations` 的行级原语(v4.4 P1-3,MUST 铁闸 2)。
//!
//! 取代 `quota/<root>/ledger.jsonl` 的「无超时文件锁 + 全量 replay + 逐条
//! fsync」(L-02):预算判定的读与预留行的写落在调用方的同一个写事务里,
//! 串行写执行器天然互斥;词表(spawn/message/content/tool、held/committed/
//! released、release reason)由 `qaqh-agent-runtime` 的 `QuotaLedger` 侧
//! 维护,本模块只认字符串列值。

use rusqlite::{Connection, OptionalExtension, params};

use super::DbError;

#[derive(Debug, Clone, Copy, Default)]
pub struct QuotaTotals {
    /// state='held' 行的 amount 合计。
    pub held: u64,
    /// state='committed' 行的 amount 合计。
    pub committed: u64,
    /// state='held' 的行数(预留笔数)。
    pub held_count: u64,
}

/// 预算水位:held 与 committed 同计 `total_used`(旧 LedgerState::total_used 同律)。
pub fn used_totals(conn: &Connection, root_session_id: &str) -> Result<QuotaTotals, DbError> {
    let (held, committed, count): (i64, i64, i64) = conn.query_row(
        "SELECT \
           COALESCE(SUM(CASE WHEN state = 'held' THEN amount END), 0), \
           COALESCE(SUM(CASE WHEN state = 'committed' THEN amount END), 0), \
           COALESCE(SUM(CASE WHEN state = 'held' THEN 1 END), 0) \
         FROM quota_reservations WHERE root_session_id = ?1",
        [root_session_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok(QuotaTotals {
        held: held.max(0) as u64,
        committed: committed.max(0) as u64,
        held_count: count.max(0) as u64,
    })
}

pub struct NewReservation<'a> {
    pub reservation_id: &'a str,
    pub root_session_id: &'a str,
    pub child_session_id: Option<&'a str>,
    pub kind: &'a str,
    pub amount: u64,
    pub canonical_key: &'a str,
    pub reserved_at_ms: i64,
}

/// INSERT 一条 held 预留;重复 id 主键冲突当场响(旧 replay 的
/// 「duplicate reservation = 损坏」同律)。
pub fn insert_reserved(conn: &Connection, row: &NewReservation<'_>) -> Result<(), DbError> {
    conn.execute(
        "INSERT INTO quota_reservations \
         (reservation_id, root_session_id, child_session_id, kind, amount, \
          canonical_key, state, release_reason, reserved_at_ms, settled_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'held', NULL, ?7, NULL)",
        params![
            row.reservation_id,
            row.root_session_id,
            row.child_session_id,
            row.kind,
            row.amount as i64,
            row.canonical_key,
            row.reserved_at_ms,
        ],
    )?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct StoredReservation {
    /// held | committed | released。
    pub state: String,
    pub amount: u64,
}

pub fn get_reservation(
    conn: &Connection,
    reservation_id: &str,
) -> Result<Option<StoredReservation>, DbError> {
    Ok(conn
        .query_row(
            "SELECT state, amount FROM quota_reservations WHERE reservation_id = ?1",
            [reservation_id],
            |row| {
                Ok(StoredReservation {
                    state: row.get(0)?,
                    amount: row.get::<_, i64>(1)?.max(0) as u64,
                })
            },
        )
        .optional()?)
}

/// held → 终态迁移。返回受影响行数(0 = 并发下已被别的写者定结,调用方
/// 据此做幂等判定)。
pub fn settle(
    conn: &Connection,
    reservation_id: &str,
    to_state: &str,
    release_reason: Option<&str>,
    settled_at_ms: i64,
) -> Result<usize, DbError> {
    let changed = conn.execute(
        "UPDATE quota_reservations \
         SET state = ?2, release_reason = ?3, settled_at_ms = ?4 \
         WHERE reservation_id = ?1 AND state = 'held'",
        params![reservation_id, to_state, release_reason, settled_at_ms],
    )?;
    Ok(changed)
}

/// 本 root 下仍 held、child 不在 canonical 集合中的 spawn 预留
/// (reconciliation 候选;过滤语义与旧 reconcile_spawns 逐条对齐)。
pub fn stale_spawn_reservations(
    conn: &Connection,
    root_session_id: &str,
    canonical_children: &std::collections::HashSet<String>,
) -> Result<Vec<(String, String)>, DbError> {
    let mut stmt = conn.prepare(
        "SELECT reservation_id, child_session_id FROM quota_reservations \
         WHERE root_session_id = ?1 AND state = 'held' AND kind = 'spawn' \
           AND child_session_id IS NOT NULL",
    )?;
    let rows = stmt
        .query_map([root_session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter(|(_, child)| !canonical_children.contains(child))
        .collect())
}
