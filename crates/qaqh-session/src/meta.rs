//! Session metadata on top of the fact journal.
//!
//! `meta.json` is gone: title, archived, cwd, profile, effort, mode, tool mode,
//! skills state, the frozen annotation and usage totals are all committed as
//! `SessionTitleChanged` / `SessionMetadataChanged` facts. Reads replay the
//! committed prefix through [`ProjectionSet`]; there is no second durable
//! truth to drift from the journal.
//!
//! Writes need the journal's single-writer lock. When a session actor is
//! running it owns that lock and exposes [`crate::meta::SessionMetaPatch`] work
//! through its mailbox (see `qaqh_runtime::session_meta`); these functions are
//! the fallback used when the session is not loaded, and they fail loudly
//! (`batch writer lock unavailable`) instead of racing the actor.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use qaqh_types::SessionMeta;

use crate::canonical::{
    generate_ulid, has_committed_facts, open_session_identity, read_session_facts,
};
use crate::context_projection::ContextBudget;
use crate::projection::{Projection, ProjectionSet};
use crate::session_commit::SessionCommit;
use crate::session_fact_v2::{
    EventId, FactPayload, FactSchema, MetadataSource, SessionFact, SessionId, SessionMetadataChanged,
    SessionMetadataPatch, SessionTitleChanged, TitleSource,
};

/// The one budget every reader and writer of a session directory must agree on.
pub fn default_context_budget() -> ContextBudget {
    ContextBudget {
        max_messages: 4096,
        max_payload_bytes: 64 * 1024 * 1024,
        max_checkpoint_bytes: 16 * 1024 * 1024,
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

/// Read a session's metadata by replaying its committed facts.
pub fn read(session_dir: impl AsRef<Path>) -> Option<SessionMeta> {
    let dir = session_dir.as_ref();
    if !has_committed_facts(dir) {
        return None;
    }
    let identity = open_session_identity(dir).ok()?;
    let facts = read_session_facts(dir, identity.session_id.clone(), identity.log_id.clone()).ok()?;
    let mut projections = ProjectionSet::default();
    for fact in &facts {
        let _ = projections.apply(fact);
    }
    let snapshot = projections.meta.snapshot();
    let mut meta = SessionMeta {
        session_id: identity.session_id.as_str().to_string(),
        model: snapshot.model.clone().unwrap_or_default(),
        effort: snapshot.effort.clone(),
        profile: snapshot.profile.clone(),
        mode: snapshot.mode,
        tool_mode: snapshot.tool_mode.clone(),
        custom_tools: snapshot.custom_tools.clone(),
        archived: snapshot.archived,
        ephemeral: snapshot.ephemeral,
        skills: snapshot.skills.clone(),
        frozen_annotation: snapshot.frozen_annotation.clone(),
        title: snapshot.title.clone(),
        cwd: snapshot.cwd.clone(),
        ..SessionMeta::default()
    };
    if snapshot.created_at_ms > 0 {
        meta.created_at = (snapshot.created_at_ms as u64) / 1000;
    }
    meta.updated_at = facts
        .last()
        .map(|fact| (fact.ts_ms.max(0) as u64) / 1000)
        .unwrap_or(meta.created_at);
    meta.turn_count = projections.conversation.snapshot().turns.len();
    if let Some(usage) = &snapshot.usage {
        meta.usage_totals = usage.totals.clone();
        meta.usage_requests = usage.requests;
        meta.cache_reported_requests = usage.cache_reported_requests;
    }
    Some(meta)
}

/// Open the journal writer for a metadata write.
///
/// Fails with `batch writer lock unavailable` when an actor owns the session;
/// callers must route the write through the actor in that case.
fn writer(dir: &Path) -> Result<SessionCommit, String> {
    let identity = open_session_identity(dir).map_err(|error| error.to_string())?;
    SessionCommit::open(
        dir,
        identity.session_id,
        identity.log_id,
        default_context_budget(),
    )
    .map_err(|error| format!("open session journal for metadata write: {error}"))
}

fn commit_one(
    dir: &Path,
    batch_prefix: &str,
    payload: FactPayload,
) -> Result<(), String> {
    let mut commit = writer(dir)?;
    let head = commit.head().map_err(|error| error.to_string())?;
    let fact = SessionFact {
        schema: FactSchema::v2(),
        session_id: commit.session_id().clone(),
        log_id: commit.log_id().clone(),
        fact_seq: 0,
        event_id: EventId::new(generate_ulid()),
        ts_ms: now_ms(),
        causation_id: None,
        turn_id: None,
        call_id: None,
        interaction_id: None,
        payload,
    };
    let session_dir = commit.session_dir().to_path_buf();
    commit
        .commit(&format!("{batch_prefix}-{}", head + 1), head, vec![fact])
        .map_err(|error| error.to_string())?;
    drop(commit);
    refresh_index(&session_dir);
    Ok(())
}

/// Rebuild this session's `index.jsonl` entry from the journal.
///
/// `index.jsonl` is a listing cache, not a truth: `SessionManager::list` reads
/// it, so every metadata write (actor or direct) has to refresh it or the
/// session list shows stale titles/archives.
pub fn refresh_index(session_dir: &Path) {
    let Some(parent) = session_dir.parent() else {
        return;
    };
    if let Some(meta) = read(session_dir) {
        crate::store::upsert_index(parent, &meta);
    }
}

/// Commit a metadata patch. See the module docs for the writer-lock caveat.
pub fn patch(session_dir: impl AsRef<Path>, patch: SessionMetadataPatch) -> Result<(), String> {
    commit_one(
        session_dir.as_ref(),
        "meta",
        FactPayload::SessionMetadataChanged(SessionMetadataChanged {
            patch,
            source: MetadataSource::Api,
            changed_at_ms: now_ms(),
        }),
    )
}

/// Commit a title change. See the module docs for the writer-lock caveat.
pub fn set_title(
    session_dir: impl AsRef<Path>,
    title: &str,
    source: TitleSource,
) -> Result<(), String> {
    commit_one(
        session_dir.as_ref(),
        "title",
        FactPayload::SessionTitleChanged(SessionTitleChanged {
            title: title.to_string(),
            source,
            changed_at_ms: now_ms(),
        }),
    )
}

/// Convenience: the session id a directory belongs to, if it is a session.
pub fn session_id_of(session_dir: impl AsRef<Path>) -> Option<SessionId> {
    open_session_identity(session_dir).ok().map(|i| i.session_id)
}
