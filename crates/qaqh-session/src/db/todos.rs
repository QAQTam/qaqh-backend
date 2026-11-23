//! `session_todos` / `session_todo_state` 的行级原语(v4.4 P1-3)。
//!
//! todo 的数据模型(`TodoStore`)住在 `qaqh-workspace`,本模块只认列形态;
//! 两侧映射由 `qaqh-runtime` 的 `TodoHost` 实现完成。原子性在这里收口:
//! [`mutate_todo`] 在**单个写事务**内完成读-改-写——写执行器串行化 ⇒
//! 同一会话的并发 todo_update 互不丢更新(取代旧全局 `TODO_LOCK`)。

use rusqlite::{Connection, OptionalExtension, params};

use super::{Db, DbError};

#[derive(Debug, Clone)]
pub struct TodoRow {
    pub todo_id: String,
    pub title: String,
    pub description: String,
    /// snake_case 词表:pending | in_progress | completed | cancelled。
    pub status: String,
    pub evidence: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TodoStateRow {
    /// snake_case 词表:manual | goal。
    pub mode: String,
    pub current_id: Option<String>,
    pub auto_turns: i64,
    pub max_auto_turns: i64,
    pub archived: bool,
    pub cleared_notified: bool,
}

impl Default for TodoStateRow {
    fn default() -> Self {
        Self {
            mode: "manual".to_string(),
            current_id: None,
            auto_turns: 0,
            max_auto_turns: 24,
            archived: false,
            cleared_notified: false,
        }
    }
}

/// 读一个会话的清单;`None` = 从未建过清单(对应旧「文件不存在」语义,
/// `todo.status` 查询面对前端返回 Null 的判据)。
pub fn load(conn: &Connection, session_id: &str) -> Result<Option<(Vec<TodoRow>, TodoStateRow)>, DbError> {
    let state: Option<TodoStateRow> = conn
        .query_row(
            "SELECT mode, current_id, auto_turns, max_auto_turns, archived, cleared_notified
             FROM session_todo_state WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok(TodoStateRow {
                    mode: row.get(0)?,
                    current_id: row.get(1)?,
                    auto_turns: row.get(2)?,
                    max_auto_turns: row.get(3)?,
                    archived: row.get::<_, i64>(4)? != 0,
                    cleared_notified: row.get::<_, i64>(5)? != 0,
                })
            },
        )
        .optional()?;
    let Some(state) = state else {
        return Ok(None);
    };
    let mut stmt = conn.prepare(
        "SELECT todo_id, title, description, status, evidence
         FROM session_todos WHERE session_id = ?1 ORDER BY ord",
    )?;
    let items = stmt
        .query_map([session_id], |row| {
            Ok(TodoRow {
                todo_id: row.get(0)?,
                title: row.get(1)?,
                description: row.get(2)?,
                status: row.get(3)?,
                evidence: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some((items, state)))
}

/// 全量落库(在调用方事务内):条目整体重写 + 状态行 upsert。
pub fn save(
    conn: &Connection,
    session_id: &str,
    items: &[TodoRow],
    state: &TodoStateRow,
) -> Result<(), DbError> {
    conn.execute("DELETE FROM session_todos WHERE session_id = ?1", [session_id])?;
    for (ord, item) in items.iter().enumerate() {
        conn.execute(
            "INSERT INTO session_todos \
             (session_id, ord, todo_id, title, description, status, evidence) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session_id,
                ord as i64,
                item.todo_id,
                item.title,
                item.description,
                item.status,
                item.evidence,
            ],
        )?;
    }
    conn.execute(
        "INSERT OR REPLACE INTO session_todo_state \
         (session_id, mode, current_id, auto_turns, max_auto_turns, archived, cleared_notified) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            session_id,
            state.mode,
            state.current_id,
            state.auto_turns,
            state.max_auto_turns,
            state.archived,
            state.cleared_notified,
        ],
    )?;
    Ok(())
}

/// 单写事务内的读-改-写。`f` 拿到当前清单(无记录 = 空 + 默认状态);
/// 返回 `Err` 则整个事务回滚、不落库。
pub fn mutate_todo<R, F>(db: &Db, session_id: &str, f: F) -> Result<R, DbError>
where
    R: Send + 'static,
    F: FnOnce(&mut Vec<TodoRow>, &mut TodoStateRow) -> Result<R, String> + Send + 'static,
{
    let session_id = session_id.to_string();
    db.write_blocking(move |conn| -> Result<R, DbError> {
        let tx = conn.transaction()?;
        let (mut items, mut state) = load(&tx, &session_id)?.unwrap_or_else(|| {
            (Vec::new(), TodoStateRow::default())
        });
        let outcome = f(&mut items, &mut state).map_err(DbError::Workspace)?;
        save(&tx, &session_id, &items, &state)?;
        tx.commit()?;
        Ok(outcome)
    })
}

/// 只读加载(阻塞版,调用方须在同步线程)。
pub fn load_todo(db: &Db, session_id: &str) -> Result<Option<(Vec<TodoRow>, TodoStateRow)>, DbError> {
    let session_id = session_id.to_string();
    db.read_blocking(move |conn| load(conn, &session_id))
}
