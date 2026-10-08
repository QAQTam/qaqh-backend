//! Durable replay window and cursor expiry contract.
//!
//! `ReplayWindowManifest` is the canonical source for the retained fact
//! window. It copies the session logical clock and never advances time itself.

use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::{
    CanonicalError, clock::ContentClock, log::write_json_atomic, recovery::sha256_content_hash,
};
use crate::{
    projection::ReplayWindow,
    session_fact_v2::{
        ContentHash, END_OF_FACT, LogId, MAX_SAFE_FACT_SEQ, ReliableCursor, ResetReason,
        ResetRequired, SessionFact, projection_slots,
    },
};

pub const REPLAY_WINDOW_FILE: &str = "replay-window.json";
pub const REPLAY_WINDOW_SCHEMA: &str = "qaqh.replay-window/v1";
pub const DEFAULT_WINDOW_CAPACITY_FACTS: u64 = 100_000;
pub const DEFAULT_WINDOW_CAPACITY_BYTES: u64 = 64 * 1024 * 1024;
pub const DEFAULT_REPLAY_WINDOW_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
pub const DEFAULT_SNAPSHOT_MAX_AGE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayWindowReason {
    CapacityFacts,
    CapacityBytes,
    Retention,
    SnapshotRotated,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayWindowConfig {
    pub window_capacity_facts: u64,
    pub window_capacity_bytes: u64,
    pub replay_window_retention_ms: i64,
    pub snapshot_max_age_ms: i64,
}

impl Default for ReplayWindowConfig {
    fn default() -> Self {
        Self {
            window_capacity_facts: DEFAULT_WINDOW_CAPACITY_FACTS,
            window_capacity_bytes: DEFAULT_WINDOW_CAPACITY_BYTES,
            replay_window_retention_ms: DEFAULT_REPLAY_WINDOW_RETENTION_MS,
            snapshot_max_age_ms: DEFAULT_SNAPSHOT_MAX_AGE_MS,
        }
    }
}

impl ReplayWindowConfig {
    fn validate(self) -> Result<(), CanonicalError> {
        if self.window_capacity_facts == 0 {
            return Err(invalid_replay_window(
                "window_capacity_facts must be positive",
            ));
        }
        if self.window_capacity_bytes == 0 {
            return Err(invalid_replay_window(
                "window_capacity_bytes must be positive",
            ));
        }
        if self.replay_window_retention_ms < 0 {
            return Err(invalid_replay_window(
                "replay_window_retention_ms must not be negative",
            ));
        }
        if self.snapshot_max_age_ms < 0 {
            return Err(invalid_replay_window(
                "snapshot_max_age_ms must not be negative",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotStatus {
    Valid,
    Missing,
    HashMismatch,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayWindowManifest {
    pub schema: String,
    pub log_id: LogId,
    pub generation: u64,
    pub earliest_available_fact_seq: u64,
    pub latest_fact_seq: u64,
    pub earliest_cursor: ReliableCursor,
    pub snapshot_cursor: Option<ReliableCursor>,
    pub snapshot_generation: u64,
    pub snapshot_hash: ContentHash,
    pub snapshot_path: String,
    pub snapshot_fact_seq: Option<u64>,
    pub snapshot_created_at_logical_ms: Option<i64>,
    pub snapshot_expires_at_logical_ms: Option<i64>,
    pub logical_now_ms: i64,
    pub retained_from_ms: i64,
    pub retained_until_ms: i64,
    pub window_capacity_facts: u64,
    pub window_capacity_bytes: u64,
    pub retained_facts: u64,
    pub retained_bytes: u64,
    pub reason: ReplayWindowReason,
}

impl ReplayWindowManifest {
    pub fn validate(&self) -> Result<(), CanonicalError> {
        if self.schema != REPLAY_WINDOW_SCHEMA {
            return Err(invalid_replay_window(format!(
                "unexpected schema {}",
                self.schema
            )));
        }
        if self.log_id.as_str().is_empty() {
            return Err(invalid_replay_window("log_id must not be empty"));
        }
        if self.generation == 0 {
            return Err(invalid_replay_window("generation must be positive"));
        }
        validate_fact_seq(
            "earliest_available_fact_seq",
            self.earliest_available_fact_seq,
        )?;
        if self.latest_fact_seq > MAX_SAFE_FACT_SEQ {
            return Err(invalid_replay_window(
                "latest_fact_seq exceeds safe integer range",
            ));
        }
        self.earliest_cursor.validate()?;
        if self.earliest_cursor.log_id != self.log_id
            || self.earliest_cursor.fact_seq != self.earliest_available_fact_seq
            || self.earliest_cursor.projection_index != 0
        {
            return Err(invalid_replay_window(
                "earliest_cursor must match the log and earliest available fact",
            ));
        }
        if self.logical_now_ms < 0 {
            return Err(invalid_replay_window("logical_now_ms must not be negative"));
        }
        if self.window_capacity_facts == 0 || self.window_capacity_bytes == 0 {
            return Err(invalid_replay_window("window capacities must be positive"));
        }
        if self.retained_facts == 0 {
            if self.retained_bytes != 0 || self.retained_from_ms != 0 || self.retained_until_ms != 0
            {
                return Err(invalid_replay_window(
                    "empty replay window must not retain bytes or timestamps",
                ));
            }
        } else {
            if self.earliest_available_fact_seq > self.latest_fact_seq {
                return Err(invalid_replay_window(
                    "non-empty replay window has no retained fact range",
                ));
            }
            let expected = self
                .latest_fact_seq
                .checked_sub(self.earliest_available_fact_seq)
                .and_then(|value| value.checked_add(1))
                .ok_or_else(|| invalid_replay_window("retained fact count overflow"))?;
            if self.retained_facts != expected {
                return Err(invalid_replay_window(
                    "retained_facts does not match the retained fact range",
                ));
            }
            if self.retained_from_ms > self.retained_until_ms {
                return Err(invalid_replay_window(
                    "retained_from_ms must not exceed retained_until_ms",
                ));
            }
        }

        match self.snapshot_fact_seq {
            None => {
                if self.snapshot_generation != 0
                    || !self.snapshot_path.is_empty()
                    || self.snapshot_hash != sha256_content_hash(b"")
                    || self.snapshot_created_at_logical_ms.is_some()
                    || self.snapshot_expires_at_logical_ms.is_some()
                    || self.snapshot_cursor.is_some()
                {
                    return Err(invalid_replay_window(
                        "missing snapshot must clear snapshot metadata and cursor",
                    ));
                }
            }
            Some(snapshot_fact_seq) => {
                validate_fact_seq("snapshot_fact_seq", snapshot_fact_seq)?;
                if self.snapshot_generation == 0 {
                    return Err(invalid_replay_window(
                        "snapshot generation must be positive",
                    ));
                }
                if !valid_snapshot_path(&self.snapshot_path) {
                    return Err(invalid_replay_window(
                        "snapshot_path must be a relative path below snapshots/",
                    ));
                }
                validate_content_hash("snapshot_hash", &self.snapshot_hash)?;
                let created = self.snapshot_created_at_logical_ms.ok_or_else(|| {
                    invalid_replay_window("snapshot_created_at_logical_ms is required")
                })?;
                let expires = self.snapshot_expires_at_logical_ms.ok_or_else(|| {
                    invalid_replay_window("snapshot_expires_at_logical_ms is required")
                })?;
                if created < 0 || expires < created {
                    return Err(invalid_replay_window(
                        "snapshot logical timestamps are invalid",
                    ));
                }
                if let Some(cursor) = &self.snapshot_cursor {
                    cursor.validate_snapshot_cursor()?;
                    if cursor.log_id != self.log_id
                        || cursor.fact_seq != snapshot_fact_seq
                        || cursor.projection_index != END_OF_FACT
                    {
                        return Err(invalid_replay_window(
                            "snapshot_cursor must match the snapshot boundary",
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn snapshot_status(
        &self,
        session_dir: impl AsRef<Path>,
        config: ReplayWindowConfig,
    ) -> Result<SnapshotStatus, CanonicalError> {
        config.validate()?;
        let Some(snapshot_fact_seq) = self.snapshot_fact_seq else {
            return Ok(SnapshotStatus::Missing);
        };
        if snapshot_fact_seq > self.latest_fact_seq
            || self.snapshot_generation == 0
            || !valid_snapshot_path(&self.snapshot_path)
            || validate_content_hash("snapshot_hash", &self.snapshot_hash).is_err()
        {
            return Ok(SnapshotStatus::HashMismatch);
        }
        let Some(created_at) = self.snapshot_created_at_logical_ms else {
            return Ok(SnapshotStatus::HashMismatch);
        };
        let Some(expires_at) = self.snapshot_expires_at_logical_ms else {
            return Ok(SnapshotStatus::HashMismatch);
        };
        if created_at < 0 || expires_at < created_at {
            return Ok(SnapshotStatus::HashMismatch);
        }

        let path = snapshot_path(session_dir.as_ref(), &self.snapshot_path);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(SnapshotStatus::Missing);
            }
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Ok(SnapshotStatus::HashMismatch);
        }
        let bytes = fs::read(path)?;
        if sha256_content_hash(&bytes) != self.snapshot_hash {
            return Ok(SnapshotStatus::HashMismatch);
        }
        let max_age_expiry = created_at.saturating_add(config.snapshot_max_age_ms);
        if expires_at < self.logical_now_ms || max_age_expiry < self.logical_now_ms {
            return Ok(SnapshotStatus::Expired);
        }
        Ok(SnapshotStatus::Valid)
    }

    pub fn replay_window(&self) -> ReplayWindow {
        ReplayWindow {
            log_id: self.log_id.clone(),
            earliest_available_fact_seq: self.earliest_available_fact_seq,
            snapshot_cursor: self.snapshot_cursor.clone(),
        }
    }

    pub fn reset_for_cursor(
        &self,
        session_dir: impl AsRef<Path>,
        cursor: &ReliableCursor,
        facts: &[SessionFact],
        config: ReplayWindowConfig,
    ) -> Result<Option<ResetRequired>, CanonicalError> {
        cursor.validate()?;
        if cursor.log_id != self.log_id {
            return Ok(Some(self.reset_required(
                ResetReason::LogIdMismatch,
                self.snapshot_cursor.clone(),
            )));
        }
        if cursor.fact_seq < self.earliest_available_fact_seq {
            let status = self.snapshot_status(session_dir, config)?;
            let (reason, snapshot_cursor) = match status {
                SnapshotStatus::Valid => (ResetReason::CursorExpired, self.snapshot_cursor.clone()),
                SnapshotStatus::Missing => (ResetReason::SnapshotMissing, None),
                SnapshotStatus::HashMismatch => (ResetReason::SnapshotHashMismatch, None),
                SnapshotStatus::Expired => (ResetReason::SnapshotExpired, None),
            };
            return Ok(Some(self.reset_required(reason, snapshot_cursor)));
        }

        if cursor.fact_seq > self.latest_fact_seq {
            return Ok(Some(self.reset_required(ResetReason::CursorExpired, None)));
        }
        let Some(fact) = facts.iter().find(|fact| fact.fact_seq == cursor.fact_seq) else {
            return Ok(Some(self.reset_required(ResetReason::UnknownFact, None)));
        };
        let max_projection_index = projection_slots(&fact.payload)
            .iter()
            .map(|slot| slot.as_u16())
            .max();
        if max_projection_index.is_none_or(|index| cursor.projection_index > index) {
            return Ok(Some(self.reset_required(ResetReason::CursorExpired, None)));
        }
        Ok(None)
    }

    fn reset_required(
        &self,
        reason: ResetReason,
        snapshot_cursor: Option<ReliableCursor>,
    ) -> ResetRequired {
        ResetRequired {
            log_id: self.log_id.clone(),
            snapshot_cursor,
            reason,
        }
    }
}

pub fn replay_window_path(session_dir: impl AsRef<Path>) -> PathBuf {
    session_dir.as_ref().join(REPLAY_WINDOW_FILE)
}

pub fn load_replay_window_manifest(
    session_dir: impl AsRef<Path>,
) -> Result<Option<ReplayWindowManifest>, CanonicalError> {
    let path = replay_window_path(session_dir);
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let manifest: ReplayWindowManifest = serde_json::from_slice(&bytes)?;
    manifest.validate()?;
    Ok(Some(manifest))
}

pub fn recover_replay_window_manifest(
    session_dir: impl AsRef<Path>,
    log_id: &LogId,
    facts: &[SessionFact],
    config: ReplayWindowConfig,
) -> Result<(ReplayWindowManifest, ContentClock), CanonicalError> {
    config.validate()?;
    let session_dir = session_dir.as_ref();
    let existing = load_replay_window_manifest(session_dir)?;
    if let Some(manifest) = &existing
        && manifest.log_id != *log_id
    {
        return Err(CanonicalError::IdentityMismatch {
            field: "replay_window.log_id",
        });
    }
    let clock = ContentClock::open_or_recover(
        session_dir,
        facts,
        existing.as_ref().map(|manifest| manifest.logical_now_ms),
    )?;
    let manifest = rebuild_and_persist(session_dir, log_id, facts, &clock, config, existing)?;
    Ok((manifest, clock))
}

fn rebuild_and_persist(
    session_dir: &Path,
    log_id: &LogId,
    facts: &[SessionFact],
    clock: &ContentClock,
    config: ReplayWindowConfig,
    existing: Option<ReplayWindowManifest>,
) -> Result<ReplayWindowManifest, CanonicalError> {
    let mut candidate =
        compute_manifest(session_dir, log_id, facts, clock, config, existing.as_ref())?;
    match existing {
        None => {
            candidate.generation = 1;
            candidate.validate()?;
            write_json_atomic(&replay_window_path(session_dir), &candidate)?;
            Ok(candidate)
        }
        Some(existing) => {
            candidate.generation = existing.generation;
            if candidate == existing {
                return Ok(existing);
            }
            candidate.generation = existing
                .generation
                .checked_add(1)
                .ok_or_else(|| invalid_replay_window("manifest generation is exhausted"))?;
            candidate.validate()?;
            write_json_atomic(&replay_window_path(session_dir), &candidate)?;
            Ok(candidate)
        }
    }
}

fn compute_manifest(
    session_dir: &Path,
    log_id: &LogId,
    facts: &[SessionFact],
    clock: &ContentClock,
    config: ReplayWindowConfig,
    existing: Option<&ReplayWindowManifest>,
) -> Result<ReplayWindowManifest, CanonicalError> {
    let facts = normalized_facts(facts)?;
    let latest_fact_seq = facts.last().map(|fact| fact.fact_seq).unwrap_or(0);
    let snapshot_status = match existing {
        Some(manifest) => manifest.snapshot_status(session_dir, config)?,
        None => SnapshotStatus::Missing,
    };
    let snapshot_fact_seq = existing.and_then(|manifest| manifest.snapshot_fact_seq);
    let snapshot_floor = if snapshot_status == SnapshotStatus::Valid {
        snapshot_fact_seq
            .ok_or_else(|| invalid_replay_window("valid snapshot is missing its fact sequence"))?
            .checked_add(1)
            .ok_or_else(|| invalid_replay_window("snapshot fact sequence is exhausted"))?
    } else {
        1
    };

    let seq_floor = latest_fact_seq
        .saturating_sub(config.window_capacity_facts)
        .saturating_add(1)
        .max(1);
    let byte_floor = byte_floor(&facts, latest_fact_seq, config.window_capacity_bytes)?;
    let time_floor = time_floor(&facts, latest_fact_seq, clock.logical_now_ms(), config);
    let earliest_available_fact_seq = seq_floor
        .max(byte_floor)
        .max(time_floor)
        .max(snapshot_floor);
    if earliest_available_fact_seq > MAX_SAFE_FACT_SEQ {
        return Err(invalid_replay_window(
            "earliest available fact sequence exceeds safe integer range",
        ));
    }

    let retained_facts = if latest_fact_seq >= earliest_available_fact_seq {
        latest_fact_seq - earliest_available_fact_seq + 1
    } else {
        0
    };
    let retained = facts
        .iter()
        .filter(|fact| fact.fact_seq >= earliest_available_fact_seq)
        .collect::<Vec<_>>();
    let retained_bytes = retained.iter().try_fold(0_u64, |total, fact| {
        total
            .checked_add(canonical_fact_len(fact)?)
            .ok_or_else(|| invalid_replay_window("retained byte count overflow"))
    })?;
    let retained_from_ms = retained.iter().map(|fact| fact.ts_ms).min().unwrap_or(0);
    let retained_until_ms = retained.iter().map(|fact| fact.ts_ms).max().unwrap_or(0);
    let earliest_cursor = ReliableCursor {
        log_id: log_id.clone(),
        fact_seq: earliest_available_fact_seq,
        projection_index: 0,
    };
    let snapshot_cursor = if snapshot_status == SnapshotStatus::Valid {
        snapshot_fact_seq.map(|fact_seq| ReliableCursor {
            log_id: log_id.clone(),
            fact_seq,
            projection_index: END_OF_FACT,
        })
    } else {
        None
    };
    let reason = replay_window_reason(
        seq_floor,
        byte_floor,
        time_floor,
        snapshot_floor,
        earliest_available_fact_seq,
    );
    let (
        snapshot_generation,
        snapshot_hash,
        snapshot_path,
        snapshot_created_at_logical_ms,
        snapshot_expires_at_logical_ms,
    ) = match snapshot_fact_seq {
        Some(_) => {
            let manifest = existing.expect("snapshot metadata requires an existing manifest");
            (
                manifest.snapshot_generation,
                manifest.snapshot_hash.clone(),
                manifest.snapshot_path.clone(),
                manifest.snapshot_created_at_logical_ms,
                manifest.snapshot_expires_at_logical_ms,
            )
        }
        None => (0, sha256_content_hash(b""), String::new(), None, None),
    };

    Ok(ReplayWindowManifest {
        schema: REPLAY_WINDOW_SCHEMA.into(),
        log_id: log_id.clone(),
        generation: 0,
        earliest_available_fact_seq,
        latest_fact_seq,
        earliest_cursor,
        snapshot_cursor,
        snapshot_generation,
        snapshot_hash,
        snapshot_path,
        snapshot_fact_seq,
        snapshot_created_at_logical_ms,
        snapshot_expires_at_logical_ms,
        logical_now_ms: clock.logical_now_ms(),
        retained_from_ms,
        retained_until_ms,
        window_capacity_facts: config.window_capacity_facts,
        window_capacity_bytes: config.window_capacity_bytes,
        retained_facts,
        retained_bytes,
        reason,
    })
}

fn normalized_facts(facts: &[SessionFact]) -> Result<Vec<&SessionFact>, CanonicalError> {
    let mut sorted = facts.iter().collect::<Vec<_>>();
    sorted.sort_by_key(|fact| fact.fact_seq);
    let mut previous = None;
    for fact in &sorted {
        validate_fact_seq("fact_seq", fact.fact_seq)?;
        if previous == Some(fact.fact_seq) {
            return Err(invalid_replay_window(
                "committed facts contain a duplicate fact_seq",
            ));
        }
        previous = Some(fact.fact_seq);
    }
    Ok(sorted)
}

fn byte_floor(
    facts: &[&SessionFact],
    latest_fact_seq: u64,
    capacity_bytes: u64,
) -> Result<u64, CanonicalError> {
    if facts.is_empty() {
        return Ok(1);
    }
    let mut floor = latest_fact_seq;
    let mut total = 0_u64;
    for fact in facts.iter().rev() {
        let bytes = canonical_fact_len(fact)?;
        if fact.fact_seq == latest_fact_seq {
            floor = fact.fact_seq;
            total = bytes;
            continue;
        }
        if total.saturating_add(bytes) > capacity_bytes {
            floor = fact.fact_seq.saturating_add(1);
            break;
        }
        total = total.saturating_add(bytes);
        floor = fact.fact_seq;
    }
    Ok(floor)
}

fn time_floor(
    facts: &[&SessionFact],
    latest_fact_seq: u64,
    logical_now_ms: i64,
    config: ReplayWindowConfig,
) -> u64 {
    let cutoff = logical_now_ms.saturating_sub(config.replay_window_retention_ms);
    facts
        .iter()
        .find(|fact| fact.ts_ms >= cutoff)
        .map(|fact| fact.fact_seq)
        .unwrap_or(latest_fact_seq.max(1))
}

fn canonical_fact_len(fact: &SessionFact) -> Result<u64, CanonicalError> {
    Ok(serde_json::to_vec(fact)?.len() as u64)
}

fn replay_window_reason(
    seq_floor: u64,
    byte_floor: u64,
    time_floor: u64,
    snapshot_floor: u64,
    earliest: u64,
) -> ReplayWindowReason {
    if earliest <= 1 {
        return ReplayWindowReason::Manual;
    }
    if snapshot_floor == earliest {
        ReplayWindowReason::SnapshotRotated
    } else if time_floor == earliest {
        ReplayWindowReason::Retention
    } else if byte_floor == earliest {
        ReplayWindowReason::CapacityBytes
    } else if seq_floor == earliest {
        ReplayWindowReason::CapacityFacts
    } else {
        ReplayWindowReason::Manual
    }
}

fn snapshot_path(session_dir: &Path, relative_path: &str) -> PathBuf {
    session_dir.join("snapshots").join(relative_path)
}

fn valid_snapshot_path(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let path = Path::new(path);
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn validate_fact_seq(field: &str, fact_seq: u64) -> Result<(), CanonicalError> {
    if fact_seq == 0 || fact_seq > MAX_SAFE_FACT_SEQ {
        return Err(invalid_replay_window(format!(
            "{field} must be in 1..={MAX_SAFE_FACT_SEQ}"
        )));
    }
    Ok(())
}

fn validate_content_hash(field: &str, value: &ContentHash) -> Result<(), CanonicalError> {
    let Some(hash) = value.as_str().strip_prefix("sha256:") else {
        return Err(invalid_replay_window(format!(
            "{field} must use sha256:<64 lowercase hex>"
        )));
    };
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(invalid_replay_window(format!(
            "{field} must use sha256:<64 lowercase hex>"
        )));
    }
    Ok(())
}

fn invalid_replay_window(message: impl Into<String>) -> CanonicalError {
    CanonicalError::InvalidReplayWindow(message.into())
}
