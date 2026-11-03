//! Session metadata reads/writes over the SQLite business row.
//!
//! 列(cwd/model/effort/profile/mode/tool_mode/archived/ephemeral)在
//! `sessions` 表;skills/custom_tools/frozen_annotation/usage 暂无业务列,
//! 这些字段随旧 meta 存储退役不再对外承诺。

use qaqh_session::session_fact_v2::{SessionMetadataPatch, TitleSource};
use qaqh_types::SessionMeta;

/// Read a session's business row as a [`SessionMeta`].
pub(crate) fn read(
    owners: &qaqh_session::owner::OwnerDirectory,
    session_id: &str,
) -> Option<SessionMeta> {
    let owned = session_id.to_string();
    let found = owners
        .db()
        .read_blocking(move |conn| {
            qaqh_session::db::store::session_row_with_turn_count(conn, &owned)
        })
        .ok()?      // DbError → None
        ?;          // 行不存在 → None
    let (row, turn_count) = found;
    Some(SessionMeta {
        session_id: row.session_id,
        title: Some(row.title).filter(|title| !title.is_empty()),
        cwd: row.cwd,
        model: row.model.unwrap_or_default(),
        effort: row.effort,
        profile: row.profile,
        mode: row
            .mode
            .and_then(|mode| mode.parse::<u8>().ok())
            .unwrap_or(0),
        tool_mode: row.tool_mode.unwrap_or_default(),
        model_selection: row.model_selection,
        custom_tools: row.custom_tools,
        skills: row.skills.unwrap_or_default(),
        frozen_annotation: row.frozen_annotation,
        archived: row.archived,
        ephemeral: row.ephemeral,
        turn_count: turn_count.max(0) as usize,
        created_at: (row.created_at_ms.max(0) as u64) / 1000,
        updated_at: (row.updated_at_ms.max(0) as u64) / 1000,
        ..SessionMeta::default()
    })
}

/// Apply a metadata patch to the business row.
pub(crate) fn patch(
    owners: &qaqh_session::owner::OwnerDirectory,
    session_id: &str,
    patch: SessionMetadataPatch,
) -> Result<(), String> {
    let columns = qaqh_session::db::store::SessionColumnPatch {
        title: None,
        cwd: patch.cwd.clone(),
        model: patch.model.clone(),
        effort: patch.effort.clone(),
        profile: patch.profile.clone(),
        mode: patch.mode.map(|mode| mode.to_string()),
        tool_mode: patch.tool_mode.clone(),
        model_selection: patch.model_selection.clone(),
        custom_tools: patch.custom_tools.clone(),
        skills: patch.skills.clone(),
        frozen_annotation: patch.frozen_annotation.clone(),
        archived: patch.archived,
        ephemeral: patch.ephemeral,
    };
    let owned = session_id.to_string();
    owners
        .db()
        .write_blocking(move |conn| {
            qaqh_session::db::store::update_session_columns(conn, &owned, &columns)
        })
        .map_err(|error| format!("business session update for {session_id}: {error}"))?;
    Ok(())
}

/// Commit a title change to the business row.
pub(crate) fn set_title(
    owners: &qaqh_session::owner::OwnerDirectory,
    session_id: &str,
    title: &str,
    _source: TitleSource,
) -> Result<(), String> {
    let owned = session_id.to_string();
    let title = title.to_string();
    owners
        .db()
        .write_blocking(move |conn| {
            qaqh_session::db::store::update_session_columns(
                conn,
                &owned,
                &qaqh_session::db::store::SessionColumnPatch {
                    title: Some(title),
                    ..Default::default()
                },
            )
        })
        .map_err(|error| format!("business title update for {session_id}: {error}"))?;
    Ok(())
}
