//! Read-only access to the committed prefix of a canonical session log.

use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
};

use super::{
    EVENTS_COMMIT_FILE, EVENTS_FILE, EVENTS_POISON_FILE,
    log::CanonicalError,
    types::{EVENTS_COMMIT_SCHEMA, EventsCommit},
};
use crate::session_fact_v2::{LogId, MAX_SAFE_FACT_SEQ, SessionFact, SessionId};

/// A snapshot reader for `events.jsonl` bounded by `events.commit.json`.
///
/// Opening validates the marker and committed prefix but never repairs,
/// truncates or locks the log. Every read uses the marker snapshot captured at
/// open time as its hard upper bound; bytes beyond `committed_offset` are
/// deliberately ignored.
#[derive(Debug)]
pub struct CommittedFactReader {
    session_dir: PathBuf,
    session_id: SessionId,
    log_id: LogId,
    commit: EventsCommit,
}

impl CommittedFactReader {
    pub fn open(
        session_dir: impl AsRef<Path>,
        session_id: SessionId,
        log_id: LogId,
    ) -> Result<Self, CanonicalError> {
        let session_dir = session_dir.as_ref().to_path_buf();
        let commit = read_commit_marker(&session_dir, &log_id)?;
        let reader = Self {
            session_dir,
            session_id,
            log_id,
            commit,
        };
        reader.read_committed()?;
        Ok(reader)
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn log_id(&self) -> &LogId {
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

    pub fn read_all(&self) -> Result<Vec<SessionFact>, CanonicalError> {
        self.read_committed()
    }

    /// Read an inclusive committed fact range.
    pub fn read_range(
        &self,
        start_fact_seq: u64,
        end_fact_seq: u64,
    ) -> Result<Vec<SessionFact>, CanonicalError> {
        if start_fact_seq == 0
            || start_fact_seq > end_fact_seq
            || end_fact_seq > self.commit.committed_fact_seq
        {
            return Err(CanonicalError::InvalidFactRange {
                start_fact_seq,
                end_fact_seq,
                committed_fact_seq: self.commit.committed_fact_seq,
            });
        }

        let facts = self.read_committed()?;
        Ok(facts
            .into_iter()
            .filter(|fact| fact.fact_seq >= start_fact_seq && fact.fact_seq <= end_fact_seq)
            .collect())
    }

    /// Read committed facts with `fact_seq` strictly greater than the cursor.
    pub fn read_after(&self, fact_seq: u64) -> Result<Vec<SessionFact>, CanonicalError> {
        if fact_seq >= self.commit.committed_fact_seq {
            return Ok(Vec::new());
        }
        self.read_range(fact_seq + 1, self.commit.committed_fact_seq)
    }

    fn read_committed(&self) -> Result<Vec<SessionFact>, CanonicalError> {
        let bytes = self.read_committed_bytes()?;
        let mut facts = Vec::new();
        let last_fact_seq = scan_committed_facts(&bytes, &self.session_id, &self.log_id, |fact| {
            facts.push(fact);
            Ok(())
        })?;
        let last_event_id = facts.last().map(|fact| fact.event_id.clone());

        if last_fact_seq != self.commit.committed_fact_seq
            || bytes.len() as u64 != self.commit.committed_offset
            || last_event_id != self.commit.last_barrier_event_id
        {
            return Err(CanonicalError::CommitRecoveryRequired(format!(
                "commit marker does not match committed prefix: marker=({}, {}, {:?}), prefix=({}, {}, {:?})",
                self.commit.committed_fact_seq,
                self.commit.committed_offset,
                self.commit.last_barrier_event_id,
                last_fact_seq,
                bytes.len(),
                last_event_id
            )));
        }
        Ok(facts)
    }

    fn read_committed_bytes(&self) -> Result<Vec<u8>, CanonicalError> {
        let file = match File::open(self.events_path()) {
            Ok(file) => file,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound && self.commit.committed_offset == 0 =>
            {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error.into()),
        };
        let file_len = file.metadata()?.len();
        if file_len < self.commit.committed_offset {
            return Err(CanonicalError::CommitRecoveryRequired(format!(
                "events.jsonl is shorter than committed offset: {file_len} < {}",
                self.commit.committed_offset
            )));
        }

        let mut bytes = Vec::new();
        file.take(self.commit.committed_offset)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != self.commit.committed_offset {
            return Err(CanonicalError::CommitRecoveryRequired(
                "events.jsonl changed while reading committed prefix".into(),
            ));
        }
        Ok(bytes)
    }
}

fn read_commit_marker(session_dir: &Path, log_id: &LogId) -> Result<EventsCommit, CanonicalError> {
    if session_dir.join(EVENTS_POISON_FILE).exists() {
        return Err(CanonicalError::CommitRecoveryRequired(
            "events poison marker is present".into(),
        ));
    }

    let commit_path = session_dir.join(EVENTS_COMMIT_FILE);
    let bytes = match fs::read(&commit_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(CanonicalError::CommitRecoveryRequired(
                "commit marker is missing".into(),
            ));
        }
        Err(error) => return Err(error.into()),
    };
    let marker: EventsCommit = serde_json::from_slice(&bytes).map_err(|error| {
        CanonicalError::CommitRecoveryRequired(format!("commit marker is corrupt: {error}"))
    })?;
    if marker.schema != EVENTS_COMMIT_SCHEMA {
        return Err(CanonicalError::CommitRecoveryRequired(format!(
            "unexpected commit marker schema {}",
            marker.schema
        )));
    }
    if &marker.log_id != log_id {
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

pub(super) fn scan_committed_facts(
    bytes: &[u8],
    session_id: &SessionId,
    log_id: &LogId,
    mut visit: impl FnMut(SessionFact) -> Result<(), CanonicalError>,
) -> Result<u64, CanonicalError> {
    if bytes.is_empty() {
        return Ok(0);
    }
    if bytes.last().copied() != Some(b'\n') {
        return Err(CanonicalError::CommitRecoveryRequired(
            "canonical log has a torn tail".into(),
        ));
    }

    let mut position = 0usize;
    let mut expected_seq = 1u64;
    while position < bytes.len() {
        let remaining = &bytes[position..];
        let newline = remaining
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(|| {
                CanonicalError::CommitRecoveryRequired(
                    "committed prefix contains a torn JSONL line".into(),
                )
            })?;
        let line = &remaining[..newline];
        if line.is_empty() {
            return Err(CanonicalError::CommitRecoveryRequired(
                "committed prefix contains an empty JSONL line".into(),
            ));
        }
        let fact: SessionFact = serde_json::from_slice(line).map_err(|error| {
            CanonicalError::CommitRecoveryRequired(format!(
                "committed fact at offset {position} is invalid: {error}"
            ))
        })?;
        if &fact.session_id != session_id {
            return Err(CanonicalError::IdentityMismatch {
                field: "session_id",
            });
        }
        if &fact.log_id != log_id {
            return Err(CanonicalError::IdentityMismatch { field: "log_id" });
        }
        fact.validate().map_err(|error| {
            CanonicalError::CommitRecoveryRequired(format!(
                "committed fact at offset {position} failed validation: {error}"
            ))
        })?;
        if fact.fact_seq != expected_seq {
            return Err(CanonicalError::CommitRecoveryRequired(format!(
                "committed fact_seq gap: expected {expected_seq}, got {}",
                fact.fact_seq
            )));
        }
        visit(fact)?;
        expected_seq = expected_seq
            .checked_add(1)
            .ok_or(CanonicalError::FactSeqExhausted)?;
        position = position
            .checked_add(newline + 1)
            .ok_or(CanonicalError::FactSeqExhausted)?;
    }

    Ok(expected_seq - 1)
}
