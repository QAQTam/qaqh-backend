//! Legacy reconciliation and cutover/rollback state machine.
//!
//! The mapping index is derived compatibility data. The migration state is a
//! diagnostics sidecar; it never becomes a canonical session fact source.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use qaqh_message::legacy_writer::LegacyWriterFacade;

use crate::session_fact_v2::{ContentHash, LogId, MAX_SAFE_FACT_SEQ, SessionId, ValidationError};

use super::log::{CanonicalError, CanonicalLog, write_json_atomic};
use super::types::WriterId;

pub const MIGRATION_STATUS_SCHEMA: &str = "qaqh.migration-status/v1";
pub const MIGRATION_STATE_SCHEMA: &str = "qaqh.migration-state/v1";
pub const MIGRATION_STATE_FILE: &str = "diagnostics/migration-state.json";
pub const LEGACY_MAPPING_FILE: &str = "diagnostics/legacy-mapping.jsonl";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacySource {
    MessagesJsonl,
    RingingJournal,
    RingingLatest,
    RingingTimeline,
    RingingOffload,
    MetaJson,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum V1DeliveryKind {
    Reliable,
    Replaceable,
    Ephemeral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CanonicalSeq {
    pub fact_seq: u64,
    pub projection_index: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LegacyMappingKey {
    pub legacy_identity: String,
    pub derived_ordinal: u32,
    pub canonical_seq: CanonicalSeq,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyMappingTarget {
    pub key: LegacyMappingKey,
    pub delivery: V1DeliveryKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyMapping {
    pub legacy_source: LegacySource,
    pub source_generation_id: String,
    pub legacy_key: String,
    pub legacy_msg_id: Option<String>,
    pub session_id: SessionId,
    pub log_id: LogId,
    pub targets: Vec<LegacyMappingTarget>,
    pub source_hash: ContentHash,
    pub mapped_at_ms: i64,
}

impl LegacyMapping {
    pub fn legacy_identity(source_generation_id: &str, legacy_key: &str) -> String {
        serde_json::to_string(&(source_generation_id, legacy_key))
            .expect("serializing two strings cannot fail")
    }

    pub fn validate(&self) -> Result<(), MigrationError> {
        if self.source_generation_id.is_empty() || self.legacy_key.is_empty() {
            return Err(MigrationError::InvalidMapping(
                "source_generation_id and legacy_key must not be empty".into(),
            ));
        }
        if self.targets.is_empty() {
            return Err(MigrationError::InvalidMapping(
                "legacy mapping requires at least one target".into(),
            ));
        }
        let expected_identity = Self::legacy_identity(&self.source_generation_id, &self.legacy_key);
        let mut unique = HashSet::new();
        for target in &self.targets {
            if target.key.legacy_identity != expected_identity {
                return Err(MigrationError::InvalidMapping(
                    "target legacy_identity does not match the mapping source".into(),
                ));
            }
            validate_canonical_seq(&target.key.canonical_seq)?;
            if !unique.insert((target.key.derived_ordinal, target.key.canonical_seq)) {
                return Err(MigrationError::MappingConflict);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReconciliationMetrics {
    pub missing_canonical: u64,
    pub duplicate_canonical: u64,
    pub projection_mismatch: u64,
    pub seq_gap: u64,
    pub replay_live_mismatch: u64,
    pub content_ref_missing: u64,
    pub torn_tail_unresolved: u64,
}

impl ReconciliationMetrics {
    pub fn ok(self) -> bool {
        self == Self::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationStage {
    S0,
    S1,
    S2,
    S3,
    S4,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationStatus {
    pub schema: String,
    pub session_id: SessionId,
    pub stage: MigrationStage,
    pub source_generation_id: String,
    pub canonical_log_id: LogId,
    pub writer_id: String,
    pub generation_epoch: u64,
    #[serde(with = "u128_string")]
    pub fencing_token: u128,
    pub mapping_count: u64,
    pub canonical_count: u64,
    pub missing_canonical: u64,
    pub duplicate_canonical: u64,
    pub projection_mismatch: u64,
    pub seq_gap: u64,
    pub replay_live_mismatch: u64,
    pub content_ref_missing: u64,
    pub torn_tail_unresolved: u64,
    pub ok: bool,
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationState {
    pub schema: String,
    pub session_id: SessionId,
    pub stage: MigrationStage,
    pub source_generation_id: String,
    pub canonical_log_id: LogId,
    pub writer_id: String,
    pub generation_epoch: u64,
    #[serde(with = "u128_string")]
    pub fencing_token: u128,
}

#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("migration io error: {0}")]
    Io(#[from] io::Error),
    #[error("migration json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("canonical error: {0}")]
    Canonical(#[from] CanonicalError),
    #[error("invalid legacy mapping: {0}")]
    InvalidMapping(String),
    #[error("legacy mapping unique-key conflict")]
    MappingConflict,
    #[error("migration cutover gate is closed")]
    CutoverGate,
    #[error("automatic rollback is forbidden after S3")]
    RollbackForbidden,
    #[error("invalid migration stage transition: {0}")]
    InvalidStage(String),
    #[error("source generation is stale: {0}")]
    StaleGeneration(String),
    #[error("canonical sequence is invalid: {0}")]
    Validation(#[from] ValidationError),
}

#[derive(Debug, Clone)]
pub struct MigrationController {
    session_dir: PathBuf,
    state: MigrationState,
}

impl MigrationController {
    pub fn load_or_create(
        session_dir: impl AsRef<Path>,
        session_id: SessionId,
        log_id: LogId,
        source_generation_id: impl Into<String>,
    ) -> Result<Self, MigrationError> {
        let session_dir = session_dir.as_ref().to_path_buf();
        let source_generation_id = source_generation_id.into();
        let state_path = session_dir.join(MIGRATION_STATE_FILE);
        let state = match fs::read(&state_path) {
            Ok(bytes) => {
                let state: MigrationState = serde_json::from_slice(&bytes)?;
                if state.schema != MIGRATION_STATE_SCHEMA {
                    return Err(MigrationError::InvalidStage(format!(
                        "unexpected migration state schema {}",
                        state.schema
                    )));
                }
                if state.session_id != session_id || state.canonical_log_id != log_id {
                    return Err(MigrationError::InvalidStage(
                        "migration state identity mismatch".into(),
                    ));
                }
                if state.source_generation_id != source_generation_id {
                    return Err(MigrationError::StaleGeneration(state.source_generation_id));
                }
                state
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => MigrationState {
                schema: MIGRATION_STATE_SCHEMA.into(),
                session_id,
                stage: MigrationStage::S0,
                source_generation_id,
                canonical_log_id: log_id,
                writer_id: String::new(),
                generation_epoch: 0,
                fencing_token: 0,
            },
            Err(error) => return Err(error.into()),
        };
        Ok(Self { session_dir, state })
    }

    pub fn stage(&self) -> MigrationStage {
        self.state.stage
    }

    pub fn status(
        &self,
        metrics: ReconciliationMetrics,
        mapping_count: u64,
        canonical_count: u64,
    ) -> MigrationStatus {
        MigrationStatus {
            schema: MIGRATION_STATUS_SCHEMA.into(),
            session_id: self.state.session_id.clone(),
            stage: self.state.stage,
            source_generation_id: self.state.source_generation_id.clone(),
            canonical_log_id: self.state.canonical_log_id.clone(),
            writer_id: self.state.writer_id.clone(),
            generation_epoch: self.state.generation_epoch,
            fencing_token: self.state.fencing_token,
            mapping_count,
            canonical_count,
            missing_canonical: metrics.missing_canonical,
            duplicate_canonical: metrics.duplicate_canonical,
            projection_mismatch: metrics.projection_mismatch,
            seq_gap: metrics.seq_gap,
            replay_live_mismatch: metrics.replay_live_mismatch,
            content_ref_missing: metrics.content_ref_missing,
            torn_tail_unresolved: metrics.torn_tail_unresolved,
            ok: metrics.ok(),
            error_code: (!metrics.ok()).then(|| "E_RECONCILE_MISMATCH".into()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn cutover(
        &mut self,
        target: MigrationStage,
        writer_id: impl Into<String>,
        generation_epoch: u64,
        fencing_token: u128,
        metrics: ReconciliationMetrics,
        now_ms: i64,
        lease_duration_ms: i64,
    ) -> Result<MigrationStatus, MigrationError> {
        let _legacy_writer = LegacyWriterFacade::lock();
        if target != next_stage(self.state.stage) {
            return Err(MigrationError::InvalidStage(
                "cutover must advance exactly one stage".into(),
            ));
        }
        if target >= MigrationStage::S3 && !metrics.ok() {
            return Err(MigrationError::CutoverGate);
        }

        let writer_id = writer_id.into();
        if target == MigrationStage::S3 {
            let mut log = CanonicalLog::open(
                &self.session_dir,
                self.state.session_id.clone(),
                self.state.canonical_log_id.clone(),
            )?;
            log.rotate_writer_fence(
                WriterId::new(writer_id.clone()),
                generation_epoch,
                fencing_token,
                now_ms,
                lease_duration_ms,
            )?;
        }

        self.state.stage = target;
        self.state.writer_id = writer_id;
        self.state.generation_epoch = generation_epoch;
        self.state.fencing_token = fencing_token;
        self.persist()?;
        Ok(self.status(metrics, 0, 0))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn rollback(
        &mut self,
        target: MigrationStage,
        writer_id: impl Into<String>,
        generation_epoch: u64,
        fencing_token: u128,
        metrics: ReconciliationMetrics,
    ) -> Result<MigrationStatus, MigrationError> {
        let _legacy_writer = LegacyWriterFacade::lock();
        if self.state.stage >= MigrationStage::S3 {
            return Err(MigrationError::RollbackForbidden);
        }
        if target > MigrationStage::S2 || target >= self.state.stage {
            return Err(MigrationError::InvalidStage(
                "rollback target must be an earlier S0-S2 stage".into(),
            ));
        }
        self.state.stage = target;
        self.state.writer_id = writer_id.into();
        self.state.generation_epoch = generation_epoch;
        self.state.fencing_token = fencing_token;
        self.persist()?;
        Ok(self.status(metrics, 0, 0))
    }

    fn persist(&self) -> Result<(), MigrationError> {
        write_json_atomic(&self.session_dir.join(MIGRATION_STATE_FILE), &self.state)?;
        Ok(())
    }
}

pub fn append_legacy_mapping(
    session_dir: impl AsRef<Path>,
    mapping: &LegacyMapping,
) -> Result<(), MigrationError> {
    let _legacy_writer = LegacyWriterFacade::lock();
    mapping.validate()?;
    let path = session_dir.as_ref().join(LEGACY_MAPPING_FILE);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let existing = load_mapping_keys(session_dir.as_ref())?;
    let mut seen = existing;
    let mut lines = Vec::new();
    for target in &mapping.targets {
        if !seen.insert(target.key.clone()) {
            return Err(MigrationError::MappingConflict);
        }
        lines.push(serde_json::to_vec(&PersistedMappingTarget {
            legacy_source: mapping.legacy_source,
            source_generation_id: mapping.source_generation_id.clone(),
            legacy_key: mapping.legacy_key.clone(),
            legacy_msg_id: mapping.legacy_msg_id.clone(),
            session_id: mapping.session_id.clone(),
            log_id: mapping.log_id.clone(),
            target: target.clone(),
            source_hash: mapping.source_hash.clone(),
            mapped_at_ms: mapping.mapped_at_ms,
        })?);
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    for mut line in lines {
        line.push(b'\n');
        file.write_all(&line)?;
    }
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

pub fn load_mapping_keys(
    session_dir: impl AsRef<Path>,
) -> Result<HashSet<LegacyMappingKey>, MigrationError> {
    let path = session_dir.as_ref().join(LEGACY_MAPPING_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(error) => return Err(error.into()),
    };
    let mut keys = HashSet::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let record: PersistedMappingTarget = serde_json::from_slice(line)?;
        if !keys.insert(record.target.key) {
            return Err(MigrationError::MappingConflict);
        }
    }
    Ok(keys)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PersistedMappingTarget {
    legacy_source: LegacySource,
    source_generation_id: String,
    legacy_key: String,
    legacy_msg_id: Option<String>,
    session_id: SessionId,
    log_id: LogId,
    #[serde(flatten)]
    target: LegacyMappingTarget,
    source_hash: ContentHash,
    mapped_at_ms: i64,
}

fn validate_canonical_seq(seq: &CanonicalSeq) -> Result<(), MigrationError> {
    if seq.fact_seq == 0 || seq.fact_seq > MAX_SAFE_FACT_SEQ {
        return Err(MigrationError::InvalidMapping(
            "canonical_seq.fact_seq is out of range".into(),
        ));
    }
    if seq
        .projection_index
        .is_some_and(|index| index > crate::session_fact_v2::MAX_RELIABLE_PROJECTION_INDEX)
    {
        return Err(MigrationError::InvalidMapping(
            "canonical_seq.projection_index is out of range".into(),
        ));
    }
    Ok(())
}

fn next_stage(stage: MigrationStage) -> MigrationStage {
    match stage {
        MigrationStage::S0 => MigrationStage::S1,
        MigrationStage::S1 => MigrationStage::S2,
        MigrationStage::S2 => MigrationStage::S3,
        MigrationStage::S3 => MigrationStage::S4,
        MigrationStage::S4 => MigrationStage::S4,
    }
}

mod u128_string {
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &u128, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u128, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.is_empty() || (value.len() > 1 && value.starts_with('0')) {
            return Err(D::Error::custom(
                "fencing token must be canonical unsigned decimal",
            ));
        }
        value
            .parse::<u128>()
            .map_err(|_| D::Error::custom("fencing token overflow or invalid decimal"))
    }
}
