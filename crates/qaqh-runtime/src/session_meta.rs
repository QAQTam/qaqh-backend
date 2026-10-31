//! Session metadata reads and writes after `meta.json` was retired.
//!
//! Reads replay the journal through `qaqh_session::meta`. Writes prefer the
//! running actor (it holds the journal's single-writer lock); when the session
//! is not loaded, we open the journal ourselves and commit the fact directly.

use qaqh_session::session_fact_v2::{SessionMetadataPatch, TitleSource};
use qaqh_types::SessionMeta;

/// Directory that owns a session id (no I/O).
pub(crate) fn dir(session_id: &str) -> std::path::PathBuf {
    qaqh_types::platform::sessions_dir().join(session_id)
}

/// Read a session's metadata from its committed facts.
pub(crate) fn read(session_id: &str) -> Option<SessionMeta> {
    qaqh_session::meta::read(dir(session_id))
}

/// Apply a metadata patch. See the module docs for the writer-lock rule.
pub(crate) fn patch(
    registry: Option<&crate::AgentRegistry>,
    session_id: &str,
    patch: SessionMetadataPatch,
) -> Result<(), String> {
    if let Some(registry) = registry
        && registry
            .write_session_metadata(session_id, patch.clone())
            .is_ok()
    {
        return Ok(());
    }
    qaqh_session::meta::patch(dir(session_id), patch)
}

/// Commit a title change. See the module docs for the writer-lock rule.
pub(crate) fn set_title(
    registry: Option<&crate::AgentRegistry>,
    session_id: &str,
    title: &str,
    source: TitleSource,
) -> Result<(), String> {
    if let Some(registry) = registry
        && registry
            .write_session_title(session_id, title, source)
            .is_ok()
    {
        return Ok(());
    }
    qaqh_session::meta::set_title(dir(session_id), title, source)
}
