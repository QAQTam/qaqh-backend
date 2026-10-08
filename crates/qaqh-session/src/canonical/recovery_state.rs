//! Canonical recovery state and commit marker repair.
//!
//! Repair is deliberately separate from recovery-plan execution. It only
//! validates the committed prefix, repairs `events.commit.json`, and removes
//! poison evidence after the repair is durable.

use std::{
    fs::{self, File, OpenOptions},
    io,
    path::Path,
};

use super::{
    CanonicalError, EVENTS_COMMIT_FILE, EVENTS_FILE, EVENTS_LOCK_FILE, EVENTS_POISON_FILE,
    UPGRADE_FENCE_FILE, WRITER_FENCE_FILE,
    log::write_json_atomic,
    reader::scan_committed_facts,
    types::{
        EVENTS_COMMIT_SCHEMA, EventsCommit, UPGRADE_FENCE_SCHEMA, UpgradeFence, UpgradeState,
        WRITER_FENCE_SCHEMA, WriterFence,
    },
};
use crate::session_fact_v2::{
    EventId, FactPayload, LogId, MAX_SAFE_FACT_SEQ, RecoveryOutcome, SessionId,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalRecoveryState {
    pub outcome: RecoveryOutcome,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRepairOutcome {
    pub commit: EventsCommit,
    pub previous_commit_generation: Option<u64>,
    pub marker_rebuilt: bool,
    pub commit_repaired: bool,
    pub truncated_bytes: u64,
    pub poison_cleared: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScannedPrefix {
    fact_seq: u64,
    offset: u64,
    last_event_id: Option<EventId>,
    contains_tombstone: bool,
}

/// Inspect the current on-disk recovery state without mutating the session.
pub fn inspect_recovery_state(
    session_dir: impl AsRef<Path>,
    session_id: &SessionId,
    log_id: &LogId,
) -> Result<CanonicalRecoveryState, CanonicalError> {
    let session_dir = session_dir.as_ref();
    fs::create_dir_all(session_dir)?;
    let _guard = lock_events(session_dir)?;
    inspect_recovery_state_unlocked(session_dir, session_id, log_id)
}

/// Repair the commit marker under the canonical `events.lock`.
///
/// This function is idempotent: a second call over a repaired log returns an
/// unchanged outcome without advancing the marker generation.
pub fn repair_commit_marker(
    session_dir: impl AsRef<Path>,
    session_id: &SessionId,
    log_id: &LogId,
) -> Result<CommitRepairOutcome, CanonicalError> {
    let session_dir = session_dir.as_ref();
    fs::create_dir_all(session_dir)?;
    let _guard = lock_events(session_dir)?;
    repair_commit_marker_unlocked(session_dir, session_id, log_id)
}

pub(crate) fn repair_commit_marker_unlocked(
    session_dir: &Path,
    session_id: &SessionId,
    log_id: &LogId,
) -> Result<CommitRepairOutcome, CanonicalError> {
    let poison_present = session_dir.join(EVENTS_POISON_FILE).exists();
    let events = read_events_bytes(session_dir)?;
    let candidate_offset = last_complete_offset(&events);
    let torn_tail = candidate_offset < events.len() as u64;
    let prefix = scan_prefix(&events[..candidate_offset as usize], session_id, log_id)?;
    let valid_marker = read_valid_commit_marker(session_dir, log_id)?;

    if let Some(marker) = valid_marker {
        if marker.committed_offset > candidate_offset {
            return Err(CanonicalError::CommitRecoveryRequired(
                "commit marker points beyond the last complete canonical fact".into(),
            ));
        }
        let committed_prefix = scan_prefix(
            &events[..marker.committed_offset as usize],
            session_id,
            log_id,
        )?;
        if marker.committed_fact_seq != committed_prefix.fact_seq
            || marker.last_barrier_event_id != committed_prefix.last_event_id
        {
            return Err(CanonicalError::CommitRecoveryRequired(
                "commit marker does not match the committed prefix".into(),
            ));
        }

        let truncated_bytes = events
            .len()
            .checked_sub(marker.committed_offset as usize)
            .ok_or_else(|| {
                CanonicalError::CommitRecoveryRequired(
                    "commit marker offset exceeds events length".into(),
                )
            })? as u64;
        ensure_events_file(session_dir)?;
        if truncated_bytes > 0 {
            truncate_events(session_dir, marker.committed_offset)?;
        }

        if poison_present || truncated_bytes > 0 {
            let previous_generation = marker.commit_generation;
            let repaired = EventsCommit {
                commit_generation: previous_generation.checked_add(1).ok_or_else(|| {
                    CanonicalError::CommitRecoveryRequired(
                        "commit marker generation is exhausted".into(),
                    )
                })?,
                ..marker
            };
            write_json_atomic(&session_dir.join(EVENTS_COMMIT_FILE), &repaired)?;
            if poison_present {
                remove_file_and_sync_parent(&session_dir.join(EVENTS_POISON_FILE))?;
            }
            return Ok(CommitRepairOutcome {
                commit: repaired,
                previous_commit_generation: Some(previous_generation),
                marker_rebuilt: false,
                commit_repaired: true,
                truncated_bytes,
                poison_cleared: poison_present,
            });
        }

        return Ok(CommitRepairOutcome {
            commit: marker.clone(),
            previous_commit_generation: Some(marker.commit_generation),
            marker_rebuilt: false,
            commit_repaired: false,
            truncated_bytes: 0,
            poison_cleared: false,
        });
    }

    if poison_present || torn_tail {
        return Err(CanonicalError::CommitRecoveryRequired(
            "commit marker cannot be rebuilt while poison or torn tail evidence exists".into(),
        ));
    }
    validate_writer_fence_identity(session_dir, session_id, log_id)?;
    ensure_events_file(session_dir)?;
    let marker = EventsCommit {
        schema: EVENTS_COMMIT_SCHEMA.into(),
        log_id: log_id.clone(),
        committed_fact_seq: prefix.fact_seq,
        committed_offset: prefix.offset,
        last_barrier_event_id: prefix.last_event_id,
        commit_generation: 0,
    };
    write_json_atomic(&session_dir.join(EVENTS_COMMIT_FILE), &marker)?;
    Ok(CommitRepairOutcome {
        commit: marker,
        previous_commit_generation: None,
        marker_rebuilt: true,
        commit_repaired: false,
        truncated_bytes: 0,
        poison_cleared: false,
    })
}

pub(crate) fn inspect_recovery_state_unlocked(
    session_dir: &Path,
    session_id: &SessionId,
    log_id: &LogId,
) -> Result<CanonicalRecoveryState, CanonicalError> {
    if session_dir.join(EVENTS_POISON_FILE).exists() {
        return Ok(CanonicalRecoveryState {
            outcome: RecoveryOutcome::CommitRecoveryRequired,
            reason: Some("events poison marker is present".into()),
        });
    }

    let Some(marker) = read_valid_commit_marker(session_dir, log_id)? else {
        return Ok(CanonicalRecoveryState {
            outcome: RecoveryOutcome::CommitRecoveryRequired,
            reason: Some("commit marker is missing, corrupt, or unprovable".into()),
        });
    };
    let events = read_events_bytes(session_dir)?;
    let candidate_offset = last_complete_offset(&events);
    if candidate_offset != events.len() as u64 || marker.committed_offset != candidate_offset {
        return Ok(CanonicalRecoveryState {
            outcome: RecoveryOutcome::CommitRecoveryRequired,
            reason: Some("canonical log has an uncommitted or torn suffix".into()),
        });
    }
    let prefix = match scan_prefix(
        &events[..marker.committed_offset as usize],
        session_id,
        log_id,
    ) {
        Ok(prefix) => prefix,
        Err(_) => {
            return Ok(CanonicalRecoveryState {
                outcome: RecoveryOutcome::CommitRecoveryRequired,
                reason: Some("committed prefix does not match the commit marker".into()),
            });
        }
    };
    if prefix.fact_seq != marker.committed_fact_seq
        || prefix.last_event_id != marker.last_barrier_event_id
    {
        return Ok(CanonicalRecoveryState {
            outcome: RecoveryOutcome::CommitRecoveryRequired,
            reason: Some("committed prefix does not match the commit marker".into()),
        });
    }
    if prefix.contains_tombstone {
        return Ok(CanonicalRecoveryState {
            outcome: RecoveryOutcome::Tombstone,
            reason: None,
        });
    }
    if read_upgrade_fence(session_dir, log_id)?
        .is_some_and(|fence| fence.state == UpgradeState::ReadOnly)
    {
        return Ok(CanonicalRecoveryState {
            outcome: RecoveryOutcome::ReadOnlyUpgradeRequired,
            reason: Some("upgrade fence requires read-only".into()),
        });
    }
    Ok(CanonicalRecoveryState {
        outcome: RecoveryOutcome::Writable,
        reason: None,
    })
}

fn scan_prefix(
    bytes: &[u8],
    session_id: &SessionId,
    log_id: &LogId,
) -> Result<ScannedPrefix, CanonicalError> {
    let mut last_event_id = None;
    let mut contains_tombstone = false;
    let fact_seq = scan_committed_facts(bytes, session_id, log_id, |fact| {
        last_event_id = Some(fact.event_id.clone());
        contains_tombstone |= matches!(fact.payload, FactPayload::SessionDeleted(_));
        Ok(())
    })?;
    Ok(ScannedPrefix {
        fact_seq,
        offset: bytes.len() as u64,
        last_event_id,
        contains_tombstone,
    })
}

fn read_valid_commit_marker(
    session_dir: &Path,
    log_id: &LogId,
) -> Result<Option<EventsCommit>, CanonicalError> {
    let bytes = match fs::read(session_dir.join(EVENTS_COMMIT_FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Ok(marker) = serde_json::from_slice::<EventsCommit>(&bytes) else {
        return Ok(None);
    };
    if marker.schema != EVENTS_COMMIT_SCHEMA
        || marker.log_id != *log_id
        || marker.committed_fact_seq > MAX_SAFE_FACT_SEQ
    {
        return Ok(None);
    }
    Ok(Some(marker))
}

fn read_upgrade_fence(
    session_dir: &Path,
    log_id: &LogId,
) -> Result<Option<UpgradeFence>, CanonicalError> {
    let bytes = match fs::read(session_dir.join(UPGRADE_FENCE_FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let fence: UpgradeFence = serde_json::from_slice(&bytes)?;
    if fence.schema != UPGRADE_FENCE_SCHEMA {
        return Err(CanonicalError::InvalidFence(format!(
            "unexpected upgrade fence schema {}",
            fence.schema
        )));
    }
    if fence.log_id != *log_id {
        return Err(CanonicalError::IdentityMismatch {
            field: "upgrade_fence.log_id",
        });
    }
    Ok(Some(fence))
}

fn validate_writer_fence_identity(
    session_dir: &Path,
    session_id: &SessionId,
    log_id: &LogId,
) -> Result<(), CanonicalError> {
    let bytes = match fs::read(session_dir.join(WRITER_FENCE_FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let fence: WriterFence = serde_json::from_slice(&bytes).map_err(|error| {
        CanonicalError::CommitRecoveryRequired(format!("writer fence is corrupt: {error}"))
    })?;
    if fence.schema != WRITER_FENCE_SCHEMA
        || fence.session_id != *session_id
        || fence.log_id != *log_id
    {
        return Err(CanonicalError::CommitRecoveryRequired(
            "writer fence identity does not match the canonical log".into(),
        ));
    }
    Ok(())
}

fn read_events_bytes(session_dir: &Path) -> Result<Vec<u8>, CanonicalError> {
    match fs::read(session_dir.join(EVENTS_FILE)) {
        Ok(bytes) => Ok(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

fn last_complete_offset(bytes: &[u8]) -> u64 {
    if bytes.is_empty() || bytes.last() == Some(&b'\n') {
        return bytes.len() as u64;
    }
    bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|offset| offset as u64 + 1)
        .unwrap_or(0)
}

fn truncate_events(session_dir: &Path, committed_offset: u64) -> Result<(), CanonicalError> {
    let events = OpenOptions::new()
        .read(true)
        .write(true)
        .open(session_dir.join(EVENTS_FILE))?;
    events.set_len(committed_offset)?;
    events.sync_all()?;
    Ok(())
}

fn ensure_events_file(session_dir: &Path) -> Result<(), CanonicalError> {
    let events = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(session_dir.join(EVENTS_FILE))?;
    events.sync_all()?;
    Ok(())
}

fn remove_file_and_sync_parent(path: &Path) -> Result<(), CanonicalError> {
    match fs::remove_file(path) {
        Ok(()) => sync_parent_dir(path.parent().ok_or_else(|| {
            CanonicalError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "canonical sidecar has no parent",
            ))
        })?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn lock_events(session_dir: &Path) -> Result<File, CanonicalError> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(session_dir.join(EVENTS_LOCK_FILE))?;
    file.lock()?;
    Ok(file)
}

#[cfg(unix)]
fn sync_parent_dir(parent: &Path) -> Result<(), CanonicalError> {
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_dir(parent: &Path) -> Result<(), CanonicalError> {
    let _ = parent;
    Ok(())
}
