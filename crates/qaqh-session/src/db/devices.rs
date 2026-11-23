//! `paired_devices` 的行级原语(v4.4 P1-3,MUST 铁闸 2)。
//!
//! 取代 `<data_dir>/ringing-devices.json` 的「全量覆写 + 损坏即空启动」
//! (L-20):设备注册表跑在鉴权热路径上,状态权威是库表;token 摘要列
//! `token_digest` 带 UNIQUE(跨代旧文件里的重复条目在吸入期即主键冲突响,
//! 不再是静默 last-wins)。

use rusqlite::{Connection, OptionalExtension};

use super::DbError;

#[derive(Debug, Clone)]
pub struct DeviceRow {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    /// snake_case 词表:view | interact | admin。
    pub scope: String,
    pub token_digest: String,
    pub created_at_ms: i64,
    pub last_seen_ms: i64,
}

pub fn insert(conn: &Connection, row: &DeviceRow) -> Result<(), DbError> {
    conn.execute(
        "INSERT INTO paired_devices \
         (device_id, name, platform, scope, token_digest, created_at_ms, last_seen_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            row.device_id,
            row.name,
            row.platform,
            row.scope,
            row.token_digest,
            row.created_at_ms,
            row.last_seen_ms,
        ],
    )?;
    Ok(())
}

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceRow> {
    Ok(DeviceRow {
        device_id: row.get(0)?,
        name: row.get(1)?,
        platform: row.get(2)?,
        scope: row.get(3)?,
        token_digest: row.get(4)?,
        created_at_ms: row.get(5)?,
        last_seen_ms: row.get(6)?,
    })
}

const COLUMNS: &str = "device_id, name, platform, scope, token_digest, created_at_ms, last_seen_ms";

pub fn get_by_id(conn: &Connection, device_id: &str) -> Result<Option<DeviceRow>, DbError> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM paired_devices WHERE device_id = ?1"),
            [device_id],
            map_row,
        )
        .optional()?)
}

pub fn get_by_digest(conn: &Connection, token_digest: &str) -> Result<Option<DeviceRow>, DbError> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM paired_devices WHERE token_digest = ?1"),
            [token_digest],
            map_row,
        )
        .optional()?)
}

pub fn list(conn: &Connection) -> Result<Vec<DeviceRow>, DbError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM paired_devices ORDER BY created_at_ms"
    ))?;
    let rows = stmt
        .query_map([], map_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 删除并返回被删行(不存在 = None,调用方据此回 404)。
pub fn delete(conn: &Connection, device_id: &str) -> Result<Option<DeviceRow>, DbError> {
    let Some(row) = get_by_id(conn, device_id)? else {
        return Ok(None);
    };
    conn.execute("DELETE FROM paired_devices WHERE device_id = ?1", [device_id])?;
    Ok(Some(row))
}

/// 节流更新 last_seen(与旧 touch 的 60s 窗口同律):只有距上次命中
/// 超过 `min_delta_ms` 才产生写。返回是否真的落了库。
pub fn touch(
    conn: &Connection,
    device_id: &str,
    now_ms: i64,
    min_delta_ms: i64,
) -> Result<bool, DbError> {
    let changed = conn.execute(
        "UPDATE paired_devices SET last_seen_ms = ?1 \
         WHERE device_id = ?2 AND ?1 - last_seen_ms >= ?3",
        rusqlite::params![now_ms, device_id, min_delta_ms],
    )?;
    Ok(changed > 0)
}
