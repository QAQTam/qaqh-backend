//! Write-ahead log for conversation persist ops (enqueue-time durability).
//!
//! Layout of `<session_dir>/messages.wal`:
//!
//! ```text
//! {"type":"qaqh-wal-v1","next_seq":1}
//! {"seq":1,"op":{"Append":{...}}}
//! {"seq":2,"op":{"UpdateMeta":{...}}}
//! ```
//!
//! Durability contract:
//!
//! - `MessageStore::flush_meta` logs every message-bearing op BEFORE it enters
//!   the in-memory drain queue, so a process death after `log_op` never loses
//!   an already-completed round (the archive itself only sees the op at the
//!   next `drain_persist_ops`).
//! - `sync` is invoked at round boundaries (round-boundary fsync policy, user
//!   decision 2026-09-02): the archive append path already fsyncs, so the WAL
//!   only has to cover enqueue → drain, which is at most one round.
//! - The host calls `checkpoint` (truncate to header) after a successful
//!   drain. Replay is idempotent (msg_id dedupe on the archive tail), so a
//!   crash between "applied" and "checkpointed" converges instead of
//!   duplicating.
//! - `SaveFull` is deliberately NOT logged: it is a generation rewrite
//!   (undo / defensive repair). Losing one on a crash degrades gracefully
//!   (the rewrite is simply not applied), whereas replaying appends across an
//!   applied generation rewrite would resurrect undone turns.
//! - Compaction itself is an ordinary `Append` carrying the summary message
//!   and the new covered watermark, so it is covered by the same WAL path as
//!   normal messages.
//!
//! Recovery lives in `qaqh-session` (`SessionManager::replay_message_wal`),
//! which owns the apply mapping; this module only owns the file format.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::effect::PersistOp;
use crate::legacy_writer::LegacyWriterFacade;

pub(crate) const WAL_FILE_NAME: &str = "messages.wal";

/// Read granularity for the WAL reader. Small on purpose: a WAL is a handful of
/// lines, and a finer read keeps "did the device fail on line N?" observable
/// (and the fault-injection tests deterministic) instead of letting one 64 KiB
/// read swallow the whole log.
const LINE_CHUNK: usize = 512;
const HEADER_KIND: &str = "qaqh-wal-v1";

#[derive(Serialize, Deserialize)]
struct WalHeader {
    #[serde(rename = "type")]
    kind: String,
    next_seq: u64,
}

#[derive(Serialize)]
struct WalLineWrite<'a> {
    seq: u64,
    op: &'a PersistOp,
}

#[derive(Deserialize)]
struct WalLineRead {
    #[allow(dead_code)]
    seq: u64,
    op: PersistOp,
}

/// Append-only WAL handle owned by a `MessageStore`.
pub struct WalWriter {
    path: PathBuf,
    file: File,
    next_seq: u64,
}

fn header_path(path: &Path) -> PathBuf {
    path.with_extension("wal.tmp")
}

fn nanos_now() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Quarantine name for a parse failure (torn tail) — "corrupt".
fn quarantine_path(path: &Path) -> PathBuf {
    path.with_extension(format!("wal.corrupt-{}", nanos_now()))
}

/// Quarantine name for a mid-file read failure — "unreadable", so an operator
/// can tell an IO fault apart from a torn write by filename alone.
fn io_quarantine_path(path: &Path) -> PathBuf {
    path.with_extension(format!("wal.unreadable-{}", nanos_now()))
}

/// Read the existing WAL header. Returns `(next_seq, has_op_lines)`.
/// A missing file yields `(1, false)`; an unreadable/corrupt header yields
/// `None` so the caller can rotate the file away.
fn scan_file(path: &Path) -> io::Result<Option<(u64, bool)>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut reader = BufReader::new(file);
    let mut first = String::new();
    if reader.read_line(&mut first)? == 0 {
        // Empty file — treat as fresh.
        return Ok(Some((1, false)));
    }
    let header: WalHeader = serde_json::from_str(first.trim_end())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if header.kind != HEADER_KIND {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown WAL header kind",
        ));
    }
    let mut has_ops = false;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        has_ops = true;
        // Stop counting at the first torn tail: everything after a partial
        // write is meaningless. `next_seq` continuity is not needed past this
        // point because the recovery path rewrites (checkpoints) the file.
        if serde_json::from_str::<WalLineRead>(line.trim_end()).is_err() {
            break;
        }
    }
    Ok(Some((header.next_seq, has_ops)))
}

impl WalWriter {
    /// Open (or create) the WAL inside `session_dir`.
    ///
    /// Recovery (`SessionManager::replay_message_wal`) runs before a store is
    /// created, so finding op lines here means an unrecovered stale log: it is
    /// rotated to `messages.wal.stale-<ts>` instead of being appended after.
    pub fn open(session_dir: &Path) -> io::Result<Self> {
        let _legacy_writer = LegacyWriterFacade::lock();
        let path = session_dir.join(WAL_FILE_NAME);
        let scanned = scan_file(&path)?;
        let (next_seq, has_ops) = scanned.unwrap_or((1, false));
        if has_ops {
            let stale = path.with_extension(format!("wal.stale-{}", nanos_now()));
            fs::rename(&path, &stale)?;
            log::error!(
                "WAL: unrecovered op lines at open — rotated to {} (recovery should have run first)",
                stale.display()
            );
            write_header(&path, 1)?;
            return Ok(Self {
                file: open_append(&path)?,
                path,
                next_seq: 1,
            });
        }
        if scanned.is_none() {
            write_header(&path, next_seq)?;
        }
        Ok(Self {
            file: open_append(&path)?,
            path,
            next_seq,
        })
    }

    /// Append one op. Returns the assigned sequence number.
    /// `write` only — pair with [`Self::sync`] at the round boundary.
    pub fn log_op(&mut self, op: &PersistOp) -> io::Result<u64> {
        let _legacy_writer = LegacyWriterFacade::lock();
        let seq = self.next_seq;
        let line = serde_json::to_string(&WalLineWrite { seq, op })
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.next_seq += 1;
        Ok(seq)
    }

    /// Round-boundary fsync (user decision: `sync_data` — content, not metadata).
    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_data()
    }

    /// Truncate the log back to a bare header (all logged ops have been
    /// applied to the archive). Atomic via temp + rename; a crash that loses
    /// the rename resurfaces the old ops, which replay dedupes idempotently.
    pub fn checkpoint(&mut self) -> io::Result<()> {
        let _legacy_writer = LegacyWriterFacade::lock();
        write_header(&self.path, self.next_seq)?;
        self.file = open_append(&self.path)?;
        Ok(())
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn write_header(path: &Path, next_seq: u64) -> io::Result<()> {
    let tmp = header_path(path);
    let header = WalHeader {
        kind: HEADER_KIND.to_string(),
        next_seq,
    };
    {
        let mut file = File::create(&tmp)?;
        let line = serde_json::to_string(&header)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// Open the WAL at `session_dir` for recovery.
///
/// A missing file is `Ok(None)` (nothing to replay). Any other failure to
/// open — including `PermissionDenied` and IO faults reported by the probe
/// read — is an `Err`: recovery must not treat "cannot read the log" as
/// "the log is empty" (fail-closed).
pub fn open_reader(session_dir: &Path) -> io::Result<Option<WalReader>> {
    let path = session_dir.join(WAL_FILE_NAME);
    Ok(OpenFile::at(&path)?.map(|open| WalReader::new(path, open)))
}

/// Read all logged ops for recovery.
///
/// Fail-closed contract:
/// - a torn/corrupt line (recoverable prefix followed by garbage — the normal
///   crash-mid-write shape) stops the scan and the file is quarantined to
///   `messages.wal.corrupt-<ts>`; ops before the bad line are returned;
/// - a mid-file IO error is *not* a torn tail: it is logged, the whole file is
///   quarantined to `messages.wal.unreadable-<ts>`, and the ops read before it
///   are returned (the log itself is kept, never truncated).
///
/// This infallible signature exists for callers that have no durability
/// decision to make. A caller that can checkpoint (replay + truncate) MUST use
/// [`open_reader`] and honour [`WalReader::has_failed`], or it will destroy the
/// ops the fault hid.
pub fn read_ops(session_dir: &Path) -> Vec<PersistOp> {
    let mut reader = match open_reader(session_dir) {
        Ok(Some(reader)) => reader,
        Ok(None) => return Vec::new(),
        Err(error) => {
            let path = session_dir.join(WAL_FILE_NAME);
            log::error!(
                "WAL: cannot read {} ({error}) — keeping the file as evidence",
                path.display()
            );
            return Vec::new();
        }
    };
    match reader.finish() {
        Ok(ops) => ops,
        Err(error) => {
            // `finish` logs the fault and quarantines; this arm exists so the
            // return type stays infallible, mirroring the legacy signature.
            log::error!("WAL: {}", error);
            reader.prefix_ops()
        }
    }
}

/// Streaming WAL reader used by recovery (`SessionManager::replay_message_wal`).
///
/// `next_op` reports an IO fault as `Err` instead of silently mapping it to
/// EOF, so the caller can keep the (still valid) ops it already has and skip
/// the destructive checkpoint.
pub struct WalReader {
    reader: WalSource,
    /// Bytes buffered from the source but not yet consumed by `read_line`.
    buffer: Vec<u8>,
    path: PathBuf,
    prefix: Vec<PersistOp>,
    first_line: bool,
    /// `Some(kind)` once an IO fault was reported; the reader is then "spent".
    failed_kind: Option<io::ErrorKind>,
    quarantined: bool,
}

impl WalReader {
    fn new(path: PathBuf, open: OpenFile) -> Self {
        Self {
            reader: open.reader,
            buffer: Vec::new(),
            path,
            prefix: Vec::new(),
            first_line: true,
            failed_kind: None,
            quarantined: false,
        }
    }

    /// True once this reader reported an IO fault. A caller that sees `true`
    /// must not checkpoint: the log contains ops that were never read.
    pub fn has_failed(&self) -> bool {
        self.failed_kind.is_some()
    }

    /// Number of ops parsed so far (the valid prefix). Used for logging.
    pub fn prefix_len(&self) -> usize {
        self.prefix.len()
    }

    /// The ops parsed so far. Only needed by the infallible [`read_ops`]
    /// wrapper; callers with a durability decision stream via [`Self::next_op`].
    fn prefix_ops(&mut self) -> Vec<PersistOp> {
        std::mem::take(&mut self.prefix)
    }

    /// Next op, or `Ok(None)` at a clean end of log (including a torn tail,
    /// which is quarantined but needs no caller decision).
    pub fn next_op(&mut self) -> io::Result<Option<PersistOp>> {
        if let Some(kind) = self.failed_kind {
            return Err(io::Error::new(kind, "WAL: read already failed"));
        }
        loop {
            let line = match self.read_line() {
                Ok(line) => line,
                Err(error) => return Err(self.fail_io(error)),
            };
            if line.is_empty() {
                return Ok(None);
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            if self.first_line {
                // Header is skipped; a corrupt header line falls through to the
                // parse arm and is quarantined like any other torn line.
                self.first_line = false;
                match serde_json::from_str::<WalHeader>(trimmed) {
                    Ok(header) if header.kind == HEADER_KIND => continue,
                    _ => return Ok(self.fail_corrupt(trimmed)),
                }
            }
            match serde_json::from_str::<WalLineRead>(trimmed) {
                Ok(entry) => {
                    self.prefix.push(entry.op.clone());
                    return Ok(Some(entry.op));
                }
                Err(_) => return Ok(self.fail_corrupt(trimmed)),
            }
        }
    }

    /// Drain the log, returning the ops it contained. A torn tail is normal
    /// recovery input; an IO fault is an error (the file was quarantined).
    pub fn finish(&mut self) -> Result<Vec<PersistOp>, WalReadError> {
        loop {
            match self.next_op() {
                Ok(Some(_)) => {}
                Ok(None) => return Ok(self.prefix.clone()),
                Err(error) => {
                    log::error!("WAL: {error}");
                    return Err(WalReadError::Io(error));
                }
            }
        }
    }

    /// One line for the open-time probe. A fault here is an *open* failure:
    /// unreadable heads must never be reported as "empty log".
    fn probe_head(&mut self) -> io::Result<Option<String>> {
        match self.read_line() {
            Ok(line) if line.is_empty() => Ok(None),
            Ok(line) => Ok(Some(line)),
            Err(error) => Err(io::Error::new(
                error.kind(),
                format!(
                    "WAL: cannot read the head of {}: {error} (keeping it as evidence)",
                    self.path.display()
                ),
            )),
        }
    }

    /// Read one line (newline-terminated, newline included) or `Ok(String::new())`
    /// at a clean EOF. Bytes come from the source in bounded chunks, so a device
    /// that fails mid-line surfaces here as `Err` — never as a short line.
    fn read_line(&mut self) -> io::Result<String> {
        let mut line: Vec<u8> = Vec::new();
        loop {
            if let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let taken = newline + 1;
                line.extend_from_slice(&self.buffer[..taken]);
                self.buffer.drain(..taken);
                break;
            }
            line.append(&mut self.buffer);
            let chunk = self.reader.read_chunk(LINE_CHUNK)?;
            if chunk.is_empty() {
                break;
            }
            self.buffer = chunk;
        }
        // Charge per completed line: the source turns the plan's byte limit into
        // a refusal once this pass has consumed it, so the ops before the limit
        // are handed to the caller and the fault surfaces on the next line.
        self.reader.charge(line.len() as u64)?;
        Ok(String::from_utf8_lossy(&line).into_owned())
    }

    /// Discard everything read so far and restart at `position` (used after the
    /// open-time probe, which walks the file only to validate its head).
    fn restart_at(&mut self, position: u64) -> io::Result<()> {
        self.reader.seek(position)?;
        self.buffer.clear();
        Ok(())
    }

    /// IO fault: log, quarantine the whole file, remember it for the caller.
    fn fail_io(&mut self, error: io::Error) -> io::Error {
        let kind = error.kind();
        let message = error.to_string();
        self.record(WalReadError::Io(error));
        io::Error::new(kind, message)
    }

    /// Parse failure at a "clean" offset: torn tail. Quarantine, stop.
    fn fail_corrupt(&mut self, line: &str) -> Option<PersistOp> {
        let error = match serde_json::from_str::<WalLineRead>(line) {
            Err(error) => error,
            Ok(_) => return None,
        };
        let quarantine = quarantine_path(&self.path);
        log::error!(
            "WAL: corrupt line in {} ({error}) — quarantining to {}",
            self.path.display(),
            quarantine.display()
        );
        self.quarantine_to(&quarantine);
        None
    }

    /// Log the fault, quarantine the file, remember that this reader must not
    /// be followed by a checkpoint.
    fn record(&mut self, error: WalReadError) -> WalReadError {
        let quarantine = io_quarantine_path(&self.path);
        if let WalReadError::Io(source) = &error {
            log::error!(
                "WAL: read {} failed after {} op(s) ({source}) — quarantining to {} \
                 (not truncating)",
                self.path.display(),
                self.prefix.len(),
                quarantine.display()
            );
            self.failed_kind = Some(source.kind());
            self.quarantine_to(&quarantine);
        }
        error
    }

    fn quarantine_to(&mut self, quarantine: &Path) {
        if self.quarantined {
            return;
        }
        self.quarantined = true;
        if let Err(copy_error) = fs::copy(&self.path, quarantine) {
            log::error!("WAL: quarantine copy failed: {copy_error}");
        }
    }
}

/// Errors a recovery read can report. Both variants mean "the caller must not
/// destroy this log": evidence has been quarantined next to it.
#[derive(Debug)]
pub enum WalReadError {
    /// The log could not be opened at all.
    Open(io::Error),
    /// A mid-file read failed (disk EIO, sharing violation, AV interference).
    Io(io::Error),
}

impl WalReadError {
    fn source_error(&self) -> &io::Error {
        match self {
            Self::Open(error) | Self::Io(error) => error,
        }
    }

    pub fn kind(&self) -> io::ErrorKind {
        self.source_error().kind()
    }
}

impl std::fmt::Display for WalReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open(error) => write!(f, "cannot open WAL: {error}"),
            Self::Io(error) => write!(f, "WAL read failed: {error}"),
        }
    }
}

impl std::error::Error for WalReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source_error())
    }
}

/// Open the WAL for reading.
///
/// Production: a plain [`File::open`]. Test builds route through a deterministic
/// injection seam (see `mod io_fault_tests`): a real disk cannot be asked to
/// fail on a specific read, and we want this failure path exercised in CI.
fn open_file_for_read(path: &Path) -> io::Result<WalSource> {
    #[cfg(any(test, feature = "test-harness"))]
    {
        io_fault_tests::open_with_fault(path)
    }
    #[cfg(not(any(test, feature = "test-harness")))]
    {
        WalSource::open(path)
    }
}

/// Read source shared by production and tests.
///
/// The trait exists so the fault-injection seam (test builds only, see
/// `mod io_fault_tests`) is exactly the same code path as production: the
/// reader above never learns whether the bytes come from a healthy disk or from
/// a device that fails after N bytes.
trait ByteSource: Send {
    /// Read up to `limit` bytes; `Ok(Vec::new())` is a clean EOF. `limit` bounds
    /// the *pass* (production: 64 KiB) and doubles as the interleaving point
    /// where a fault may surface — checks belong to the source, not the reader.
    fn read_chunk(&mut self, limit: usize) -> io::Result<Vec<u8>>;

    /// Reposition the source. The reader only ever seeks forward (probe →
    /// rewind) or back to a position it has already read.
    fn seek(&mut self, position: u64) -> io::Result<()>;

    /// Account for `bytes` the reader accepted, failing when the injected fault
    /// plan says this pass is done. Production is a no-op.
    fn charge(&mut self, bytes: u64) -> io::Result<()>;
}

/// Production read source: the file itself, plus an optional fault plan that
/// test builds arm from the process-global slot.
///
/// The plan is a **byte budget for the current pass** rather than an absolute
/// read count, because the reader seeks: the open-time probe walks the file,
/// then rewinds and replays. Rewinding resets the budget, so a disk that goes
/// bad mid-file (disk EIO / Windows sharing violation / AV interference) fails
/// again on replay instead of "healing" — the injected fault stays observable
/// from the caller, which is the whole point of the fix.
struct WalSource {
    /// Bytes already pulled from the file, kept so the probe's forward pass and
    /// the replay pass share one buffer instead of re-reading the device.
    buffer: Vec<u8>,
    /// Cursor into `buffer`.
    cursor: usize,
    /// True once the file reported a clean EOF.
    eof: bool,
    /// Injected fault plan (test builds only; `None` in production).
    #[cfg(any(test, feature = "test-harness"))]
    fault: Option<FaultPlan>,
    /// Forward pass number: 1 = open-time probe, 2 = replay.
    pass: usize,
    /// Bytes handed out in the current pass.
    pass_bytes: u64,
    file: File,
}

/// Injected read fault: fail after `skip_bytes` of the `pass`-th forward pass.
///
/// The pass distinction matters: a device that fails *mid-file* still lets the
/// file open (the head is readable), and the failure shows up on the next full
/// walk — which is the replay. Passing `pass = 1` models "the head cannot be
/// read at all" (sharing violation, permission loss).
#[cfg(any(test, feature = "test-harness"))]
#[derive(Clone, Copy)]
pub struct FaultPlan {
    pub pass: usize,
    pub skip_bytes: u64,
    pub kind: io::ErrorKind,
}

impl WalSource {
    fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            buffer: Vec::new(),
            cursor: 0,
            eof: false,
            #[cfg(any(test, feature = "test-harness"))]
            fault: None,
            pass: 1,
            pass_bytes: 0,
            file: File::open(path)?,
        })
    }

    #[allow(clippy::io_other_error)] // preserves the injected fault kind
    fn faulted(&self) -> io::Error {
        #[cfg(any(test, feature = "test-harness"))]
        let kind = self.fault.map_or(io::ErrorKind::Other, |plan| plan.kind);
        #[cfg(not(any(test, feature = "test-harness")))]
        let kind = io::ErrorKind::Other;
        if kind == io::ErrorKind::Other {
            io::Error::other("injected WAL read fault")
        } else {
            io::Error::new(kind, "injected WAL read fault")
        }
    }

    /// Start a new forward pass at `position` (rewind / restart).
    fn advance_pass(&mut self, position: u64) {
        self.pass += 1;
        self.pass_bytes = position;
    }

    /// Bytes the current pass may still read before the injected fault fires.
    /// `None` in production and on passes the plan does not target.
    fn remaining_budget(&self) -> Option<u64> {
        #[cfg(any(test, feature = "test-harness"))]
        if let Some(plan) = self.fault
            && plan.pass == self.pass
        {
            return Some(plan.skip_bytes.saturating_sub(self.pass_bytes));
        }
        None
    }
}

impl ByteSource for WalSource {
    fn read_chunk(&mut self, limit: usize) -> io::Result<Vec<u8>> {
        if self.cursor >= self.buffer.len() && !self.eof {
            // Never read past the fault point: the plan ends on a byte boundary
            // inside the file, so capping the fill is what makes "the next read
            // fails" observable instead of the whole log arriving in one 64 KiB
            // gulp and the fault never firing.
            let want = match self.remaining_budget() {
                Some(0) => return Err(self.faulted()),
                Some(remaining) => (remaining as usize).clamp(1, limit),
                None => limit,
            };
            let mut chunk = vec![0u8; want];
            match self.file.read(&mut chunk) {
                Ok(0) => self.eof = true,
                Ok(read) => {
                    chunk.truncate(read);
                    self.buffer.extend_from_slice(&chunk);
                }
                Err(error) => return Err(error),
            }
        }
        let end = (self.cursor + limit).min(self.buffer.len());
        let chunk = self.buffer[self.cursor..end].to_vec();
        self.cursor = end;
        Ok(chunk)
    }

    fn seek(&mut self, position: u64) -> io::Result<()> {
        if position <= self.buffer.len() as u64 {
            // Rewinding into bytes already pulled from the device: a pure cursor
            // move. A rewind starts the next pass, so a plan targeting the
            // replay fires there instead of "healing" after the probe.
            self.cursor = position as usize;
            self.advance_pass(position);
            return Ok(());
        }
        Seek::seek(&mut self.file, SeekFrom::Start(position))?;
        self.buffer.clear();
        self.cursor = 0;
        self.eof = false;
        self.advance_pass(position);
        Ok(())
    }

    /// Account for `bytes` the reader accepted. The fault is reported only when
    /// the pass crosses the plan's limit, i.e. after the valid prefix has been
    /// handed to the caller — the interesting case is "ops before the fault
    /// survive", not "the first read fails".
    /// Account for `bytes` the reader accepted.
    ///
    /// The plan's `skip_bytes` is the size of the readable prefix, so the line
    /// ending exactly at the limit is delivered to the caller; the fault fires
    /// on the *following* read, which is how a disk going bad mid-file behaves.
    fn charge(&mut self, bytes: u64) -> io::Result<()> {
        let over = self.remaining_budget().is_some_and(|left| bytes > left);
        self.pass_bytes += bytes;
        if over {
            return Err(self.faulted());
        }
        Ok(())
    }
}

/// Test-only handle used by downstream crates (`qaqh-session`) to drive the
/// recovery path with a deterministic read fault.
#[cfg(any(test, feature = "test-harness"))]
pub(crate) mod fault_harness {
    pub use super::FaultPlan;
    pub use super::io_fault_tests::{FaultArm, arm, prefix_bytes};
}

/// File opened by the probe in [`OpenFile::at`], handed to [`WalReader`].
struct OpenFile {
    reader: WalSource,
}

impl OpenFile {
    /// Open `path` while checking that the readable prefix is still intact.
    ///
    /// Two production failure modes are indistinguishable from "nothing to
    /// replay" if the open itself is all we do:
    /// - `PermissionDenied` (Windows sharing violation / AV lock) — returned as
    ///   `Err` here, never as "empty log";
    /// - a fault *during* the probe read with a valid prefix already read
    ///   (e.g. the device fails after `next_seq` parsed): the prefix is
    ///   returned together with `io_error`, so the caller stops before the bad
    ///   line instead of appending a fresh log over unread ops.
    fn at(path: &Path) -> io::Result<Option<Self>> {
        let source = match open_file_for_read(path) {
            Ok(source) => source,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut reader = WalReader::new(path.to_path_buf(), OpenFile { reader: source });
        while let Some(line) = reader.probe_head()? {
            if line.trim_end().is_empty() || self::is_header_line(line.trim_end()) {
                continue;
            }
            // First op line: the head is intact, so the reader replays from the
            // beginning instead of appending a fresh log over ops that were
            // never read.
            reader.restart_at(0)?;
            return Ok(Some(OpenFile {
                reader: reader.reader,
            }));
        }
        // Clean EOF with no op line: nothing to replay (the head was readable,
        // so no op was lost).
        Ok(Some(OpenFile {
            reader: reader.reader,
        }))
    }
}

/// True when `line` is a valid WAL header line.
fn is_header_line(line: &str) -> bool {
    serde_json::from_str::<WalHeader>(line).is_ok_and(|header| header.kind == HEADER_KIND)
}

/// Reset the WAL file to a bare header after a **complete** replay.
///
/// Fail-closed preconditions (defense in depth — recovery should have bailed
/// out earlier, but this is the function that destroys data):
/// - the log must be readable when the caller gets here; an IO error aborts the
///   truncation and keeps the ops alive for the next attempt;
/// - the whole log must have been read without an IO fault. A torn tail is
///   *expected* (crash mid-write), so it does not refuse the checkpoint: the
///   ops before it were applied and truncating them is the point.
///
/// `Ok(false)` means "not checkpointed", never "truncated anyway".
pub fn checkpoint_file(session_dir: &Path) -> io::Result<bool> {
    let _legacy_writer = LegacyWriterFacade::lock();
    let path = session_dir.join(WAL_FILE_NAME);
    match open_reader(session_dir)? {
        None => Ok(true),
        Some(mut reader) => {
            let intact = match reader.finish() {
                Ok(_) => true,
                Err(error) => {
                    log::error!(
                        "WAL: checkpoint {} refused — {error} (log kept verbatim)",
                        path.display()
                    );
                    false
                }
            };
            if !intact {
                return Ok(false);
            }
            write_header(&path, 1)?;
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::PersistOp;
    use qaqh_types::Message;

    fn append_op(seed: &str, ids: u64) -> PersistOp {
        PersistOp::Append {
            seed: seed.to_string(),
            messages: vec![Message {
                msg_id: Some(ids),
                role: "user".into(),
                name: None,
                content: vec![qaqh_types::ContentBlock::text("hello")],
            }],
            model: "m".into(),
            effort: None,
            compact_skip: 0,
            compact_covered_through_msg_id: None,
            turn_count: 1,
        }
    }

    #[test]
    fn log_read_and_checkpoint_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut writer = WalWriter::open(dir.path()).expect("open");
        writer.log_op(&append_op("s", 1)).expect("log 1");
        writer.log_op(&append_op("s", 2)).expect("log 2");
        writer.sync().expect("sync");

        let ops = read_ops(dir.path());
        assert_eq!(ops.len(), 2);

        writer.checkpoint().expect("checkpoint");
        assert!(read_ops(dir.path()).is_empty());
    }

    #[test]
    fn torn_tail_keeps_prefix_and_survives_reload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut writer = WalWriter::open(dir.path()).expect("open");
        writer.log_op(&append_op("s", 1)).expect("log 1");
        writer.sync().expect("sync");
        let path = dir.path().join(WAL_FILE_NAME);
        // Simulate a crash mid-write: append a truncated JSON line.
        {
            let mut f = OpenOptions::new().append(true).open(&path).expect("append");
            f.write_all(b"{\"seq\":2,\"op\":{\"App").expect("torn");
        }
        let ops = read_ops(dir.path());
        assert_eq!(ops.len(), 1, "prefix before the torn line must survive");
        // Quarantine copy exists next to the log.
        let has_quarantine = std::fs::read_dir(dir.path())
            .expect("readdir")
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains("wal.corrupt-"));
        assert!(has_quarantine, "torn WAL must be quarantined, not deleted");
    }

    #[test]
    fn reopened_writer_continues_after_checkpoint() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let mut writer = WalWriter::open(dir.path()).expect("open");
            writer.log_op(&append_op("s", 1)).expect("log");
            writer.checkpoint().expect("checkpoint");
        }
        let mut writer = WalWriter::open(dir.path()).expect("reopen");
        writer.log_op(&append_op("s", 2)).expect("log after reopen");
        assert_eq!(read_ops(dir.path()).len(), 1);
    }
}

/// Regression tests for BUG-2026-09-13-06: an IO error while reading the WAL
/// must not be reported as "empty log", and a checkpoint must never truncate a
/// log whose ops were not fully read.
///
/// The fault is injected at the `File` syscall boundary (a `File` handle is a
/// real fd, so no wrapper reader can be substituted): `open_file_for_read`
/// returns a file already rewound and pre-seeked past the injected bytes of
/// sequential `read` calls, so a mid-stream EIO becomes deterministic without
/// touching the fd itself.
#[cfg(any(test, feature = "test-harness"))]
mod io_fault_tests {
    use super::*;
    use crate::effect::PersistOp;
    use std::cell::RefCell;

    // Fault plan for the *current thread*.
    //
    // Thread-local, not global: the plan is read on the same thread that armed
    // it, so arming never has to take a lock — a lock would deadlock, because
    // the armed test calls straight into `open_file_for_read`, which is exactly
    // where the plan is consumed.
    thread_local! {
        static PLAN: RefCell<Option<FaultConfig>> = const { RefCell::new(None) };
    }

    /// Aliased locally so the test bodies read naturally.
    use super::FaultPlan as FaultConfig;

    /// Arm the fault and return a guard that disarms it on drop.
    pub fn arm(config: FaultConfig) -> FaultArm {
        PLAN.with(|plan| *plan.borrow_mut() = Some(config));
        FaultArm
    }

    pub struct FaultArm;

    impl Drop for FaultArm {
        fn drop(&mut self) {
            PLAN.with(|plan| *plan.borrow_mut() = None);
        }
    }

    /// Test replacement for [`open_file_for_read`]: copies the armed plan into
    /// the fresh source. Reads the plan lock-free (see [`PLAN`]).
    pub(super) fn open_with_fault(path: &Path) -> io::Result<WalSource> {
        let mut source = WalSource::open(path)?;
        PLAN.with(|plan| {
            if let Some(config) = *plan.borrow() {
                source.fault = Some(config);
            }
        });
        Ok(source)
    }

    #[cfg(test)]
    const FAULT_MESSAGE: &str = "injected WAL read fault";

    /// Byte offset just past `ops` op lines (header included) — i.e. the size of
    /// the prefix a reader can hand out before the fault must fire.
    pub fn prefix_bytes(ops: usize, op: &PersistOp) -> u64 {
        let header = WalHeader {
            kind: HEADER_KIND.to_string(),
            next_seq: 1,
        };
        let header_len = serde_json::to_string(&header).expect("header json").len() + 1;
        let line_len = serde_json::to_string(&WalLineWrite { seq: 1, op })
            .expect("line json")
            .len()
            + 1;
        header_len as u64 + ops as u64 * line_len as u64
    }

    #[cfg(test)]
    #[test]
    fn io_fault_after_the_prefix_is_not_reported_as_an_empty_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_fixture(dir.path(), 3);
        let prefix = prefix_bytes(1, &fixture_op(0));

        let _armed = arm(FaultConfig {
            pass: 2,
            skip_bytes: prefix,
            kind: io::ErrorKind::Other,
        });
        let mut reader = open_reader(dir.path())
            .expect("open must not fail")
            .expect("log present");

        let first = reader.next_op().expect("op 1").expect("op present");
        assert!(matches!(first, PersistOp::Append { .. }));

        // The regression: this used to be an indistinguishable `Ok(None)`.
        let error = reader
            .next_op()
            .expect_err("an IO fault must not look like EOF");
        assert!(error.to_string().contains(FAULT_MESSAGE), "{error}");
        assert!(reader.has_failed(), "the reader must be marked as faulted");

        assert_eq!(reader.prefix_len(), 1, "valid prefix is preserved");
        assert_eq!(
            quarantine_files(dir.path(), "wal.unreadable-").len(),
            1,
            "the faulting log must be quarantined as evidence"
        );
        assert!(
            dir.path().join(WAL_FILE_NAME).exists(),
            "quarantine is a copy: the original is never deleted"
        );
    }

    #[cfg(test)]
    #[test]
    fn read_ops_keeps_the_prefix_and_quarantines_the_unreadable_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_fixture(dir.path(), 3);
        let _armed = arm(FaultConfig {
            pass: 2,
            skip_bytes: prefix_bytes(1, &fixture_op(0)),
            kind: io::ErrorKind::Other,
        });

        let ops = read_ops(dir.path());

        assert_eq!(ops.len(), 1, "the valid prefix is returned, not dropped");
        assert_eq!(
            quarantine_files(dir.path(), "wal.corrupt-").len(),
            0,
            "an IO fault is not a torn tail"
        );
        assert_eq!(
            quarantine_files(dir.path(), "wal.unreadable-").len(),
            1,
            "an IO fault must be quarantined, not treated as EOF"
        );
        assert!(
            dir.path().join(WAL_FILE_NAME).exists(),
            "the unreadable log must survive for the next recovery attempt"
        );
    }

    #[cfg(test)]
    #[test]
    fn an_unreadable_log_is_detected_at_open_instead_of_being_called_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_fixture(dir.path(), 1);

        let _armed = arm(FaultConfig {
            pass: 1,
            skip_bytes: 0,
            kind: io::ErrorKind::PermissionDenied,
        });

        assert!(
            open_reader(dir.path()).is_err(),
            "a log that cannot be read must never be reported as an empty log"
        );
        assert!(
            dir.path().join(WAL_FILE_NAME).exists(),
            "the unreadable log stays on disk as evidence"
        );
    }

    #[cfg(test)]
    #[test]
    fn checkpoint_refuses_to_truncate_an_unreadable_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_fixture(dir.path(), 2);

        let _armed = arm(FaultConfig {
            pass: 2,
            skip_bytes: prefix_bytes(1, &fixture_op(0)),
            kind: io::ErrorKind::Other,
        });
        let checkpointed = checkpoint_file(dir.path());

        assert!(
            !matches!(checkpointed, Ok(true)),
            "a faulted log must not be checkpointed"
        );
        assert_eq!(
            quarantine_files(dir.path(), "wal.unreadable-").len(),
            1,
            "the refusal must leave evidence"
        );

        // Disarm and prove the unread op is still replayable.
        drop(_armed);
        let ops = read_ops(dir.path());
        assert_eq!(ops.len(), 2, "the unread ops must still be replayable");
    }

    /// A clean log (no fault armed) is checkpointed for real, and a torn tail is
    /// refused — the preconditions are not vacuous.
    #[cfg(test)]
    #[test]
    fn checkpoint_still_truncates_a_clean_log_and_refuses_a_torn_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_fixture(dir.path(), 2);
        assert!(checkpoint_file(dir.path()).expect("checkpoint"));
        assert!(read_ops(dir.path()).is_empty(), "clean log is truncated");

        write_fixture(dir.path(), 1);
        let path = dir.path().join(WAL_FILE_NAME);
        {
            let mut file = OpenOptions::new().append(true).open(&path).expect("append");
            file.write_all(b"{\"seq\":2,\"op\":{\"Up")
                .expect("torn write");
        }
        // A torn tail is normal crash input: the valid prefix was replayed, so
        // the checkpoint proceeds — and the quarantine copy still holds the
        // torn bytes as evidence.
        assert!(
            checkpoint_file(dir.path()).expect("checkpoint call"),
            "a torn tail does not refuse the checkpoint (the prefix was replayed)"
        );
        assert!(
            read_ops(dir.path()).is_empty(),
            "the prefix was checkpointed"
        );
        assert_eq!(
            quarantine_files(dir.path(), "wal.corrupt-").len(),
            1,
            "the torn remainder is kept as evidence"
        );
    }

    /// The op `write_fixture` logs.
    ///
    /// `msg_id` varies per line, so the byte offset helper measures the *first*
    /// line and the fixture keeps every line the same length by padding the text.
    #[cfg(test)]
    fn fixture_op(index: usize) -> PersistOp {
        let text = format!("line-{index:04}");
        PersistOp::Append {
            seed: "s".into(),
            messages: vec![qaqh_types::Message {
                msg_id: Some(1),
                role: "user".into(),
                name: None,
                content: vec![qaqh_types::ContentBlock::text(&text)],
            }],
            model: "m".into(),
            effort: None,
            compact_skip: 0,
            compact_covered_through_msg_id: None,
            turn_count: 1,
        }
    }

    /// Write a valid header plus `ops` op lines; no fault is armed here.
    #[cfg(test)]
    fn write_fixture(dir: &Path, ops: usize) {
        let mut writer = WalWriter::open(dir).expect("open");
        for index in 0..ops {
            writer.log_op(&fixture_op(index)).expect("log op");
        }
        writer.sync().expect("sync");
    }

    #[cfg(test)]
    fn quarantine_files(dir: &Path, marker: &str) -> Vec<String> {
        std::fs::read_dir(dir)
            .expect("readdir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(marker))
            .collect()
    }
}
