//! The **only** read entry point for a session's committed fact prefix.
//!
//! One storage generation remains: the `storage-format` batch journal. The
//! legacy `events.commit.json` reader was removed in the 2026-10 hard cut —
//! sessions written by it are now rejected instead of silently read through a
//! second code path. Callers must not probe files themselves; that is how
//! `registry.rs` and `host_impl.rs` each grew a legacy-only `exists()` check and
//! both silently read zero for every batch-journal session.

use std::path::Path;

use crate::session_fact_v2::{LogId, SessionFact, SessionId};

use super::batch::{is_batch_root, read_committed_facts};
use super::identity::{CanonicalIdentityError, CanonicalSessionIdentity};
use super::log::CanonicalError;

/// Committed facts plus the head sequence they end at.
pub struct SessionFactView {
    /// Facts requested by the caller (already filtered by `after` when given).
    pub facts: Vec<SessionFact>,
    /// Last committed `fact_seq` of the session, regardless of the window.
    pub head_fact_seq: u64,
}

fn require_batch_root(session_dir: &Path) -> Result<(), CanonicalError> {
    if is_batch_root(session_dir) {
        return Ok(());
    }
    Err(CanonicalError::NotWritable(format!(
        "{}: no batch journal (legacy events.commit.json sessions are no longer readable)",
        session_dir.display()
    )))
}

/// Read committed facts, optionally only those after `after_fact_seq`, plus the
/// session head. This is the form the mailbox projections need: they want a
/// delta and a high-water mark from the same snapshot.
pub fn read_session_facts_from(
    session_dir: impl AsRef<Path>,
    session_id: SessionId,
    log_id: LogId,
    after_fact_seq: Option<u64>,
) -> Result<SessionFactView, CanonicalError> {
    let dir = session_dir.as_ref();
    require_batch_root(dir)?;
    let all = read_committed_facts(dir, session_id, log_id)?;
    let head_fact_seq = all.last().map(|fact| fact.fact_seq).unwrap_or(0);
    let facts = match after_fact_seq {
        Some(seq) => all.into_iter().filter(|fact| fact.fact_seq > seq).collect(),
        None => all,
    };
    Ok(SessionFactView {
        facts,
        head_fact_seq,
    })
}

/// Read every committed fact of a session.
pub fn read_session_facts(
    session_dir: impl AsRef<Path>,
    session_id: SessionId,
    log_id: LogId,
) -> Result<Vec<SessionFact>, CanonicalError> {
    read_session_facts_from(session_dir, session_id, log_id, None).map(|view| view.facts)
}

/// Whether the directory holds committed facts.
///
/// Use this instead of probing storage files; batch-journal roots are the only
/// generation that exists.
pub fn has_committed_facts(session_dir: impl AsRef<Path>) -> bool {
    is_batch_root(session_dir.as_ref())
}

/// Open the session identity. Only batch-journal roots are valid.
///
/// The identity sidecar is never minted here: a session directory without one
/// is not a session.
pub fn open_session_identity(
    session_dir: impl AsRef<Path>,
) -> Result<CanonicalSessionIdentity, CanonicalIdentityError> {
    let dir = session_dir.as_ref();
    if !is_batch_root(dir) {
        return Err(CanonicalIdentityError::LegacyStorageRemoved);
    }
    CanonicalSessionIdentity::open(dir)
}
