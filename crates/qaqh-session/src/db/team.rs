//! Team task board 的 SQLite 业务存储(ADR-0033:留言板存储随机构退役)。
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
    /// advisory 文件触及范围(工作区相对前缀;ADR-0033 切片B)。
    pub write_scopes: Vec<String>,
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
    /// advisory 文件触及范围;空切片落 NULL。
    pub write_scopes: &'a [String],
    pub created_by: &'a str,
    pub created_at_ms: i64,
}

pub fn task_insert(conn: &Connection, task: &NewTeamTask<'_>) -> rusqlite::Result<()> {
    let write_scopes = if task.write_scopes.is_empty() {
        None
    } else {
        Some(
            serde_json::to_string(task.write_scopes)
                .expect("write_scopes json serialization cannot fail"),
        )
    };
    conn.execute(
        "INSERT INTO team_tasks
            (task_id, root_session_id, title, description_ref, state, claim_epoch,
             write_scopes, created_by, created_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7, ?8, ?8)",
        params![
            task.task_id,
            task.root_session_id,
            task.title,
            task.description_ref,
            TASK_OPEN,
            write_scopes,
            task.created_by,
            task.created_at_ms,
        ],
    )?;
    Ok(())
}

const TASK_COLUMNS: &str = "task_id, root_session_id, title, description_ref, state,
    owner_path, owner_session_id, claim_epoch, result_ref, write_scopes, created_by,
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
        // 与 acceptance 同律:advisory 数据解析失败按空处理,不让坏范围
        // 阻断整个任务板读取面。
        write_scopes: row
            .get::<_, Option<String>>(9)?
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default(),
        created_by: row.get(10)?,
        created_at_ms: row.get(11)?,
        updated_at_ms: row.get(12)?,
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

/// 认领:仅 open 可认领,epoch 单调递增;未满足的依赖阻塞认领(dsh 同律:
/// 只有全部依赖完成后任务才可 claim)。
pub fn task_claim(
    conn: &Connection,
    root_session_id: &str,
    task_id: &str,
    owner_path: &str,
    owner_session_id: &str,
    now_ms: i64,
) -> Result<(), String> {
    let unmet = unmet_dependency_count(conn, root_session_id, task_id)?;
    if unmet > 0 {
        return Err(format!(
            "task {task_id} is not ready: {unmet} unmet dependencies"
        ));
    }
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

/// 未满足依赖数:依赖行缺失或依赖任务未达 completed/closed 都算未满足。
/// team_task_dependencies 无 root 列——以任务自身的 root 圈定范围,
/// 依赖经 task_id(全局主键)关联到其所属任务。
fn unmet_dependency_count(
    conn: &Connection,
    root_session_id: &str,
    task_id: &str,
) -> Result<i64, String> {
    conn.query_row(
        "SELECT COUNT(*) FROM team_task_dependencies d
         JOIN team_tasks t ON t.task_id = d.task_id AND t.root_session_id = ?2
         LEFT JOIN team_tasks p ON p.task_id = d.depends_on
         WHERE d.task_id = ?1
           AND (p.task_id IS NULL OR p.state NOT IN ('completed','closed'))",
        params![task_id, root_session_id],
        |row| row.get(0),
    )
    .map_err(|error| error.to_string())
}

/// 根指派(ADR-0033 切片B,dsh reassign 同律):open/claimed 均可指派,落为
/// claimed;epoch 单调递增使并发中的旧 claim 视图失效(CAS 纪律)。仅根可调,
/// 权限检查在宿主层收口。
pub fn task_reassign(
    conn: &Connection,
    root_session_id: &str,
    task_id: &str,
    owner_path: &str,
    owner_session_id: &str,
    now_ms: i64,
) -> Result<(), String> {
    let changed = conn
        .execute(
            "UPDATE team_tasks SET state = ?3, owner_path = ?4, owner_session_id = ?5,
                claim_epoch = claim_epoch + 1, updated_at_ms = ?6
             WHERE task_id = ?1 AND root_session_id = ?2 AND state IN ('open','claimed')",
            params![
                task_id,
                root_session_id,
                TASK_CLAIMED,
                owner_path,
                owner_session_id,
                now_ms
            ],
        )
        .map_err(|error| error.to_string())?;
    if changed == 0 {
        return Err(format!(
            "task {task_id} not found or is terminal; reassign requires an active task"
        ));
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
#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let mut conn = Connection::open_in_memory().expect("in-memory db");
        crate::db::schema::initialize(&mut conn).expect("initialize schema");
        conn
    }

    fn insert(conn: &Connection, task_id: &str, scopes: &[&str]) {
        task_insert(
            conn,
            &NewTeamTask {
                task_id,
                root_session_id: "root",
                title: task_id,
                description_ref: None,
                write_scopes: &scopes
                    .iter()
                    .map(|scope| scope.to_string())
                    .collect::<Vec<_>>(),
                created_by: "/root",
                created_at_ms: 1,
            },
        )
        .expect("insert task");
    }

    /// ADR-0033 切片B 闸门:write_scopes 落库读回、依赖未达标阻塞认领、
    /// 达标后可认领、根指派换 owner 且 epoch 单调递增。
    #[test]
    fn write_scopes_round_trip_claim_gate_and_reassign() {
        let conn = test_conn();
        insert(&conn, "t-blocked", &["src"]);
        insert(&conn, "t-dep", &[]);

        let row = task_get(&conn, "root", "t-blocked").unwrap().unwrap();
        assert_eq!(row.write_scopes, vec!["src".to_string()], "scopes 落库读回");
        let plain = task_get(&conn, "root", "t-dep").unwrap().unwrap();
        assert!(plain.write_scopes.is_empty(), "空范围落 NULL 读回空");

        task_add_dependency(&conn, "root", "t-blocked", "t-dep", 3).unwrap();
        let error = task_claim(&conn, "root", "t-blocked", "/root", "root", 4).unwrap_err();
        assert!(error.contains("not ready"), "依赖未达标阻塞认领: {error}");

        task_claim(&conn, "root", "t-dep", "/root", "root", 5).unwrap();
        task_complete(&conn, "root", "t-dep", "/root", None, 6).unwrap();
        task_claim(&conn, "root", "t-blocked", "/root", "root", 7).unwrap();
        let before = task_get(&conn, "root", "t-blocked").unwrap().unwrap();
        assert_eq!(before.state, TASK_CLAIMED);

        task_reassign(&conn, "root", "t-blocked", "/root/worker", "worker-ses", 7).unwrap();
        let after = task_get(&conn, "root", "t-blocked").unwrap().unwrap();
        assert_eq!(after.owner_path.as_deref(), Some("/root/worker"));
        assert_eq!(after.owner_session_id.as_deref(), Some("worker-ses"));
        assert!(
            after.claim_epoch > before.claim_epoch,
            "reassign 递增 epoch"
        );
    }
}
