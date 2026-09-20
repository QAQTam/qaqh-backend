//! Canonical `events.jsonl` writer and commit high-water.
//!
//! This is the storage slice only. It owns writer fencing, JSONL append/fsync
//! and the `events.commit.json` barrier; it does not own SessionActor state,
//! projections or recovery-plan execution.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::session_fact_v2::{EventId, MAX_SAFE_FACT_SEQ, SessionFact, ValidationError};

use super::reader::scan_committed_facts;
use super::types::{
    AppendRejected, EVENTS_COMMIT_SCHEMA, EventsCommit, UPGRADE_FENCE_SCHEMA, UpgradeFence,
    UpgradeState, WRITER_FENCE_SCHEMA, WriterFence, WriterId, WriterLease,
};

pub const EVENTS_FILE: &str = "events.jsonl";
pub const EVENTS_LOCK_FILE: &str = "events.lock";
pub const WRITER_FENCE_FILE: &str = "writer-fence.json";
pub const EVENTS_COMMIT_FILE: &str = "events.commit.json";
pub const EVENTS_POISON_FILE: &str = "events.poison.json";
pub const UPGRADE_FENCE_FILE: &str = "upgrade-fence.json";

#[derive(Debug, Error)]
pub enum CanonicalError {
    #[error("canonical io error: {0}")]
    Io(#[from] io::Error),

    #[error("canonical json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("canonical fact validation failed: {0}")]
    Validation(#[from] ValidationError),

    #[error("writer {writer_id} already owns the fence until {lease_expires_at_ms}")]
    WriterBusy {
        writer_id: WriterId,
        lease_expires_at_ms: i64,
    },

    #[error("writer fence is invalid: {0}")]
    InvalidFence(String),

    #[error("canonical identity mismatch: {field}")]
    IdentityMismatch { field: &'static str },

    #[error("{0}")]
    StaleWriter(#[from] AppendRejected),

    #[error("commit recovery required: {0}")]
    CommitRecoveryRequired(String),

    #[error("canonical log is not writable: {0}")]
    NotWritable(String),

    #[error("recovery intent is invalid: {0}")]
    InvalidRecoveryIntent(String),

    #[error("recovery intent conflict: {0}")]
    RecoveryIntentConflict(String),

    #[error("writer lease duration must be positive, got {0}")]
    InvalidLeaseDuration(i64),

    #[error("canonical fact sequence is exhausted")]
    FactSeqExhausted,

    #[error("writer fence token is exhausted")]
    FenceTokenExhausted,

    #[error(
        "invalid canonical fact range {start_fact_seq}..={end_fact_seq} for committed high-water {committed_fact_seq}"
    )]
    InvalidFactRange {
        start_fact_seq: u64,
        end_fact_seq: u64,
        committed_fact_seq: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LogState {
    Writable,
    CommitRecoveryRequired(String),
    ReadOnlyUpgradeRequired(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommittedPrefix {
    fact_seq: u64,
    offset: u64,
    last_event_id: Option<EventId>,
}

/// One session's canonical log. The struct does not hold the OS lock between
/// operations; every fence/append/commit mutation reacquires `events.lock` and
/// validates the persisted fence inside that critical section.
#[derive(Debug)]
pub struct CanonicalLog {
    session_dir: PathBuf,
    session_id: crate::session_fact_v2::SessionId,
    log_id: crate::session_fact_v2::LogId,
    commit: EventsCommit,
    state: LogState,
}

impl CanonicalLog {
    pub fn open(
        session_dir: impl AsRef<Path>,
        session_id: crate::session_fact_v2::SessionId,
        log_id: crate::session_fact_v2::LogId,
    ) -> Result<Self, CanonicalError> {
        let session_dir = session_dir.as_ref().to_path_buf();
        fs::create_dir_all(&session_dir)?;
        let mut log = Self {
            session_dir,
            session_id,
            log_id,
            commit: EventsCommit::empty(crate::session_fact_v2::LogId::new("")),
            state: LogState::Writable,
        };
        let _guard = log.lock_exclusive()?;
        log.commit = log.load_or_initialize_commit()?;
        log.reconcile_events_with_commit()?;
        log.validate_committed_prefix()?;
        if let Some(fence) = log.read_upgrade_fence()?
            && fence.state == UpgradeState::ReadOnly
        {
            log.state = LogState::ReadOnlyUpgradeRequired(format!(
                "upgrade fence generation {} requires read-only",
                fence.generation
            ));
        }
        Ok(log)
    }

    pub fn session_id(&self) -> &crate::session_fact_v2::SessionId {
        &self.session_id
    }

    pub fn log_id(&self) -> &crate::session_fact_v2::LogId {
        &self.log_id
    }

    pub fn committed(&self) -> &EventsCommit {
        &self.commit
    }

    pub fn events_path(&self) -> PathBuf {
        self.session_dir.join(EVENTS_FILE)
    }

    pub fn commit_path(&self) -> PathBuf {
        self.session_dir.join(EVENTS_COMMIT_FILE)
    }

    pub fn poison_path(&self) -> PathBuf {
        self.session_dir.join(EVENTS_POISON_FILE)
    }

    pub fn fence_path(&self) -> PathBuf {
        self.session_dir.join(WRITER_FENCE_FILE)
    }

    pub fn upgrade_fence_path(&self) -> PathBuf {
        self.session_dir.join(UPGRADE_FENCE_FILE)
    }

    pub fn writer_fence(&self) -> Result<Option<WriterFence>, CanonicalError> {
        let _guard = self.lock_exclusive()?;
        self.read_fence()
    }

    pub fn upgrade_fence(&self) -> Result<Option<UpgradeFence>, CanonicalError> {
        let _guard = self.lock_exclusive()?;
        self.read_upgrade_fence()
    }

    pub fn acquire_writer(
        &mut self,
        writer_id: WriterId,
        now_ms: i64,
        lease_duration_ms: i64,
    ) -> Result<WriterLease, CanonicalError> {
        if lease_duration_ms <= 0 {
            return Err(CanonicalError::InvalidLeaseDuration(lease_duration_ms));
        }
        let _guard = self.lock_exclusive()?;
        self.ensure_writable()?;

        let current = self.read_fence()?;
        if let Some(fence) = &current {
            self.validate_fence_identity(fence)?;
            if fence.lease_expires_at_ms > now_ms {
                return Err(CanonicalError::WriterBusy {
                    writer_id: fence.writer_id.clone(),
                    lease_expires_at_ms: fence.lease_expires_at_ms,
                });
            }
        }

        let (generation_epoch, fencing_token) = match current {
            Some(fence) => (
                fence
                    .generation_epoch
                    .checked_add(1)
                    .ok_or(CanonicalError::FenceTokenExhausted)?,
                fence
                    .fencing_token
                    .checked_add(1)
                    .ok_or(CanonicalError::FenceTokenExhausted)?,
            ),
            None => (1, 1),
        };
        let lease_expires_at_ms = now_ms.saturating_add(lease_duration_ms);
        let fence = WriterFence {
            schema: WRITER_FENCE_SCHEMA.into(),
            session_id: self.session_id.clone(),
            log_id: self.log_id.clone(),
            writer_id: writer_id.clone(),
            generation_epoch,
            fencing_token,
            acquired_at_ms: now_ms,
            lease_expires_at_ms,
        };
        write_json_atomic(&self.fence_path(), &fence)?;
        Ok(WriterLease {
            writer_id,
            log_id: self.log_id.clone(),
            generation_epoch,
            fencing_token,
            lease_expires_at_ms,
        })
    }

    pub fn renew_writer(
        &mut self,
        lease: &WriterLease,
        now_ms: i64,
        lease_duration_ms: i64,
    ) -> Result<WriterLease, CanonicalError> {
        if lease_duration_ms <= 0 {
            return Err(CanonicalError::InvalidLeaseDuration(lease_duration_ms));
        }
        let _guard = self.lock_exclusive()?;
        self.ensure_writable()?;
        let mut fence = self.required_fence()?;
        self.verify_lease(&fence, lease, now_ms)?;
        fence.lease_expires_at_ms = now_ms.saturating_add(lease_duration_ms);
        write_json_atomic(&self.fence_path(), &fence)?;
        Ok(WriterLease {
            writer_id: fence.writer_id,
            log_id: fence.log_id,
            generation_epoch: fence.generation_epoch,
            fencing_token: fence.fencing_token,
            lease_expires_at_ms: fence.lease_expires_at_ms,
        })
    }

    /// Append one fact and advance the durable commit marker.
    ///
    /// `fact.fact_seq` is assigned inside the writer critical section; callers
    /// must provide the remaining envelope fields and the typed payload.
    pub fn append(
        &mut self,
        lease: &WriterLease,
        mut fact: SessionFact,
        now_ms: i64,
    ) -> Result<SessionFact, CanonicalError> {
        let _guard = self.lock_exclusive()?;
        self.ensure_writable()?;
        let fence = self.required_fence()?;
        self.verify_lease(&fence, lease, now_ms)?;
        self.verify_fact_identity(&fact)?;

        let next_fact_seq = self
            .commit
            .committed_fact_seq
            .checked_add(1)
            .filter(|seq| *seq <= MAX_SAFE_FACT_SEQ)
            .ok_or(CanonicalError::FactSeqExhausted)?;
        fact.fact_seq = next_fact_seq;
        fact.validate()?;

        let mut encoded = serde_json::to_vec(&fact)?;
        encoded.push(b'\n');

        let mut events = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(self.events_path())?;
        let committed_offset = events.metadata()?.len();
        if committed_offset != self.commit.committed_offset {
            return Err(CanonicalError::CommitRecoveryRequired(format!(
                "events.jsonl length {committed_offset} does not match committed offset {}",
                self.commit.committed_offset
            )));
        }
        let next_offset = committed_offset
            .checked_add(encoded.len() as u64)
            .ok_or(CanonicalError::FactSeqExhausted)?;
        let next_commit = EventsCommit {
            schema: EVENTS_COMMIT_SCHEMA.into(),
            log_id: self.log_id.clone(),
            committed_fact_seq: next_fact_seq,
            committed_offset: next_offset,
            last_barrier_event_id: Some(fact.event_id.clone()),
            commit_generation: self
                .commit
                .commit_generation
                .checked_add(1)
                .ok_or(CanonicalError::FactSeqExhausted)?,
        };
        if let Err(error) = events
            .write_all(&encoded)
            .and_then(|()| events.flush())
            .and_then(|()| events.sync_all())
        {
            let message = format!("canonical append barrier failed: {error}");
            self.state = LogState::CommitRecoveryRequired(message.clone());
            return Err(CanonicalError::CommitRecoveryRequired(message));
        }
        if let Err(error) = write_json_atomic(&self.commit_path(), &next_commit) {
            let message = error.to_string();
            self.state = LogState::CommitRecoveryRequired(message.clone());
            return Err(CanonicalError::CommitRecoveryRequired(message));
        }
        self.commit = next_commit;
        Ok(fact)
    }

    fn ensure_writable(&self) -> Result<(), CanonicalError> {
        match &self.state {
            LogState::Writable => Ok(()),
            LogState::CommitRecoveryRequired(reason) => {
                Err(CanonicalError::NotWritable(reason.clone()))
            }
            LogState::ReadOnlyUpgradeRequired(reason) => {
                Err(CanonicalError::NotWritable(reason.clone()))
            }
        }
    }

    fn verify_lease(
        &self,
        fence: &WriterFence,
        lease: &WriterLease,
        now_ms: i64,
    ) -> Result<(), CanonicalError> {
        self.validate_fence_identity(fence)?;
        if fence.log_id != lease.log_id
            || lease.log_id != self.log_id
            || fence.writer_id != lease.writer_id
            || fence.generation_epoch != lease.generation_epoch
            || fence.fencing_token != lease.fencing_token
            || fence.lease_expires_at_ms <= now_ms
        {
            return Err(AppendRejected::stale_writer(
                fence.fencing_token,
                lease.fencing_token,
                fence.generation_epoch,
            )
            .into());
        }
        Ok(())
    }

    fn verify_fact_identity(&self, fact: &SessionFact) -> Result<(), CanonicalError> {
        if fact.session_id != self.session_id {
            return Err(CanonicalError::IdentityMismatch {
                field: "session_id",
            });
        }
        if fact.log_id != self.log_id {
            return Err(CanonicalError::IdentityMismatch { field: "log_id" });
        }
        Ok(())
    }

    fn validate_fence_identity(&self, fence: &WriterFence) -> Result<(), CanonicalError> {
        if fence.schema != WRITER_FENCE_SCHEMA {
            return Err(CanonicalError::InvalidFence(format!(
                "unexpected schema {}",
                fence.schema
            )));
        }
        if fence.session_id != self.session_id {
            return Err(CanonicalError::IdentityMismatch {
                field: "writer_fence.session_id",
            });
        }
        if fence.log_id != self.log_id {
            return Err(CanonicalError::IdentityMismatch {
                field: "writer_fence.log_id",
            });
        }
        Ok(())
    }

    fn required_fence(&self) -> Result<WriterFence, CanonicalError> {
        self.read_fence()?
            .ok_or_else(|| CanonicalError::InvalidFence("writer fence is missing".into()))
    }

    fn read_fence(&self) -> Result<Option<WriterFence>, CanonicalError> {
        match read_json(&self.fence_path()) {
            Ok(fence) => Ok(fence),
            Err(CanonicalError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn read_upgrade_fence(&self) -> Result<Option<UpgradeFence>, CanonicalError> {
        let Some(fence) = read_json::<UpgradeFence>(&self.upgrade_fence_path())? else {
            return Ok(None);
        };
        if fence.schema != UPGRADE_FENCE_SCHEMA {
            return Err(CanonicalError::InvalidFence(format!(
                "unexpected upgrade fence schema {}",
                fence.schema
            )));
        }
        if fence.log_id != self.log_id {
            return Err(CanonicalError::IdentityMismatch {
                field: "upgrade_fence.log_id",
            });
        }
        Ok(Some(fence))
    }

    fn load_or_initialize_commit(&self) -> Result<EventsCommit, CanonicalError> {
        if self.poison_path().exists() {
            return Err(CanonicalError::CommitRecoveryRequired(
                "events poison marker is present".into(),
            ));
        }
        match read_json::<EventsCommit>(&self.commit_path()) {
            Ok(Some(marker)) => {
                if marker.schema != EVENTS_COMMIT_SCHEMA {
                    return Err(CanonicalError::CommitRecoveryRequired(format!(
                        "unexpected commit marker schema {}",
                        marker.schema
                    )));
                }
                if marker.log_id != self.log_id {
                    return Err(CanonicalError::CommitRecoveryRequired(
                        "commit marker log_id does not match canonical log".into(),
                    ));
                }
                if marker.committed_fact_seq > MAX_SAFE_FACT_SEQ {
                    return Err(CanonicalError::CommitRecoveryRequired(
                        "commit marker fact_seq exceeds safe integer range".into(),
                    ));
                }
                Ok(marker)
            }
            Ok(None) => self.rebuild_missing_commit_marker(),
            Err(CanonicalError::Json(error)) => Err(CanonicalError::CommitRecoveryRequired(
                format!("commit marker is corrupt: {error}"),
            )),
            Err(error) => Err(error),
        }
    }

    fn rebuild_missing_commit_marker(&self) -> Result<EventsCommit, CanonicalError> {
        let bytes = match fs::read(self.events_path()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        let prefix = self.scan_complete_facts(&bytes)?;
        let marker = EventsCommit {
            schema: EVENTS_COMMIT_SCHEMA.into(),
            log_id: self.log_id.clone(),
            committed_fact_seq: prefix.fact_seq,
            committed_offset: prefix.offset,
            last_barrier_event_id: prefix.last_event_id,
            commit_generation: 0,
        };
        write_json_atomic(&self.commit_path(), &marker)?;
        log::warn!(
            "[qaqh-session] rebuilt missing commit marker at fact_seq={} offset={}",
            marker.committed_fact_seq,
            marker.committed_offset
        );
        Ok(marker)
    }

    fn reconcile_events_with_commit(&self) -> Result<(), CanonicalError> {
        let events_path = self.events_path();
        let events = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&events_path)?;
        let len = events.metadata()?.len();
        if len < self.commit.committed_offset {
            return Err(CanonicalError::CommitRecoveryRequired(format!(
                "events.jsonl is shorter than committed offset: {len} < {}",
                self.commit.committed_offset
            )));
        }
        if len > self.commit.committed_offset {
            events.set_len(self.commit.committed_offset)?;
            events.sync_all()?;
            log::warn!(
                "[qaqh-session] canonical log truncated uncommitted suffix: {} -> {}",
                len,
                self.commit.committed_offset
            );
        }
        Ok(())
    }

    fn validate_committed_prefix(&self) -> Result<(), CanonicalError> {
        let committed_offset = self.commit.committed_offset;
        let file = File::open(self.events_path())?;
        let mut bytes = Vec::new();
        file.take(committed_offset).read_to_end(&mut bytes)?;
        if bytes.len() as u64 != committed_offset {
            return Err(CanonicalError::CommitRecoveryRequired(
                "events.jsonl changed while validating committed prefix".into(),
            ));
        }
        let prefix = self.scan_complete_facts(&bytes)?;
        if prefix.fact_seq != self.commit.committed_fact_seq
            || prefix.offset != self.commit.committed_offset
            || prefix.last_event_id != self.commit.last_barrier_event_id
        {
            return Err(CanonicalError::CommitRecoveryRequired(format!(
                "commit marker does not match committed prefix: marker=({}, {}, {:?}), prefix=({}, {}, {:?})",
                self.commit.committed_fact_seq,
                self.commit.committed_offset,
                self.commit.last_barrier_event_id,
                prefix.fact_seq,
                prefix.offset,
                prefix.last_event_id
            )));
        }
        Ok(())
    }

    fn scan_complete_facts(&self, bytes: &[u8]) -> Result<CommittedPrefix, CanonicalError> {
        let mut last_event_id = None;
        let fact_seq = scan_committed_facts(bytes, &self.session_id, &self.log_id, |fact| {
            last_event_id = Some(fact.event_id.clone());
            Ok(())
        })?;

        Ok(CommittedPrefix {
            fact_seq,
            offset: bytes.len() as u64,
            last_event_id,
        })
    }

    fn lock_exclusive(&self) -> Result<File, CanonicalError> {
        let path = self.session_dir.join(EVENTS_LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        file.lock()?;
        Ok(file)
    }
}

fn read_json<T>(path: &Path) -> Result<Option<T>, CanonicalError>
where
    T: DeserializeOwned,
{
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn write_json_atomic<T>(path: &Path, value: &T) -> Result<(), CanonicalError>
where
    T: Serialize,
{
    let parent = path.parent().ok_or_else(|| {
        CanonicalError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "canonical sidecar has no parent",
        ))
    })?;
    fs::create_dir_all(parent)?;
    let temp = path.with_extension("tmp");
    let bytes = serde_json::to_vec_pretty(value)?;
    {
        let mut file = File::create(&temp)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()?;
    }
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error.into());
    }
    sync_parent_dir(parent)?;
    Ok(())
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
