//! In-process sparse offset/digest index over the committed prefix (B2.1
//! phase 2).
//!
//! Purpose: `CommittedFactReader::open` re-parsed and re-validated the entire
//! committed prefix on every open (O(history) parse per call — the daemon's
//! mailbox wait path opens on every commit wake). This module keeps, per
//! canonical session directory, a sparse seq→(offset, digest) index whose
//! entries are only ever rooted at a **fully validated scan** of the prefix
//! and then advanced exclusively by this process's own durable appends.
//!
//! Fail-closed contract (`plan §3`): the index accelerates validation, it
//! never skips it.
//! - A warm open streams the indexed byte range through SHA-256 and compares
//!   against the recorded digest; any in-place byte change (corruption,
//!   rewrite) is rejected even when the marker and file length still match.
//! - Identity/length/rewrite staleness self-invalidates the lookup: the
//!   index hits only when its `log_id`/`session_id` match the marker and its
//!   `last_seq`/`last_offset` equal the marker's (plus a file-length check).
//!   Truncating repair therefore falls back to the cold full validating scan
//!   and re-promotes.
//! - The cold path (no index: fresh process, foreign writer, or test writing
//!   events.jsonl directly) always parses and validates every fact.
//!
//! Honest complexity record: a warm open still reads + hashes the indexed
//! prefix (O(history) bytes, zero parse/allocate); it is not O(1). True O(1)
//! would require trusting the in-process writer without re-reading bytes,
//! which crosses the fail-closed boundary — out of scope here.
//!
//! Memory: one entry per session directory that has been validated in this
//! process; ≤1024 sparse checkpoints (stride 512 facts, ~56 B each) per
//! entry, bounded ~64 KiB per session. Entries are never pruned (same
//! lifecycle as the commit-notify registry); growth is bounded by session
//! count, not history size.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, MutexGuard},
};

use sha2::{Digest, Sha256};

use super::types::EventsCommit;
use crate::session_fact_v2::{EventId, LogId, SessionId};

/// One sparse checkpoint: after `CHECKPOINT_STRIDE` facts, the running digest
/// state covers exactly `offset` bytes of the committed prefix and the last
/// parsed fact was `fact_seq` / `last_event_id`.
#[derive(Clone)]
pub(super) struct Checkpoint {
    pub fact_seq: u64,
    pub offset: u64,
    pub digest: [u8; 32],
    pub last_event_id: EventId,
}

pub(super) struct CommittedPrefixIndex {
    pub session_id: SessionId,
    pub log_id: LogId,
    pub last_seq: u64,
    pub last_offset: u64,
    pub last_event_id: EventId,
    digest: Sha256,
    checkpoints: Vec<Checkpoint>,
}

/// Facts between sparse checkpoints. 512 × ~560 B facts ≈ 280 KiB parsed per
/// seek-granularity step; the digest verification pass over the skipped
/// region remains the dominant (unavoidable) cost.
pub(super) const CHECKPOINT_STRIDE: u64 = 512;
/// Upper bound on retained checkpoints per session (~56 B each). Overflow
/// drops the oldest half; regions below the retained window fall back to the
/// cold full scan on seek.
pub(super) const MAX_CHECKPOINTS: usize = 1024;

static INDEXES: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<CommittedPrefixIndex>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn registry() -> MutexGuard<'static, HashMap<PathBuf, Arc<Mutex<CommittedPrefixIndex>>>> {
    INDEXES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn entry(session_dir: &Path) -> Option<Arc<Mutex<CommittedPrefixIndex>>> {
    registry().get(session_dir).cloned()
}

/// Root a fresh index at a fully validated scan result.
///
/// Callers must only invoke this after a complete validating pass over
/// exactly `last_offset` bytes ending at `last_seq`; `digest` must be the
/// SHA-256 state over those bytes and `checkpoints` the sparse marks taken
/// during the same pass.
pub(super) fn promote_full_scan(
    session_dir: &Path,
    session_id: &SessionId,
    log_id: &LogId,
    last_seq: u64,
    last_offset: u64,
    last_event_id: &EventId,
    digest: Sha256,
    checkpoints: Vec<Checkpoint>,
) {
    if last_offset == 0 {
        // An empty prefix needs no acceleration.
        return;
    }
    let index = CommittedPrefixIndex {
        session_id: session_id.clone(),
        log_id: log_id.clone(),
        last_seq,
        last_offset,
        last_event_id: last_event_id.clone(),
        digest,
        checkpoints,
    };
    registry().insert(session_dir.to_path_buf(), Arc::new(Mutex::new(index)));
}

/// Advance an existing index by one durable append.
///
/// Called from [`super::CanonicalLog::append`] after the commit marker write
/// is durable. `line` is the exact serialized fact bytes (including the
/// trailing newline) now covered by `commit`. A no-op when no rooted index
/// exists for the directory (the next cold open re-promotes) or when the
/// index is stale relative to the marker (defensive; self-invalidates).
pub(super) fn advance_append(
    session_dir: &Path,
    session_id: &SessionId,
    log_id: &LogId,
    commit: &EventsCommit,
    line: &[u8],
    event_id: &EventId,
) {
    let Some(index) = entry(session_dir) else {
        return;
    };
    let mut index = index
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if index.log_id != *log_id
        || index.session_id != *session_id
        || index.last_seq.checked_add(1) != Some(commit.committed_fact_seq)
        || index.last_offset.checked_add(line.len() as u64) != Some(commit.committed_offset)
    {
        // Stale beyond incremental repair (repair paths fall back to cold
        // scan); drop so a later open re-promotes from a validated state.
        drop(index);
        registry().remove(session_dir);
        return;
    }
    index.digest.update(line);
    index.last_seq = commit.committed_fact_seq;
    index.last_offset = commit.committed_offset;
    index.last_event_id = event_id.clone();
    if index.last_seq % CHECKPOINT_STRIDE == 0 {
        let checkpoint = Checkpoint {
            fact_seq: index.last_seq,
            offset: index.last_offset,
            digest: index.digest.clone().finalize().into(),
            last_event_id: event_id.clone(),
        };
        index.checkpoints.push(checkpoint);
        if index.checkpoints.len() > MAX_CHECKPOINTS {
            let keep_from = index.checkpoints.len() / 2;
            index.checkpoints.drain(..keep_from);
        }
    }
}

/// Look up a warm index matching `commit` exactly.
///
/// Returns the index only when its identity matches the marker and its
/// coverage (`last_seq`/`last_offset`) equals the marker's committed
/// position. The caller must additionally verify the file length equals
/// `committed_offset` before trusting the index (done by the reader, which
/// already stats the file).
pub(super) fn lookup(
    session_dir: &Path,
    session_id: &SessionId,
    log_id: &LogId,
    commit: &EventsCommit,
) -> Option<Arc<Mutex<CommittedPrefixIndex>>> {
    let index = entry(session_dir)?;
    {
        let index = index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if index.session_id != *session_id
            || index.log_id != *log_id
            || index.last_seq != commit.committed_fact_seq
            || index.last_offset != commit.committed_offset
        {
            return None;
        }
    }
    Some(index)
}

/// Largest checkpoint with `fact_seq < from_seq`, for seek-based range
/// reads. Strictly less: checkpoints record the offset *after* their fact,
/// so a checkpoint at exactly `from_seq` would start the scan past the first
/// requested fact. `None` when no checkpoint covers the requested start
/// (cold full scan).
pub(super) fn seek_checkpoint(index: &CommittedPrefixIndex, from_seq: u64) -> Option<Checkpoint> {
    index
        .checkpoints
        .iter()
        .rev()
        .find(|checkpoint| checkpoint.fact_seq < from_seq)
        .cloned()
}

/// Digest recorded for the whole indexed prefix (bytes `0..last_offset`).
pub(super) fn prefix_digest(index: &CommittedPrefixIndex) -> [u8; 32] {
    index.digest.clone().finalize().into()
}

/// Access for seek-based reads: `(last_event_id at checkpoint)`.
pub(super) fn checkpoint_event_id(checkpoint: &Checkpoint) -> &EventId {
    &checkpoint.last_event_id
}

/// Diagnostic/test hook: drop all in-process index state (forces cold
/// validating scans until re-promotion).
#[doc(hidden)]
pub fn clear_for_test() {
    registry().clear();
}
