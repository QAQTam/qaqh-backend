//! File state tracker — records all file operations for context injection.
//!
//! Generates a compact XML summary injected into the [Environment] block
//! at each turn, so the model always knows current file states without re-reading.
//!
//! Format: `<file_state>\n  path  200L  (edited)\n  ...\n</file_state>`

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

#[derive(Clone, Debug)]
struct FileEntry {
    op: &'static str,
    line_count: usize,
    order: u64, // monotonically increasing for recency sort
}

static STATE: OnceLock<Mutex<HashMap<String, FileEntry>>> = OnceLock::new();
static COUNTER: AtomicU64 = AtomicU64::new(0);
const MAX_SUMMARY_FILES: usize = 20;

fn state() -> &'static Mutex<HashMap<String, FileEntry>> {
    STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_order() -> u64 {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn insert(path: &str, op: &'static str, line_count: usize) {
    let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
    s.insert(
        path.to_string(),
        FileEntry {
            op,
            line_count,
            order: next_order(),
        },
    );
}

/// Record a file read (full file, no range).
pub fn record_read(path: &str, content: &str, line_count: usize) {
    crate::file_cache::store(path, content, line_count);
    insert(path, "read", line_count);
}

/// Record a file write (create/overwrite/append).
pub fn record_write(path: &str, line_count: usize) {
    crate::file_cache::invalidate(path);
    let s = state().lock().unwrap_or_else(|e| e.into_inner());
    let op = if s.contains_key(path) {
        "edited"
    } else {
        "created"
    };
    drop(s);
    insert(path, op, line_count);
}

/// Record a file edit.
pub fn record_edit(path: &str, line_count: usize) {
    crate::file_cache::invalidate(path);
    insert(path, "edited", line_count);
}

/// Record a file deletion.
pub fn record_delete(path: &str) {
    crate::file_cache::invalidate(path);
    insert(path, "deleted", 0);
}

/// Record a file move (both source and dest).
pub fn record_move(source: &str, dest: &str) {
    crate::file_cache::invalidate(source);
    crate::file_cache::invalidate(dest);
    let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
    s.remove(source);
    s.insert(
        dest.to_string(),
        FileEntry {
            op: "moved",
            line_count: 0,
            order: next_order(),
        },
    );
}

/// Generate file state summary. Capped at 20 most recently touched files.
pub fn summary() -> String {
    let s = state().lock().unwrap_or_else(|e| e.into_inner());
    if s.is_empty() {
        return String::new();
    }
    let mut entries: Vec<(&String, &FileEntry)> = s.iter().collect();
    entries.sort_by_key(|(_, e)| -(e.order as i64)); // most recent first
    let total = entries.len();
    entries.truncate(MAX_SUMMARY_FILES);

    let mut out = String::from("<file_state>\n");
    for (path, e) in &entries {
        let lines = if e.line_count > 0 {
            format!("{}L", e.line_count)
        } else {
            String::new()
        };
        out.push_str(&format!(
            "  {:<50} {:>6}  ({})\n",
            path,
            if lines.is_empty() { "—" } else { &lines },
            e.op,
        ));
    }
    if total > MAX_SUMMARY_FILES {
        out.push_str(&format!(
            "  ... ({} more files)\n",
            total - MAX_SUMMARY_FILES
        ));
    }
    out.push_str("</file_state>");
    out
}

/// Clear all tracked state (session reset).
pub fn clear() {
    state().lock().unwrap_or_else(|e| e.into_inner()).clear();
}
