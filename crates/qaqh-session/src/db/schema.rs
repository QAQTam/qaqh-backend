//! 当前业务 schema:按实际业务访问组织(架构报告一 §7.2),不按现有 Rust 结构体建表。
//!
//! 版本策略:最小 schema 版本 + 明确的向前升级步骤;数据库比代码新直接报错,
//! 不做运行时兼容适配。schema 6(spec-exec-session-lifecycle §9):工具调用
//! 保存契约 state/outcome,新增 exec_sessions 表,turns 保存 phase;5→6 是
//! 一次性升级,不在运行路径同时支持两套 schema。schema 7:content id 形态
//! 收口——`session_contents.content_id` / `exec_sessions.output_content_id`
//! 库内统一裸 64-hex,wire ref `sha256:<hex>` 只在视图边界组装(见
//! `store::bare_content_id` / `store::canonical_content_ref`)。

use rusqlite::{Connection, Transaction, TransactionBehavior};

use super::DbError;
use super::store::now_ms;

pub const SCHEMA_VERSION: u32 = 8;

const DDL_BASE: &str = r#"
CREATE TABLE sessions (
    session_id        TEXT PRIMARY KEY,
    parent_session_id TEXT,
    title             TEXT NOT NULL DEFAULT '',
    title_source      TEXT,
    cwd               TEXT,
    model             TEXT,
    tool_mode         TEXT,
    model_selection   TEXT,
    custom_tools_json TEXT,
    frozen_annotation TEXT,
    settings_revision INTEGER NOT NULL DEFAULT 0,
    archived          INTEGER NOT NULL DEFAULT 0,
    ephemeral         INTEGER NOT NULL DEFAULT 0,
    created_at_ms     INTEGER NOT NULL,
    updated_at_ms     INTEGER NOT NULL
);
CREATE INDEX idx_sessions_updated ON sessions(updated_at_ms);

CREATE TABLE turns (
    turn_id        TEXT PRIMARY KEY,
    session_id     TEXT NOT NULL REFERENCES sessions(session_id),
    seq            INTEGER NOT NULL,
    input_id       TEXT,
    phase          TEXT,
    status         TEXT NOT NULL,
    outcome_json   TEXT,
    started_at_ms  INTEGER NOT NULL,
    finished_at_ms INTEGER
);
CREATE INDEX idx_turns_session ON turns(session_id, seq);

CREATE TABLE blocks (
    block_id      TEXT PRIMARY KEY,
    session_id    TEXT NOT NULL,
    turn_id       TEXT REFERENCES turns(turn_id),
    ord           INTEGER NOT NULL,
    kind          TEXT NOT NULL,
    text          TEXT,
    call_id       TEXT,
    payload_json  TEXT,
    created_at_ms INTEGER NOT NULL
);
CREATE INDEX idx_blocks_turn ON blocks(turn_id, ord);
CREATE INDEX idx_blocks_session ON blocks(session_id, ord);

CREATE TABLE tool_calls (
    call_id          TEXT PRIMARY KEY,
    session_id       TEXT NOT NULL,
    turn_id          TEXT,
    tool_name        TEXT NOT NULL,
    args_json        TEXT,
    state_json       TEXT NOT NULL,
    state_kind       TEXT NOT NULL,
    exec_session_id  TEXT,
    interaction_id   TEXT,
    output_text      TEXT,
    output_ref       TEXT,
    output_truncated INTEGER NOT NULL DEFAULT 0,
    meta_json        TEXT,
    effects_json     TEXT,
    code_delta_json  TEXT,
    started_at_ms    INTEGER,
    finished_at_ms   INTEGER
);
CREATE INDEX idx_tool_calls_session ON tool_calls(session_id, started_at_ms);
CREATE INDEX idx_tool_calls_turn ON tool_calls(turn_id);
CREATE INDEX idx_tool_calls_exec ON tool_calls(exec_session_id);

CREATE TABLE exec_sessions (
    exec_session_id       TEXT PRIMARY KEY,
    session_id            TEXT NOT NULL,
    origin_turn_id        TEXT,
    origin_call_id        TEXT,
    epoch                 INTEGER NOT NULL DEFAULT 0,
    tty                   INTEGER NOT NULL DEFAULT 0,
    command               TEXT NOT NULL DEFAULT '',
    cwd                   TEXT,
    state_json            TEXT NOT NULL,
    state_kind            TEXT NOT NULL,
    started_at_ms         INTEGER NOT NULL,
    finished_at_ms        INTEGER,
    next_offset           INTEGER NOT NULL DEFAULT 0,
    output_complete       INTEGER NOT NULL DEFAULT 1,
    capture_omitted_bytes INTEGER NOT NULL DEFAULT 0,
    stream_dropped_bytes  INTEGER NOT NULL DEFAULT 0,
    output_content_id     TEXT,
    output_preview        TEXT NOT NULL DEFAULT '',
    preview_start_offset  INTEGER NOT NULL DEFAULT 0,
    updated_at_ms         INTEGER NOT NULL
);
CREATE INDEX idx_exec_sessions_session ON exec_sessions(session_id, started_at_ms);

CREATE TABLE interactions (
    interaction_id TEXT PRIMARY KEY,
    session_id     TEXT NOT NULL,
    turn_id        TEXT,
    call_id        TEXT,
    kind           TEXT NOT NULL,
    payload_json   TEXT NOT NULL,
    status         TEXT NOT NULL,
    decision_json  TEXT,
    created_at_ms  INTEGER NOT NULL,
    resolved_at_ms INTEGER
);
CREATE INDEX idx_interactions_pending ON interactions(session_id, status);

CREATE TABLE session_contents (
    session_id  TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
    content_id  TEXT NOT NULL,
    media_type  TEXT NOT NULL,
    bytes       BLOB NOT NULL,
    truncated   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (session_id, content_id)
);

CREATE TABLE workspaces (
    workspace_id TEXT PRIMARY KEY,
    path         TEXT NOT NULL,
    title        TEXT NOT NULL,
    order_index  INTEGER NOT NULL UNIQUE
);
CREATE TABLE workspace_sessions (
    session_id   TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
    workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id) ON DELETE CASCADE
);
CREATE INDEX idx_workspace_sessions_workspace ON workspace_sessions(workspace_id);
"#;

const DDL_TEAM: &str = r#"
DROP TABLE IF EXISTS team_tasks;
DROP TABLE IF EXISTS board_channels;
DROP TABLE IF EXISTS board_posts;
DROP TABLE IF EXISTS board_subscriptions;

CREATE TABLE team_tasks (
    task_id          TEXT PRIMARY KEY,
    root_session_id  TEXT NOT NULL,
    title            TEXT NOT NULL,
    description_ref  TEXT,
    state            TEXT NOT NULL,
    owner_path       TEXT,
    owner_session_id TEXT,
    claim_epoch      INTEGER NOT NULL DEFAULT 0,
    result_ref       TEXT,
    created_by       TEXT NOT NULL,
    created_at_ms    INTEGER NOT NULL,
    updated_at_ms    INTEGER NOT NULL
);
CREATE INDEX idx_team_tasks_root ON team_tasks(root_session_id, state);

CREATE TABLE team_task_dependencies (
    task_id    TEXT NOT NULL REFERENCES team_tasks(task_id) ON DELETE CASCADE,
    depends_on TEXT NOT NULL,
    PRIMARY KEY (task_id, depends_on)
);

CREATE TABLE team_task_artifacts (
    task_id     TEXT NOT NULL REFERENCES team_tasks(task_id) ON DELETE CASCADE,
    content_id  TEXT NOT NULL,
    media_type  TEXT NOT NULL,
    added_at_ms INTEGER NOT NULL
);

CREATE TABLE team_task_acceptance (
    task_id    TEXT PRIMARY KEY REFERENCES team_tasks(task_id) ON DELETE CASCADE,
    items_json TEXT NOT NULL
);

CREATE TABLE board_channels (
    channel_id      TEXT PRIMARY KEY,
    root_session_id TEXT NOT NULL,
    name            TEXT NOT NULL,
    topic           TEXT,
    created_by      TEXT NOT NULL,
    created_at_ms   INTEGER NOT NULL
);
CREATE INDEX idx_board_channels_root ON board_channels(root_session_id);

CREATE TABLE board_threads (
    thread_id       TEXT PRIMARY KEY,
    channel_id      TEXT NOT NULL REFERENCES board_channels(channel_id) ON DELETE CASCADE,
    title           TEXT NOT NULL,
    task_id         TEXT,
    created_by      TEXT NOT NULL,
    created_at_ms   INTEGER NOT NULL
);
CREATE INDEX idx_board_threads_root ON board_threads(channel_id, created_at_ms, thread_id);

CREATE TABLE board_posts (
    post_id           TEXT PRIMARY KEY,
    root_session_id   TEXT NOT NULL,
    thread_id         TEXT NOT NULL REFERENCES board_threads(thread_id) ON DELETE CASCADE,
    task_id           TEXT,
    author_path       TEXT NOT NULL,
    author_session_id TEXT,
    body              TEXT NOT NULL,
    reply_to          TEXT,
    created_at_ms     INTEGER NOT NULL
);
CREATE INDEX idx_board_posts_root ON board_posts(root_session_id, created_at_ms);

CREATE TABLE board_subscriptions (
    root_session_id TEXT NOT NULL,
    target_kind     TEXT NOT NULL,
    target_id       TEXT NOT NULL,
    subscriber_path TEXT NOT NULL,
    subscribed      INTEGER NOT NULL,
    updated_at_ms   INTEGER NOT NULL,
    PRIMARY KEY (root_session_id, target_kind, target_id, subscriber_path)
);
"#;

/// 初始化 schema:空库建到最新版;已有库校验版本并按步进升级,比代码新直接报错。
pub fn initialize(conn: &mut Connection) -> Result<(), DbError> {
    // 版本读取、去重、DDL 和版本提交共用写事务,避免并发启动重复决定迁移。
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)",
        [],
    )?;
    // 旧版没有唯一约束,INSERT OR REPLACE 实际追加了版本历史。
    // DDL 与版本记录一直同事务提交,最高版本才是已完成的升级。
    let existing: Option<u32> =
        tx.query_row("SELECT MAX(version) FROM schema_version", [], |row| {
            row.get(0)
        })?;
    tx.execute_batch(
        "DELETE FROM schema_version
         WHERE rowid <> (SELECT rowid FROM schema_version ORDER BY version DESC LIMIT 1);
         CREATE UNIQUE INDEX IF NOT EXISTS idx_schema_version_singleton ON schema_version ((1));",
    )?;
    match existing {
        Some(found) if found > SCHEMA_VERSION => {
            return Err(DbError::SchemaTooNew {
                found,
                supported: SCHEMA_VERSION,
            });
        }
        // schema 5 → 6(spec-exec-session-lifecycle §9.3)后接 6→7、7→8:
        // 链式执行,否则旧代库会跳过后续的数据归一。
        Some(5) => {
            upgrade_5_to_6(&tx)?;
            upgrade_6_to_7(&tx)?;
            upgrade_7_to_8(&tx)?;
        }
        // schema 6 → 7:content id 形态收口(裸 hex),数据归一在新 daemon
        // 对外服务前完成;随后接 7→8 存量 effects 归一。
        Some(6) => {
            upgrade_6_to_7(&tx)?;
            upgrade_7_to_8(&tx)?;
        }
        // schema 7 → 8:skills 裁剪的存量 effects 归一(ADR-0010)。
        Some(7) => {
            upgrade_7_to_8(&tx)?;
        }
        Some(found) if found < SCHEMA_VERSION => {
            return Err(DbError::SchemaTooOld {
                found,
                supported: SCHEMA_VERSION,
            });
        }
        Some(_) => {}
        None => {
            tx.execute_batch(DDL_BASE)?;
            tx.execute_batch(DDL_TEAM)?;
        }
    }
    tx.execute(
        "INSERT OR REPLACE INTO schema_version (version) VALUES (?1)",
        [SCHEMA_VERSION],
    )?;
    tx.commit()?;
    Ok(())
}

/// schema 5 → 6(spec-exec-session-lifecycle §9.1/§9.3):
///
/// - `tool_calls.status`/`error_json` 双权威列删除,契约 `state_json` 成为唯一
///   权威;终态 outcome 不可变。
/// - 新增 `exec_sessions` 表;`turns` 增加 `phase` 列。
/// - 历史 backgrounded 证据只在完整合法 JSON(`status == "backgrounded"`)时
///   采信;仅在文本里出现某个单词不构成修正证据。
/// - 事实不足不补成成功:非终态迁移为 interrupted(legacy_state_unknown),
///   旧取消不伪造用户取消原因。
fn upgrade_5_to_6(tx: &Transaction<'_>) -> Result<(), DbError> {
    tx.execute_batch(
        r#"
        ALTER TABLE turns ADD COLUMN phase TEXT;
        -- 旧非终态回合无法确认结果 → interrupted/unknown_fact(spec §9.3);
        -- 事实不足不补成成功。
        UPDATE turns SET status = 'interrupted', phase = NULL,
               outcome_json = '{"kind":"interrupted","data":{"reason":"unknown_fact"}}'
         WHERE status = 'active';
        CREATE TABLE tool_calls_v6 (
            call_id          TEXT PRIMARY KEY,
            session_id       TEXT NOT NULL,
            turn_id          TEXT,
            tool_name        TEXT NOT NULL,
            args_json        TEXT,
            state_json       TEXT NOT NULL,
            state_kind       TEXT NOT NULL,
            exec_session_id  TEXT,
            interaction_id   TEXT,
            output_text      TEXT,
            output_ref       TEXT,
            output_truncated INTEGER NOT NULL DEFAULT 0,
            meta_json        TEXT,
            effects_json     TEXT,
            code_delta_json  TEXT,
            started_at_ms    INTEGER,
            finished_at_ms   INTEGER
        );
        CREATE TABLE exec_sessions (
            exec_session_id       TEXT PRIMARY KEY,
            session_id            TEXT NOT NULL,
            origin_turn_id        TEXT,
            origin_call_id        TEXT,
            epoch                 INTEGER NOT NULL DEFAULT 0,
            tty                   INTEGER NOT NULL DEFAULT 0,
            command               TEXT NOT NULL DEFAULT '',
            cwd                   TEXT,
            state_json            TEXT NOT NULL,
            state_kind            TEXT NOT NULL,
            started_at_ms         INTEGER NOT NULL,
            finished_at_ms        INTEGER,
            next_offset           INTEGER NOT NULL DEFAULT 0,
            output_complete       INTEGER NOT NULL DEFAULT 1,
            capture_omitted_bytes INTEGER NOT NULL DEFAULT 0,
            stream_dropped_bytes  INTEGER NOT NULL DEFAULT 0,
            output_content_id     TEXT,
            output_preview        TEXT NOT NULL DEFAULT '',
            preview_start_offset  INTEGER NOT NULL DEFAULT 0,
            updated_at_ms         INTEGER NOT NULL
        );
        CREATE INDEX idx_exec_sessions_session ON exec_sessions(session_id, started_at_ms);
        "#,
    )
    .map_err(|error| DbError::Corrupt(format!("upgrade 5→6 ddl: {error}")))?;

    let mut source = tx
        .prepare(
            "SELECT call_id, session_id, turn_id, tool_name, args_json, status,
                    output_text, output_truncated, error_json, meta_json, effects_json,
                    code_delta_json, started_at_ms, finished_at_ms, output_ref
             FROM tool_calls",
        )
        .map_err(|error| DbError::Corrupt(format!("upgrade 5→6 read tool_calls: {error}")))?;
    let mut rows = source
        .query([])
        .map_err(|error| DbError::Corrupt(format!("upgrade 5→6 query: {error}")))?;
    let mut migrated = 0i64;
    let mut corrected_backgrounded = 0i64;
    loop {
        let row = rows
            .next()
            .map_err(|error| DbError::Corrupt(format!("upgrade 5→6 iterate: {error}")))?;
        let Some(row) = row else { break };
        let call_id: String = row.get(0).map_err(corrupt("call_id"))?;
        let session_id: String = row.get(1).map_err(corrupt("session_id"))?;
        let turn_id: Option<String> = row.get(2).map_err(corrupt("turn_id"))?;
        let tool_name: String = row.get(3).map_err(corrupt("tool_name"))?;
        let args_json: Option<String> = row.get(4).map_err(corrupt("args_json"))?;
        let status: String = row.get(5).map_err(corrupt("status"))?;
        let output_text: Option<String> = row.get(6).map_err(corrupt("output_text"))?;
        let output_truncated: bool = row
            .get::<_, Option<i64>>(7)
            .map_err(corrupt("output_truncated"))?
            .unwrap_or(0)
            != 0;
        let error_json: Option<String> = row.get(8).map_err(corrupt("error_json"))?;
        let meta_json: Option<String> = row.get(9).map_err(corrupt("meta_json"))?;
        let effects_json: Option<String> = row.get(10).map_err(corrupt("effects_json"))?;
        let code_delta_json: Option<String> = row.get(11).map_err(corrupt("code_delta_json"))?;
        let started_at_ms: Option<i64> = row.get(12).map_err(corrupt("started_at_ms"))?;
        let finished_at_ms: Option<i64> = row.get(13).map_err(corrupt("finished_at_ms"))?;
        let output_ref: Option<String> = row.get(14).map_err(corrupt("output_ref"))?;

        let migrated_call = migrate_tool_call_row(MigrationInput {
            call_id: &call_id,
            status: &status,
            output_text: output_text.as_deref(),
            error_json: error_json.as_deref(),
        });
        if migrated_call.backgrounded_evidence {
            corrected_backgrounded += 1;
        }
        tx.execute(
            "INSERT INTO tool_calls_v6
                (call_id, session_id, turn_id, tool_name, args_json,
                 state_json, state_kind, exec_session_id, interaction_id,
                 output_text, output_ref, output_truncated, meta_json, effects_json,
                 code_delta_json, started_at_ms, finished_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?16, ?10, ?11, ?12, ?13, ?14, ?15)",
            rusqlite::params![
                call_id,
                session_id,
                turn_id,
                tool_name,
                args_json,
                migrated_call.state_json,
                migrated_call.state_kind,
                migrated_call.exec_session_id,
                output_text,
                output_truncated as i64,
                meta_json,
                effects_json,
                code_delta_json,
                started_at_ms,
                finished_at_ms,
                output_ref,
            ],
        )
        .map_err(|error| DbError::Corrupt(format!("upgrade 5→6 insert {call_id}: {error}")))?;

        if let Some(exec) = migrated_call.exec_session {
            tx.execute(
                "INSERT INTO exec_sessions
                    (exec_session_id, session_id, origin_turn_id, origin_call_id, epoch,
                     tty, command, cwd, state_json, state_kind, started_at_ms, finished_at_ms,
                     next_offset, output_complete, capture_omitted_bytes, stream_dropped_bytes,
                     output_content_id, output_preview, preview_start_offset, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, 0, 0, ?5, NULL, ?6, ?7, ?8, ?9, 0, 0, 0, 0, NULL, '', 0, ?10)",
                rusqlite::params![
                    exec.exec_session_id,
                    session_id,
                    turn_id,
                    call_id,
                    exec.command,
                    exec.state_json,
                    exec.state_kind,
                    exec.started_at_ms.unwrap_or_else(|| started_at_ms.unwrap_or(0)),
                    exec.finished_at_ms.or(finished_at_ms),
                    now_ms(),
                ],
            )
            .map_err(|error| {
                DbError::Corrupt(format!("upgrade 5→6 exec session {call_id}: {error}"))
            })?;
        }
        migrated += 1;
    }
    drop(rows);
    drop(source);
    tx.execute("DROP TABLE tool_calls", [])
        .map_err(|error| DbError::Corrupt(format!("upgrade 5→6 drop old: {error}")))?;
    tx.execute("ALTER TABLE tool_calls_v6 RENAME TO tool_calls", [])
        .map_err(|error| DbError::Corrupt(format!("upgrade 5→6 rename: {error}")))?;
    tx.execute_batch(
        r#"
        CREATE INDEX idx_tool_calls_session ON tool_calls(session_id, started_at_ms);
        CREATE INDEX idx_tool_calls_turn ON tool_calls(turn_id);
        CREATE INDEX idx_tool_calls_exec ON tool_calls(exec_session_id);
        "#,
    )
    .map_err(|error| DbError::Corrupt(format!("upgrade 5→6 index: {error}")))?;
    log::info!(
        "[schema] upgraded 5→6: {migrated} tool calls migrated, {corrected_backgrounded} corrected to yielded by explicit backgrounded evidence"
    );
    Ok(())
}

/// schema 6 → 7:content id 形态收口(裸 hex 归一)。
///
/// 背景:`session_contents.content_id` 与 `exec_sessions.output_content_id`
/// 的库内 canonical 形态是裸 64-hex;6 代执行会话终态落库曾把带 `sha256:`
/// 前缀的形态写进主键,而读取端(`GET /ringing/v2/content`,入口剥前缀)
/// 按裸 hex 精确匹配 → 执行会话输出引用永远 404。本迁移一次性归一存量:
///
/// 1. 同会话内"裸 + 前缀"并存的重复行(同一摘要,字节必然相同)保留裸行、
///    删除前缀行——否则随后的剥前缀 UPDATE 会撞 `(session_id, content_id)`
///    主键;
/// 2. 其余前缀行剥前缀改写(`substr(content_id, 8)`),`exec_sessions` 的
///    输出引用列同步归一。
///
/// 视图边界(`store::canonical_content_ref`)负责给 wire ref 补回前缀。
fn upgrade_6_to_7(tx: &Transaction<'_>) -> Result<(), DbError> {
    tx.execute_batch(
        r#"
        DELETE FROM session_contents AS sc
         WHERE sc.content_id LIKE 'sha256:%'
           AND EXISTS (
               SELECT 1 FROM session_contents b
                WHERE b.session_id = sc.session_id
                  AND b.content_id = substr(sc.content_id, 8)
           );
        UPDATE session_contents SET content_id = substr(content_id, 8)
         WHERE content_id LIKE 'sha256:%';
        UPDATE exec_sessions SET output_content_id = substr(output_content_id, 8)
         WHERE output_content_id LIKE 'sha256:%';
        "#,
    )
    .map_err(|error| DbError::Corrupt(format!("upgrade 6→7 normalize content ids: {error}")))?;
    log::info!("[schema] upgraded 6→7: content ids normalized to bare hex");
    Ok(())
}

/// schema 7 → 8：skills 裁剪（`198e8ede`，ADR-0010 清单"Skills 子系统裁剪"条）
/// 的数据层补齐。
///
/// 背景：该提交把 `ToolEffectInfo` 的 `skill_activated`/`skill_released` 变体
/// 从类型层删了，但旧 daemon 写进 `tool_calls.effects_json` 的存量行仍带这两个
/// kind ⇒ 读侧 `parse_typed::<Vec<ToolEffectInfo>>` fail-closed 报
/// `unknown variant`（`DbError::Corrupt`），含 legacy 行的历史会话被永久毒化，
/// 所有走 `owner.snapshot()` 的会话级读端点（bootstrap/approvals）500。
///
/// 规则：逐行解析 JSON 数组，**只**剔除 `kind` 恰为两个 legacy 名的元素；
/// 数组清空后与写入侧形态对齐落 NULL（写侧约定 `!effects.is_empty()` 才存）。
/// 解析不通过的行原样不动——那不是本次裁剪的债，形状错误仍归读侧 Corrupt
/// 如实报出，本迁移不顺手掩盖。
fn upgrade_7_to_8(tx: &Transaction<'_>) -> Result<(), DbError> {
    const LEGACY_KINDS: [&str; 2] = ["skill_activated", "skill_released"];
    // LIKE 只做候选预筛（避免全表逐行 parse）；是否真含 legacy kind 由
    // 解析后的精确判定决定，子串假阳性不会被改写。
    let mut statement = tx
        .prepare(
            "SELECT call_id, effects_json FROM tool_calls
             WHERE effects_json LIKE '%skill_activated%'
                OR effects_json LIKE '%skill_released%'",
        )
        .map_err(|error| DbError::Corrupt(format!("upgrade 7→8 read tool_calls: {error}")))?;
    let candidates = statement
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .map_err(|error| DbError::Corrupt(format!("upgrade 7→8 query: {error}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| DbError::Corrupt(format!("upgrade 7→8 iterate: {error}")))?;
    drop(statement);

    let mut stripped_rows = 0i64;
    let mut stripped_entries = 0i64;
    for (call_id, effects_json) in candidates {
        let Ok(serde_json::Value::Array(entries)) = serde_json::from_str::<serde_json::Value>(&effects_json)
        else {
            // 不是合法 JSON 数组：非本次裁剪的债，留给读侧如实报 Corrupt。
            continue;
        };
        let total = entries.len();
        let kept: Vec<serde_json::Value> = entries
            .into_iter()
            .filter(|entry| {
                !matches!(
                    entry.get("kind").and_then(|kind| kind.as_str()),
                    Some(kind) if LEGACY_KINDS.contains(&kind)
                )
            })
            .collect();
        if kept.len() == total {
            // 子串假阳性（如 task_text 里提到 legacy 名）：原样保留。
            continue;
        }
        stripped_entries += (total - kept.len()) as i64;
        stripped_rows += 1;
        if kept.is_empty() {
            tx.execute(
                "UPDATE tool_calls SET effects_json = NULL WHERE call_id = ?1",
                [&call_id],
            )
            .map_err(|error| {
                DbError::Corrupt(format!("upgrade 7→8 nullify {call_id}: {error}"))
            })?;
        } else {
            let text = serde_json::to_string(&serde_json::Value::Array(kept)).map_err(|error| {
                DbError::Corrupt(format!("upgrade 7→8 serialize {call_id}: {error}"))
            })?;
            tx.execute(
                "UPDATE tool_calls SET effects_json = ?1 WHERE call_id = ?2",
                rusqlite::params![text, call_id],
            )
            .map_err(|error| {
                DbError::Corrupt(format!("upgrade 7→8 rewrite {call_id}: {error}"))
            })?;
        }
    }
    log::info!(
        "[schema] upgraded 7→8: stripped {stripped_entries} legacy skill effect entries from {stripped_rows} tool_calls rows"
    );
    Ok(())
}

fn corrupt(field: &'static str) -> impl Fn(rusqlite::Error) -> DbError {
    move |error| DbError::Corrupt(format!("upgrade 5→6 {field}: {error}"))
}

struct MigrationInput<'a> {
    call_id: &'a str,
    status: &'a str,
    output_text: Option<&'a str>,
    error_json: Option<&'a str>,
}

struct MigratedToolCall {
    state_json: String,
    state_kind: &'static str,
    exec_session_id: Option<String>,
    exec_session: Option<MigratedExecSession>,
    backgrounded_evidence: bool,
}

struct MigratedExecSession {
    exec_session_id: String,
    command: String,
    state_json: String,
    state_kind: &'static str,
    started_at_ms: Option<i64>,
    finished_at_ms: Option<i64>,
}

/// 单行 5→6 迁移规则(spec §9.3):
/// - 终态 finished/failed:完整合法 ExecOutput JSON 且 `status == "backgrounded"`
///   → 成功 yielded + 历史执行会话 interrupted/upgrade_state_unknown(记录修正)。
///   其余 failed → failed(错误入 outcome,不改原始正文)。
/// - 旧 cancelled:取消事实确认,但原因无证据 → interrupted/legacy_state_unknown,
///   不伪造用户取消。
/// - 非终态(declared/awaiting_permission/running/backgrounded):running →
///   interrupted/lost_execution;其余 → interrupted/legacy_state_unknown。
///   backgrounded 行同时分配仅用于历史关联的 ExecSessionId。
fn migrate_tool_call_row(input: MigrationInput<'_>) -> MigratedToolCall {
    use qaqh_ringing_contract::state::{ExecSessionOutcome, ExecSessionState, ToolCallState};
    use qaqh_types::execution_contract::{
        ExecSessionId, SessionToolError, ToolCallOutcome, ToolInterruptionReason,
    };

    let legacy_error = input.error_json.and_then(|text| {
        serde_json::from_str::<SessionToolError>(text)
            .ok()
            .or_else(|| {
                // 旧 ToolError(带 hint)的形状:details_ref 缺失时尝试宽松解析。
                serde_json::from_str::<serde_json::Value>(text)
                    .ok()
                    .map(|value| SessionToolError {
                        code: value
                            .get("code")
                            .and_then(|code| code.as_str())
                            .unwrap_or("tool_failed")
                            .to_string(),
                        message: value
                            .get("message")
                            .and_then(|message| message.as_str())
                            .unwrap_or("legacy failure")
                            .to_string(),
                        retryable: value
                            .get("retryable")
                            .and_then(|retryable| retryable.as_bool())
                            .unwrap_or(false),
                        details_ref: None,
                    })
            })
    });

    // §9.3:唯一可采信的 backgrounded 证据 = 完整合法 JSON 且字段严格相等。
    let backgrounded_json = input
        .output_text
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .filter(|value| value.is_object())
        .filter(|value| {
            value.get("status").and_then(|status| status.as_str()) == Some("backgrounded")
        });

    let exec_session_for = |command: String, started: Option<i64>, finished: Option<i64>| {
        let exec_session_id = ExecSessionId::new(format!(
            "execs_{}",
            qaqh_ringing_contract::ulid::generate_ulid()
        ));
        let state = ExecSessionState::Finished {
            outcome: ExecSessionOutcome::Interrupted {
                reason: qaqh_ringing_contract::state::ExecInterruptReason::UpgradeStateUnknown,
            },
        };
        MigratedExecSession {
            exec_session_id: exec_session_id.0.clone(),
            command,
            state_json: serde_json::to_string(&state)
                .unwrap_or_else(|_| "{\"kind\":\"finished\",\"outcome\":{\"kind\":\"interrupted\",\"reason\":\"upgrade_state_unknown\"}}".to_string()),
            state_kind: "finished",
            started_at_ms: started,
            finished_at_ms: finished,
        }
    };

    let make = |state: ToolCallState,
                kind: &'static str,
                exec_session_id: Option<String>,
                exec_session: Option<MigratedExecSession>,
                backgrounded_evidence: bool| {
        MigratedToolCall {
        state_json: serde_json::to_string(&state)
            .unwrap_or_else(|_| "{\"kind\":\"finished\",\"outcome\":{\"kind\":\"interrupted\",\"reason\":\"legacy_state_unknown\"}}".to_string()),
        state_kind: kind,
        exec_session_id,
        exec_session,
        backgrounded_evidence,
    }
    };

    match input.status {
        "finished" | "failed" => {
            if let Some(_backgrounded) = backgrounded_json {
                let command = input
                    .output_text
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                    .and_then(|value| {
                        value
                            .get("command")
                            .and_then(|command| command.as_str())
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                let exec = exec_session_for(command, None, None);
                let exec_session_id = exec.exec_session_id.clone();
                let outcome = ToolCallOutcome::Yielded {
                    exec_session_id: ExecSessionId(exec_session_id.clone()),
                };
                return make(
                    ToolCallState::Finished { outcome },
                    "finished",
                    Some(exec_session_id),
                    Some(exec),
                    true,
                );
            }
            if input.status == "finished" {
                return make(
                    ToolCallState::Finished {
                        outcome: ToolCallOutcome::Completed,
                    },
                    "finished",
                    None,
                    None,
                    false,
                );
            }
            let error = legacy_error.unwrap_or(SessionToolError {
                code: "tool_failed".into(),
                message: "legacy failure without structured error".into(),
                retryable: false,
                details_ref: None,
            });
            return make(
                ToolCallState::Finished {
                    outcome: ToolCallOutcome::Failed { error },
                },
                "finished",
                None,
                None,
                false,
            );
        }
        "cancelled" => {
            return make(
                ToolCallState::Finished {
                    outcome: ToolCallOutcome::Interrupted {
                        reason: ToolInterruptionReason::LegacyStateUnknown,
                    },
                },
                "finished",
                None,
                None,
                false,
            );
        }
        "running" => {
            return make(
                ToolCallState::Finished {
                    outcome: ToolCallOutcome::Interrupted {
                        reason: ToolInterruptionReason::LostExecution,
                    },
                },
                "finished",
                None,
                None,
                false,
            );
        }
        "backgrounded" => {
            let command = input
                .output_text
                .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                .and_then(|value| {
                    value
                        .get("command")
                        .and_then(|command| command.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_default();
            let exec = exec_session_for(command, None, None);
            let exec_session_id = exec.exec_session_id.clone();
            let outcome = ToolCallOutcome::Yielded {
                exec_session_id: ExecSessionId(exec_session_id.clone()),
            };
            return make(
                ToolCallState::Finished { outcome },
                "finished",
                Some(exec_session_id),
                Some(exec),
                false,
            );
        }
        // declared / awaiting_permission / 未知历史值:事实不足,不补成功。
        _ => {
            let _ = input.call_id;
            return make(
                ToolCallState::Finished {
                    outcome: ToolCallOutcome::Interrupted {
                        reason: ToolInterruptionReason::LegacyStateUnknown,
                    },
                },
                "finished",
                None,
                None,
                false,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // v5 的迁移输入结构独立于最新版 DDL,避免仅验证 Rust 单行映射而漏掉升级链路。
    const V5: &str = r#"
        CREATE TABLE schema_version (version INTEGER NOT NULL);
        INSERT INTO schema_version VALUES (5), (5);
        CREATE TABLE turns (
            turn_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, seq INTEGER NOT NULL,
            input_id TEXT, mode TEXT, status TEXT NOT NULL, outcome_json TEXT,
            started_at_ms INTEGER NOT NULL, finished_at_ms INTEGER
        );
        INSERT INTO turns VALUES ('turn', 'session', 1, NULL, NULL, 'active', NULL, 10, NULL);
        CREATE TABLE tool_calls (
            call_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, turn_id TEXT,
            tool_name TEXT NOT NULL, args_json TEXT, status TEXT NOT NULL,
            output_text TEXT, output_ref TEXT, output_truncated INTEGER NOT NULL DEFAULT 0,
            error_json TEXT, meta_json TEXT, effects_json TEXT, code_delta_json TEXT,
            started_at_ms INTEGER, finished_at_ms INTEGER
        );
        INSERT INTO tool_calls (call_id, session_id, turn_id, tool_name, status,
                                output_text, output_ref, started_at_ms, finished_at_ms)
        VALUES ('call', 'session', 'turn', 'shell', 'failed',
                '{"status":"backgrounded","command":"cargo watch"}', 'content-ref', 10, 20);
    "#;

    #[test]
    fn initialize_keeps_one_version_and_recovers_mixed_version_history() {
        let mut conn = Connection::open_in_memory().expect("open");
        for _ in 0..3 {
            initialize(&mut conn).expect("fresh/repeated initialize");
        }
        // 还原真实故障:已是 v6 的业务表,但版本历史仍以 v5 开头。
        conn.execute_batch(
            "DROP INDEX idx_schema_version_singleton;
             DELETE FROM schema_version;
             INSERT INTO schema_version VALUES (5), (5), (6);",
        )
        .expect("legacy metadata");
        conn.execute(
            "INSERT INTO sessions (session_id, title, created_at_ms, updated_at_ms)
             VALUES ('retained', 'keep', 1, 1)",
            [],
        )
        .expect("business data");
        for _ in 0..3 {
            initialize(&mut conn).expect("reopen migrated database");
            let versions: (i64, u32) = conn
                .query_row(
                    "SELECT COUNT(*), MAX(version) FROM schema_version",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("version");
            assert_eq!(versions, (1, SCHEMA_VERSION));
        }
        assert!(
            conn.execute("INSERT INTO schema_version VALUES (5)", [])
                .is_err()
        );
        let title: String = conn
            .query_row(
                "SELECT title FROM sessions WHERE session_id = 'retained'",
                [],
                |row| row.get(0),
            )
            .expect("retained session");
        assert_eq!(title, "keep");
    }

    /// 64-hex 占位摘要(裸形态)。
    const BARE_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BARE_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn initialize_upgrades_v5_atomically_and_preserves_output_references() {
        let mut conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(V5).expect("v5 fixture");
        // v5 真实库必含 session_contents(schema 4→5 引入,且当时无升级路径、
        // v5 一律全新建库)。fixture 补齐该表以覆盖 5→6→7 链式归一:
        // 同会话"裸+前缀"并存的重复行去重,无裸兄弟的前缀行剥前缀。
        conn.execute_batch(&format!(
            "CREATE TABLE sessions (
                session_id TEXT PRIMARY KEY,
                title TEXT NOT NULL DEFAULT '',
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );
            INSERT INTO sessions VALUES ('session', 'v5', 1, 1);
            CREATE TABLE session_contents (
                session_id  TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
                content_id  TEXT NOT NULL,
                media_type  TEXT NOT NULL,
                bytes       BLOB NOT NULL,
                truncated   INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (session_id, content_id)
            );
            INSERT INTO session_contents (session_id, content_id, media_type, bytes, truncated) VALUES
                ('session', 'sha256:{BARE_A}', 'text/plain; charset=utf-8', x'647570', 0),
                ('session', '{BARE_A}', 'text/plain', x'647570', 0),
                ('session', 'sha256:{BARE_B}', 'text/plain', x'6f6e6c79', 0);"
        ))
        .expect("v5 session_contents");
        // 在 DDL 已执行后制造行读取失败,检查整次升级是否回滚。
        conn.execute("UPDATE tool_calls SET output_text = x'ff'", [])
            .expect("invalid text");
        assert!(initialize(&mut conn).is_err());
        let before: (i64, u32, String) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM schema_version),
                        (SELECT MAX(version) FROM schema_version), status FROM turns",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("rolled back state");
        assert_eq!(before, (2, 5, "active".into()));
        let added_tables: i64 = conn
            .query_row("SELECT COUNT(*) FROM sqlite_master WHERE name IN ('exec_sessions', 'tool_calls_v6', 'idx_schema_version_singleton')", [], |row| row.get(0))
            .expect("rolled back DDL");
        assert_eq!(added_tables, 0);
        assert!(conn.prepare("SELECT phase FROM turns").is_err());

        conn.execute(
            "UPDATE tool_calls SET output_text = ?1",
            [r#"{"status":"backgrounded","command":"cargo watch"}"#],
        )
        .expect("restore row");
        initialize(&mut conn).expect("upgrade");
        let migrated: (String, String, String) = conn
            .query_row(
                "SELECT output_ref, state_json, exec_session_id FROM tool_calls",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("migrated tool call");
        assert_eq!(migrated.0, "content-ref");
        assert!(migrated.1.contains("yielded"));
        let exec: (String, String) = conn
            .query_row(
                "SELECT exec_session_id, state_json FROM exec_sessions",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("historical exec session");
        assert_eq!(migrated.2, exec.0);
        assert!(exec.1.contains("upgrade_state_unknown"));
        // 6→7 链式归一:同会话"裸+前缀"并存的行去重,孤立前缀行剥前缀。
        let mut statement = conn
            .prepare("SELECT content_id FROM session_contents ORDER BY content_id")
            .expect("contents statement");
        let contents: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .expect("contents rows")
            .collect::<Result<_, _>>()
            .expect("contents collect");
        drop(statement);
        assert_eq!(contents, [BARE_A, BARE_B]);
        let prefixed_outputs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM exec_sessions WHERE output_content_id LIKE 'sha256:%'",
                [],
                |row| row.get(0),
            )
            .expect("exec outputs");
        assert_eq!(prefixed_outputs, 0);
        initialize(&mut conn).expect("reopen after upgrade");
        let versions: (i64, u32) = conn
            .query_row(
                "SELECT COUNT(*), MAX(version) FROM schema_version",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("committed version");
        assert_eq!(versions, (1, SCHEMA_VERSION));
    }

    // v6 形态最小 fixture:6→7 归一所触碰的表与版本行,外加 7→8 链式执行
    // 所需的 tool_calls(真实 v6 库必含)。
    const V6: &str = r#"
        CREATE TABLE schema_version (version INTEGER NOT NULL);
        INSERT INTO schema_version VALUES (6);
        CREATE TABLE tool_calls (
            call_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, turn_id TEXT,
            tool_name TEXT NOT NULL, args_json TEXT, state_json TEXT NOT NULL,
            state_kind TEXT NOT NULL, exec_session_id TEXT, interaction_id TEXT,
            output_text TEXT, output_ref TEXT, output_truncated INTEGER NOT NULL DEFAULT 0,
            meta_json TEXT, effects_json TEXT, code_delta_json TEXT,
            started_at_ms INTEGER, finished_at_ms INTEGER
        );
        INSERT INTO tool_calls (call_id, session_id, tool_name, state_json, state_kind, effects_json)
        VALUES ('chain', 's1', 'read', '{"kind":"finished"}', 'finished',
                '[{"kind":"skill_activated","name":"old"}]');
        CREATE TABLE sessions (
            session_id TEXT PRIMARY KEY,
            title TEXT NOT NULL DEFAULT '',
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL
        );
        CREATE TABLE session_contents (
            session_id  TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
            content_id  TEXT NOT NULL,
            media_type  TEXT NOT NULL,
            bytes       BLOB NOT NULL,
            truncated   INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (session_id, content_id)
        );
        CREATE TABLE exec_sessions (
            exec_session_id   TEXT PRIMARY KEY,
            session_id        TEXT NOT NULL,
            state_json        TEXT NOT NULL,
            state_kind        TEXT NOT NULL,
            started_at_ms     INTEGER NOT NULL,
            output_content_id TEXT,
            updated_at_ms     INTEGER NOT NULL
        );
        INSERT INTO sessions VALUES ('s1', 'v6', 1, 1), ('s2', 'v6', 1, 1);
        INSERT INTO session_contents (session_id, content_id, media_type, bytes, truncated) VALUES
            ('s1', 'sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 'text/plain; charset=utf-8', x'626f6479', 0),
            ('s1', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 'text/plain', x'626f6479', 0),
            ('s2', 'sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 'text/plain', x'626f6479', 0);
        INSERT INTO exec_sessions
            (exec_session_id, session_id, state_json, state_kind, started_at_ms, output_content_id, updated_at_ms)
        VALUES ('execs-1', 's1', '{"kind":"finished","outcome":{"kind":"exited","exit_code":0}}', 'finished', 1,
                'sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 1);
    "#;

    #[test]
    fn initialize_upgrades_v6_and_normalizes_content_ids_to_bare_hex() {
        let mut conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(V6).expect("v6 fixture");
        initialize(&mut conn).expect("upgrade");
        let versions: (i64, u32) = conn
            .query_row(
                "SELECT COUNT(*), MAX(version) FROM schema_version",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("version");
        assert_eq!(versions, (1, SCHEMA_VERSION));

        // s1 的"裸+前缀"并存行去重后保留裸行;s2 的孤立前缀行剥前缀;
        // 跨会话同摘要互不影响(主键按 session_id 分离)。
        let mut statement = conn
            .prepare("SELECT session_id, content_id FROM session_contents ORDER BY content_id")
            .expect("contents statement");
        let contents: Vec<(String, String)> = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("contents rows")
            .collect::<Result<_, _>>()
            .expect("contents collect");
        drop(statement);
        assert_eq!(
            contents,
            [
                ("s1".to_string(), BARE_A.to_string()),
                ("s2".to_string(), BARE_B.to_string()),
            ]
        );
        // exec_sessions 的输出引用同步归一;归一后按裸 hex 可命中
        // (daemon GET 入口剥前缀后的查询形态)。
        let exec_ref: Option<String> = conn
            .query_row(
                "SELECT output_content_id FROM exec_sessions WHERE exec_session_id = 'execs-1'",
                [],
                |row| row.get(0),
            )
            .expect("exec ref");
        assert_eq!(exec_ref.as_deref(), Some(BARE_A));
        let hit: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_contents WHERE session_id = 's1' AND content_id = ?1",
                [BARE_A],
                |row| row.get(0),
            )
            .expect("bare lookup");
        assert_eq!(hit, 1);
        // 链式 7→8 同步执行:legacy effects 行落 NULL。
        let chain_effects: Option<String> = conn
            .query_row(
                "SELECT effects_json FROM tool_calls WHERE call_id = 'chain'",
                [],
                |row| row.get(0),
            )
            .expect("chain row");
        assert_eq!(chain_effects, None);
    }

    // v7 形态最小 fixture:仅 7→8 所触碰的表与版本行。tool_calls 列形态取
    // 6 代重建后的权威形状。
    const V7: &str = r#"
        CREATE TABLE schema_version (version INTEGER NOT NULL);
        INSERT INTO schema_version VALUES (7);
        CREATE TABLE tool_calls (
            call_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, turn_id TEXT,
            tool_name TEXT NOT NULL, args_json TEXT, state_json TEXT NOT NULL,
            state_kind TEXT NOT NULL, exec_session_id TEXT, interaction_id TEXT,
            output_text TEXT, output_ref TEXT, output_truncated INTEGER NOT NULL DEFAULT 0,
            meta_json TEXT, effects_json TEXT, code_delta_json TEXT,
            started_at_ms INTEGER, finished_at_ms INTEGER
        );
        -- 混合行:legacy 条目 + 合法 subagent 条目 → 只剥 legacy。
        INSERT INTO tool_calls (call_id, session_id, tool_name, state_json, state_kind, effects_json)
        VALUES ('mixed', 's1', 'read', '{"kind":"finished"}', 'finished',
                '[{"kind":"skill_activated","name":"old"},{"kind":"subagent_spawned","child_session_id":"c1","name":"sub","task_text":"job"}]');
        -- 纯 legacy 行 → 清空落 NULL(与写侧 `!effects.is_empty()` 形态对齐)。
        INSERT INTO tool_calls (call_id, session_id, tool_name, state_json, state_kind, effects_json)
        VALUES ('legacy-only', 's1', 'read', '{"kind":"finished"}', 'finished',
                '[{"kind":"skill_released","name":"old"}]');
        -- 子串假阳性:task_text 提到 legacy 名,kind 合法 → 原样保留。
        INSERT INTO tool_calls (call_id, session_id, tool_name, state_json, state_kind, effects_json)
        VALUES ('false-positive', 's1', 'read', '{"kind":"finished"}', 'finished',
                '[{"kind":"subagent_spawned","child_session_id":"c2","name":"n","task_text":"mention skill_activated inside"}]');
        -- 坏 JSON 但含 legacy 子串:非裁剪的债,原样留给读侧如实报 Corrupt。
        INSERT INTO tool_calls (call_id, session_id, tool_name, state_json, state_kind, effects_json)
        VALUES ('broken', 's1', 'read', '{"kind":"finished"}', 'finished',
                'not-json skill_released');
    "#;

    #[test]
    fn initialize_upgrades_v7_by_stripping_legacy_skill_effects() {
        use qaqh_ringing_contract::state::ToolEffectInfo;
        let mut conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(V7).expect("v7 fixture");
        initialize(&mut conn).expect("upgrade");
        let versions: (i64, u32) = conn
            .query_row(
                "SELECT COUNT(*), MAX(version) FROM schema_version",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("version");
        assert_eq!(versions, (1, SCHEMA_VERSION));

        // 混合行:剥 legacy 后可被读侧 parse_typed 消费,合法条目原样在。
        let effects: Option<String> = conn
            .query_row(
                "SELECT effects_json FROM tool_calls WHERE call_id = 'mixed'",
                [],
                |row| row.get(0),
            )
            .expect("mixed row");
        let effects = serde_json::from_str::<Vec<ToolEffectInfo>>(&effects.expect("mixed effects"))
            .expect("mixed effects readable");
        assert_eq!(
            effects,
            vec![ToolEffectInfo::SubagentSpawned {
                child_session_id: "c1".into(),
                name: "sub".into(),
                task_text: "job".into(),
            }]
        );

        // 纯 legacy 行 → NULL。
        let legacy_only: Option<String> = conn
            .query_row(
                "SELECT effects_json FROM tool_calls WHERE call_id = 'legacy-only'",
                [],
                |row| row.get(0),
            )
            .expect("legacy-only row");
        assert_eq!(legacy_only, None);

        // 假阳性与坏 JSON 行不动。
        for call_id in ["false-positive", "broken"] {
            let raw: Option<String> = conn
                .query_row(
                    "SELECT effects_json FROM tool_calls WHERE call_id = ?1",
                    [call_id],
                    |row| row.get(0),
                )
                .expect("untouched row");
            assert!(raw.is_some(), "{call_id} must stay untouched");
        }
        let false_positive = conn
            .query_row(
                "SELECT effects_json FROM tool_calls WHERE call_id = 'false-positive'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("false-positive row");
        assert!(serde_json::from_str::<Vec<ToolEffectInfo>>(&false_positive).is_ok());
    }

    /// 反证（规则 18）：对真实生产库的**副本**跑迁移，迁移前必须红
    /// （毒化会话以 unknown variant 报 Corrupt），迁移后全部会话必须可读。
    /// 路径来自环境变量 `QAQH_DB_SNAPSHOT`；默认 ignore，仅供人工取证。
    #[test]
    #[ignore = "requires QAQH_DB_SNAPSHOT pointing at a copy of a production db"]
    fn real_db_snapshot_upgrades_and_all_sessions_load() {
        let path = std::env::var("QAQH_DB_SNAPSHOT").expect("QAQH_DB_SNAPSHOT");
        let mut conn = Connection::open(&path).expect("open snapshot");

        let ids: Vec<String> = conn
            .prepare("SELECT session_id FROM sessions")
            .expect("sessions stmt")
            .query_map([], |row| row.get(0))
            .expect("sessions ids")
            .collect::<Result<_, _>>()
            .expect("sessions collect");
        assert!(!ids.is_empty(), "snapshot must contain sessions");

        let corrupt_before: Vec<String> = ids
            .iter()
            .filter(|id| {
                let error = super::super::view::load_session_state(&conn, id)
                    .err()
                    .map(|error| error.to_string())
                    .unwrap_or_default();
                error.contains("unknown variant `skill_activated`")
                    || error.contains("unknown variant `skill_released`")
            })
            .cloned()
            .collect();
        assert!(
            !corrupt_before.is_empty(),
            "expected legacy skill effect corruption before migration"
        );

        initialize(&mut conn).expect("upgrade snapshot");

        for id in &ids {
            super::super::view::load_session_state(&conn, id)
                .unwrap_or_else(|error| panic!("session {id} unreadable after migration: {error}"));
        }
    }

    #[test]
    fn initialize_rejects_unsupported_versions_without_changing_metadata() {
        for version in [4, SCHEMA_VERSION + 1] {
            let mut conn = Connection::open_in_memory().expect("open");
            conn.execute_batch("CREATE TABLE schema_version (version INTEGER NOT NULL)")
                .expect("version table");
            conn.execute("INSERT INTO schema_version VALUES (?1), (?1)", [version])
                .expect("version history");
            let error = initialize(&mut conn).expect_err("unsupported version");
            assert!(matches!(
                error,
                DbError::SchemaTooNew { found, .. } | DbError::SchemaTooOld { found, .. }
                    if found == version
            ));
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
                .expect("unchanged metadata");
            assert_eq!(count, 2);
        }
    }

    #[test]
    fn migration_maps_legacy_statuses_to_immutable_outcomes() {
        let finished = migrate_tool_call_row(MigrationInput {
            call_id: "call_1",
            status: "finished",
            output_text: Some("done"),
            error_json: None,
        });
        assert_eq!(finished.state_kind, "finished");
        assert!(finished.state_json.contains("\"completed\""));

        let cancelled = migrate_tool_call_row(MigrationInput {
            call_id: "call_2",
            status: "cancelled",
            output_text: None,
            error_json: None,
        });
        assert!(cancelled.state_json.contains("legacy_state_unknown"));

        let running = migrate_tool_call_row(MigrationInput {
            call_id: "call_3",
            status: "running",
            output_text: None,
            error_json: None,
        });
        assert!(running.state_json.contains("lost_execution"));
    }

    #[test]
    fn migration_corrects_backgrounded_only_on_strict_json_evidence() {
        // 完整合法 JSON 且 status 严格等于 backgrounded → yielded + 历史会话。
        let evidence = migrate_tool_call_row(MigrationInput {
            call_id: "call_4",
            status: "failed",
            output_text: Some(
                r#"{"status":"backgrounded","process_id":42,"command":"cargo watch","timed_out":true}"#,
            ),
            error_json: None,
        });
        assert!(evidence.backgrounded_evidence);
        assert!(evidence.state_json.contains("\"yielded\""));
        assert!(evidence.exec_session_id.is_some());
        let exec = evidence.exec_session.expect("historical exec session");
        assert_eq!(exec.state_kind, "finished");
        assert!(exec.state_json.contains("upgrade_state_unknown"));

        // 只是文本里出现单词 → 不构成修正证据。
        let no_evidence = migrate_tool_call_row(MigrationInput {
            call_id: "call_5",
            status: "failed",
            output_text: Some("error: process backgrounded somewhere"),
            error_json: Some(r#"{"code":"timeout","message":"t","retryable":false}"#),
        });
        assert!(!no_evidence.backgrounded_evidence);
        assert!(no_evidence.state_json.contains("\"failed\""));
    }
}
