//! Isolated, single-writer batch journal for the new session owner.
//!
//! This is a storage primitive, not an effect-dispatch capability: callers must
//! still validate business transitions and context semantics.
//! It intentionally does not share files or recovery with the legacy fact log.
//! All blocking operations belong on a bounded blocking executor.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::SessionBlobStore;
use super::log::{CanonicalError, write_json_atomic};
use crate::session_fact_v2::{LogId, MAX_SAFE_FACT_SEQ, SessionFact, SessionId};

const DIRECTORY: &str = "committed-batches";
const MARKER: &str = "commit.json";
const JOURNAL: &str = "batches.jsonl";
const LOCK: &str = "writer.lock";
const FORMAT: &str = "qaqh.session-batches";
// A distinct storage identity lets format-aware readers reject the wrong log.
// Pre-existing binaries do not recognize this marker; deployment must isolate
// the new data root from them until an explicit compatibility fence exists.
const FORMAT_FILE: &str = "storage-format";
const MAX_BATCH_BYTES: usize = 4 * 1024 * 1024;
const MAX_BATCH_FACTS: usize = 256;
const MAX_BATCH_ID_BYTES: usize = 128;
// Temporary bound until a durable sparse receipt index replaces this map.
const MAX_INDEX_ENTRIES: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    format: String,
    session_id: SessionId,
    log_id: LogId,
    offset: u64,
    fact_seq: u64,
    batch_count: u64,
    last_digest: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    id: String,
    expected_head: u64,
    previous_digest: Option<String>,
    digest: String,
    facts: Vec<SessionFact>,
}

/// A durable batch position. This is not proof that its effects may execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchReceipt {
    pub batch_id: String,
    pub digest: String,
    pub first_fact_seq: u64,
    pub last_fact_seq: u64,
}

/// Commit failures before writing differ from uncertain outcomes after IO begins.
#[derive(Debug)]
pub enum BatchCommitError {
    /// No journal bytes were attempted; the caller may correct the proposal.
    NotCommitted(CanonicalError),
    /// The marker or journal may have changed. Fence dispatch and reopen/lookup.
    Unknown(CanonicalError),
}

/// Deterministic fault injection points around the batch durability barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchFaultPoint {
    /// Fail after appending the batch line to `batches.jsonl` and syncing it,
    /// before `commit.json` is updated (simulates a crash leaving an
    /// uncommitted tail).
    AfterJournalAppendBeforeMarker,
    /// Fail after `commit.json` has been atomically written, before returning
    /// the receipt (simulates marker durability succeeded while the caller
    /// loses the commit response).
    AfterMarkerWrite,
}

/// Dedicated journal lock is held for this owner's entire lifetime.
/// A newly opened owner validates its committed prefix before truncating a tail.
#[derive(Debug)]
pub struct BatchJournal {
    dir: PathBuf,
    session_id: SessionId,
    log_id: LogId,
    marker: Marker,
    receipts: HashMap<String, BatchReceipt>,
    event_ids: HashSet<String>,
    read_only: bool,
    fenced: bool,
    fault_once: Option<BatchFaultPoint>,
    _lock: File,
}

impl BatchJournal {
    /// Create a new isolated journal. Never use this to open an existing one.
    pub fn create(
        session_dir: impl AsRef<Path>,
        session_id: SessionId,
        log_id: LogId,
    ) -> Result<Self, CanonicalError> {
        let parent = session_dir.as_ref();
        let dir = parent.join(DIRECTORY);
        if parent.join(super::EVENTS_FILE).exists()
            || parent.join(super::EVENTS_COMMIT_FILE).exists()
        {
            return Err(CanonicalError::NotWritable(
                "batch journal requires an isolated data root".into(),
            ));
        }
        let format = parent.join(FORMAT_FILE);
        let mut identity = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(format)?;
        identity.write_all(FORMAT.as_bytes())?;
        identity.sync_all()?;
        sync_dir(parent)?;
        fs::create_dir(&dir)?;
        sync_dir(parent)?;
        let lock = lock(&dir)?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(JOURNAL))?;
        file.sync_all()?;
        let marker = Marker {
            format: FORMAT.into(),
            session_id: session_id.clone(),
            log_id: log_id.clone(),
            offset: 0,
            fact_seq: 0,
            batch_count: 0,
            last_digest: None,
        };
        write_json_atomic(&dir.join(MARKER), &marker)?;
        Ok(Self {
            dir,
            session_id,
            log_id,
            marker,
            receipts: HashMap::new(),
            event_ids: HashSet::new(),
            read_only: false,
            fenced: false,
            fault_once: None,
            _lock: lock,
        })
    }

    /// Open an existing journal. A missing, invalid or inconsistent marker is
    /// never reconstructed from the journal: doing so could commit an unknown tail.
    pub fn open(
        session_dir: impl AsRef<Path>,
        session_id: SessionId,
        log_id: LogId,
    ) -> Result<Self, CanonicalError> {
        Self::open_with(session_dir, session_id, log_id, true)
    }

    /// Open the committed prefix **without** the single-writer lock.
    ///
    /// A view never truncates, repairs or syncs the journal: it only exposes the
    /// prefix the marker already acknowledges, so it can run while the owning
    /// write handle is alive. Use it for projection/replay/read-model paths.
    pub fn open_view(
        session_dir: impl AsRef<Path>,
        session_id: SessionId,
        log_id: LogId,
    ) -> Result<Self, CanonicalError> {
        Self::open_with(session_dir, session_id, log_id, false)
    }

    fn open_with(
        session_dir: impl AsRef<Path>,
        session_id: SessionId,
        log_id: LogId,
        writable: bool,
    ) -> Result<Self, CanonicalError> {
        let dir = session_dir.as_ref().join(DIRECTORY);
        if fs::read(session_dir.as_ref().join(FORMAT_FILE))? != FORMAT.as_bytes() {
            return Err(CanonicalError::CommitRecoveryRequired(
                "batch storage format mismatch".into(),
            ));
        }
        let lock = if writable {
            lock(&dir)?
        } else {
            OpenOptions::new().read(true).open(dir.join(LOCK))?
        };
        let marker: Marker = serde_json::from_slice(&fs::read(dir.join(MARKER))?)?;
        if marker.format != FORMAT || marker.session_id != session_id || marker.log_id != log_id {
            return Err(CanonicalError::CommitRecoveryRequired(
                "batch marker format or identity mismatch".into(),
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join(JOURNAL))?;
        let length = file.metadata()?.len();
        if length < marker.offset {
            return Err(CanonicalError::CommitRecoveryRequired(
                "batch journal shorter than marker".into(),
            ));
        }
        let mut reader = BufReader::new((&file).take(marker.offset));
        let mut receipts = HashMap::new();
        let mut event_ids = HashSet::new();
        let mut head = 0u64;
        let mut count = 0u64;
        let mut offset = 0u64;
        let mut last_digest = None;
        let mut read_only = false;
        while offset < marker.offset {
            let mut line = Vec::new();
            let read = (&mut reader)
                .take((MAX_BATCH_BYTES + 1) as u64)
                .read_until(b'\n', &mut line)?;
            if read == 0 || read > MAX_BATCH_BYTES || line.last() != Some(&b'\n') {
                return Err(CanonicalError::CommitRecoveryRequired(
                    "invalid or oversized committed batch".into(),
                ));
            }
            offset = offset
                .checked_add(read as u64)
                .ok_or(CanonicalError::FactSeqExhausted)?;
            let batch: Batch = serde_json::from_slice(&line)?;
            validate_id(&batch.id)?;
            if batch.expected_head != head
                || batch.facts.is_empty()
                || batch.facts.len() > MAX_BATCH_FACTS
            {
                return Err(CanonicalError::CommitRecoveryRequired(
                    "invalid batch head or fact count".into(),
                ));
            }
            let first = head + 1;
            for fact in &batch.facts {
                if read_only {
                    return Err(CanonicalError::CommitRecoveryRequired(
                        "fact after session tombstone".into(),
                    ));
                }
                head = head
                    .checked_add(1)
                    .filter(|n| *n <= MAX_SAFE_FACT_SEQ)
                    .ok_or(CanonicalError::FactSeqExhausted)?;
                validate_fact(fact, &session_id, &log_id, head)?;
                if !event_ids.insert(fact.event_id.as_str().to_owned()) {
                    return Err(CanonicalError::CommitRecoveryRequired(
                        "duplicate committed event id".into(),
                    ));
                }
                if event_ids.len() > MAX_INDEX_ENTRIES {
                    return Err(CanonicalError::NotWritable(
                        "batch event index capacity exceeded".into(),
                    ));
                }
                read_only = matches!(
                    fact.payload,
                    crate::session_fact_v2::FactPayload::SessionDeleted(_)
                );
            }
            if batch.previous_digest != last_digest {
                return Err(CanonicalError::CommitRecoveryRequired(
                    "batch digest chain mismatch".into(),
                ));
            }
            let digest = batch_digest(
                &batch.id,
                batch.expected_head,
                batch.previous_digest.as_deref(),
                &batch.facts,
            )?;
            if digest != batch.digest {
                return Err(CanonicalError::CommitRecoveryRequired(
                    "batch digest mismatch".into(),
                ));
            }
            let receipt = BatchReceipt {
                batch_id: batch.id.clone(),
                digest: digest.clone(),
                first_fact_seq: first,
                last_fact_seq: head,
            };
            if receipts.insert(batch.id, receipt).is_some() {
                return Err(CanonicalError::CommitRecoveryRequired(
                    "duplicate committed batch id".into(),
                ));
            }
            if receipts.len() > MAX_INDEX_ENTRIES {
                return Err(CanonicalError::NotWritable(
                    "batch receipt index capacity exceeded".into(),
                ));
            }
            count = count
                .checked_add(1)
                .ok_or(CanonicalError::FactSeqExhausted)?;
            last_digest = Some(digest);
        }
        if offset != marker.offset
            || head != marker.fact_seq
            || count != marker.batch_count
            || last_digest != marker.last_digest
        {
            return Err(CanonicalError::CommitRecoveryRequired(
                "batch marker does not match committed prefix".into(),
            ));
        }
        // Only the verified marker is authoritative; never promote a complete tail.
        // A read-only view must not repair: it just ignores bytes past the marker.
        if writable {
            if length != marker.offset {
                file.set_len(marker.offset)?;
            }
            // Reconcile a possibly observed rename whose directory sync failed:
            // the visible marker is not a durable acknowledgment until both it
            // and its directory have been synchronized successfully.
            file.sync_all()?;
            File::open(dir.join(MARKER))?.sync_all()?;
            sync_dir(&dir)?;
        }
        Ok(Self {
            dir,
            session_id,
            log_id,
            marker,
            receipts,
            event_ids,
            read_only,
            fenced: false,
            fault_once: None,
            _lock: lock,
        })
    }

    /// Arm a one-shot fault at a specific durability seam for the next commit.
    pub fn inject_fault_once(&mut self, point: BatchFaultPoint) {
        self.fault_once = Some(point);
    }

    pub fn committed_head(&self) -> u64 {
        self.marker.fact_seq
    }

    /// Current writer idempotency-index estimate. This is a shallow estimate:
    /// hash-table buckets and allocator overhead are not observable here.
    pub fn index_memory_usage(&self) -> (u64, u64) {
        let entries = self.receipts.len().saturating_add(self.event_ids.len());
        let heap_bytes = self
            .receipts
            .capacity()
            .saturating_mul(std::mem::size_of::<(String, BatchReceipt)>())
            .saturating_add(
                self.event_ids
                    .capacity()
                    .saturating_mul(std::mem::size_of::<String>()),
            )
            .saturating_add(
                self.receipts
                    .iter()
                    .map(|(id, receipt)| {
                        id.capacity()
                            .saturating_add(receipt.batch_id.capacity())
                            .saturating_add(receipt.digest.capacity())
                    })
                    .sum::<usize>(),
            )
            .saturating_add(self.event_ids.iter().map(String::capacity).sum::<usize>());
        (entries as u64, heap_bytes as u64)
    }

    /// Visit the current immutable committed prefix in batches. The visitor may
    /// stop on a domain error; bytes beyond the captured marker are never read.
    pub fn visit_committed<E>(
        &self,
        mut visit: impl FnMut(&[SessionFact]) -> Result<(), E>,
    ) -> Result<(), BatchReadError<E>> {
        self.ensure_healthy().map_err(BatchReadError::Storage)?;
        let file =
            File::open(self.dir.join(JOURNAL)).map_err(|e| BatchReadError::Storage(e.into()))?;
        let mut reader = BufReader::new(file.take(self.marker.offset));
        let mut offset = 0u64;
        let mut head = 0u64;
        let mut previous_digest = None;
        let mut count = 0u64;
        while offset < self.marker.offset {
            let mut line = Vec::new();
            let read = (&mut reader)
                .take((MAX_BATCH_BYTES + 1) as u64)
                .read_until(b'\n', &mut line)
                .map_err(|e| BatchReadError::Storage(e.into()))?;
            if read == 0 || read > MAX_BATCH_BYTES || line.last() != Some(&b'\n') {
                return Err(BatchReadError::Storage(
                    CanonicalError::CommitRecoveryRequired("invalid committed batch".into()),
                ));
            }
            offset = offset
                .checked_add(read as u64)
                .ok_or(BatchReadError::Storage(CanonicalError::FactSeqExhausted))?;
            let batch: Batch =
                serde_json::from_slice(&line).map_err(|e| BatchReadError::Storage(e.into()))?;
            if batch.expected_head != head
                || batch.previous_digest != previous_digest
                || batch.facts.is_empty()
                || batch.facts.len() > MAX_BATCH_FACTS
            {
                return Err(BatchReadError::Storage(
                    CanonicalError::CommitRecoveryRequired("committed batch chain mismatch".into()),
                ));
            }
            let digest = batch_digest(&batch.id, head, previous_digest.as_deref(), &batch.facts)
                .map_err(BatchReadError::Storage)?;
            if digest != batch.digest {
                return Err(BatchReadError::Storage(
                    CanonicalError::CommitRecoveryRequired(
                        "committed batch digest mismatch".into(),
                    ),
                ));
            }
            for fact in &batch.facts {
                head = head
                    .checked_add(1)
                    .ok_or(BatchReadError::Storage(CanonicalError::FactSeqExhausted))?;
                validate_fact(fact, &self.session_id, &self.log_id, head)
                    .map_err(BatchReadError::Storage)?;
            }
            visit(&batch.facts).map_err(BatchReadError::Visitor)?;
            previous_digest = Some(digest);
            count = count
                .checked_add(1)
                .ok_or(BatchReadError::Storage(CanonicalError::FactSeqExhausted))?;
        }
        if offset != self.marker.offset
            || head != self.marker.fact_seq
            || previous_digest != self.marker.last_digest
            || count != self.marker.batch_count
        {
            return Err(BatchReadError::Storage(
                CanonicalError::CommitRecoveryRequired("committed prefix changed".into()),
            ));
        }
        Ok(())
    }

    /// Lookup is meaningful only on a newly opened or otherwise healthy owner.
    pub fn lookup(&self, id: &str) -> Result<Option<&BatchReceipt>, CanonicalError> {
        self.ensure_healthy()?;
        Ok(self.receipts.get(id))
    }

    /// A retry must use the original id, expected head and exact proposal.
    /// Facts must arrive with `fact_seq = 0`; the journal assigns sequence numbers.
    pub fn commit(
        &mut self,
        id: &str,
        expected_head: u64,
        mut facts: Vec<SessionFact>,
    ) -> Result<BatchReceipt, BatchCommitError> {
        let prepared = (|| -> Result<(Batch, Vec<u8>, BatchReceipt), CanonicalError> {
            self.ensure_healthy()?;
            validate_id(id)?;
            if facts.is_empty() || facts.len() > MAX_BATCH_FACTS {
                return Err(CanonicalError::NotWritable(
                    "invalid batch fact count".into(),
                ));
            }
            let mut head = expected_head;
            let mut ids = HashSet::new();
            let fact_count = facts.len();
            for (index, fact) in facts.iter_mut().enumerate() {
                if fact.fact_seq != 0 {
                    return Err(CanonicalError::NotWritable(
                        "proposal fact sequence must be zero".into(),
                    ));
                }
                head = head
                    .checked_add(1)
                    .filter(|n| *n <= MAX_SAFE_FACT_SEQ)
                    .ok_or(CanonicalError::FactSeqExhausted)?;
                fact.fact_seq = head;
                validate_fact(fact, &self.session_id, &self.log_id, head)?;
                if matches!(
                    fact.payload,
                    crate::session_fact_v2::FactPayload::SessionDeleted(_)
                ) && index + 1 != fact_count
                {
                    return Err(CanonicalError::NotWritable(
                        "session tombstone must end batch".into(),
                    ));
                }
                if !ids.insert(fact.event_id.as_str().to_owned()) {
                    return Err(CanonicalError::NotWritable(
                        "duplicate event id within batch".into(),
                    ));
                }
            }
            if let Some(receipt) = self.receipts.get(id) {
                // The digest binds the predecessor, so recover it from the
                // committed batch, not from the current journal head.
                let stored = self.read_batch(id)?;
                if stored.digest != receipt.digest
                    || batch_digest(
                        &stored.id,
                        stored.expected_head,
                        stored.previous_digest.as_deref(),
                        &stored.facts,
                    )? != receipt.digest
                {
                    return Err(CanonicalError::CommitRecoveryRequired(
                        "committed retry batch changed".into(),
                    ));
                }
                let digest =
                    batch_digest(id, expected_head, stored.previous_digest.as_deref(), &facts)?;
                if receipt.digest == digest {
                    return Ok((stored, Vec::new(), receipt.clone()));
                }
                return Err(CanonicalError::NotWritable(
                    "batch id reused with different content".into(),
                ));
            }
            if expected_head != self.marker.fact_seq {
                return Err(CanonicalError::NotWritable(
                    "batch expected head mismatch".into(),
                ));
            }
            if self.read_only {
                return Err(CanonicalError::NotWritable("session is deleted".into()));
            }
            if ids.iter().any(|event| self.event_ids.contains(event)) {
                return Err(CanonicalError::NotWritable(
                    "event id already committed".into(),
                ));
            }
            if self.receipts.len() >= MAX_INDEX_ENTRIES
                || self.event_ids.len().saturating_add(ids.len()) > MAX_INDEX_ENTRIES
            {
                return Err(CanonicalError::NotWritable(
                    "batch index capacity exceeded".into(),
                ));
            }
            for fact in &facts {
                validate_fact_refs(
                    self.dir.parent().ok_or_else(|| {
                        CanonicalError::NotWritable("batch directory has no parent".into())
                    })?,
                    fact,
                )?;
            }
            let previous_digest = self.marker.last_digest.clone();
            let digest = batch_digest(id, expected_head, previous_digest.as_deref(), &facts)?;
            let receipt = BatchReceipt {
                batch_id: id.into(),
                digest: digest.clone(),
                first_fact_seq: expected_head + 1,
                last_fact_seq: head,
            };
            let batch = Batch {
                id: id.into(),
                expected_head,
                previous_digest,
                digest,
                facts,
            };
            let mut bytes = encode_bounded(&batch)?;
            bytes.push(b'\n');
            if bytes.len() > MAX_BATCH_BYTES {
                return Err(CanonicalError::NotWritable(
                    "batch exceeds byte budget".into(),
                ));
            }
            Ok((batch, bytes, receipt))
        })()
        .map_err(BatchCommitError::NotCommitted)?;
        let (batch, encoded, receipt) = prepared;
        if encoded.is_empty() {
            return Ok(receipt);
        }
        let next_offset = self
            .marker
            .offset
            .checked_add(encoded.len() as u64)
            .ok_or_else(|| BatchCommitError::NotCommitted(CanonicalError::FactSeqExhausted))?;
        let next_count = self
            .marker
            .batch_count
            .checked_add(1)
            .ok_or_else(|| BatchCommitError::NotCommitted(CanonicalError::FactSeqExhausted))?;
        let next = Marker {
            format: FORMAT.into(),
            session_id: self.session_id.clone(),
            log_id: self.log_id.clone(),
            offset: next_offset,
            fact_seq: receipt.last_fact_seq,
            batch_count: next_count,
            last_digest: Some(receipt.digest.clone()),
        };
        let fault = self.fault_once.take();
        let result = (|| -> Result<(), CanonicalError> {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(self.dir.join(JOURNAL))?;
            if file.metadata()?.len() != self.marker.offset {
                return Err(CanonicalError::CommitRecoveryRequired(
                    "batch journal length changed".into(),
                ));
            }
            file.seek(SeekFrom::Start(self.marker.offset))?;
            file.write_all(&encoded)?;
            file.sync_all()?;
            if fault == Some(BatchFaultPoint::AfterJournalAppendBeforeMarker) {
                return Err(CanonicalError::CommitRecoveryRequired(
                    "injected fault after journal append before commit marker".into(),
                ));
            }
            write_json_atomic(&self.dir.join(MARKER), &next)?;
            if fault == Some(BatchFaultPoint::AfterMarkerWrite) {
                return Err(CanonicalError::CommitRecoveryRequired(
                    "injected fault after commit marker write".into(),
                ));
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.fenced = true;
            return Err(BatchCommitError::Unknown(error));
        }
        self.marker = next;
        for fact in batch.facts {
            self.read_only |= matches!(
                fact.payload,
                crate::session_fact_v2::FactPayload::SessionDeleted(_)
            );
            self.event_ids.insert(fact.event_id.as_str().to_owned());
        }
        self.receipts.insert(id.to_owned(), receipt.clone());
        Ok(receipt)
    }

    fn ensure_healthy(&self) -> Result<(), CanonicalError> {
        if self.fenced {
            Err(CanonicalError::CommitRecoveryRequired(
                "batch owner fenced after uncertain write; reopen and reconcile".into(),
            ))
        } else {
            Ok(())
        }
    }

    fn read_batch(&self, id: &str) -> Result<Batch, CanonicalError> {
        let file = File::open(self.dir.join(JOURNAL))?;
        let mut reader = BufReader::new(file.take(self.marker.offset));
        loop {
            let mut line = Vec::new();
            let read = (&mut reader)
                .take((MAX_BATCH_BYTES + 1) as u64)
                .read_until(b'\n', &mut line)?;
            if read == 0 {
                return Err(CanonicalError::CommitRecoveryRequired(
                    "committed batch disappeared".into(),
                ));
            }
            if read > MAX_BATCH_BYTES || line.last() != Some(&b'\n') {
                return Err(CanonicalError::CommitRecoveryRequired(
                    "invalid committed batch".into(),
                ));
            }
            let batch: Batch = serde_json::from_slice(&line)?;
            if batch.id == id {
                return Ok(batch);
            }
        }
    }
}

#[derive(Debug)]
pub enum BatchReadError<E> {
    Storage(CanonicalError),
    Visitor(E),
}

/// True when `session_dir` is owned by the isolated batch journal.
pub fn is_batch_root(session_dir: &Path) -> bool {
    session_dir.join(FORMAT_FILE).exists()
}

/// Read the committed prefix of a batch-journal session without taking the
/// writer lock. This is the read-side counterpart of [`BatchJournal::open`]
/// for projection, replay and agent-graph rebuilds.
pub fn read_committed_facts(
    session_dir: impl AsRef<Path>,
    session_id: SessionId,
    log_id: LogId,
) -> Result<Vec<SessionFact>, CanonicalError> {
    let session_dir = session_dir.as_ref();
    if !is_batch_root(session_dir) {
        return Err(CanonicalError::CommitRecoveryRequired(format!(
            "{} is not a batch journal root",
            session_dir.display()
        )));
    }
    let journal = BatchJournal::open_view(session_dir, session_id, log_id)?;
    let mut facts = Vec::new();
    journal
        .visit_committed(|batch| {
            facts.extend_from_slice(batch);
            Ok::<(), CanonicalError>(())
        })
        .map_err(|error| match error {
            BatchReadError::Storage(error) => error,
            BatchReadError::Visitor(error) => error,
        })?;
    Ok(facts)
}

fn lock(dir: &Path) -> Result<File, CanonicalError> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(dir.join(LOCK))?;
    file.try_lock().map_err(|error| {
        CanonicalError::NotWritable(format!("batch writer lock unavailable: {error}"))
    })?;
    Ok(file)
}

fn validate_id(id: &str) -> Result<(), CanonicalError> {
    if id.is_empty()
        || id.len() > MAX_BATCH_ID_BYTES
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(CanonicalError::NotWritable("invalid batch id".into()));
    }
    Ok(())
}

fn validate_fact(
    fact: &SessionFact,
    session_id: &SessionId,
    log_id: &LogId,
    seq: u64,
) -> Result<(), CanonicalError> {
    if &fact.session_id != session_id || &fact.log_id != log_id || fact.fact_seq != seq {
        return Err(CanonicalError::CommitRecoveryRequired(
            "batch fact identity or sequence mismatch".into(),
        ));
    }
    fact.validate()?;
    Ok(())
}

fn validate_fact_refs(session_dir: &Path, fact: &SessionFact) -> Result<(), CanonicalError> {
    use crate::session_fact_v2::{
        FactPayload as P, InterAgentContent, RecoveryAction, ToolReplayCapability,
    };
    let mut refs = Vec::new();
    match &fact.payload {
        P::ContextMessageCommitted(p) => refs.push(&p.message_ref),
        P::ContextCheckpointCommitted(p) => refs.push(&p.checkpoint_ref),
        P::ProviderResponseCommitted(p) => {
            refs.push(&p.message_ref);
            refs.extend(&p.opaque_refs);
        }
        P::InputAccepted(p) => {
            refs.extend(p.content_ref.iter());
            refs.extend(&p.attachments);
        }
        P::AssistantBlockSealed(p) => refs.push(&p.content_ref),
        P::ToolCallDeclared(p) => refs.push(&p.args_ref),
        P::ToolIntent(p) => {
            refs.extend(p.effective_args_ref.iter());
            refs.extend(p.policy_decision.reason_ref.iter());
            if let ToolReplayCapability::Reconcile { probe_ref } = &p.replay_capability {
                refs.push(probe_ref);
            }
        }
        P::ToolFinished(p) => {
            refs.extend(p.output_ref.iter());
            refs.extend(p.evidence_ref.iter());
            if let Some(error) = &p.error {
                refs.extend(error.details_ref.iter());
            }
        }
        P::InteractionRequested(p) => refs.push(&p.request_ref),
        P::InteractionResolved(p) => refs.push(&p.decision_ref),
        P::TurnFinished(p) => {
            if let Some(error) = &p.error {
                refs.extend(error.details_ref.iter());
            }
        }
        P::CompactionApplied(p) => refs.push(&p.summary_ref),
        P::WorkspaceResourceChanged(p) => refs.push(&p.summary_ref),
        P::SubagentFinished(p) => refs.extend(p.result_ref.iter()),
        P::InterAgentCommunication(p) => {
            if let InterAgentContent::ContentRef { content_ref } = &p.content {
                refs.push(content_ref);
            }
        }
        P::SessionRecovered(p) => {
            for action in &p.actions {
                match action {
                    RecoveryAction::ToolFinished { completion } => {
                        refs.extend(completion.output_ref.iter());
                        refs.extend(completion.evidence_ref.iter());
                        if let Some(error) = &completion.error {
                            refs.extend(error.details_ref.iter());
                        }
                    }
                    RecoveryAction::SubagentFinished { result_ref, .. } => {
                        refs.extend(result_ref.iter())
                    }
                    _ => {}
                }
            }
        }
        P::SessionCreated(_)
        | P::QueuedInputSettled(_)
        | P::TurnStarted(_)
        | P::ModelRoundStarted(_)
        | P::InteractionExpired(_)
        | P::TurnInterrupted(_)
        | P::SessionMetadataChanged(_)
        | P::SessionTitleChanged(_)
        | P::SessionDeleted(_)
        | P::SubagentSpawned(_)
        | P::DriverChanged(_) => {}
    }
    if !refs.is_empty() {
        let blobs = SessionBlobStore::open(session_dir)?;
        for reference in refs {
            let bytes = blobs.get_bounded(reference, MAX_BATCH_BYTES)?;
            if blobs.put_durable(&bytes)? != *reference {
                return Err(CanonicalError::CommitRecoveryRequired(
                    "blob identity mismatch".into(),
                ));
            }
        }
    }
    Ok(())
}

fn batch_digest(
    id: &str,
    expected_head: u64,
    previous_digest: Option<&str>,
    facts: &[SessionFact],
) -> Result<String, CanonicalError> {
    let encoded = encode_bounded(&(id, expected_head, previous_digest, facts))?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(encoded))))
}

fn encode_bounded(value: &impl Serialize) -> Result<Vec<u8>, CanonicalError> {
    struct Bounded(Vec<u8>);
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > MAX_BATCH_BYTES.saturating_sub(self.0.len()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "batch exceeds byte budget",
                ));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut out = Bounded(Vec::new());
    serde_json::to_writer(&mut out, value)?;
    Ok(out.0)
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> Result<(), CanonicalError> {
    File::open(path)?.sync_all()?;
    Ok(())
}
#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> Result<(), CanonicalError> {
    Err(CanonicalError::NotWritable(
        "durable batch directory synchronization is unsupported on this platform".into(),
    ))
}
