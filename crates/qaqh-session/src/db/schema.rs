//! Schema v1:按实际业务访问组织(架构报告一 §7.2),不按现有 Rust 结构体建表。
//!
//! 版本策略:最小 schema 版本 + 明确的向前升级步骤;数据库比代码新直接报错,
//! 不做运行时兼容适配。

use rusqlite::{Connection, OptionalExtension};

use super::DbError;

pub const SCHEMA_VERSION: u32 = 1;

const DDL_V1: &str = r#"
CREATE TABLE sessions (
    session_id        TEXT PRIMARY KEY,
    parent_session_id TEXT,
    title             TEXT NOT NULL DEFAULT '',
    title_source      TEXT,
    cwd               TEXT,
    model             TEXT,
    effort            TEXT,
    profile           TEXT,
    mode              TEXT,
    tool_mode         TEXT,
    skills_json       TEXT,
    custom_tools_json TEXT,
    frozen_annotation TEXT,
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
    mode           TEXT,
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
    status           TEXT NOT NULL,
    output_text      TEXT,
    output_ref       TEXT,
    output_truncated INTEGER NOT NULL DEFAULT 0,
    error_json       TEXT,
    meta_json        TEXT,
    effects_json     TEXT,
    code_delta_json  TEXT,
    started_at_ms    INTEGER,
    finished_at_ms   INTEGER
);
CREATE INDEX idx_tool_calls_session ON tool_calls(session_id, started_at_ms);
CREATE INDEX idx_tool_calls_turn ON tool_calls(turn_id);

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

CREATE TABLE team_tasks (
    task_id       TEXT PRIMARY KEY,
    team_id       TEXT NOT NULL,
    title         TEXT NOT NULL,
    state         TEXT NOT NULL,
    claimed_by    TEXT,
    payload_json  TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
CREATE INDEX idx_team_tasks_team ON team_tasks(team_id, state);

CREATE TABLE board_channels (
    channel_id    TEXT PRIMARY KEY,
    board_id      TEXT NOT NULL,
    name          TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);

CREATE TABLE board_posts (
    post_id       TEXT PRIMARY KEY,
    channel_id    TEXT NOT NULL,
    thread_id     TEXT,
    author        TEXT NOT NULL,
    body          TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);
CREATE INDEX idx_board_posts_channel ON board_posts(channel_id, created_at_ms);

CREATE TABLE board_subscriptions (
    subscriber  TEXT NOT NULL,
    target_json TEXT NOT NULL,
    PRIMARY KEY (subscriber, target_json)
);

CREATE TABLE blobs (
    hash          TEXT PRIMARY KEY,
    bytes         BLOB NOT NULL,
    size          INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL
);
"#;

/// 初始化 schema:空库建 v1;已有库校验版本,比代码新直接报错。
pub fn initialize(conn: &mut Connection) -> Result<(), DbError> {
    let tx = conn.transaction()?;
    tx.execute(
        "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)",
        [],
    )?;
    let existing: Option<u32> = tx
        .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
        .optional()?;
    match existing {
        Some(found) if found > SCHEMA_VERSION => {
            return Err(DbError::SchemaTooNew { found, supported: SCHEMA_VERSION });
        }
        Some(_) => {
            // 当前只有 v1;未来升级在这里按 found 版本追加明确步骤。
        }
        None => {
            tx.execute_batch(DDL_V1)?;
            tx.execute("INSERT INTO schema_version (version) VALUES (?1)", [SCHEMA_VERSION])?;
        }
    }
    tx.commit()?;
    Ok(())
}
