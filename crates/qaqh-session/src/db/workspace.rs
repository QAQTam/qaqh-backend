use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use qaqh_ringing_contract::{WorkspaceEntry, WorkspaceMeta};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::Digest;

use super::{DbError, store};

fn generate_id(path: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let hash = sha2::Sha256::digest(path.as_bytes());
    let short = hex::encode(&hash[..4]);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("ws-{ms}-{short}-{counter}")
}

fn map_workspace(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceMeta> {
    Ok(WorkspaceMeta {
        id: row.get(0)?,
        path: row.get(1)?,
        title: row.get(2)?,
        order: row.get::<_, i64>(3)?.max(0) as u32,
        session_ids: Vec::new(),
    })
}

pub fn list(conn: &Connection) -> rusqlite::Result<Vec<WorkspaceMeta>> {
    let mut statement = conn.prepare(
        "SELECT workspace_id, path, title, order_index
         FROM workspaces ORDER BY order_index",
    )?;
    let mut workspaces = statement
        .query_map([], map_workspace)?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut statement =
        conn.prepare("SELECT workspace_id, session_id FROM workspace_sessions ORDER BY rowid")?;
    let memberships = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for membership in memberships {
        let (workspace_id, session_id) = membership?;
        if let Some(workspace) = workspaces.iter_mut().find(|item| item.id == workspace_id) {
            workspace.session_ids.push(session_id);
        }
    }
    Ok(workspaces)
}

pub fn list_entries(conn: &Connection) -> rusqlite::Result<Vec<WorkspaceEntry>> {
    Ok(list(conn)?
        .into_iter()
        .map(|workspace| {
            let missing_dir = !Path::new(&workspace.path).is_dir();
            workspace.entry(missing_dir)
        })
        .collect())
}

pub fn workspace_of(conn: &Connection, session_id: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT workspace_id FROM workspace_sessions WHERE session_id = ?1",
        [session_id],
        |row| row.get(0),
    )
    .optional()
}

pub fn session_ids(conn: &Connection) -> rusqlite::Result<HashMap<String, String>> {
    let mut statement = conn.prepare("SELECT session_id, workspace_id FROM workspace_sessions")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.collect()
}

pub fn create(conn: &mut Connection, path: &str) -> Result<WorkspaceMeta, DbError> {
    if !Path::new(path).is_dir() {
        return Err(DbError::Workspace(format!(
            "workspace.create: not a directory: {path}"
        )));
    }
    let canonical_path = crate::grouping::canonical_cwd(Path::new(path));
    let title = Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.to_string());

    let tx = conn.transaction()?;
    if let Some(existing) = list(&tx)?.into_iter().find(|workspace| {
        crate::grouping::normalize_path(&workspace.path)
            == crate::grouping::normalize_path(&canonical_path)
    }) {
        tx.commit()?;
        return Ok(existing);
    }

    let order: i64 = tx.query_row(
        "SELECT COALESCE(MAX(order_index) + 1, 0) FROM workspaces",
        [],
        |row| row.get(0),
    )?;
    let id = generate_id(&canonical_path);
    tx.execute(
        "INSERT INTO workspaces(workspace_id, path, title, order_index)
         VALUES (?1, ?2, ?3, ?4)",
        params![id, canonical_path, title, order],
    )?;

    let listed_sessions = store::list_sessions(&tx, false, 256)?;
    let known_memberships = session_ids(&tx)?;
    let mut members = Vec::new();
    for session in listed_sessions {
        if known_memberships.contains_key(&session.session_id)
            || !session
                .cwd
                .as_deref()
                .is_some_and(|cwd| crate::grouping::cwd_belongs(cwd, &canonical_path))
        {
            continue;
        }
        tx.execute(
            "INSERT INTO workspace_sessions(session_id, workspace_id) VALUES (?1, ?2)",
            params![session.session_id, id],
        )?;
        members.push(session.session_id);
    }
    tx.commit()?;

    Ok(WorkspaceMeta {
        id,
        path: canonical_path,
        title,
        order: order.max(0) as u32,
        session_ids: members,
    })
}

pub fn attach_by_cwd(
    conn: &mut Connection,
    session_id: &str,
    cwd: &str,
) -> Result<Option<String>, DbError> {
    let cwd = cwd.trim();
    if cwd.is_empty() || cwd == "." {
        return Ok(None);
    }
    let canonical_path = crate::grouping::canonical_cwd(Path::new(cwd));
    let tx = conn.transaction()?;
    let existing = list(&tx)?
        .into_iter()
        .find(|workspace| crate::grouping::cwd_belongs(&canonical_path, &workspace.path));
    let target = if let Some(workspace) = existing {
        workspace
    } else {
        let title = Path::new(&canonical_path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| canonical_path.clone());
        let order: i64 = tx.query_row(
            "SELECT COALESCE(MAX(order_index) + 1, 0) FROM workspaces",
            [],
            |row| row.get(0),
        )?;
        let id = generate_id(&canonical_path);
        tx.execute(
            "INSERT INTO workspaces(workspace_id, path, title, order_index)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, canonical_path, title, order],
        )?;
        WorkspaceMeta {
            id,
            path: canonical_path,
            title,
            order: order.max(0) as u32,
            session_ids: Vec::new(),
        }
    };

    tx.execute(
        "INSERT INTO workspace_sessions(session_id, workspace_id) VALUES (?1, ?2)
         ON CONFLICT(session_id) DO UPDATE SET workspace_id = excluded.workspace_id",
        params![session_id, target.id],
    )?;
    tx.commit()?;
    Ok(Some(target.id))
}

pub fn rename(
    conn: &mut Connection,
    workspace_id: &str,
    title: &str,
) -> Result<WorkspaceMeta, DbError> {
    let tx = conn.transaction()?;
    let changed = tx.execute(
        "UPDATE workspaces SET title = ?2 WHERE workspace_id = ?1",
        params![workspace_id, title],
    )?;
    if changed == 0 {
        return Err(DbError::Workspace(format!(
            "workspace.rename: unknown id {workspace_id}"
        )));
    }
    let workspace = list(&tx)?
        .into_iter()
        .find(|workspace| workspace.id == workspace_id)
        .expect("updated workspace remains in the same transaction");
    tx.commit()?;
    Ok(workspace)
}

pub fn delete(conn: &mut Connection, workspace_id: &str) -> Result<(), DbError> {
    let changed = conn.execute(
        "DELETE FROM workspaces WHERE workspace_id = ?1",
        [workspace_id],
    )?;
    if changed == 0 {
        return Err(DbError::Workspace(format!(
            "workspace.delete: unknown id {workspace_id}"
        )));
    }
    Ok(())
}

pub fn move_session(
    conn: &mut Connection,
    session_id: &str,
    workspace_id: &str,
) -> Result<(), DbError> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM workspaces WHERE workspace_id = ?1)",
        [workspace_id],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(DbError::Workspace(format!(
            "workspace.move_session: unknown workspace {workspace_id}"
        )));
    }
    conn.execute(
        "INSERT INTO workspace_sessions(session_id, workspace_id) VALUES (?1, ?2)
         ON CONFLICT(session_id) DO UPDATE SET workspace_id = excluded.workspace_id",
        params![session_id, workspace_id],
    )?;
    Ok(())
}

pub fn remove_session(conn: &Connection, session_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM workspace_sessions WHERE session_id = ?1",
        [session_id],
    )?;
    Ok(())
}
