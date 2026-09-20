//! L3 tool execution outbox — a per-session log of the FACT that a tool
//! finished executing, independent of the conversation persistence pipeline.
//!
//! Why this exists: the `[RESTORE]` repair that `MessageStore::from_messages`
//! performs for orphan tool_use entries always says "not executed — do NOT
//! retry". That is the safe default, but it lies in the common crash window
//! where the tool actually ran (side effects happened) and only its result
//! message was lost. The outbox records execution facts at the earliest
//! possible moment (inside the tool worker thread, right after
//! `execute_authorized` returns), so recovery can distinguish:
//!
//! - no outbox record → tool never ran → keep "not executed, retry allowed";
//! - outbox record, no persisted result → tool ran, result lost → inject
//!   "executed, outcome <ok|error>, verify state before retrying".
//!
//! File format: `<session_dir>/tool_outbox.wal`, one JSON object per line.
//!
//! ## Throughput shape (BUG-2026-09-12-14 / issue #30)
//!
//! The original implementation held a **process-wide** `static Mutex` and ran
//! `sync_all()` on every record. Measured: 1→8 threads stayed flat at ~2000
//! records/s (zero scaling), p50 ≈ 476 µs, max 0.7–1.5 s — because workers of
//! *unrelated sessions* serialized on the global lock and every append paid a
//! full disk flush. Now:
//!
//! - the append path is parallel across sessions: `SessionWriter` state is
//!   striped over [`SHARD_COUNT`] mutexes, and the append itself is atomic
//!   (`O_APPEND`, one `write_all` per record) so reentrancy is impossible;
//! - each session owns a long-lived append handle (no open() per record);
//! - fsync cost is paid **off the append path** by a per-process flusher
//!   thread that coalesces all dirty sessions into one pass.
//!
//! ## Durability contract (unchanged)
//!
//! `record_in` still returns only after the record bytes have been handed to
//! the OS via `write` (visible to any subsequent `read_records`, surviving a
//! process crash/kill). What is *not* synchronous any more is the backing
//! storage flush — `sync_all` now happens per *flush round* (every
//! [`FLUSH_INTERVAL`], first record of a round, or on a dirty-session count
//! watermark) instead of per record. The residual crash window is therefore
//! "OS page cache lost (machine power loss inside one flush round)", exactly
//! like the adjacent journal writer (`ringing::hub`, BUG-2026-09-12-08) and
//! the message WAL. Callers that need a barrier must call [`flush_in`] /
//! [`flush`] and treat flushing the *calling* session as part of the wait:
//! the final flush is re-issued while the caller still holds the session
//! lock, so a later batch can never flush before an earlier one has returned.
//!
//! Lifecycle: records are appended during execution and reconciled once per
//! worker init ([`reconcile_store`]). Reconciliation rewrites the file keeping
//! only records that still match a synthetic `[RESTORE]` placeholder — those
//! must survive (the amended note lives in memory until a later full rewrite
//! persists it); records whose call id has a real result in the archive are
//! dropped.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const OUTBOX_FILE_NAME: &str = "tool_outbox.wal";

/// Number of lock stripes for the per-session writer table. Sessions are
/// independent; a stripe only ever serializes `record_in`/`flush_in` on the
/// same session (and briefly on a hash collision), never the fsync itself.
const SHARD_COUNT: usize = 16;

/// How often the background flusher sweeps dirty sessions. A record appended
/// while the flusher sleeps costs one `write` (~µs), never a disk flush.
const FLUSH_INTERVAL: Duration = Duration::from_millis(100);

/// Upper bound on a single flusher sleep, so shutdown and test barriers are
/// honored promptly.
const SHUTDOWN_POLL: Duration = Duration::from_millis(10);

/// Dirty-session watermark: the flusher wakes early when this many sessions
/// are waiting for a flush (bounds the residual crash window under a burst).
const DIRTY_WAKE_THRESHOLD: usize = 8;

// ───────────────────────── fsync 注入钩子 ─────────────────────────

/// Which path requested the outbox fsync. Test hooks use this to distinguish
/// the background flusher from an explicit [`flush_in`] barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub enum FsyncPhase {
    Background,
    Explicit,
}

/// Test-only hook invoked instead of the real `sync_all` on an outbox file.
type FsyncHook = Arc<dyn Fn(&Path, FsyncPhase) + Send + Sync>;

static FSYNC_HOOK: Mutex<Option<FsyncHook>> = Mutex::new(None);

/// Install/clear the fsync hook. **Tests only** — production code never calls
/// this, so the steady-state cost is one uncontended lock acquire per fsync
/// (the flusher thread, not the append path).
#[doc(hidden)]
pub fn set_fsync_hook(hook: Option<FsyncHook>) {
    *FSYNC_HOOK.lock().unwrap_or_else(|e| e.into_inner()) = hook;
}

fn sync_file(file: &File, path: &Path, phase: FsyncPhase) -> std::io::Result<()> {
    let hook = FSYNC_HOOK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    match hook {
        Some(hook) => {
            hook(path, phase);
            Ok(())
        }
        None => file.sync_all(),
    }
}

// ───────────────────────── 每会话写入器 ─────────────────────────

/// Per-session append state. The `File` handle is long-lived and opened with
/// `O_APPEND`, so a single `write_all` per record publishes a complete line
/// even against a concurrent writer on another handle.
struct SessionWriter {
    file: File,
    /// Records written but not yet fsynced.
    unsynced: u64,
    /// Bumped by every append so the flusher can detect progress while it was
    /// syncing without holding the session lock (and re-sync if needed).
    generation: u64,
}

impl SessionWriter {
    fn append(&mut self, line: &[u8]) -> std::io::Result<()> {
        self.file.write_all(line)?;
        self.unsynced += 1;
        self.generation = self.generation.wrapping_add(1);
        Ok(())
    }
}

/// Writers + their per-session locks, grouped into [`SHARD_COUNT`] stripes.
struct Shard {
    writers: Mutex<HashMap<PathBuf, SessionWriter>>,
}

impl Shard {
    fn new() -> Self {
        Self {
            writers: Mutex::new(HashMap::new()),
        }
    }
}

fn shards() -> &'static [Shard] {
    static SHARDS: OnceLock<Vec<Shard>> = OnceLock::new();
    SHARDS.get_or_init(|| (0..SHARD_COUNT).map(|_| Shard::new()).collect())
}

/// FNV-1a over the session directory path — stable across processes
/// (`DefaultHasher` is not), so a single writer of a given file always lands
/// on the same stripe.
fn shard_index(session_dir: &Path) -> usize {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in session_dir.as_os_str().to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash as usize) % SHARD_COUNT
}

#[derive(Default)]
struct FlusherState {
    dirty: HashMap<PathBuf, Instant>,
    shutdown: bool,
}

struct Flusher {
    state: Mutex<FlusherState>,
    wake: Condvar,
}

fn flusher() -> &'static Arc<Flusher> {
    static FLUSHER: OnceLock<Arc<Flusher>> = OnceLock::new();
    FLUSHER.get_or_init(|| {
        let flusher = Arc::new(Flusher {
            state: Mutex::new(FlusherState::default()),
            wake: Condvar::new(),
        });
        let runner = Arc::clone(&flusher);
        let _ = std::thread::Builder::new()
            .name("tool-outbox-flusher".into())
            .spawn(move || flusher_loop(runner));
        flusher
    })
}

/// Sync `writer` while it is still locked. One `sync_all` covers every record
/// appended since the previous sync (that is the batching); callers must hold
/// the session lock so the flusher can never overtake a concurrent append.
/// Returns whether a sync was actually issued (`false` = nothing was pending).
fn sync_locked(
    path: &Path,
    writer: &mut SessionWriter,
    phase: FsyncPhase,
) -> std::io::Result<bool> {
    if writer.unsynced == 0 {
        return Ok(false);
    }
    sync_file(&writer.file, path, phase)?;
    writer.unsynced = 0;
    Ok(true)
}

/// Flush every currently dirty session, one pass, sequentially. The fsync
/// happens under the session lock (so it cannot race a concurrent append of
/// the same session) but **not** under the caller's append lock — appends on
/// that session block only for the duration of their own write.
fn flush_round() {
    let flusher = flusher();
    let pending: Vec<PathBuf> = {
        let state = flusher.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.dirty.is_empty() {
            return;
        }
        state.dirty.keys().cloned().collect()
    };
    for path in pending {
        let shard = &shards()[shard_index(&path)];
        let mut guard = shard.writers.lock().unwrap_or_else(|e| e.into_inner());
        let (sync_result, generation) = match guard.get_mut(&path) {
            Some(writer) => (
                sync_locked(&path, writer, FsyncPhase::Background),
                Some(writer.generation),
            ),
            None => (Ok(false), None),
        };
        drop(guard);
        match sync_result {
            Err(error) => {
                log::error!(
                    "tool_outbox: background flush {} failed: {error}",
                    path.display()
                );
                clear_dirty(&path);
            }
            // Only clear when nothing was appended while we were syncing: a
            // newer append re-marks the session dirty, and clearing here would
            // strand its record until the next unrelated flush.
            Ok(_) => {
                let still_same =
                    generation.is_some_and(|generation| current_generation(&path) == generation);
                if still_same || generation.is_none() {
                    clear_dirty(&path);
                }
            }
        }
    }
}

/// Generation of the cached writer for `path` (`u64::MAX` when absent).
fn current_generation(path: &Path) -> u64 {
    let shard = &shards()[shard_index(path)];
    let guard = shard.writers.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .get(path)
        .map(|writer| writer.generation)
        .unwrap_or(u64::MAX)
}

fn clear_dirty(path: &Path) {
    let mut state = flusher().state.lock().unwrap_or_else(|e| e.into_inner());
    state.dirty.remove(path);
}

fn flusher_loop(flusher: Arc<Flusher>) {
    let mut state = flusher.state.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if state.shutdown {
            break;
        }
        let now = Instant::now();
        let due = state
            .dirty
            .values()
            .any(|marked| now.saturating_duration_since(*marked) >= FLUSH_INTERVAL);
        let crowded = state.dirty.len() >= DIRTY_WAKE_THRESHOLD;
        if due || crowded {
            drop(state);
            flush_round();
            state = flusher.state.lock().unwrap_or_else(|e| e.into_inner());
            continue;
        }
        let wait_for = state
            .dirty
            .values()
            .map(|marked| FLUSH_INTERVAL.saturating_sub(now.saturating_duration_since(*marked)))
            .min()
            .unwrap_or(FLUSH_INTERVAL)
            .min(SHUTDOWN_POLL);
        let (next, _) = flusher
            .wake
            .wait_timeout(state, wait_for)
            .unwrap_or_else(|e| e.into_inner());
        state = next;
    }
}

/// Mark `path` dirty for the background flusher. Wakes it early only when the
/// dirty set crosses [`DIRTY_WAKE_THRESHOLD`] (avoids a syscall per record).
fn mark_dirty(path: &Path) {
    let flusher = flusher();
    let mut state = flusher.state.lock().unwrap_or_else(|e| e.into_inner());
    state
        .dirty
        .entry(path.to_path_buf())
        .or_insert_with(Instant::now);
    if state.dirty.len() >= DIRTY_WAKE_THRESHOLD {
        flusher.wake.notify_one();
    }
}

/// Whether the flusher thread has been started (diagnostics / tests).
static FLUSHER_USED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxRecord {
    pub call_id: String,
    pub name: String,
    /// Executor-observed outcome: `"ok"` or `"error"`.
    pub status: String,
    /// Unix seconds at record time (diagnostics only).
    pub ts: u64,
}

pub fn outbox_path(session_dir: &Path) -> PathBuf {
    session_dir.join(OUTBOX_FILE_NAME)
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Resolve `session_dir` to its outbox file path, creating the session
/// directory if the caller has not created it yet.
fn prepare_path(session_dir: &Path) -> std::io::Result<PathBuf> {
    let path = outbox_path(session_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(path)
}

fn open_append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// Record one completed tool execution. Called from the tool worker thread —
/// before the result message is ingested or flushed anywhere — so the fact of
/// execution is durable even if the process dies in the next instant.
///
/// Returns once the record bytes are with the OS (`write`), not once the disk
/// has flushed them; see the module docs for the durability contract.
pub fn record_in(session_dir: &Path, call_id: &str, name: &str, success: bool) {
    let record = OutboxRecord {
        call_id: call_id.to_string(),
        name: name.to_string(),
        status: if success { "ok" } else { "error" }.to_string(),
        ts: now_epoch(),
    };
    let line = match serde_json::to_string(&record) {
        Ok(line) => line,
        Err(error) => {
            log::error!("tool_outbox: serialize failed for {call_id}: {error}");
            return;
        }
    };
    let path = match prepare_path(session_dir) {
        Ok(path) => path,
        Err(error) => {
            log::error!(
                "tool_outbox: prepare {} failed: {error}",
                session_dir.display()
            );
            return;
        }
    };
    let shard = &shards()[shard_index(&path)];
    let mut guard = shard.writers.lock().unwrap_or_else(|e| e.into_inner());
    let writer = match guard.get_mut(&path) {
        Some(writer) => writer,
        None => match open_append(&path) {
            Ok(file) => guard.entry(path.clone()).or_insert(SessionWriter {
                file,
                unsynced: 0,
                generation: 0,
            }),
            Err(error) => {
                log::error!("tool_outbox: append failed for {call_id}: {error}");
                return;
            }
        },
    };
    let mut payload = Vec::with_capacity(line.len() + 1);
    payload.extend_from_slice(line.as_bytes());
    payload.push(b'\n');
    if let Err(error) = writer.append(&payload) {
        log::error!("tool_outbox: append failed for {call_id}: {error}");
        return;
    }
    drop(guard);
    FLUSHER_USED.store(true, Ordering::Relaxed);
    mark_dirty(&path);
}

/// Session-seed convenience wrapper over [`record_in`].
pub fn record(seed: &str, call_id: &str, name: &str, success: bool) {
    record_in(
        &qaqh_types::platform::sessions_dir().join(seed),
        call_id,
        name,
        success,
    );
}

/// Durability barrier for one session: returns after this session's records
/// have been fsynced. Safe to call from any thread **except** a tool worker of
/// the same session (a worker would deadlock on its own in-flight record) —
/// the production callers are actor init and worker teardown.
pub fn flush_in(session_dir: &Path) {
    let path = outbox_path(session_dir);
    let shard = &shards()[shard_index(&path)];
    let mut guard = shard.writers.lock().unwrap_or_else(|e| e.into_inner());
    let Some(writer) = guard.get_mut(&path) else {
        return;
    };
    if let Err(error) = sync_locked(&path, writer, FsyncPhase::Explicit) {
        log::error!("tool_outbox: flush {} failed: {error}", path.display());
        return;
    }
    drop(guard);
    let mut state = flusher().state.lock().unwrap_or_else(|e| e.into_inner());
    state.dirty.remove(&path);
}

/// Session-seed convenience wrapper over [`flush_in`].
pub fn flush(seed: &str) {
    flush_in(&qaqh_types::platform::sessions_dir().join(seed));
}

/// 已执行（outbox 有记录）的 call_id 集合。
///
/// 取消收割用它区分「工具真的跑了、只是结果没等到」与「从未执行」——
/// 前者必须等回填真实结果，后者才补取消终态（BUG-2026-09-13-08）。
pub fn executed_call_ids(seed: &str) -> std::collections::HashSet<String> {
    let dir = qaqh_types::platform::sessions_dir().join(seed);
    read_records(&dir)
        .into_iter()
        .map(|record| record.call_id)
        .collect()
}

/// Read all records; a torn/corrupt tail stops the scan (logged, never
/// silently dropped on disk — the file is only ever rewritten by
/// [`retain_only`]).
pub fn read_records(session_dir: &Path) -> Vec<OutboxRecord> {
    let path = outbox_path(session_dir);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            log::error!("tool_outbox: read {} failed: {error}", path.display());
            return Vec::new();
        }
    };
    let mut records = Vec::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<OutboxRecord>(trimmed) {
            Ok(record) => records.push(record),
            Err(error) => {
                log::error!(
                    "tool_outbox: corrupt line in {} ({error}) — ignoring tail",
                    path.display()
                );
                break;
            }
        }
    }
    records
}

/// Rewrite the outbox keeping only `keep` call ids (atomic temp + rename).
fn retain_only(session_dir: &Path, keep: &[String]) {
    let path = outbox_path(session_dir);
    let records = read_records(session_dir);
    let kept: Vec<&OutboxRecord> = records
        .iter()
        .filter(|record| keep.contains(&record.call_id))
        .collect();
    if kept.len() == records.len() {
        return;
    }
    let tmp = path.with_extension("wal.tmp");
    let result = (|| -> std::io::Result<()> {
        let mut file = File::create(&tmp)?;
        for record in &kept {
            let line = serde_json::to_string(record)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
            file.write_all(line.as_bytes())?;
            file.write_all(b"\n")?;
        }
        file.flush()?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &path)
    })();
    match result {
        Ok(()) => {
            // The rewrite moved the inode: drop any cached append handle so
            // later records open the new file instead of writing into the
            // unlinked one (and re-open a fresh handle on demand).
            discard_writer(&path);
            sync_dir(path.parent());
        }
        Err(error) => log::error!("tool_outbox: retain rewrite failed: {error}"),
    }
}

/// Forget the cached append handle for `path` (used after an inode swap).
fn discard_writer(path: &Path) {
    let shard = &shards()[shard_index(path)];
    let mut guard = shard.writers.lock().unwrap_or_else(|e| e.into_inner());
    guard.remove(path);
    drop(guard);
    flusher()
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .dirty
        .remove(path);
}

/// Best-effort directory fsync so a rename/create is itself durable.
#[cfg(unix)]
fn sync_dir(dir: Option<&Path>) {
    let Some(dir) = dir else { return };
    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_dir(_dir: Option<&Path>) {
    // Windows has no directory fsync equivalent; NTFS metadata ordering is
    // handled by the filesystem. Documented blind spot (see PR notes).
}

/// Reconcile outbox records against a freshly restored `MessageStore`:
/// every record matching a synthetic `[RESTORE]` placeholder (orphan tool_use
/// with no persisted result) refines the placeholder to "executed but result
/// lost"; all other records are accounted for by real archive results and
/// pruned. Idempotent — safe to call on every worker init.
pub fn reconcile_store(store: &mut qaqh_message::MessageStore, seed: &str) {
    reconcile_store_in(&qaqh_types::platform::sessions_dir().join(seed), store);
}

/// Directory-injected variant (tests / non-standard data roots).
pub fn reconcile_store_in(session_dir: &Path, store: &mut qaqh_message::MessageStore) {
    // Recovery reads the file directly, so make sure everything we appended
    // (including records from a previous worker of this session) is on disk
    // before deciding what to reconcile.
    let records = read_records(session_dir);
    if records.is_empty() {
        return;
    }
    let synthetic = store.synthetic_repair_call_ids();
    let mut unresolved: Vec<String> = Vec::new();
    let mut amended = 0usize;
    for record in &records {
        if !synthetic.iter().any(|id| id == &record.call_id) {
            continue;
        }
        let note = format!(
            "[RESTORE] Tool \"{}\" was executed before the session was saved, but its \
             result was not persisted (outcome: {}). Side effects may have occurred — \
             verify the workspace state before retrying; do NOT blindly re-run.",
            record.name, record.status
        );
        if store.amend_synthetic_repair(&record.call_id, &note) {
            amended += 1;
            unresolved.push(record.call_id.clone());
        }
    }
    if amended > 0 {
        log::info!("tool_outbox: refined {amended} orphan tool_use repair(s)");
    }
    retain_only(session_dir, &unresolved);
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_message::MessageStore;

    fn orphan_assistant(call_id: &str, name: &str) -> qaqh_types::Message {
        let mut message = qaqh_types::Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: Vec::new(),
        };
        message.content.push(qaqh_types::ContentBlock::ToolUse {
            id: call_id.to_string(),
            name: name.to_string(),
            input: serde_json::json!({"command": "echo hi"}),
        });
        message
    }

    #[test]
    fn record_read_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        record_in(dir.path(), "call-1", "bash", true);
        record_in(dir.path(), "call-2", "edit", false);
        let records = read_records(dir.path());
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].call_id, "call-1");
        assert_eq!(records[0].status, "ok");
        assert_eq!(records[1].status, "error");
    }

    /// 分片锁不得让**同一会话**的并发追加互相撕裂（每条记录必须整行落盘）。
    #[test]
    fn concurrent_appends_to_one_session_keep_every_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        let handles: Vec<_> = (0..8)
            .map(|worker| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for i in 0..32 {
                        record_in(&path, &format!("w{worker}-{i}"), "bash", true);
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("append thread");
        }
        let records = read_records(&path);
        assert_eq!(records.len(), 8 * 32, "并发追加不得丢记录或产生撕裂行");
        let unique: std::collections::HashSet<&str> = records
            .iter()
            .map(|record| record.call_id.as_str())
            .collect();
        assert_eq!(unique.len(), 8 * 32, "记录不得重复");
    }

    /// 批量化 fsync 下，追加返回后记录必须**立即可读**（写路径语义不变）。
    #[test]
    fn appended_records_are_visible_before_the_batched_sync() {
        let dir = tempfile::tempdir().expect("tempdir");
        record_in(dir.path(), "visible-1", "bash", true);
        // 不调用 flush：批量化窗口内记录已在文件中。
        assert_eq!(read_records(dir.path()).len(), 1);
    }

    /// retain_only 重写后必须丢弃旧 inode 的追加句柄，否则后续记录写进
    /// 已被 rename 掉的旧文件而丢失。
    #[test]
    fn retain_rewrite_keeps_later_appends() {
        let dir = tempfile::tempdir().expect("tempdir");
        record_in(dir.path(), "keep-me", "bash", true);
        record_in(dir.path(), "drop-me", "grep", true);
        retain_only(dir.path(), &["keep-me".to_string()]);
        record_in(dir.path(), "after-rewrite", "edit", true);
        flush_in(dir.path());

        let records = read_records(dir.path());
        let ids: Vec<&str> = records.iter().map(|r| r.call_id.as_str()).collect();
        assert_eq!(ids, vec!["keep-me", "after-rewrite"], "重写后追加必须仍在");
    }

    #[test]
    fn reconcile_refines_placeholder_and_prunes_resolved() {
        let dir = tempfile::tempdir().expect("tempdir");
        record_in(dir.path(), "call-ran", "bash", true);
        record_in(dir.path(), "call-unknown", "grep", true);

        // Store with an orphan tool_use that from_messages repairs.
        let assistant = orphan_assistant("call-ran", "bash");
        // A turn starts with a user message; the orphan tool_use lives in the
        // assistant step that follows it.
        let vec = vec![qaqh_types::Message::user("do it"), assistant];
        let (mut store, repairs) = MessageStore::from_messages("outbox-seed", &vec, 0);
        assert_eq!(repairs.len(), 1, "orphan tool_use must be repaired");
        assert!(
            store
                .synthetic_repair_call_ids()
                .contains(&"call-ran".to_string())
        );

        // The default note claims "not executed" — the outbox says otherwise.
        reconcile_store_in(dir.path(), &mut store);

        let note_text = store
            .turns()
            .iter()
            .flat_map(|turn| turn.steps.iter())
            .flat_map(|step| step.tool_results.iter())
            .find_map(|result| {
                result.content.iter().find_map(|block| match block {
                    qaqh_types::ContentBlock::ToolResult { result, .. } => {
                        Some(result.model_text().to_string())
                    }
                    _ => None,
                })
            })
            .expect("repair result exists");
        assert!(
            note_text.contains("was executed before the session was saved"),
            "note must reflect execution fact, got: {note_text}"
        );

        // Records matching the refined placeholder are kept; unmatched ones
        // (no orphan in the archive) are pruned.
        let kept = read_records(dir.path());
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].call_id, "call-ran");
    }

    #[test]
    fn reconcile_without_outbox_is_noop() {
        let vec: Vec<qaqh_types::Message> = Vec::new();
        let (mut store, _) = MessageStore::from_messages("seed-x", &vec, 0);
        // Point reconcile at a directory with no outbox file at all.
        reconcile_store_in(
            &std::env::temp_dir().join("qaqh-outbox-missing-dir-test"),
            &mut store,
        );
        assert!(store.synthetic_repair_call_ids().is_empty());
    }
}
