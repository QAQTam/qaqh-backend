//! 当前业务 schema:按实际业务访问组织(架构报告一 §7.2),不按现有 Rust 结构体建表。
//!
//! 版本策略:最小 schema 版本 + 明确的向前升级步骤;数据库比代码新直接报错,
//! 不做运行时兼容适配。

use rusqlite::{Connection, OptionalExtension};

use super::DbError;

pub const SCHEMA_VERSION: u32 = 4;

const DDL_BASE: &str = r#"
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
    model_selection   TEXT,
    skills_json       TEXT,
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

CREATE TABLE blobs (
    hash          TEXT PRIMARY KEY,
    bytes         BLOB NOT NULL,
    size          INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL
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
    root_session_id TEXT NOT NULL,
    channel_id      TEXT NOT NULL REFERENCES board_channels(channel_id) ON DELETE CASCADE,
    title           TEXT NOT NULL,
    task_id         TEXT,
    created_by      TEXT NOT NULL,
    created_at_ms   INTEGER NOT NULL
);
CREATE INDEX idx_board_threads_root ON board_threads(root_session_id, channel_id);

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
            return Err(DbError::SchemaTooNew {
                found,
                supported: SCHEMA_VERSION,
            });
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
