//! 业务表直查函数:全部接收 `&Connection`,由调用方在 `Db::write`/`Db::read`
//! 闭包内组合成短事务。不做 Repository 抽象。

use super::DbError;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
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
    pub model_selection: Option<qaqh_types::ModelSelection>,
    pub custom_tools: Option<Vec<String>>,
    pub skills: Option<qaqh_types::SkillSessionStateV2>,
    pub frozen_annotation: Option<String>,
    pub ephemeral: bool,
}

pub fn create_session(conn: &mut Connection, session: &NewSession) -> rusqlite::Result<()> {
    let now = now_ms();
    conn.execute(
        "INSERT INTO sessions
            (session_id, parent_session_id, title, title_source, cwd, model, effort,
             profile, mode, tool_mode, model_selection, custom_tools_json, skills_json,
             frozen_annotation, ephemeral, created_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?16)",
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
            session
                .model_selection
                .as_ref()
                .map(|selection| serde_json::to_string(selection))
                .transpose()
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))?,
            session
                .custom_tools
                .as_ref()
                .map(|tools| serde_json::to_string(tools))
                .transpose()
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))?,
            session
                .skills
                .as_ref()
                .map(|skills| serde_json::to_string(skills))
                .transpose()
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))?,
            session.frozen_annotation,
            i64::from(session.ephemeral),
            now,
        ],
    )?;
    Ok(())
}

pub fn session_tree(conn: &Connection, root: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "WITH RECURSIVE tree(id) AS (SELECT session_id FROM sessions WHERE session_id = ?1 \
         UNION SELECT s.session_id FROM sessions s JOIN tree ON s.parent_session_id = tree.id) \
         SELECT id FROM tree",
    )?;
    stmt.query_map([root], |row| row.get::<_, String>(0))?
        .collect()
}

/// Delete a business session and its descendants in one transaction.
pub fn delete_session_tree(conn: &mut Connection, root: &str) -> Result<Vec<String>, DbError> {
    let tx = conn.transaction()?;
    let ids = session_tree(&tx, root)?;
    if ids.is_empty() {
        return Err(DbError::SessionNotFound {
            session_id: root.to_string(),
        });
    }
    for id in &ids {
        for table in [
            "blocks",
            "tool_calls",
            "exec_sessions",
            "interactions",
            "turns",
        ] {
            tx.execute(&format!("DELETE FROM {table} WHERE session_id = ?1"), [id])?;
        }
        tx.execute("DELETE FROM team_tasks WHERE root_session_id = ?1", [id])?;
        tx.execute(
            "DELETE FROM board_channels WHERE root_session_id = ?1",
            [id],
        )?;
        tx.execute(
            "DELETE FROM board_subscriptions WHERE root_session_id = ?1",
            [id],
        )?;
        tx.execute("DELETE FROM sessions WHERE session_id = ?1", [id])?;
    }
    tx.commit()?;
    Ok(ids)
}

/// Spawn 期登记:行不存在则插入默认行,已存在则不动(幂等)。
/// 血缘后补:spawn 先建行、拿到父会话后再把 `parent_session_id` 补上——
/// 只在当前为 NULL 时写,不覆盖已有的明确血缘。
pub fn ensure_session(
    conn: &mut Connection,
    session_id: &str,
    parent_session_id: Option<&str>,
    ephemeral: bool,
) -> rusqlite::Result<()> {
    let now = now_ms();
    conn.execute(
        "INSERT INTO sessions
            (session_id, parent_session_id, title, title_source, cwd, model, effort,
             profile, mode, tool_mode, ephemeral, created_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?1, 'user', NULL, NULL, NULL, NULL, NULL, NULL, ?3, ?4, ?4)
         ON CONFLICT(session_id) DO NOTHING",
        params![session_id, parent_session_id, i64::from(ephemeral), now],
    )?;
    if let Some(parent) = parent_session_id {
        conn.execute(
            "UPDATE sessions SET parent_session_id = ?2, updated_at_ms = ?3
             WHERE session_id = ?1 AND parent_session_id IS NULL",
            params![session_id, parent, now],
        )?;
    }
    Ok(())
}

/// 单会话业务行(元数据读取用)。
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub session_id: String,
    pub title: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub profile: Option<String>,
    pub mode: Option<String>,
    pub tool_mode: Option<String>,
    pub model_selection: Option<qaqh_types::ModelSelection>,
    pub custom_tools: Vec<String>,
    pub skills: Option<qaqh_types::SkillSessionStateV2>,
    pub frozen_annotation: Option<String>,
    pub settings_revision: u64,
    pub archived: bool,
    pub ephemeral: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

pub fn session_row(conn: &Connection, session_id: &str) -> rusqlite::Result<Option<SessionRow>> {
    conn.query_row(
        "SELECT session_id, title, cwd, model, effort, profile, mode, tool_mode,
                model_selection, custom_tools_json, skills_json, frozen_annotation,
                settings_revision, archived, ephemeral, created_at_ms, updated_at_ms
         FROM sessions WHERE session_id = ?1",
        [session_id],
        |row| {
            Ok(SessionRow {
                session_id: row.get(0)?,
                title: row.get(1)?,
                cwd: row.get(2)?,
                model: row.get(3)?,
                effort: row.get(4)?,
                profile: row.get(5)?,
                mode: row.get(6)?,
                tool_mode: row.get(7)?,
                model_selection: row
                    .get::<_, Option<String>>(8)?
                    .and_then(|json| serde_json::from_str(&json).ok()),
                custom_tools: row
                    .get::<_, Option<String>>(9)?
                    .and_then(|json| serde_json::from_str(&json).ok())
                    .unwrap_or_default(),
                skills: row
                    .get::<_, Option<String>>(10)?
                    .and_then(|json| serde_json::from_str(&json).ok()),
                frozen_annotation: row.get(11)?,
                settings_revision: row.get::<_, i64>(12)?.max(0) as u64,
                archived: row.get::<_, i64>(13)? != 0,
                ephemeral: row.get::<_, i64>(14)? != 0,
                created_at_ms: row.get(15)?,
                updated_at_ms: row.get(16)?,
            })
        },
    )
    .optional()
}

/// 行 + 回合计数,一次读取(session_meta 组装用)。
pub fn session_row_with_turn_count(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Option<(SessionRow, i64)>> {
    let count = turn_count(conn, session_id)?;
    Ok(session_row(conn, session_id)?.map(|row| (row, count)))
}

pub fn session_parent(conn: &Connection, session_id: &str) -> rusqlite::Result<Option<String>> {
    match conn.query_row(
        "SELECT parent_session_id FROM sessions WHERE session_id = ?1",
        [session_id],
        |row| row.get::<_, Option<String>>(0),
    ) {
        Ok(parent) => Ok(parent),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn child_session_ids(
    conn: &Connection,
    parent_session_id: &str,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT session_id FROM sessions WHERE parent_session_id = ?1 ORDER BY created_at_ms",
    )?;
    let rows = stmt.query_map([parent_session_id], |row| row.get(0))?;
    rows.collect()
}

pub fn turn_count(conn: &Connection, session_id: &str) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM turns WHERE session_id = ?1",
        [session_id],
        |row| row.get(0),
    )
}

/// 会话列表行:列表页直接查询,不重建投影(列与 [`SessionRow`] 对齐,另带
/// 回合计数)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListRow {
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub title: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub profile: Option<String>,
    pub mode: Option<String>,
    pub tool_mode: Option<String>,
    pub model_selection: Option<qaqh_types::ModelSelection>,
    pub settings_revision: u64,
    pub turn_count: i64,
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
        "SELECT session_id, parent_session_id, title, cwd, model, effort, profile,
                mode, tool_mode, model_selection, settings_revision,
                (SELECT COUNT(*) FROM turns WHERE turns.session_id = sessions.session_id)
                    AS turn_count,
                archived, ephemeral, created_at_ms, updated_at_ms
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
            effort: row.get(5)?,
            profile: row.get(6)?,
            mode: row.get(7)?,
            tool_mode: row.get(8)?,
            model_selection: row
                .get::<_, Option<String>>(9)?
                .and_then(|json| serde_json::from_str(&json).ok()),
            settings_revision: row.get::<_, i64>(10)?.max(0) as u64,
            turn_count: row.get(11)?,
            archived: row.get::<_, i64>(12)? != 0,
            ephemeral: row.get::<_, i64>(13)? != 0,
            created_at_ms: row.get(14)?,
            updated_at_ms: row.get(15)?,
        })
    })?;
    rows.collect()
}

/// 元数据补丁的 SQLite 列投影:出现的字段写列,未出现的不动。
/// `model_selection` 的 `Use` 落 JSON、`Inherit` 清列(跟随全局默认)。
#[derive(Debug, Clone, Default)]
pub struct SessionColumnPatch {
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub profile: Option<String>,
    pub mode: Option<String>,
    pub tool_mode: Option<String>,
    pub model_selection: Option<qaqh_types::SettingValue<qaqh_types::ModelSelection>>,
    pub custom_tools: Option<Vec<String>>,
    pub skills: Option<qaqh_types::SkillSessionStateV2>,
    pub frozen_annotation: Option<String>,
    pub archived: Option<bool>,
    pub ephemeral: Option<bool>,
    pub expected_settings_revision: Option<u64>,
    pub increment_settings_revision: bool,
}

pub fn update_session_columns(
    conn: &mut Connection,
    session_id: &str,
    patch: &SessionColumnPatch,
) -> rusqlite::Result<usize> {
    let mut sets: Vec<String> = vec!["updated_at_ms = ?1".to_string()];
    let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(now_ms())];
    // 文本列统一字符串入参;JSON 列先序列化;`None` = 不动该列。
    let mut push_column = |column: &str, value: Option<String>| {
        if let Some(value) = value {
            values.push(Box::new(value) as Box<dyn rusqlite::ToSql>);
            sets.push(format!("{column} = ?{}", values.len()));
        }
    };
    push_column("title", patch.title.clone());
    push_column("cwd", patch.cwd.clone());
    push_column("model", patch.model.clone());
    push_column("effort", patch.effort.clone());
    push_column("profile", patch.profile.clone());
    push_column("mode", patch.mode.clone());
    push_column("tool_mode", patch.tool_mode.clone());
    push_column(
        "model_selection",
        patch
            .model_selection
            .clone()
            .map(|selection| match selection {
                qaqh_types::SettingValue::Use { value } => serde_json::to_string(&value).map(Some),
                qaqh_types::SettingValue::Inherit => Ok(None),
            })
            .transpose()
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))
            .map(|nested| nested.flatten())?,
    );
    if let Some(tools) = patch.custom_tools.as_ref() {
        let json = serde_json::to_string(tools)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))?;
        push_column("custom_tools_json", Some(json));
    }
    if let Some(skills) = patch.skills.as_ref() {
        let json = serde_json::to_string(skills)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))?;
        push_column("skills_json", Some(json));
    }
    push_column("frozen_annotation", patch.frozen_annotation.clone());
    if matches!(
        patch.model_selection,
        Some(qaqh_types::SettingValue::Inherit)
    ) {
        sets.push("model_selection = NULL".to_string());
    }
    if let Some(flag) = patch.archived {
        values.push(Box::new(i64::from(flag)) as Box<dyn rusqlite::ToSql>);
        sets.push(format!("archived = ?{}", values.len()));
    }
    if let Some(flag) = patch.ephemeral {
        values.push(Box::new(i64::from(flag)) as Box<dyn rusqlite::ToSql>);
        sets.push(format!("ephemeral = ?{}", values.len()));
    }
    if patch.increment_settings_revision {
        sets.push("settings_revision = settings_revision + 1".to_string());
    }
    values.push(Box::new(session_id.to_string()) as Box<dyn rusqlite::ToSql>);
    let placeholders: Vec<String> = (1..=values.len())
        .map(|index| format!("?{index}"))
        .collect();
    let session_id_placeholder = placeholders.last().expect("session id placeholder");
    let mut predicate = format!("session_id = {session_id_placeholder}");
    if let Some(expected_revision) = patch.expected_settings_revision {
        values.push(Box::new(expected_revision as i64) as Box<dyn rusqlite::ToSql>);
        predicate.push_str(&format!(" AND settings_revision = ?{}", values.len()));
    }
    let sql = format!("UPDATE sessions SET {} WHERE {predicate}", sets.join(", "),);
    conn.execute(&sql, rusqlite::params_from_iter(values))
}

pub fn session_settings_revision(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Option<u64>> {
    conn.query_row(
        "SELECT settings_revision FROM sessions WHERE session_id = ?1",
        [session_id],
        |row| row.get::<_, i64>(0).map(|revision| revision.max(0) as u64),
    )
    .optional()
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
        "INSERT INTO turns (turn_id, session_id, seq, input_id, mode, phase, status, started_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, 'running', 'active', ?6)",
        params![turn_id, session_id, seq, input_id, mode, now_ms()],
    )?;
    Ok(())
}

/// 当前活跃回合:工具落库时的回合归属来源(owner 决定业务顺序,数据库是事实)。
pub fn active_turn_id(conn: &Connection, session_id: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT turn_id FROM turns WHERE session_id = ?1 AND status = 'active'
         ORDER BY seq DESC LIMIT 1",
        [session_id],
        |row| row.get(0),
    )
    .optional()
}

/// 当前活跃回合的非终态 phase('running' | 'waiting_user' | 'cancelling')。
pub fn active_turn_phase(conn: &Connection, session_id: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT phase FROM turns WHERE session_id = ?1 AND status = 'active'
         ORDER BY seq DESC LIMIT 1",
        [session_id],
        |row| row.get(0),
    )
    .optional()
}

/// 回合非终态 phase 迁移(owner 定序);终态由 finish_turn 清空。
pub fn set_turn_phase(conn: &mut Connection, turn_id: &str, phase: &str) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        "UPDATE turns SET phase = ?2 WHERE turn_id = ?1 AND status = 'active' AND phase != ?2",
        params![turn_id, phase],
    )?;
    Ok(changed > 0)
}

/// `outcome_json` 保存 serde 形式的契约 `TurnOutcome`;视图层反序列化,坏数据报错。
pub fn finish_turn(
    conn: &mut Connection,
    turn_id: &str,
    status: &str,
    outcome_json: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE turns SET status = ?2, outcome_json = ?3, finished_at_ms = ?4, phase = NULL
         WHERE turn_id = ?1",
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
    /// 回合内块的归属;会话级块(如压缩摘要)为 None。
    pub turn_id: Option<String>,
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

/// One turn's transcript, ordered by the same session-global block ordinal as bootstrap.
pub fn read_transcript_for_turn(
    conn: &Connection,
    session_id: &str,
    turn_id: &str,
) -> rusqlite::Result<Vec<BlockRow>> {
    let mut stmt = conn.prepare(
        "SELECT block_id, turn_id, ord, kind, text, call_id, payload_json, created_at_ms
         FROM blocks
         WHERE session_id = ?1 AND turn_id = ?2
         ORDER BY ord ASC",
    )?;
    let rows = stmt.query_map(params![session_id, turn_id], |row| {
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
    })?;
    rows.collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockRow {
    pub block_id: String,
    /// 压缩块等无回合锚点的条目为 NULL;读取侧必须容忍(架构 DDL 允许 NULL)。
    pub turn_id: Option<String>,
    pub ord: i64,
    pub kind: String,
    pub text: Option<String>,
    pub call_id: Option<String>,
    pub payload_json: Option<String>,
    pub created_at_ms: i64,
}

// ---------------------------------------------------------------------------
// tool_calls(契约 state 单一权威;state_kind 是查询冗余列)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ToolCallUpsert {
    pub call_id: String,
    pub session_id: String,
    /// 工具一律在回合内调度;旧链未写回合表时为 None(P3 接线后总有值)。
    pub turn_id: Option<String>,
    pub tool_name: String,
    pub args_json: Option<String>,
    /// 契约 `ToolCallState` 的 serde 形式(唯一权威)。
    pub state_json: String,
    /// state.kind 的查询冗余(declared/awaiting_interaction/running/cancelling/finished)。
    pub state_kind: &'static str,
}

pub fn upsert_tool_call(conn: &mut Connection, call: &ToolCallUpsert) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO tool_calls (call_id, session_id, turn_id, tool_name, args_json, state_json, state_kind, started_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(call_id) DO UPDATE SET
            args_json = COALESCE(excluded.args_json, args_json),
            state_json = excluded.state_json,
            state_kind = excluded.state_kind",
        params![
            call.call_id,
            call.session_id,
            call.turn_id,
            call.tool_name,
            call.args_json,
            call.state_json,
            call.state_kind,
            now_ms(),
        ],
    )?;
    Ok(())
}

/// 工具调用终态落库。upsert:调度前被拒的工具没有 started 行,终态直落。
#[derive(Debug, Clone)]
pub struct ToolCallFinish {
    pub call_id: String,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub tool_name: String,
    /// 契约 `ToolCallState::Finished{outcome}` 的 serde 形式(不可变终态)。
    pub state_json: String,
    /// 终态调用关联的执行会话(仅 yielded)。
    pub exec_session_id: Option<String>,
    pub output_text: Option<String>,
    pub output_ref: Option<String>,
    pub output_truncated: bool,
    /// 契约 `ToolMetrics` 的 serde 形式。
    pub meta_json: Option<String>,
    /// 契约 `ToolEffectInfo[]` 的 serde 形式(owner 消费的结构化效果)。
    pub effects_json: Option<String>,
    /// 契约 `CodeDelta` 的 serde 形式。
    pub code_delta_json: Option<String>,
}

pub fn finish_tool_call(conn: &mut Connection, finish: &ToolCallFinish) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO tool_calls
            (call_id, session_id, turn_id, tool_name, state_json, state_kind,
             exec_session_id, output_text, output_ref, output_truncated,
             meta_json, effects_json, code_delta_json, started_at_ms, finished_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, 'finished', ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT(call_id) DO UPDATE SET
            state_json = excluded.state_json,
            state_kind = 'finished',
            exec_session_id = excluded.exec_session_id,
            output_text = excluded.output_text,
            output_ref = excluded.output_ref,
            output_truncated = excluded.output_truncated,
            meta_json = excluded.meta_json,
            effects_json = excluded.effects_json,
            code_delta_json = excluded.code_delta_json,
            finished_at_ms = excluded.finished_at_ms",
        params![
            finish.call_id,
            finish.session_id,
            finish.turn_id,
            finish.tool_name,
            finish.state_json,
            finish.exec_session_id,
            finish.output_text,
            finish.output_ref,
            i64::from(finish.output_truncated),
            finish.meta_json,
            finish.effects_json,
            finish.code_delta_json,
            now_ms(),
            now_ms(),
        ],
    )?;
    Ok(())
}

/// 非终态状态迁移(declared → awaiting_interaction/running/cancelling)。
/// 终态只能经 finish_tool_call;不提供终态直写口。
pub fn set_tool_call_state(
    conn: &mut Connection,
    call_id: &str,
    state_json: &str,
    state_kind: &'static str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE tool_calls SET state_json = ?2, state_kind = ?3
         WHERE call_id = ?1 AND state_kind != 'finished'",
        params![call_id, state_json, state_kind],
    )?;
    Ok(())
}

/// 未完成调用:快照必须包含非终态卡片。
pub fn unfinished_tool_calls(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Vec<ToolCallRow>> {
    read_tool_calls_where(conn, session_id, "AND state_kind != 'finished'")
}

pub fn recent_tool_calls(
    conn: &Connection,
    session_id: &str,
    limit: u32,
) -> rusqlite::Result<Vec<ToolCallRow>> {
    read_tool_calls_where(
        conn,
        session_id,
        &format!("ORDER BY started_at_ms DESC LIMIT {limit}"),
    )
}

pub fn tool_calls_for_turn(
    conn: &Connection,
    session_id: &str,
    turn_id: &str,
) -> rusqlite::Result<Vec<ToolCallRow>> {
    let mut stmt = conn.prepare(
        "SELECT call_id, turn_id, tool_name, args_json, state_json, state_kind,
                exec_session_id, output_text, output_ref, output_truncated,
                meta_json, effects_json, code_delta_json, started_at_ms, finished_at_ms
         FROM tool_calls
         WHERE session_id = ?1 AND turn_id = ?2
         ORDER BY COALESCE(started_at_ms, 0), rowid",
    )?;
    let rows = stmt.query_map(params![session_id, turn_id], |row| {
        Ok(ToolCallRow {
            call_id: row.get(0)?,
            turn_id: row.get(1)?,
            tool_name: row.get(2)?,
            args_json: row.get(3)?,
            state_json: row.get(4)?,
            state_kind: row.get(5)?,
            exec_session_id: row.get(6)?,
            output_text: row.get(7)?,
            output_ref: row.get(8)?,
            output_truncated: row.get::<_, i64>(9)? != 0,
            meta_json: row.get(10)?,
            effects_json: row.get(11)?,
            code_delta_json: row.get(12)?,
            started_at_ms: row.get(13)?,
            finished_at_ms: row.get(14)?,
        })
    })?;
    rows.collect()
}

fn read_tool_calls_where(
    conn: &Connection,
    session_id: &str,
    suffix: &str,
) -> rusqlite::Result<Vec<ToolCallRow>> {
    let sql = format!(
        "SELECT call_id, turn_id, tool_name, args_json, state_json, state_kind,
                exec_session_id, output_text, output_ref, output_truncated,
                meta_json, effects_json, code_delta_json, started_at_ms, finished_at_ms
         FROM tool_calls WHERE session_id = ?1 {suffix}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([session_id], |row| {
        Ok(ToolCallRow {
            call_id: row.get(0)?,
            turn_id: row.get(1)?,
            tool_name: row.get(2)?,
            args_json: row.get(3)?,
            state_json: row.get(4)?,
            state_kind: row.get(5)?,
            exec_session_id: row.get(6)?,
            output_text: row.get(7)?,
            output_ref: row.get(8)?,
            output_truncated: row.get::<_, i64>(9)? != 0,
            meta_json: row.get(10)?,
            effects_json: row.get(11)?,
            code_delta_json: row.get(12)?,
            started_at_ms: row.get(13)?,
            finished_at_ms: row.get(14)?,
        })
    })?;
    rows.collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRow {
    pub call_id: String,
    pub turn_id: Option<String>,
    pub tool_name: String,
    pub args_json: Option<String>,
    /// 契约 `ToolCallState` 的 serde 形式(唯一权威;终态 outcome 不可变)。
    pub state_json: String,
    pub state_kind: String,
    pub exec_session_id: Option<String>,
    pub output_text: Option<String>,
    pub output_ref: Option<String>,
    pub output_truncated: bool,
    pub meta_json: Option<String>,
    pub effects_json: Option<String>,
    pub code_delta_json: Option<String>,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
}

// ---------------------------------------------------------------------------
// exec_sessions(身份与终态持久化;高频输出块只在内存捕获)
// ---------------------------------------------------------------------------

/// 执行会话行:身份、归属、启动信息、state 与输出统计(spec §9.1)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecSessionRow {
    pub exec_session_id: String,
    pub session_id: String,
    pub origin_turn_id: Option<String>,
    pub origin_call_id: Option<String>,
    pub tty: bool,
    pub command: String,
    pub cwd: Option<String>,
    /// 契约 `ExecSessionState` 的 serde 形式。
    pub state_json: String,
    pub state_kind: String,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub next_offset: u64,
    pub output_complete: bool,
    pub capture_omitted_bytes: u64,
    pub stream_dropped_bytes: u64,
    /// 终态权威输出的 session_contents 引用。
    pub output_content_id: Option<String>,
    /// 有界尾窗摘要(终态时落库;非终态以 manager 内存视图为准)。
    pub output_preview: String,
    pub preview_start_offset: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RestartRecovery {
    pub exec_sessions: usize,
    pub tool_calls: usize,
    pub turns: usize,
    pub interactions: usize,
}

impl RestartRecovery {
    pub fn total(self) -> usize {
        self.exec_sessions + self.tool_calls + self.turns + self.interactions
    }
}

/// Settle every persisted nonterminal fact from the previous daemon epoch.
/// OS processes are never reattached after restart.
pub fn recover_after_restart(conn: &mut Connection) -> rusqlite::Result<RestartRecovery> {
    let tx = conn.transaction()?;
    let now = now_ms();
    let exec_state =
        serde_json::to_string(&qaqh_ringing_contract::state::ExecSessionState::Finished {
            outcome: qaqh_ringing_contract::state::ExecSessionOutcome::Interrupted {
                reason: qaqh_ringing_contract::state::ExecInterruptReason::DaemonRestart,
            },
        })
        .expect("exec session state serialization is infallible");
    let tool_state =
        serde_json::to_string(&qaqh_ringing_contract::state::ToolCallState::Finished {
            outcome: qaqh_types::ToolCallOutcome::Interrupted {
                reason: qaqh_types::ToolInterruptionReason::DaemonRestart,
            },
        })
        .expect("tool call state serialization is infallible");
    let turn_outcome =
        serde_json::to_string(&qaqh_ringing_contract::state::TurnOutcome::Interrupted {
            reason: qaqh_ringing_contract::fact::InterruptReason::Restart,
        })
        .expect("turn outcome serialization is infallible");

    let recovery = RestartRecovery {
        exec_sessions: tx.execute(
            "UPDATE exec_sessions
             SET state_json = ?1, state_kind = 'finished', finished_at_ms = ?2,
                 output_complete = 0, updated_at_ms = ?2
             WHERE state_kind != 'finished'",
            params![exec_state, now],
        )?,
        tool_calls: tx.execute(
            "UPDATE tool_calls
             SET state_json = ?1, state_kind = 'finished', finished_at_ms = ?2
             WHERE state_kind != 'finished'",
            params![tool_state, now],
        )?,
        turns: tx.execute(
            "UPDATE turns
             SET status = 'interrupted', outcome_json = ?1, finished_at_ms = ?2, phase = NULL
             WHERE status = 'active'",
            params![turn_outcome, now],
        )?,
        interactions: tx.execute(
            "UPDATE interactions
             SET status = 'cancelled', decision_json = 'null', resolved_at_ms = ?1
             WHERE status = 'pending'",
            [now],
        )?,
    };
    tx.commit()?;
    Ok(recovery)
}

/// Persist the authoritative in-memory tail snapshot before publishing its output event.
pub fn update_exec_session_progress(
    conn: &Connection,
    exec_session_id: &str,
    next_offset: u64,
    output_preview: &str,
    preview_start_offset: u64,
    capture_omitted_bytes: u64,
    stream_dropped_bytes: u64,
) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        "UPDATE exec_sessions
         SET next_offset = ?2, output_preview = ?3, preview_start_offset = ?4,
             capture_omitted_bytes = ?5, stream_dropped_bytes = ?6, updated_at_ms = ?7
         WHERE exec_session_id = ?1 AND state_kind != 'finished' AND next_offset <= ?2",
        params![
            exec_session_id,
            next_offset as i64,
            output_preview,
            preview_start_offset as i64,
            capture_omitted_bytes as i64,
            stream_dropped_bytes as i64,
            now_ms(),
        ],
    )?;
    Ok(changed > 0)
}

pub fn upsert_exec_session(conn: &mut Connection, row: &ExecSessionRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO exec_sessions
            (exec_session_id, session_id, origin_turn_id, origin_call_id, tty, command, cwd,
             state_json, state_kind, started_at_ms, finished_at_ms, next_offset,
             output_complete, capture_omitted_bytes, stream_dropped_bytes,
             output_content_id, output_preview, preview_start_offset, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
         ON CONFLICT(exec_session_id) DO UPDATE SET
            state_json = excluded.state_json,
            state_kind = excluded.state_kind,
            finished_at_ms = excluded.finished_at_ms,
            next_offset = excluded.next_offset,
            output_complete = excluded.output_complete,
            capture_omitted_bytes = excluded.capture_omitted_bytes,
            stream_dropped_bytes = excluded.stream_dropped_bytes,
            output_content_id = excluded.output_content_id,
            output_preview = excluded.output_preview,
            preview_start_offset = excluded.preview_start_offset,
            updated_at_ms = excluded.updated_at_ms",
        params![
            row.exec_session_id,
            row.session_id,
            row.origin_turn_id,
            row.origin_call_id,
            i64::from(row.tty),
            row.command,
            row.cwd,
            row.state_json,
            row.state_kind,
            row.started_at_ms,
            row.finished_at_ms,
            row.next_offset as i64,
            i64::from(row.output_complete),
            row.capture_omitted_bytes as i64,
            row.stream_dropped_bytes as i64,
            row.output_content_id,
            row.output_preview,
            row.preview_start_offset as i64,
            now_ms(),
        ],
    )?;
    Ok(())
}

/// 终态一次性落库:已终态的行不可改写(幂等重放返回 false)。
pub fn finish_exec_session(
    conn: &mut Connection,
    exec_session_id: &str,
    state_json: &str,
    finished_at_ms: i64,
    output_complete: bool,
    output_content_id: Option<&str>,
    output_text: Option<&str>,
    output_truncated: bool,
    output_preview: &str,
    preview_start_offset: u64,
) -> rusqlite::Result<bool> {
    let tx = conn.transaction()?;
    let session_id = tx
        .query_row(
            "SELECT session_id FROM exec_sessions WHERE exec_session_id = ?1 AND state_kind != 'finished'",
            [exec_session_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(session_id) = session_id else {
        tx.rollback()?;
        return Ok(false);
    };
    let generated_content_id = output_text.map(|text| {
        use sha2::Digest as _;
        format!(
            "sha256:{}",
            hex::encode(sha2::Sha256::digest(text.as_bytes()))
        )
    });
    let content_id = output_content_id.or(generated_content_id.as_deref());
    if let (Some(content_id), Some(text)) = (content_id, output_text) {
        put_session_content(
            &tx,
            &session_id,
            content_id,
            "text/plain; charset=utf-8",
            text.as_bytes(),
            output_truncated,
        )?;
    }
    let changed = tx.execute(
        "UPDATE exec_sessions
         SET state_json = ?2, state_kind = 'finished', finished_at_ms = ?3,
             output_complete = ?4, output_content_id = ?5, output_preview = ?6,
             preview_start_offset = ?7, updated_at_ms = ?3
         WHERE exec_session_id = ?1 AND state_kind != 'finished'",
        params![
            exec_session_id,
            state_json,
            finished_at_ms,
            i64::from(output_complete),
            content_id,
            output_preview,
            preview_start_offset as i64,
        ],
    )?;
    tx.commit()?;
    Ok(changed > 0)
}

/// bootstrap 读面:全部非终态 + 最近 32 个终态摘要(spec §6.3)。
pub fn exec_sessions_for_bootstrap(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Vec<ExecSessionRow>> {
    let sql = format!(
        "SELECT exec_session_id, session_id, origin_turn_id, origin_call_id, tty, command, cwd,
                state_json, state_kind, started_at_ms, finished_at_ms, next_offset,
                output_complete, capture_omitted_bytes, stream_dropped_bytes,
                output_content_id, output_preview, preview_start_offset
         FROM exec_sessions
         WHERE session_id = ?1 AND state_kind != 'finished'
         UNION ALL
         SELECT exec_session_id, session_id, origin_turn_id, origin_call_id, tty, command, cwd,
                state_json, state_kind, started_at_ms, finished_at_ms, next_offset,
                output_complete, capture_omitted_bytes, stream_dropped_bytes,
                output_content_id, output_preview, preview_start_offset
         FROM exec_sessions
         WHERE session_id = ?1 AND state_kind = 'finished'
         ORDER BY started_at_ms DESC LIMIT 32"
    );
    read_exec_sessions_sql(conn, &sql, session_id)
}

/// 命令面按 ID 查找(越属校验前先定位)。
pub fn exec_session_by_id(
    conn: &Connection,
    exec_session_id: &str,
) -> rusqlite::Result<Option<ExecSessionRow>> {
    let sql =
        "SELECT exec_session_id, session_id, origin_turn_id, origin_call_id, tty, command, cwd,
                state_json, state_kind, started_at_ms, finished_at_ms, next_offset,
                output_complete, capture_omitted_bytes, stream_dropped_bytes,
                output_content_id, output_preview, preview_start_offset
         FROM exec_sessions WHERE exec_session_id = ?1";
    Ok(read_exec_sessions_sql(conn, sql, exec_session_id)?
        .into_iter()
        .next())
}

fn read_exec_sessions_sql(
    conn: &Connection,
    sql: &str,
    key: &str,
) -> rusqlite::Result<Vec<ExecSessionRow>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([key], |row| {
        Ok(ExecSessionRow {
            exec_session_id: row.get(0)?,
            session_id: row.get(1)?,
            origin_turn_id: row.get(2)?,
            origin_call_id: row.get(3)?,
            tty: row.get::<_, i64>(4)? != 0,
            command: row.get(5)?,
            cwd: row.get(6)?,
            state_json: row.get(7)?,
            state_kind: row.get(8)?,
            started_at_ms: row.get(9)?,
            finished_at_ms: row.get(10)?,
            next_offset: row.get::<_, i64>(11)?.max(0) as u64,
            output_complete: row.get::<_, i64>(12)? != 0,
            capture_omitted_bytes: row.get::<_, i64>(13)?.max(0) as u64,
            stream_dropped_bytes: row.get::<_, i64>(14)?.max(0) as u64,
            output_content_id: row.get(15)?,
            output_preview: row.get(16)?,
            preview_start_offset: row.get::<_, i64>(17)?.max(0) as u64,
        })
    })?;
    rows.collect()
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
    /// 契约 `InteractionKind` 的 serde 形式。
    pub kind: String,
    pub payload_json: String,
}

pub fn create_interaction(
    conn: &mut Connection,
    interaction: &NewInteraction,
) -> rusqlite::Result<()> {
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

fn rows_from<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> rusqlite::Result<Vec<T>> {
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
// session content
// ---------------------------------------------------------------------------

/// Store binary content under the session that owns it. Bodies live in the
/// business database and are removed with their session row.
pub fn put_session_content(
    conn: &Connection,
    session_id: &str,
    content_id: &str,
    media_type: &str,
    bytes: &[u8],
    truncated: bool,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO session_contents (session_id, content_id, media_type, bytes, truncated)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(session_id, content_id) DO UPDATE SET
             media_type = excluded.media_type,
             bytes = excluded.bytes,
             truncated = excluded.truncated",
        params![session_id, content_id, media_type, bytes, truncated],
    )?;
    Ok(())
}

pub fn get_session_content(
    conn: &Connection,
    session_id: &str,
    content_id: &str,
) -> rusqlite::Result<Option<(String, Vec<u8>, bool)>> {
    conn.query_row(
        "SELECT media_type, bytes, truncated FROM session_contents
         WHERE session_id = ?1 AND content_id = ?2",
        params![session_id, content_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? != 0)),
    )
    .optional()
}

pub fn copy_session_content(
    conn: &Connection,
    source_session_id: &str,
    target_session_id: &str,
    content_id: &str,
) -> rusqlite::Result<bool> {
    let Some((media_type, bytes, truncated)) =
        get_session_content(conn, source_session_id, content_id)?
    else {
        return Ok(false);
    };
    put_session_content(
        conn,
        target_session_id,
        content_id,
        &media_type,
        &bytes,
        truncated,
    )?;
    Ok(true)
}

pub fn session_content_owners(
    conn: &Connection,
    content_id: &str,
) -> rusqlite::Result<Vec<String>> {
    let mut statement = conn.prepare(
        "SELECT session_id FROM session_contents WHERE content_id = ?1 ORDER BY session_id",
    )?;
    let rows = statement.query_map([content_id], |row| row.get(0))?;
    rows.collect()
}
