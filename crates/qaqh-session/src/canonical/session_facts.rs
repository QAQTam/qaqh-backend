//! The **only** read entry point for a session's committed fact prefix.
//!
//! Two storage generations coexist on disk: the legacy `events.commit.json`
//! reader and the `storage-format` batch journal. Callers must not probe files
//! to decide which one owns a directory — doing that is how the mailbox quota
//! and subagent terminal status silently read zero for every new session
//! (`registry.rs` and `host_impl.rs` each grew their own `exists()` probe and
//! both only knew the legacy layout).
//!
//! Everything that needs facts, a head sequence, or an identity goes through
//! this module.

use std::path::Path;

use crate::session_fact_v2::{LogId, SessionFact, SessionId};

use super::batch::{is_batch_root, read_committed_facts};
use super::identity::{CanonicalIdentityError, CanonicalSessionIdentity};
use super::log::CanonicalError;
use super::reader::CommittedFactReader;

/// Committed facts plus the head sequence they end at.
pub struct SessionFactView {
    /// Facts requested by the caller (already filtered by `after` when given).
    pub facts: Vec<SessionFact>,
    /// Last committed `fact_seq` of the session, regardless of the window.
    pub head_fact_seq: u64,
}

/// Read committed facts, dispatching on the storage generation that owns
/// `session_dir`.
pub fn read_session_facts(
    session_dir: impl AsRef<Path>,
    session_id: SessionId,
    log_id: LogId,
) -> Result<Vec<SessionFact>, CanonicalError> {
    read_session_facts_from(session_dir, session_id, log_id, None).map(|view| view.facts)
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
    if is_batch_root(dir) {
        let all = read_committed_facts(dir, session_id, log_id)?;
        let head_fact_seq = all.last().map(|fact| fact.fact_seq).unwrap_or(0);
        let facts = match after_fact_seq {
            Some(seq) => all.into_iter().filter(|fact| fact.fact_seq > seq).collect(),
            None => all,
        };
        return Ok(SessionFactView {
            facts,
            head_fact_seq,
        });
    }
    let reader = CommittedFactReader::open(dir, session_id, log_id)?;
    let head_fact_seq = reader.committed().committed_fact_seq;
    let facts = match after_fact_seq {
        Some(seq) => reader.read_after(seq)?,
        None => reader.read_all()?,
    };
    Ok(SessionFactView {
        facts,
        head_fact_seq,
    })
}

/// Whether the directory holds committed facts in either generation.
///
/// Use this instead of `dir.join("events.commit.json").exists()`; that probe is
/// what made batch-journal sessions look empty to half the daemon.
pub fn has_committed_facts(session_dir: impl AsRef<Path>) -> bool {
    let dir = session_dir.as_ref();
    is_batch_root(dir) || dir.join(super::log::EVENTS_COMMIT_FILE).exists()
}

/// Open the session identity without guessing the format at the call site.
///
/// A batch-journal root owns its identity and must never have a second one
/// minted from a missing sidecar; legacy directories are allowed to create it
/// (a session can exist before its first append).
pub fn open_session_identity(
    session_dir: impl AsRef<Path>,
) -> Result<CanonicalSessionIdentity, CanonicalIdentityError> {
    let dir = session_dir.as_ref();
    if is_batch_root(dir) {
        CanonicalSessionIdentity::open(dir)
    } else {
        CanonicalSessionIdentity::open_or_create(dir)
    }
}
