//! Session metadata reads/writes over the SQLite business row.
//!
//! SessionMeta 字段从 `sessions` 业务行读取;设置写入在同一个 SQLite 串行写边界
//! 递增 settings_revision,并支持调用者提供的 CAS 前置条件。

use qaqh_ringing_contract::fact::{SessionMetadataPatch, TitleSource};
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
        ?; // 行不存在 → None
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
        settings_revision: row.settings_revision,
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
    patch_columns(owners, session_id, patch, None)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

pub(crate) fn patch_with_settings_revision(
    owners: &qaqh_session::owner::OwnerDirectory,
    session_id: &str,
    patch: SessionMetadataPatch,
    expected_revision: u64,
) -> Result<u64, MetadataPatchError> {
    patch_columns(owners, session_id, patch, Some(expected_revision))
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum MetadataPatchError {
    #[error("session {session_id} not found")]
    NotFound { session_id: String },
    #[error("settings revision conflict: expected {expected}, current {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("{0}")]
    Storage(String),
}

fn patch_columns(
    owners: &qaqh_session::owner::OwnerDirectory,
    session_id: &str,
    patch: SessionMetadataPatch,
    expected_revision: Option<u64>,
) -> Result<u64, MetadataPatchError> {
    let increment_settings_revision = patch.model.is_some()
        || patch.effort.is_some()
        || patch.profile.is_some()
        || patch.mode.is_some()
        || patch.tool_mode.is_some()
        || patch.model_selection.is_some()
        || patch.custom_tools.is_some()
        || patch.skills.is_some()
        || patch.frozen_annotation.is_some();
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
        expected_settings_revision: expected_revision,
        increment_settings_revision,
    };
    let owned = session_id.to_string();
    let (changed, current) = owners
        .db()
        .write_blocking(move |conn| {
            let changed = qaqh_session::db::store::update_session_columns(conn, &owned, &columns)?;
            let current = qaqh_session::db::store::session_settings_revision(conn, &owned)?;
            Ok::<_, qaqh_session::db::DbError>((changed, current))
        })
        .map_err(|error| {
            MetadataPatchError::Storage(format!(
                "business session update for {session_id}: {error}"
            ))
        })?;
    if changed == 0 {
        let Some(actual) = current else {
            return Err(MetadataPatchError::NotFound {
                session_id: session_id.to_string(),
            });
        };
        if let Some(expected) = expected_revision {
            return Err(MetadataPatchError::RevisionConflict { expected, actual });
        }
        return Err(MetadataPatchError::NotFound {
            session_id: session_id.to_string(),
        });
    }
    current.ok_or_else(|| MetadataPatchError::NotFound {
        session_id: session_id.to_string(),
    })
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
