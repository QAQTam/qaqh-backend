//! Team task board 与 message board 的 SQLite 业务存储。
//!
//! 全部函数接收 `&Connection`,由调用方在 `Db::write_blocking`/`read_blocking`
//! 闭包内组合。状态迁移在 SQL 的 WHERE 条件里收口:`rows_affected == 0` 即
//! 违反前置状态,直接报错,不做静默降级。

use rusqlite::{Connection, OptionalExtension, params};

pub const TASK_OPEN: &str = "open";
pub const TASK_CLAIMED: &str = "claimed";
pub const TASK_COMPLETED: &str = "completed";
pub const TASK_CLOSED: &str = "closed";
pub const TASK_CANCELLED: &str = "cancelled";

pub fn is_terminal_state(state: &str) -> bool {
    matches!(state, TASK_CLOSED | TASK_CANCELLED)
}

fn satisfies_dependency(state: &str) -> bool {
    matches!(state, TASK_COMPLETED | TASK_CLOSED)
}

/// 一次任务读取的完整行(依赖/工件/验收 join 后聚合)。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TeamTaskRow {
    pub task_id: String,
    pub root_session_id: String,
    pub title: String,
    pub description_ref: Option<String>,
    pub state: String,
    pub owner_path: Option<String>,
    pub owner_session_id: Option<String>,
    pub claim_epoch: i64,
    pub result_ref: Option<String>,
    pub created_by: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub depends_on: Vec<String>,
    /// (content_id, media_type, added_at_ms)
    pub artifacts: Vec<(String, String, i64)>,
    pub acceptance: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct NewTeamTask<'a> {
    pub task_id: &'a str,
    pub root_session_id: &'a str,
    pub title: &'a str,
    pub description_ref: Option<&'a str>,
    pub created_by: &'a str,
    pub created_at_ms: i64,
}

pub fn task_insert(conn: &Connection, task: &NewTeamTask<'_>) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO team_tasks
            (task_id, root_session_id, title, description_ref, state, claim_epoch,
             created_by, created_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7, ?7)",
        params![
            task.task_id,
            task.root_session_id,
            task.title,
            task.description_ref,
            TASK_OPEN,
            task.created_by,
            task.created_at_ms,
        ],
    )?;
    Ok(())
}

const TASK_COLUMNS: &str = "task_id, root_session_id, title, description_ref, state,
    owner_path, owner_session_id, claim_epoch, result_ref, created_by,
    created_at_ms, updated_at_ms";

fn map_task_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TeamTaskRow> {
    Ok(TeamTaskRow {
        task_id: row.get(0)?,
        root_session_id: row.get(1)?,
        title: row.get(2)?,
        description_ref: row.get(3)?,
        state: row.get(4)?,
        owner_path: row.get(5)?,
        owner_session_id: row.get(6)?,
        claim_epoch: row.get(7)?,
        result_ref: row.get(8)?,
        created_by: row.get(9)?,
        created_at_ms: row.get(10)?,
        updated_at_ms: row.get(11)?,
        depends_on: Vec::new(),
        artifacts: Vec::new(),
        acceptance: Vec::new(),
    })
}

fn attach_task_children(conn: &Connection, tasks: &mut [TeamTaskRow]) -> rusqlite::Result<()> {
    for task in tasks {
        let mut stmt = conn.prepare(
            "SELECT depends_on FROM team_task_dependencies WHERE task_id = ?1 ORDER BY rowid",
        )?;
        task.depends_on = stmt
            .query_map([&task.task_id], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut stmt = conn.prepare(
            "SELECT content_id, media_type, added_at_ms FROM team_task_artifacts
             WHERE task_id = ?1 ORDER BY added_at_ms, rowid",
        )?;
        task.artifacts = stmt
            .query_map([&task.task_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let acceptance: Option<String> = conn
            .query_row(
                "SELECT items_json FROM team_task_acceptance WHERE task_id = ?1",
                [&task.task_id],
                |row| row.get(0),
            )
            .optional()?;
        task.acceptance = acceptance
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
    }
    Ok(())
}

pub fn task_get(
    conn: &Connection,
    root_session_id: &str,
    task_id: &str,
) -> rusqlite::Result<Option<TeamTaskRow>> {
    let mut tasks: Vec<TeamTaskRow> = {
        let mut stmt = conn.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM team_tasks WHERE task_id = ?1 AND root_session_id = ?2"
        ))?;
        stmt.query_map(params![task_id, root_session_id], map_task_row)?
            .collect::<Result<Vec<_>, _>>()?
    };
    attach_task_children(conn, &mut tasks)?;
    Ok(tasks.pop())
}

pub fn task_list(
    conn: &Connection,
    root_session_id: &str,
    state: Option<&str>,
) -> rusqlite::Result<Vec<TeamTaskRow>> {
    let mut tasks: Vec<TeamTaskRow> = match state {
        Some(state) => {
            let mut stmt = conn.prepare(&format!(
                "SELECT {TASK_COLUMNS} FROM team_tasks WHERE root_session_id = ?1 AND state = ?2
                 ORDER BY created_at_ms"
            ))?;
            stmt.query_map(params![root_session_id, state], map_task_row)?
                .collect::<Result<Vec<_>, _>>()?
        }
        None => {
            let mut stmt = conn.prepare(&format!(
                "SELECT {TASK_COLUMNS} FROM team_tasks WHERE root_session_id = ?1
                 ORDER BY created_at_ms"
            ))?;
            stmt.query_map([root_session_id], map_task_row)?
                .collect::<Result<Vec<_>, _>>()?
        }
    };
    attach_task_children(conn, &mut tasks)?;
    Ok(tasks)
}

pub fn task_count(conn: &Connection, root_session_id: &str) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM team_tasks WHERE root_session_id = ?1",
        [root_session_id],
        |row| row.get(0),
    )
}

/// 认领:仅 open 可认领,epoch 单调递增。
pub fn task_claim(
    conn: &Connection,
    root_session_id: &str,
    task_id: &str,
    owner_path: &str,
    owner_session_id: &str,
    now_ms: i64,
) -> Result<(), String> {
    let changed = conn
        .execute(
            "UPDATE team_tasks SET state = ?4, owner_path = ?5, owner_session_id = ?6,
                claim_epoch = claim_epoch + 1, updated_at_ms = ?7
             WHERE task_id = ?1 AND root_session_id = ?2 AND state = ?3",
            params![
                task_id,
                root_session_id,
                TASK_OPEN,
                TASK_CLAIMED,
                owner_path,
                owner_session_id,
                now_ms
            ],
        )
        .map_err(|error| error.to_string())?;
    if changed == 0 {
        return Err(format!("task {task_id} is not open; claim requires Open"));
    }
    Ok(())
}

/// 释放:仅当前 owner 可释放;state 回 open,owner 清空,epoch 不变。
pub fn task_release(
    conn: &Connection,
    root_session_id: &str,
    task_id: &str,
    owner_path: &str,
    now_ms: i64,
) -> Result<(), String> {
    let changed = conn
        .execute(
            "UPDATE team_tasks SET state = ?4, owner_path = NULL, owner_session_id = NULL,
                updated_at_ms = ?5
             WHERE task_id = ?1 AND root_session_id = ?2 AND state = ?3 AND owner_path = ?6",
            params![
                task_id,
                root_session_id,
                TASK_CLAIMED,
                TASK_OPEN,
                now_ms,
                owner_path
            ],
        )
        .map_err(|error| error.to_string())?;
    if changed == 0 {
        return Err(format!("task {task_id} is not claimed by {owner_path}"));
    }
    Ok(())
}

fn task_state_of(
    conn: &Connection,
    root_session_id: &str,
    task_id: &str,
) -> Result<String, String> {
    conn.query_row(
        "SELECT state FROM team_tasks WHERE task_id = ?1 AND root_session_id = ?2",
        params![task_id, root_session_id],
        |row| row.get(0),
    )
    .map_err(|_| format!("task not found: {task_id}"))
}

/// 依赖检查:非终态、依赖存在、不重复、不成环。
pub fn task_add_dependency(
    conn: &Connection,
    root_session_id: &str,
    task_id: &str,
    depends_on: &str,
    now_ms: i64,
) -> Result<(), String> {
    let state = task_state_of(conn, root_session_id, task_id)?;
    if is_terminal_state(&state) {
        return Err(format!(
            "task {task_id} is terminal; cannot add dependencies"
        ));
    }
    task_state_of(conn, root_session_id, depends_on)?;
    let dup: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM team_task_dependencies WHERE task_id = ?1 AND depends_on = ?2",
            params![task_id, depends_on],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if dup > 0 {
        return Err(format!("task {task_id} already depends on {depends_on}"));
    }
    if depends_on_transitively(conn, depends_on, task_id, root_session_id, 0)? {
        return Err(format!(
            "dependency edge {depends_on} -> {task_id} would create a cycle"
        ));
    }
    conn.execute(
        "INSERT INTO team_task_dependencies (task_id, depends_on) VALUES (?1, ?2)",
        params![task_id, depends_on],
    )
    .map_err(|error| error.to_string())?;
    conn.execute(
        "UPDATE team_tasks SET updated_at_ms = ?3 WHERE task_id = ?1 AND root_session_id = ?2",
        params![task_id, root_session_id, now_ms],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

/// `from` 是否(传递地)依赖 `to`——是则 `to -> from` 成环。
fn depends_on_transitively(
    conn: &Connection,
    from: &str,
    to: &str,
    root_session_id: &str,
    depth: u32,
) -> Result<bool, String> {
    if depth > 256 {
        return Err("dependency graph too deep".to_string());
    }
    if from == to {
        return Ok(true);
    }
    let mut stmt = conn
        .prepare("SELECT depends_on FROM team_task_dependencies WHERE task_id = ?1")
        .map_err(|error| error.to_string())?;
    let deps: Vec<String> = stmt
        .query_map([from], |row| row.get(0))
        .map_err(|error| error.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|error| error.to_string())?;
    for dep in deps {
        let _ = root_session_id;
        if dep == to || depends_on_transitively(conn, &dep, to, root_session_id, depth + 1)? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn task_attach_artifact(
    conn: &Connection,
    root_session_id: &str,
    task_id: &str,
    content_id: &str,
    media_type: &str,
    now_ms: i64,
) -> Result<(), String> {
    let state = task_state_of(conn, root_session_id, task_id)?;
    if is_terminal_state(&state) {
        return Err(format!(
            "task {task_id} is terminal; cannot attach artifacts"
        ));
    }
    conn.execute(
        "INSERT INTO team_task_artifacts (task_id, content_id, media_type, added_at_ms)
         VALUES (?1, ?2, ?3, ?4)",
        params![task_id, content_id, media_type, now_ms],
    )
    .map_err(|error| error.to_string())?;
    conn.execute(
        "UPDATE team_tasks SET updated_at_ms = ?3 WHERE task_id = ?1 AND root_session_id = ?2",
        params![task_id, root_session_id, now_ms],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

pub fn task_set_acceptance(
    conn: &Connection,
    root_session_id: &str,
    task_id: &str,
    items_json: &str,
    now_ms: i64,
) -> Result<(), String> {
    let state = task_state_of(conn, root_session_id, task_id)?;
    if is_terminal_state(&state) {
        return Err(format!(
            "task {task_id} is terminal; cannot change acceptance"
        ));
    }
    conn.execute(
        "INSERT INTO team_task_acceptance (task_id, items_json) VALUES (?1, ?2)
         ON CONFLICT(task_id) DO UPDATE SET items_json = excluded.items_json",
        params![task_id, items_json],
    )
    .map_err(|error| error.to_string())?;
    conn.execute(
        "UPDATE team_tasks SET updated_at_ms = ?3 WHERE task_id = ?1 AND root_session_id = ?2",
        params![task_id, root_session_id, now_ms],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

/// 完成:claimed by owner + 全部依赖满足。
pub fn task_complete(
    conn: &Connection,
    root_session_id: &str,
    task_id: &str,
    owner_path: &str,
    result_ref: Option<&str>,
    now_ms: i64,
) -> Result<(), String> {
    let changed = conn
        .execute(
            "UPDATE team_tasks SET state = ?4, result_ref = ?5, updated_at_ms = ?6
             WHERE task_id = ?1 AND root_session_id = ?2 AND state = ?3 AND owner_path = ?7",
            params![
                task_id,
                root_session_id,
                TASK_CLAIMED,
                TASK_COMPLETED,
                result_ref,
                now_ms,
                owner_path
            ],
        )
        .map_err(|error| error.to_string())?;
    if changed == 0 {
        return Err(format!(
            "task {task_id} is not claimed by {owner_path}; complete requires the active claim"
        ));
    }
    let mut stmt = conn
        .prepare("SELECT depends_on FROM team_task_dependencies WHERE task_id = ?1")
        .map_err(|error| error.to_string())?;
    let deps: Vec<String> = stmt
        .query_map([task_id], |row| row.get(0))
        .map_err(|error| error.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|error| error.to_string())?;
    for dep in deps {
        let state = task_state_of(conn, root_session_id, &dep)?;
        if !satisfies_dependency(&state) {
            return Err(format!(
                "task {task_id} cannot complete: dependency {dep} is {state}"
            ));
        }
    }
    Ok(())
}

pub fn task_close(
    conn: &Connection,
    root_session_id: &str,
    task_id: &str,
    now_ms: i64,
) -> Result<(), String> {
    let changed = conn
        .execute(
            "UPDATE team_tasks SET state = ?4, updated_at_ms = ?5
             WHERE task_id = ?1 AND root_session_id = ?2 AND state = ?3",
            params![
                task_id,
                root_session_id,
                TASK_COMPLETED,
                TASK_CLOSED,
                now_ms
            ],
        )
        .map_err(|error| error.to_string())?;
    if changed == 0 {
        return Err(format!(
            "task {task_id} is not completed; close requires Completed"
        ));
    }
    Ok(())
}

pub fn task_cancel(
    conn: &Connection,
    root_session_id: &str,
    task_id: &str,
    reason: &str,
    now_ms: i64,
) -> Result<(), String> {
    let current = task_state_of(conn, root_session_id, task_id)?;
    if is_terminal_state(&current) {
        return Err(format!("task {task_id} is already {current}"));
    }
    let _ = reason;
    conn.execute(
        "UPDATE team_tasks SET state = ?4, updated_at_ms = ?5
         WHERE task_id = ?1 AND root_session_id = ?2",
        params![task_id, root_session_id, TASK_CANCELLED, now_ms],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// message board
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BoardChannelRow {
    pub channel_id: String,
    pub root_session_id: String,
    pub name: String,
    pub topic: Option<String>,
    pub created_by: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BoardThreadRow {
    pub thread_id: String,
    pub root_session_id: String,
    pub channel_id: String,
    pub title: String,
    pub task_id: Option<String>,
    pub created_by: String,
    pub created_at_ms: i64,
    pub post_count: i64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BoardPostRow {
    pub post_id: String,
    pub root_session_id: String,
    pub thread_id: String,
    pub task_id: Option<String>,
    pub author_path: String,
    pub author_session_id: Option<String>,
    pub body: String,
    pub reply_to: Option<String>,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BoardSubscriptionRow {
    pub root_session_id: String,
    /// `channel` | `thread`
    pub target_kind: String,
    pub target_id: String,
    pub subscriber_path: String,
    pub subscribed: bool,
    pub updated_at_ms: i64,
}

pub fn board_channel_insert(conn: &Connection, channel: &BoardChannelRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO board_channels (channel_id, root_session_id, name, topic, created_by, created_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            channel.channel_id,
            channel.root_session_id,
            channel.name,
            channel.topic,
            channel.created_by,
            channel.created_at_ms
        ],
    )?;
    Ok(())
}

pub fn board_channels(
    conn: &Connection,
    root_session_id: &str,
) -> rusqlite::Result<Vec<BoardChannelRow>> {
    let mut stmt = conn.prepare(
        "SELECT channel_id, root_session_id, name, topic, created_by, created_at_ms
         FROM board_channels WHERE root_session_id = ?1 ORDER BY created_at_ms, channel_id",
    )?;
    stmt.query_map([root_session_id], |row| {
        Ok(BoardChannelRow {
            channel_id: row.get(0)?,
            root_session_id: row.get(1)?,
            name: row.get(2)?,
            topic: row.get(3)?,
            created_by: row.get(4)?,
            created_at_ms: row.get(5)?,
        })
    })?
    .collect()
}

pub fn board_thread_insert(conn: &Connection, thread: &BoardThreadRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO board_threads (thread_id, root_session_id, channel_id, title, task_id, created_by, created_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            thread.thread_id,
            thread.root_session_id,
            thread.channel_id,
            thread.title,
            thread.task_id,
            thread.created_by,
            thread.created_at_ms
        ],
    )?;
    Ok(())
}

pub fn board_threads(
    conn: &Connection,
    root_session_id: &str,
) -> rusqlite::Result<Vec<BoardThreadRow>> {
    let mut stmt = conn.prepare(
        "SELECT t.thread_id, t.root_session_id, t.channel_id, t.title, t.task_id,
                t.created_by, t.created_at_ms,
                (SELECT COUNT(*) FROM board_posts p WHERE p.thread_id = t.thread_id) AS post_count
         FROM board_threads t WHERE t.root_session_id = ?1
         ORDER BY t.created_at_ms, t.thread_id",
    )?;
    stmt.query_map([root_session_id], |row| {
        Ok(BoardThreadRow {
            thread_id: row.get(0)?,
            root_session_id: row.get(1)?,
            channel_id: row.get(2)?,
            title: row.get(3)?,
            task_id: row.get(4)?,
            created_by: row.get(5)?,
            created_at_ms: row.get(6)?,
            post_count: row.get(7)?,
        })
    })?
    .collect()
}

pub fn board_post_insert(conn: &Connection, post: &BoardPostRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO board_posts (post_id, root_session_id, thread_id, task_id, author_path, author_session_id, body, reply_to, created_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            post.post_id,
            post.root_session_id,
            post.thread_id,
            post.task_id,
            post.author_path,
            post.author_session_id,
            post.body,
            post.reply_to,
            post.created_at_ms
        ],
    )?;
    Ok(())
}

pub fn board_posts(
    conn: &Connection,
    root_session_id: &str,
) -> rusqlite::Result<Vec<BoardPostRow>> {
    let mut stmt = conn.prepare(
        "SELECT post_id, root_session_id, thread_id, task_id, author_path, author_session_id,
                body, reply_to, created_at_ms
         FROM board_posts WHERE root_session_id = ?1 ORDER BY created_at_ms, post_id",
    )?;
    stmt.query_map([root_session_id], |row| {
        Ok(BoardPostRow {
            post_id: row.get(0)?,
            root_session_id: row.get(1)?,
            thread_id: row.get(2)?,
            task_id: row.get(3)?,
            author_path: row.get(4)?,
            author_session_id: row.get(5)?,
            body: row.get(6)?,
            reply_to: row.get(7)?,
            created_at_ms: row.get(8)?,
        })
    })?
    .collect()
}

pub fn board_subscription_set(
    conn: &Connection,
    subscription: &BoardSubscriptionRow,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO board_subscriptions
            (root_session_id, target_kind, target_id, subscriber_path, subscribed, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(root_session_id, target_kind, target_id, subscriber_path)
         DO UPDATE SET subscribed = excluded.subscribed, updated_at_ms = excluded.updated_at_ms",
        params![
            subscription.root_session_id,
            subscription.target_kind,
            subscription.target_id,
            subscription.subscriber_path,
            i64::from(subscription.subscribed),
            subscription.updated_at_ms
        ],
    )?;
    Ok(())
}

pub fn board_subscriptions(
    conn: &Connection,
    root_session_id: &str,
) -> rusqlite::Result<Vec<BoardSubscriptionRow>> {
    let mut stmt = conn.prepare(
        "SELECT root_session_id, target_kind, target_id, subscriber_path, subscribed, updated_at_ms
         FROM board_subscriptions WHERE root_session_id = ?1 ORDER BY updated_at_ms",
    )?;
    stmt.query_map([root_session_id], |row| {
        Ok(BoardSubscriptionRow {
            root_session_id: row.get(0)?,
            target_kind: row.get(1)?,
            target_id: row.get(2)?,
            subscriber_path: row.get(3)?,
            subscribed: row.get::<_, i64>(4)? != 0,
            updated_at_ms: row.get(5)?,
        })
    })?
    .collect()
}
