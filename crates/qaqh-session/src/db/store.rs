//! 业务表直查函数:全部接收 `&Connection`,由调用方在 `Db::write`/`Db::read`
//! 闭包内组合成短事务。不做 Repository 抽象。

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// sessions
// ---------------------------------------------------------------------------

/// 新会话入库(meta 快照即列)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewSession {
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub title: String,
    pub title_source: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub profile: Option<String>,
    pub mode: Option<String>,
    pub tool_mode: Option<String>,
    pub ephemeral: bool,
}

pub fn create_session(conn: &mut Connection, session: &NewSession) -> rusqlite::Result<()> {
    let now = now_ms();
    conn.execute(
        "INSERT INTO sessions
            (session_id, parent_session_id, title, title_source, cwd, model, effort,
             profile, mode, tool_mode, ephemeral, created_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            session.session_id,
            session.parent_session_id,
            session.title,
            session.title_source,
            session.cwd,
            session.model,
            session.effort,
            session.profile,
            session.mode,
            session.tool_mode,
            i64::from(session.ephemeral),
            now,
            now,
        ],
    )?;
    Ok(())
}

/// 会话列表行:列表页直接查询,不重建投影。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListRow {
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub title: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub archived: bool,
    pub ephemeral: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

pub fn list_sessions(
    conn: &Connection,
    include_archived: bool,
    limit: u32,
) -> rusqlite::Result<Vec<SessionListRow>> {
    let mut stmt = conn.prepare(
        "SELECT session_id, parent_session_id, title, cwd, model, archived, ephemeral,
                created_at_ms, updated_at_ms
         FROM sessions
         WHERE (?1 = 1 OR archived = 0)
         ORDER BY updated_at_ms DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![i64::from(include_archived), limit], |row| {
        Ok(SessionListRow {
            session_id: row.get(0)?,
            parent_session_id: row.get(1)?,
            title: row.get(2)?,
            cwd: row.get(3)?,
            model: row.get(4)?,
            archived: row.get::<_, i64>(5)? != 0,
            ephemeral: row.get::<_, i64>(6)? != 0,
            created_at_ms: row.get(7)?,
            updated_at_ms: row.get(8)?,
        })
    })?;
    rows.collect()
}

pub fn session_exists(conn: &Connection, session_id: &str) -> rusqlite::Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sessions WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// 更新单个标量 meta 字段(title/model/effort/mode/...)并触碰 updated_at。
pub fn patch_session_meta(
    conn: &mut Connection,
    session_id: &str,
    patch: &SessionMetaPatch,
) -> rusqlite::Result<()> {
    let now = now_ms();
    let SessionMetaPatch {
        title,
        title_source,
        cwd,
        model,
        effort,
        profile,
        mode,
        tool_mode,
        archived,
        ephemeral,
    } = patch;
    if let Some(title) = title {
        conn.execute(
            "UPDATE sessions SET title = ?2, title_source = ?3, updated_at_ms = ?4
             WHERE session_id = ?1",
            params![session_id, title, title_source, now],
        )?;
    }
    for (column, value) in [
        ("cwd", cwd),
        ("model", model),
        ("effort", effort),
        ("profile", profile),
        ("mode", mode),
        ("tool_mode", tool_mode),
    ] {
        if let Some(value) = value {
            let sql = format!(
                "UPDATE sessions SET {column} = ?2, updated_at_ms = ?3 WHERE session_id = ?1"
            );
            conn.execute(&sql, params![session_id, value, now])?;
        }
    }
    if let Some(archived) = archived {
        conn.execute(
            "UPDATE sessions SET archived = ?2, updated_at_ms = ?3 WHERE session_id = ?1",
            params![session_id, i64::from(*archived), now],
        )?;
    }
    if let Some(ephemeral) = ephemeral {
        conn.execute(
            "UPDATE sessions SET ephemeral = ?2, updated_at_ms = ?3 WHERE session_id = ?1",
            params![session_id, i64::from(*ephemeral), now],
        )?;
    }
    Ok(())
}

#[derive(Debug, Default, Clone)]
pub struct SessionMetaPatch {
    pub title: Option<String>,
    pub title_source: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub profile: Option<String>,
    pub mode: Option<String>,
    pub tool_mode: Option<String>,
    pub archived: Option<bool>,
    pub ephemeral: Option<bool>,
}

// ---------------------------------------------------------------------------
// turns + blocks
// ---------------------------------------------------------------------------

pub fn start_turn(
    conn: &mut Connection,
    turn_id: &str,
    session_id: &str,
    input_id: Option<&str>,
    mode: Option<&str>,
) -> rusqlite::Result<()> {
    let seq: i64 = conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM turns WHERE session_id = ?1",
        [session_id],
        |row| row.get(0),
    )?;
    conn.execute(
        "INSERT INTO turns (turn_id, session_id, seq, input_id, mode, status, started_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6)",
        params![turn_id, session_id, seq, input_id, mode, now_ms()],
    )?;
    Ok(())
}

/// `outcome_json` 保存 serde 形式的契约 `TurnOutcome`;视图层反序列化,坏数据报错。
pub fn finish_turn(
    conn: &mut Connection,
    turn_id: &str,
    status: &str,
    outcome_json: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE turns SET status = ?2, outcome_json = ?3, finished_at_ms = ?4 WHERE turn_id = ?1",
        params![turn_id, status, outcome_json, now_ms()],
    )?;
    Ok(())
}

/// 会话内全局块序:同会话所有块按插入次序编号,transcript 分页一次排好。
pub fn next_block_ord(conn: &Connection, session_id: &str) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COALESCE(MAX(ord), 0) + 1 FROM blocks WHERE session_id = ?1",
        [session_id],
        |row| row.get(0),
    )
}

#[derive(Debug, Clone)]
pub struct NewBlock {
    pub block_id: String,
    pub session_id: String,
    pub turn_id: String,
    pub kind: &'static str,
    pub text: Option<String>,
    pub call_id: Option<String>,
    /// 完整契约 `TimelineEntry` 的 serde 形式;kind/call_id/text 是查询用冗余列。
    pub payload_json: String,
}

pub fn append_block(conn: &mut Connection, block: &NewBlock) -> rusqlite::Result<i64> {
    let ord = next_block_ord(conn, &block.session_id)?;
    conn.execute(
        "INSERT INTO blocks (block_id, session_id, turn_id, ord, kind, text, call_id, payload_json, created_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            block.block_id,
            block.session_id,
            block.turn_id,
            ord,
            block.kind,
            block.text,
            block.call_id,
            block.payload_json,
            now_ms(),
        ],
    )?;
    Ok(ord)
}

/// transcript 分页:按会话全局块序倒序取一页,正序返回。
pub fn read_transcript_page(
    conn: &Connection,
    session_id: &str,
    before_ord: Option<i64>,
    limit: u32,
) -> rusqlite::Result<Vec<BlockRow>> {
    let mut stmt = conn.prepare(
        "SELECT block_id, turn_id, ord, kind, text, call_id, payload_json, created_at_ms
         FROM blocks
         WHERE session_id = ?1 AND (?2 IS NULL OR ord < ?2)
         ORDER BY ord DESC
         LIMIT ?3",
    )?;
    let mut rows: Vec<BlockRow> = stmt
        .query_map(params![session_id, before_ord, limit], |row| {
            Ok(BlockRow {
                block_id: row.get(0)?,
                turn_id: row.get(1)?,
                ord: row.get(2)?,
                kind: row.get(3)?,
                text: row.get(4)?,
                call_id: row.get(5)?,
                payload_json: row.get(6)?,
                created_at_ms: row.get(7)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    rows.reverse();
    Ok(rows)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockRow {
    pub block_id: String,
    pub turn_id: String,
    pub ord: i64,
    pub kind: String,
    pub text: Option<String>,
    pub call_id: Option<String>,
    pub payload_json: Option<String>,
    pub created_at_ms: i64,
}

// ---------------------------------------------------------------------------
// tool_calls
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ToolCallUpsert {
    pub call_id: String,
    pub session_id: String,
    pub turn_id: String,
    pub tool_name: String,
    pub args_json: Option<String>,
    pub status: &'static str,
}

pub fn upsert_tool_call(conn: &mut Connection, call: &ToolCallUpsert) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO tool_calls (call_id, session_id, turn_id, tool_name, args_json, status, started_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(call_id) DO UPDATE SET
            args_json = COALESCE(excluded.args_json, args_json),
            status = excluded.status",
        params![
            call.call_id,
            call.session_id,
            call.turn_id,
            call.tool_name,
            call.args_json,
            call.status,
            now_ms(),
        ],
    )?;
    Ok(())
}

pub fn finish_tool_call(
    conn: &mut Connection,
    call_id: &str,
    status: &str,
    output_text: Option<&str>,
    output_ref: Option<&str>,
    output_truncated: bool,
    error_json: Option<&str>,
    meta_json: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE tool_calls
         SET status = ?2, output_text = ?3, output_ref = ?4, output_truncated = ?5,
             error_json = ?6, meta_json = ?7, finished_at_ms = ?8
         WHERE call_id = ?1",
        params![
            call_id,
            status,
            output_text,
            output_ref,
            i64::from(output_truncated),
            error_json,
            meta_json,
            now_ms(),
        ],
    )?;
    Ok(())
}

/// 运行中/待授权工具:快照必须包含未完成卡片。
pub fn unfinished_tool_calls(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Vec<ToolCallRow>> {
    read_tool_calls_where(conn, session_id, "AND status IN ('declared','awaiting_permission','running')")
}

pub fn recent_tool_calls(
    conn: &Connection,
    session_id: &str,
    limit: u32,
) -> rusqlite::Result<Vec<ToolCallRow>> {
    read_tool_calls_where(conn, session_id, &format!("ORDER BY started_at_ms DESC LIMIT {limit}"))
}

fn read_tool_calls_where(
    conn: &Connection,
    session_id: &str,
    suffix: &str,
) -> rusqlite::Result<Vec<ToolCallRow>> {
    let sql = format!(
        "SELECT call_id, turn_id, tool_name, args_json, status, output_text, output_ref,
                output_truncated, error_json, meta_json, started_at_ms, finished_at_ms
         FROM tool_calls WHERE session_id = ?1 {suffix}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([session_id], |row| {
        Ok(ToolCallRow {
            call_id: row.get(0)?,
            turn_id: row.get(1)?,
            tool_name: row.get(2)?,
            args_json: row.get(3)?,
            status: row.get(4)?,
            output_text: row.get(5)?,
            output_ref: row.get(6)?,
            output_truncated: row.get::<_, i64>(7)? != 0,
            error_json: row.get(8)?,
            meta_json: row.get(9)?,
            started_at_ms: row.get(10)?,
            finished_at_ms: row.get(11)?,
        })
    })?;
    rows.collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRow {
    pub call_id: String,
    pub turn_id: String,
    pub tool_name: String,
    pub args_json: Option<String>,
    pub status: String,
    pub output_text: Option<String>,
    pub output_ref: Option<String>,
    pub output_truncated: bool,
    pub error_json: Option<String>,
    pub meta_json: Option<String>,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
}

// ---------------------------------------------------------------------------
// interactions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct NewInteraction {
    pub interaction_id: String,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub call_id: Option<String>,
    pub kind: &'static str,
    pub payload_json: String,
}

pub fn create_interaction(conn: &mut Connection, interaction: &NewInteraction) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO interactions (interaction_id, session_id, turn_id, call_id, kind, payload_json, status, created_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7)",
        params![
            interaction.interaction_id,
            interaction.session_id,
            interaction.turn_id,
            interaction.call_id,
            interaction.kind,
            interaction.payload_json,
            now_ms(),
        ],
    )?;
    Ok(())
}

/// 统一应答入口:只有 pending 状态允许更新,返回是否真的发生了状态迁移。
pub fn resolve_interaction(
    conn: &mut Connection,
    interaction_id: &str,
    status: &str,
    decision_json: &str,
) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        "UPDATE interactions
         SET status = ?2, decision_json = ?3, resolved_at_ms = ?4
         WHERE interaction_id = ?1 AND status = 'pending'",
        params![interaction_id, status, decision_json, now_ms()],
    )?;
    Ok(changed > 0)
}

pub fn pending_interactions(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Vec<InteractionRow>> {
    let mut stmt = conn.prepare(
        "SELECT interaction_id, turn_id, call_id, kind, payload_json, status, decision_json, created_at_ms, resolved_at_ms
         FROM interactions
         WHERE session_id = ?1 AND status = 'pending'
         ORDER BY created_at_ms",
    )?;
    rows_from(stmt.query_map([session_id], map_interaction)?)
}

pub fn recent_interactions(
    conn: &Connection,
    session_id: &str,
    limit: u32,
) -> rusqlite::Result<Vec<InteractionRow>> {
    let mut stmt = conn.prepare(
        "SELECT interaction_id, turn_id, call_id, kind, payload_json, status, decision_json, created_at_ms, resolved_at_ms
         FROM interactions
         WHERE session_id = ?1
         ORDER BY created_at_ms DESC
         LIMIT ?2",
    )?;
    rows_from(stmt.query_map(params![session_id, limit], map_interaction)?)
}

fn map_interaction(row: &rusqlite::Row<'_>) -> rusqlite::Result<InteractionRow> {
    Ok(InteractionRow {
        interaction_id: row.get(0)?,
        turn_id: row.get(1)?,
        call_id: row.get(2)?,
        kind: row.get(3)?,
        payload_json: row.get(4)?,
        status: row.get(5)?,
        decision_json: row.get(6)?,
        created_at_ms: row.get(7)?,
        resolved_at_ms: row.get(8)?,
    })
}

fn rows_from<T>(rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>) -> rusqlite::Result<Vec<T>> {
    rows.collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionRow {
    pub interaction_id: String,
    pub turn_id: Option<String>,
    pub call_id: Option<String>,
    pub kind: String,
    pub payload_json: String,
    pub status: String,
    pub decision_json: Option<String>,
    pub created_at_ms: i64,
    pub resolved_at_ms: Option<i64>,
}

// ---------------------------------------------------------------------------
// blobs
// ---------------------------------------------------------------------------

/// 内容寻址大内容:同 hash 同字节,重复写入静默忽略。
pub fn put_blob(conn: &mut Connection, hash: &str, bytes: &[u8]) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO blobs (hash, bytes, size, created_at_ms) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(hash) DO NOTHING",
        params![hash, bytes, bytes.len() as i64, now_ms()],
    )?;
    Ok(())
}

pub fn get_blob(conn: &Connection, hash: &str) -> rusqlite::Result<Option<Vec<u8>>> {
    conn.query_row("SELECT bytes FROM blobs WHERE hash = ?1", [hash], |row| {
        row.get::<_, Vec<u8>>(0)
    })
    .optional()
}
