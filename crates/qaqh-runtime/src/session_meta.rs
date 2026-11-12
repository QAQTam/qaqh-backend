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
        tool_mode: row.tool_mode.unwrap_or_default(),
        model_selection: row.model_selection,
        settings_revision: row.settings_revision,
        custom_tools: row.custom_tools,
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

pub(crate) use qaqh_session::owner::MetadataPatchError;

fn patch_columns(
    owners: &qaqh_session::owner::OwnerDirectory,
    session_id: &str,
    patch: SessionMetadataPatch,
    expected_revision: Option<u64>,
) -> Result<u64, MetadataPatchError> {
    let increment_settings_revision = patch.model.is_some()
        || patch.tool_mode.is_some()
        || patch.model_selection.is_some()
        || patch.custom_tools.is_some()
        || patch.frozen_annotation.is_some();
    let columns = qaqh_session::db::store::SessionColumnPatch {
        title: None,
        title_source: None,
        cwd: patch.cwd.clone(),
        model: patch.model.clone(),
        tool_mode: patch.tool_mode.clone(),
        model_selection: patch.model_selection.clone(),
        custom_tools: patch.custom_tools.clone(),
        frozen_annotation: patch.frozen_annotation.clone(),
        archived: patch.archived,
        ephemeral: patch.ephemeral,
        expected_settings_revision: expected_revision,
        increment_settings_revision,
    };
    owners.get_or_create(session_id).update_metadata(columns)
}

/// Commit a title change to the business row.
///
/// `title_source` 业务列落 snake_case 的 [`TitleSource`]：`session.rename` 写
/// `user`，模型面 `session_title` 控制面工具（qaqh-agent-runtime）写 `auto`。
/// 该列不进 v3.0 冻结契约，仅用于区分标题的来历。
pub(crate) fn set_title(
    owners: &qaqh_session::owner::OwnerDirectory,
    session_id: &str,
    title: &str,
    source: TitleSource,
) -> Result<(), String> {
    let source_text = match source {
        TitleSource::User => "user",
        TitleSource::Auto => "auto",
        TitleSource::Migration => "migration",
    };
    owners
        .get_or_create(session_id)
        .update_metadata(qaqh_session::db::store::SessionColumnPatch {
            title: Some(title.to_string()),
            title_source: Some(source_text.to_string()),
            ..Default::default()
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}
