//! Read-only access to the committed prefix of a canonical session log.

use std::{
    fs::{self, File},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use super::{
    EVENTS_COMMIT_FILE, EVENTS_FILE, EVENTS_POISON_FILE,
    log::CanonicalError,
    offset_index,
    types::{EVENTS_COMMIT_SCHEMA, EventsCommit},
};
use crate::session_fact_v2::{EventId, LogId, MAX_SAFE_FACT_SEQ, SessionFact, SessionId};
use sha2::{Digest, Sha256};

/// Observability counter: number of `SessionFact` JSON parses performed by the
/// `CommittedFactReader` scan paths. Each parse validates one committed fact;
/// the B2.1 phase-2 regression (`warm_open_skips_reparsing_indexed_prefix`)
/// uses it to prove a warm open no longer re-parses the indexed prefix.
/// Diagnostic hook only; not part of any wire contract.
pub static COMMITTED_FACT_PARSE_COUNT: AtomicU64 = AtomicU64::new(0);

fn count_parse() {
    COMMITTED_FACT_PARSE_COUNT.fetch_add(1, Ordering::Relaxed);
}

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
        // B2.1 phase2: validation-only open. A warm, exact-match index lets
        // this skip re-parsing the committed prefix (digest + tail only);
        // otherwise it falls back to the full validating scan.
        let session_dir = session_dir.as_ref().to_path_buf();
        let commit = read_commit_marker(&session_dir, &log_id)?;
        let reader = Self {
            session_dir,
            session_id,
            log_id,
            commit,
        };
        reader.validate_committed()?;
        Ok(reader)
    }

    pub(super) fn open_with_visitor(
        session_dir: impl AsRef<Path>,
        session_id: SessionId,
        log_id: LogId,
        visit: impl FnMut(SessionFact) -> Result<(), CanonicalError>,
    ) -> Result<Self, CanonicalError> {
        let session_dir = session_dir.as_ref().to_path_buf();
        let commit = read_commit_marker(&session_dir, &log_id)?;
        let reader = Self {
            session_dir,
            session_id,
            log_id,
            commit,
        };
        reader.visit_committed(visit)?;
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

        let mut facts = Vec::new();
        self.visit_committed_seek(start_fact_seq, |fact| {
            if fact.fact_seq >= start_fact_seq && fact.fact_seq <= end_fact_seq {
                facts.push(fact);
            }
            Ok(())
        })?;
        facts.shrink_to_fit();
        Ok(facts)
    }

    /// Read committed facts with `fact_seq` strictly greater than the cursor.
    pub fn read_after(&self, fact_seq: u64) -> Result<Vec<SessionFact>, CanonicalError> {
        if fact_seq >= self.commit.committed_fact_seq {
            return Ok(Vec::new());
        }
        self.read_range(fact_seq + 1, self.commit.committed_fact_seq)
    }

    fn read_committed(&self) -> Result<Vec<SessionFact>, CanonicalError> {
        let mut facts = Vec::new();
        self.visit_committed(|fact| {
            facts.push(fact);
            Ok(())
        })?;
        Ok(facts)
    }

    pub(super) fn visit_committed(
        &self,
        visit: impl FnMut(SessionFact) -> Result<(), CanonicalError>,
    ) -> Result<(), CanonicalError> {
        if self.commit.committed_offset == 0 {
            return self.validate_empty_prefix();
        }
        let mut file = self.open_events_for_scan()?;
        // Cold/full scan: parse + validate every fact, hashing the exact
        // prefix bytes so a successful pass can promote the sparse index.
        let mut hasher = Sha256::new();
        let mut checkpoints = Vec::new();
        let (last_seq, offset, last_event_id) = self.scan_lines(
            &mut file,
            0,
            0,
            None,
            Some(&mut hasher),
            Some(&mut checkpoints),
            visit,
        )?;
        offset_index::promote_full_scan(
            &self.session_dir,
            &self.session_id,
            &self.log_id,
            last_seq,
            offset,
            &last_event_id.expect("non-empty prefix ends with a fact"),
            hasher,
            checkpoints,
        );
        Ok(())
    }

    /// `committed_offset == 0`: the only valid prefix is the empty one.
    fn validate_empty_prefix(&self) -> Result<(), CanonicalError> {
        if self.commit.committed_fact_seq != 0 || self.commit.last_barrier_event_id.is_some() {
            return Err(CanonicalError::CommitRecoveryRequired(format!(
                "commit marker claims an empty prefix but a non-empty position: seq={}, event_id={:?}",
                self.commit.committed_fact_seq, self.commit.last_barrier_event_id
            )));
        }
        Ok(())
    }

    /// Validation-only scan (the `open()` path): a warm exact-match index
    /// replaces the prefix parse with a streaming digest check; only bytes
    /// past the indexed position are parsed. Any digest mismatch or stale
    /// lookup falls back to (or fails closed into) the full validating scan.
    fn validate_committed(&self) -> Result<(), CanonicalError> {
        if self.commit.committed_offset == 0 {
            return self.validate_empty_prefix();
        }
        if let Some(index) = offset_index::lookup(
            &self.session_dir,
            &self.session_id,
            &self.log_id,
            &self.commit,
        ) {
            let (base_offset, base_seq, expected, base_event_id) = {
                let index = index.lock().unwrap_or_else(|e| e.into_inner());
                (
                    index.last_offset,
                    index.last_seq,
                    offset_index::prefix_digest(&index),
                    index.last_event_id.clone(),
                )
            };
            if self.prefix_digest_differs(&expected, base_offset)? {
                return Err(CanonicalError::CommitRecoveryRequired(
                    "indexed committed prefix digest mismatch: events.jsonl changed without a \
                     matching commit marker advance"
                        .into(),
                ));
            }
            let mut file = self.open_events_for_scan()?;
            self.scan_lines(
                &mut file,
                base_offset,
                base_seq,
                Some(base_event_id),
                None,
                None,
                |_| Ok(()),
            )?;
            return Ok(());
        }
        self.visit_committed(|_| Ok(()))
    }

    /// Seek-based scan for range reads: skip parsing the prefix below
    /// `from_seq` by trusting a sparse checkpoint whose byte range is first
    /// re-verified via digest. Falls back to the full validating scan when
    /// no warm index (or no covering checkpoint) exists.
    fn visit_committed_seek(
        &self,
        from_seq: u64,
        visit: impl FnMut(SessionFact) -> Result<(), CanonicalError>,
    ) -> Result<(), CanonicalError> {
        if self.commit.committed_offset == 0 {
            return self.validate_empty_prefix();
        }
        if let Some(index) = offset_index::lookup(
            &self.session_dir,
            &self.session_id,
            &self.log_id,
            &self.commit,
        ) {
            let checkpoint = {
                let index = index.lock().unwrap_or_else(|e| e.into_inner());
                offset_index::seek_checkpoint(&index, from_seq)
            };
            if let Some(checkpoint) = checkpoint {
                if self.prefix_digest_differs(&checkpoint.digest, checkpoint.offset)? {
                    return Err(CanonicalError::CommitRecoveryRequired(
                        "indexed committed prefix digest mismatch: events.jsonl changed without \
                         a matching commit marker advance"
                            .into(),
                    ));
                }
                let mut file = self.open_events_for_scan()?;
                self.scan_lines(
                    &mut file,
                    checkpoint.offset,
                    checkpoint.fact_seq,
                    Some(checkpoint.last_event_id.clone()),
                    None,
                    None,
                    visit,
                )?;
                return Ok(());
            }
        }
        self.visit_committed(visit)
    }

    /// Stream-hash `events.jsonl` bytes `0..upto` and report whether they
    /// differ from `expected`. Reading (not parsing) is the fail-closed
    /// verification that lets the index accelerate without skipping
    /// validation.
    fn prefix_digest_differs(
        &self,
        expected: &[u8; 32],
        upto: u64,
    ) -> Result<bool, CanonicalError> {
        if upto == 0 {
            return Ok(false);
        }
        let mut file = File::open(self.events_path())?;
        let mut reader = BufReader::new(file.take(upto));
        let mut hasher = Sha256::new();
        let mut chunk = [0u8; 64 * 1024];
        let mut hashed = 0u64;
        loop {
            let count = reader.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            hasher.update(&chunk[..count]);
            hashed += count as u64;
        }
        if hashed != upto {
            return Ok(true);
        }
        Ok(hasher.finalize().as_slice() != expected)
    }

    /// Open `events.jsonl` for a scan and enforce the committed-offset upper
    /// bound contract (fail closed on truncation). Callers guarantee
    /// `committed_offset > 0`.
    fn open_events_for_scan(&self) -> Result<File, CanonicalError> {
        let file = File::open(self.events_path())?;
        let file_len = file.metadata()?.len();
        if file_len < self.commit.committed_offset {
            return Err(CanonicalError::CommitRecoveryRequired(format!(
                "events.jsonl is shorter than committed offset: {file_len} < {}",
                self.commit.committed_offset
            )));
        }
        Ok(file)
    }

    /// Parse and validate `events.jsonl` from `base_offset` (a position whose
    /// history is already trusted) through the committed offset.
    ///
    /// `base_seq`/`base_event_id` describe the fact immediately before
    /// `base_offset` (None when scanning from the start). When `hasher` is
    /// supplied every scanned byte is hashed and `checkpoints` receives
    /// sparse marks, enabling index promotion after a complete pass.
    /// Returns `(last_seq, end_offset, last_event_id)` and enforces the
    /// marker-vs-prefix equality contract.
    #[allow(clippy::too_many_arguments)]
    fn scan_lines(
        &self,
        file: &mut File,
        base_offset: u64,
        base_seq: u64,
        base_event_id: Option<EventId>,
        mut hasher: Option<&mut Sha256>,
        mut checkpoints: Option<&mut Vec<offset_index::Checkpoint>>,
        mut visit: impl FnMut(SessionFact) -> Result<(), CanonicalError>,
    ) -> Result<(u64, u64, Option<EventId>), CanonicalError> {
        file.seek(SeekFrom::Start(base_offset))?;
        let mut reader = BufReader::new(file.take(self.commit.committed_offset - base_offset));
        let mut offset = base_offset;
        let mut last_seq = base_seq;
        let mut last_event_id = base_event_id.clone();
        let mut line = Vec::new();
        while offset < self.commit.committed_offset {
            line.clear();
            let count = reader.read_until(b'\n', &mut line)?;
            if count == 0 || line.last() != Some(&b'\n') {
                return Err(CanonicalError::CommitRecoveryRequired(
                    "committed prefix contains a torn JSONL line".into(),
                ));
            }
            if let Some(hasher) = hasher.as_deref_mut() {
                hasher.update(&line);
            }
            line.pop();
            if line.is_empty() {
                return Err(CanonicalError::CommitRecoveryRequired(
                    "committed prefix contains an empty JSONL line".into(),
                ));
            }
            let fact: SessionFact = serde_json::from_slice(&line).map_err(|error| {
                CanonicalError::CommitRecoveryRequired(format!(
                    "committed fact at offset {offset} is invalid: {error}"
                ))
            })?;
            count_parse();
            if fact.session_id != self.session_id {
                return Err(CanonicalError::IdentityMismatch {
                    field: "session_id",
                });
            }
            if fact.log_id != self.log_id {
                return Err(CanonicalError::IdentityMismatch { field: "log_id" });
            }
            fact.validate().map_err(|error| {
                CanonicalError::CommitRecoveryRequired(format!(
                    "committed fact at offset {offset} failed validation: {error}"
                ))
            })?;
            let expected = last_seq
                .checked_add(1)
                .ok_or(CanonicalError::FactSeqExhausted)?;
            if fact.fact_seq != expected {
                return Err(CanonicalError::CommitRecoveryRequired(format!(
                    "committed fact_seq gap: expected {expected}, got {}",
                    fact.fact_seq
                )));
            }
            last_seq = fact.fact_seq;
            last_event_id = Some(fact.event_id.clone());
            visit(fact)?;
            offset += count as u64;
            if let (Some(checkpoints), Some(hasher)) =
                (checkpoints.as_deref_mut(), hasher.as_deref_mut())
                && last_seq % offset_index::CHECKPOINT_STRIDE == 0
            {
                checkpoints.push(offset_index::Checkpoint {
                    fact_seq: last_seq,
                    offset,
                    digest: hasher.clone().finalize().into(),
                    last_event_id: last_event_id.clone().expect("fact just parsed"),
                });
                if checkpoints.len() > offset_index::MAX_CHECKPOINTS {
                    checkpoints.drain(..checkpoints.len() / 2);
                }
            }
        }
        if last_seq != self.commit.committed_fact_seq
            || offset != self.commit.committed_offset
            || last_event_id != self.commit.last_barrier_event_id
        {
            return Err(CanonicalError::CommitRecoveryRequired(format!(
                "commit marker does not match committed prefix: marker=({}, {}, {:?}), prefix=({}, {}, {:?})",
                self.commit.committed_fact_seq,
                self.commit.committed_offset,
                self.commit.last_barrier_event_id,
                last_seq,
                offset,
                last_event_id
            )));
        }
        Ok((last_seq, offset, last_event_id))
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
