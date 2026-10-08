//! Durable recovery intent and batch identity.
//!
//! This module owns the `recovery.intent.json` storage contract. Recovery
//! execution and plan construction remain separate so the intent can be made
//! durable before any canonical recovery fact is appended.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{CanonicalError, log::write_json_atomic};
use crate::session_fact_v2::{
    ContentHash, EventId, FactPayload, LogId, MAX_SAFE_FACT_SEQ, RecoveryId, RecoveryRef,
    SessionFact,
};

pub const RECOVERY_INTENT_FILE: &str = "recovery.intent.json";
pub const RECOVERY_INTENT_SCHEMA: &str = "qaqh.recovery-intent/v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryIntent {
    pub schema: String,
    pub recovery_ref: RecoveryRef,
    pub log_id: LogId,
    pub last_good_fact_seq: u64,
    pub sorted_open_ids: Vec<String>,
    pub torn_tail_bytes_hash: ContentHash,
    pub child_terminal_digest: ContentHash,
    pub plan_hash: ContentHash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryBatchKey {
    pub log_id: LogId,
    pub recovery_input_fingerprint: ContentHash,
    pub last_good_fact_seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryIntentStatus {
    Active,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryIntentWriteOutcome {
    Written,
    Reused,
}

impl RecoveryIntentStatus {
    pub fn is_active(self) -> bool {
        self == Self::Active
    }

    pub fn is_stale(self) -> bool {
        self == Self::Stale
    }
}

impl RecoveryIntent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        recovery_id: RecoveryId,
        recovery_event_id: EventId,
        log_id: LogId,
        last_good_fact_seq: u64,
        mut sorted_open_ids: Vec<String>,
        torn_tail_bytes_hash: ContentHash,
        child_terminal_digest: ContentHash,
        plan_hash: ContentHash,
    ) -> Result<Self, CanonicalError> {
        sorted_open_ids.sort();
        sorted_open_ids.dedup();
        let recovery_input_fingerprint = recovery_input_fingerprint(
            &log_id,
            last_good_fact_seq,
            &sorted_open_ids,
            &torn_tail_bytes_hash,
            &child_terminal_digest,
        )?;
        let intent = Self {
            schema: RECOVERY_INTENT_SCHEMA.into(),
            recovery_ref: RecoveryRef {
                recovery_id,
                recovery_event_id,
                recovery_input_fingerprint,
            },
            log_id,
            last_good_fact_seq,
            sorted_open_ids,
            torn_tail_bytes_hash,
            child_terminal_digest,
            plan_hash,
        };
        intent.validate()?;
        Ok(intent)
    }

    pub fn validate(&self) -> Result<(), CanonicalError> {
        if self.schema != RECOVERY_INTENT_SCHEMA {
            return Err(invalid_recovery_intent(format!(
                "unexpected schema {}",
                self.schema
            )));
        }
        if self.log_id.as_str().is_empty() {
            return Err(invalid_recovery_intent("log_id must not be empty"));
        }
        if self.last_good_fact_seq > MAX_SAFE_FACT_SEQ {
            return Err(invalid_recovery_intent(
                "last_good_fact_seq exceeds safe integer range",
            ));
        }
        if self.recovery_ref.recovery_id.as_str().is_empty() {
            return Err(invalid_recovery_intent("recovery_id must not be empty"));
        }
        if self.recovery_ref.recovery_event_id.as_str().is_empty() {
            return Err(invalid_recovery_intent(
                "recovery_event_id must not be empty",
            ));
        }
        validate_sorted_open_ids(&self.sorted_open_ids)?;
        validate_content_hash(
            "recovery_ref.recovery_input_fingerprint",
            &self.recovery_ref.recovery_input_fingerprint,
        )?;
        validate_content_hash("torn_tail_bytes_hash", &self.torn_tail_bytes_hash)?;
        validate_content_hash("child_terminal_digest", &self.child_terminal_digest)?;
        validate_content_hash("plan_hash", &self.plan_hash)?;

        let expected = recovery_input_fingerprint(
            &self.log_id,
            self.last_good_fact_seq,
            &self.sorted_open_ids,
            &self.torn_tail_bytes_hash,
            &self.child_terminal_digest,
        )?;
        if expected != self.recovery_ref.recovery_input_fingerprint {
            return Err(invalid_recovery_intent(
                "recovery_input_fingerprint does not match the intent input",
            ));
        }
        Ok(())
    }

    pub fn batch_key(&self) -> RecoveryBatchKey {
        RecoveryBatchKey {
            log_id: self.log_id.clone(),
            recovery_input_fingerprint: self.recovery_ref.recovery_input_fingerprint.clone(),
            last_good_fact_seq: self.last_good_fact_seq,
        }
    }

    pub fn status(
        &self,
        committed_facts: &[SessionFact],
    ) -> Result<RecoveryIntentStatus, CanonicalError> {
        self.validate()?;
        let mut stale = false;
        for fact in committed_facts {
            if fact.log_id != self.log_id {
                continue;
            }
            let FactPayload::SessionRecovered(recovered) = &fact.payload else {
                continue;
            };
            let same_event = recovered.recovery_event_id == self.recovery_ref.recovery_event_id;
            let same_batch = recovered.recovery_input_fingerprint
                == self.recovery_ref.recovery_input_fingerprint
                && recovered.last_good_fact_seq == self.last_good_fact_seq;
            if same_event {
                if recovered.recovery_id != self.recovery_ref.recovery_id || !same_batch {
                    return Err(CanonicalError::RecoveryIntentConflict(
                        "SessionRecovered identity does not match recovery intent".into(),
                    ));
                }
                stale = true;
            } else if same_batch {
                return Err(CanonicalError::RecoveryIntentConflict(
                    "same recovery batch key has a different recovery identity".into(),
                ));
            }
        }
        Ok(if stale {
            RecoveryIntentStatus::Stale
        } else {
            RecoveryIntentStatus::Active
        })
    }
}

pub fn recovery_input_fingerprint(
    log_id: &LogId,
    last_good_fact_seq: u64,
    sorted_open_ids: &[String],
    torn_tail_bytes_hash: &ContentHash,
    child_terminal_digest: &ContentHash,
) -> Result<ContentHash, CanonicalError> {
    if last_good_fact_seq > MAX_SAFE_FACT_SEQ {
        return Err(invalid_recovery_intent(
            "last_good_fact_seq exceeds safe integer range",
        ));
    }
    validate_sorted_open_ids(sorted_open_ids)?;
    validate_content_hash("torn_tail_bytes_hash", torn_tail_bytes_hash)?;
    validate_content_hash("child_terminal_digest", child_terminal_digest)?;

    #[derive(Serialize)]
    struct FingerprintInput<'a> {
        log_id: &'a LogId,
        last_good_fact_seq: u64,
        sorted_open_ids: &'a [String],
        torn_tail_bytes_hash: &'a ContentHash,
        child_terminal_digest: &'a ContentHash,
    }

    let bytes = serde_json::to_vec(&FingerprintInput {
        log_id,
        last_good_fact_seq,
        sorted_open_ids,
        torn_tail_bytes_hash,
        child_terminal_digest,
    })?;
    Ok(sha256_content_hash(&bytes))
}

pub fn sha256_content_hash(bytes: &[u8]) -> ContentHash {
    ContentHash::new(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

pub fn recovery_intent_path(session_dir: impl AsRef<Path>) -> PathBuf {
    session_dir.as_ref().join(RECOVERY_INTENT_FILE)
}

pub fn load_recovery_intent(
    session_dir: impl AsRef<Path>,
) -> Result<Option<RecoveryIntent>, CanonicalError> {
    let path = recovery_intent_path(session_dir);
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let intent: RecoveryIntent = serde_json::from_slice(&bytes)?;
    intent.validate()?;
    Ok(Some(intent))
}

/// Atomically persist the intent before any canonical recovery mutation.
///
/// An active intent must be reused verbatim. A stale intent may be replaced
/// only by a different batch; reusing a batch key with a new identity or plan
/// is a conflict.
pub fn persist_recovery_intent(
    session_dir: impl AsRef<Path>,
    intent: &RecoveryIntent,
    committed_facts: &[SessionFact],
) -> Result<RecoveryIntentWriteOutcome, CanonicalError> {
    let session_dir = session_dir.as_ref();
    intent.validate()?;
    if let Some(existing) = load_recovery_intent(session_dir)? {
        if existing == *intent {
            return Ok(RecoveryIntentWriteOutcome::Reused);
        }
        if existing.status(committed_facts)?.is_active() {
            return Err(CanonicalError::RecoveryIntentConflict(
                "active recovery intent must be reused verbatim".into(),
            ));
        }
        if existing.batch_key() == intent.batch_key() {
            return Err(CanonicalError::RecoveryIntentConflict(
                "stale recovery intent cannot change identity or plan for the same batch".into(),
            ));
        }
        if existing.recovery_ref.recovery_id == intent.recovery_ref.recovery_id
            || existing.recovery_ref.recovery_event_id == intent.recovery_ref.recovery_event_id
        {
            return Err(CanonicalError::RecoveryIntentConflict(
                "recovery identity cannot be reused for a different batch".into(),
            ));
        }
    }
    write_json_atomic(&recovery_intent_path(session_dir), intent)?;
    Ok(RecoveryIntentWriteOutcome::Written)
}

/// Remove a stale intent only after its matching `SessionRecovered` is durable.
///
/// Returns `true` when an existing intent was removed and `false` when the
/// intent is active or absent.
pub fn remove_recovery_intent_if_stale(
    session_dir: impl AsRef<Path>,
    expected_intent: &RecoveryIntent,
    committed_facts: &[SessionFact],
) -> Result<bool, CanonicalError> {
    let session_dir = session_dir.as_ref();
    let Some(intent) = load_recovery_intent(session_dir)? else {
        return Ok(false);
    };
    if intent != *expected_intent {
        return Ok(false);
    }
    if intent.status(committed_facts)?.is_active() {
        return Ok(false);
    }
    let path = recovery_intent_path(session_dir);
    fs::remove_file(&path)?;
    sync_parent_dir(
        path.parent()
            .ok_or_else(|| invalid_recovery_intent("recovery intent has no parent"))?,
    )?;
    Ok(true)
}

fn validate_sorted_open_ids(open_ids: &[String]) -> Result<(), CanonicalError> {
    let mut previous: Option<&str> = None;
    for open_id in open_ids {
        if open_id.is_empty() {
            return Err(invalid_recovery_intent(
                "sorted_open_ids must not contain empty ids",
            ));
        }
        if previous.is_some_and(|previous| previous >= open_id.as_str()) {
            return Err(invalid_recovery_intent(
                "sorted_open_ids must be strictly sorted and unique",
            ));
        }
        previous = Some(open_id);
    }
    Ok(())
}

fn validate_content_hash(field: &str, value: &ContentHash) -> Result<(), CanonicalError> {
    let Some(hash) = value.as_str().strip_prefix("sha256:") else {
        return Err(invalid_recovery_intent(format!(
            "{field} must use sha256:<64 lowercase hex>"
        )));
    };
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(invalid_recovery_intent(format!(
            "{field} must use sha256:<64 lowercase hex>"
        )));
    }
    Ok(())
}

fn invalid_recovery_intent(message: impl Into<String>) -> CanonicalError {
    CanonicalError::InvalidRecoveryIntent(message.into())
}

#[cfg(unix)]
fn sync_parent_dir(parent: &Path) -> Result<(), CanonicalError> {
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_dir(parent: &Path) -> Result<(), CanonicalError> {
    let _ = parent;
    Ok(())
}
