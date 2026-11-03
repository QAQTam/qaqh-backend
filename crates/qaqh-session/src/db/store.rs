//! 业务表直查函数:全部接收 `&Connection`,由调用方在 `Db::write`/`Db::read`
//! 闭包内组合成短事务。不做 Repository 抽象。

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
        "INSERT INTO turns (turn_id, session_id, seq, input_id, mode, status, started_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6)",
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
// tool_calls
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ToolCallUpsert {
    pub call_id: String,
    pub session_id: String,
    /// 工具一律在回合内调度;旧链未写回合表时为 None(P3 接线后总有值)。
    pub turn_id: Option<String>,
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

/// 工具调用终态落库。upsert:调度前被拒的工具没有 started 行,终态直落。
#[derive(Debug, Clone)]
pub struct ToolCallFinish {
    pub call_id: String,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub tool_name: String,
    pub status: &'static str,
    pub output_text: Option<String>,
    pub output_ref: Option<String>,
    pub output_truncated: bool,
    /// 契约 `ToolError` 的 serde 形式。
    pub error_json: Option<String>,
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
            (call_id, session_id, turn_id, tool_name, status, output_text, output_ref,
             output_truncated, error_json, meta_json, effects_json, code_delta_json,
             started_at_ms, finished_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT(call_id) DO UPDATE SET
            status = excluded.status,
            output_text = excluded.output_text,
            output_ref = excluded.output_ref,
            output_truncated = excluded.output_truncated,
            error_json = excluded.error_json,
            meta_json = excluded.meta_json,
            effects_json = excluded.effects_json,
            code_delta_json = excluded.code_delta_json,
            finished_at_ms = excluded.finished_at_ms",
        params![
            finish.call_id,
            finish.session_id,
            finish.turn_id,
            finish.tool_name,
            finish.status,
            finish.output_text,
            finish.output_ref,
            i64::from(finish.output_truncated),
            finish.error_json,
            finish.meta_json,
            finish.effects_json,
            finish.code_delta_json,
            now_ms(),
            now_ms(),
        ],
    )?;
    Ok(())
}

/// 状态单列迁移(declared → awaiting_permission 等)。
pub fn set_tool_call_status(
    conn: &mut Connection,
    call_id: &str,
    status: &'static str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE tool_calls SET status = ?2 WHERE call_id = ?1",
        params![call_id, status],
    )?;
    Ok(())
}

/// 运行中/待授权/后台工具:快照必须包含未完成卡片。
pub fn unfinished_tool_calls(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Vec<ToolCallRow>> {
    read_tool_calls_where(
        conn,
        session_id,
        "AND status IN ('declared','awaiting_permission','running','backgrounded')",
    )
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

fn read_tool_calls_where(
    conn: &Connection,
    session_id: &str,
    suffix: &str,
) -> rusqlite::Result<Vec<ToolCallRow>> {
    let sql = format!(
        "SELECT call_id, turn_id, tool_name, args_json, status, output_text, output_ref,
                output_truncated, error_json, meta_json, effects_json, code_delta_json,
                started_at_ms, finished_at_ms
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
            effects_json: row.get(10)?,
            code_delta_json: row.get(11)?,
            started_at_ms: row.get(12)?,
            finished_at_ms: row.get(13)?,
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
    pub status: String,
    pub output_text: Option<String>,
    pub output_ref: Option<String>,
    pub output_truncated: bool,
    pub error_json: Option<String>,
    pub meta_json: Option<String>,
    pub effects_json: Option<String>,
    pub code_delta_json: Option<String>,
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
