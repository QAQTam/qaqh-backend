//! Persistent cross-session memory store.
//!
//! Stores agent decisions, fixes, and findings as embeddable text
//! entries that survive session restarts. Uses a simple JSON file
//! for persistence — no external database required.
//!
//! ## How it works
//!
//! 1. **Write**: Components call `MemoryStore::remember()` with context
//!    text and metadata (type, session_id, timestamp).
//! 2. **Persist**: The store auto-saves to `{data_dir}/memory.json` after
//!    each insertion.
//! 3. **Search**: The `memory_search` tool or other components call
//!    `MemoryStore::recall()` to retrieve relevant past memories.
//!
//! ## Memory Types
//!
//! - `decision`: User explicitly chose a direction
//! - `fix`: A bug was resolved with a specific solution
//! - `finding`: Agent discovered something noteworthy
//! - `summary`: End-of-session summary

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::embedder::Embedder;
use crate::error::{VectorError, VectorResult};
use crate::store::{BruteForceStore, VectorStore};

/// A single memory entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    /// Unique identifier (typically session_id + timestamp).
    pub id: String,

    /// Memory category: decision, fix, finding, summary.
    pub memory_type: String,

    /// Session where this memory was created.
    pub session_id: String,

    /// ISO 8601 timestamp.
    pub timestamp: String,

    /// The text content to embed and search.
    pub content: String,

    /// Optional structured metadata (JSON).
    #[serde(default)]
    pub metadata: String,
}

/// Persistent memory store backed by a JSON file.
///
/// # Thread Safety
///
/// All operations are guarded by a Mutex. The store is `Send + Sync`.
///
/// # Lifecycle (m12-lifecycle)
///
/// - Created once at initialization with a directory path.
/// - Flushed to disk after each write.
/// - Survives process restarts via the JSON file.
pub struct MemoryStore {
    /// File path for persistence.
    path: PathBuf,

    /// In-memory entries (for search).
    entries: Mutex<Vec<MemoryEntry>>,

    /// Optional vector store for semantic search.
    /// Lazily rebuilt from entries on load.
    vectors: Mutex<Option<BruteForceStore>>,
}

impl MemoryStore {
    /// Open or create a memory store at the given directory.
    ///
    /// Loads existing memories from `{dir}/memory.json` if present.
    pub fn open(dir: &Path) -> VectorResult<Self> {
        let path = dir.join("memory.json");
        let entries = if path.exists() {
            let raw = fs::read_to_string(&path)
                .map_err(|e| VectorError::Io(e))?;
            serde_json::from_str(&raw)
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        tracing::info!(
            "MemoryStore opened at {:?} with {} entries",
            path,
            entries.len()
        );

        Ok(Self {
            path,
            entries: Mutex::new(entries),
            vectors: Mutex::new(None),
        })
    }

    /// Create a new empty memory store (for testing).
    pub fn new_empty() -> Self {
        Self {
            path: PathBuf::from(":memory:"),
            entries: Mutex::new(Vec::new()),
            vectors: Mutex::new(None),
        }
    }

    /// Number of stored memories.
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.lock().unwrap().is_empty()
    }

    /// Store a new memory entry and persist to disk.
    pub fn remember(&self, entry: MemoryEntry) -> VectorResult<()> {
        let mut entries = self.entries.lock().unwrap();
        entries.push(entry);
        self.flush_locked(&entries)
    }

    /// Delete a memory entry by id and persist to disk.
    /// Returns true if an entry was removed.
    pub fn forget(&self, id: &str) -> VectorResult<bool> {
        let mut entries = self.entries.lock().unwrap();
        let before = entries.len();
        entries.retain(|e| e.id != id);
        let removed = entries.len() < before;
        if removed {
            // Rebuild vectors since index changed
            *self.vectors.lock().unwrap() = None;
            self.flush_locked(&entries)?;
        }
        Ok(removed)
    }

    /// Keyword-based search over memory content.
    ///
    /// Returns matching entries sorted by relevance.
    pub fn recall_keyword(&self, query: &str, top_k: usize) -> Vec<MemoryEntry> {
        let entries = self.entries.lock().unwrap();
        let query_lower = query.to_lowercase();
        let query_tokens: Vec<&str> = query_lower
            .split(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
            .filter(|s| !s.is_empty())
            .collect();

        if query_tokens.is_empty() {
            return Vec::new();
        }

        let mut scored: Vec<(usize, usize)> = entries
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let text = format!(
                    "{} {} {}",
                    entry.content,
                    entry.memory_type,
                    entry.metadata
                )
                .to_lowercase();
                let hits = query_tokens
                    .iter()
                    .filter(|t| text.contains(*t))
                    .count();
                (i, hits)
            })
            .collect();

        scored.sort_by(|a, b| b.1.cmp(&a.1));
        let k = top_k.min(scored.len());

        scored[..k]
            .iter()
            .filter(|(_, score)| *score > 0)
            .map(|(idx, _)| entries[*idx].clone())
            .collect()
    }

    /// Semantic (vector) search over memory content.
    ///
    /// Requires an embedder to encode the query. Falls back to keyword
    /// search if no embedder is provided or the vector store is empty.
    pub fn recall_semantic(
        &self,
        embedder: &dyn Embedder,
        query: &str,
        top_k: usize,
    ) -> VectorResult<Vec<MemoryEntry>> {
        // Ensure the vector store is built from current entries
        let entries = self.entries.lock().unwrap();
        let mut vectors = self.vectors.lock().unwrap();

        if vectors.is_none() || vectors.as_ref().unwrap().len() != entries.len() {
            let dim = embedder.dim();
            if dim == 0 {
                // No embedder available, fall back to keyword
                drop(entries);
                drop(vectors);
                return Ok(self.recall_keyword(query, top_k));
            }

            let mut store = BruteForceStore::new(dim);
            for entry in entries.iter() {
                let vec = embedder.embed(&entry.content)?;
                let meta = serde_json::to_string(&SearchMeta {
                    session_id: entry.session_id.clone(),
                    memory_type: entry.memory_type.clone(),
                    timestamp: entry.timestamp.clone(),
                })
                .unwrap_or_default();
                store.insert(&entry.id, vec, &meta)?;
            }
            *vectors = Some(store);
        }

        let vs = vectors.as_ref().unwrap();
        let query_vec = embedder.embed(query)?;
        let results = vs.search(&query_vec, top_k)?;

        let result_entries: Vec<MemoryEntry> = results
            .iter()
            .filter_map(|r| entries.iter().find(|e| e.id == r.id).cloned())
            .collect();

        Ok(result_entries)
    }

    /// Dump all memories as a formatted text (for LLM context injection).
    pub fn format_for_context(&self, entries: &[MemoryEntry]) -> String {
        let mut out = String::from("## Cross-session memories\n\n");
        for (i, entry) in entries.iter().enumerate() {
            out.push_str(&format!(
                "**Memory {}** [{}] ({})\n{}\n\n",
                i + 1,
                entry.memory_type,
                entry.timestamp,
                entry.content
            ));
        }
        out
    }

    // ─── Internal ──────────────────────────────��──────────────────

    fn flush_locked(&self, entries: &[MemoryEntry]) -> VectorResult<()> {
        if self.path == Path::new(":memory:") {
            return Ok(()); // No persistence for in-memory store
        }
        let raw = serde_json::to_string_pretty(entries)
            .map_err(|e| VectorError::Store(format!("serialize: {e}")))?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::write(&self.path, raw)?;
        Ok(())
    }
}

/// Lightweight metadata stored alongside the vector.
#[derive(Serialize, Deserialize)]
struct SearchMeta {
    session_id: String,
    memory_type: String,
    timestamp: String,
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_entry(id: &str, content: &str, mem_type: &str) -> MemoryEntry {
        MemoryEntry {
            id: id.to_string(),
            memory_type: mem_type.to_string(),
            session_id: "test-session".to_string(),
            timestamp: "2026-07-24T00:00:00Z".to_string(),
            content: content.to_string(),
            metadata: String::new(),
        }
    }

    #[test]
    fn new_empty_store() {
        let store = MemoryStore::new_empty();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn remember_and_recall_keyword() {
        let store = MemoryStore::new_empty();
        store.remember(make_entry("1", "Fixed Tauri build error by installing Windows SDK", "fix")).unwrap();
        store.remember(make_entry("2", "Decided to use SQLite for local storage", "decision")).unwrap();
        store.remember(make_entry("3", "Added rate limiting middleware", "finding")).unwrap();

        let results = store.recall_keyword("Tauri build", 3);
        assert!(!results.is_empty());
        assert_eq!(results[0].id, "1");
    }

    #[test]
    fn recall_no_match() {
        let store = MemoryStore::new_empty();
        store.remember(make_entry("1", "rust programming", "finding")).unwrap();
        let results = store.recall_keyword("python django", 3);
        assert!(results.is_empty());
    }

    #[test]
    fn persistence_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        // Write
        {
            let store = MemoryStore::open(path).unwrap();
            store.remember(make_entry("a", "memory alpha", "fix")).unwrap();
            store.remember(make_entry("b", "memory beta", "decision")).unwrap();
        }

        // Read back
        {
            let store = MemoryStore::open(path).unwrap();
            assert_eq!(store.len(), 2);
            let results = store.recall_keyword("alpha", 3);
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].id, "a");
        }
    }

    #[test]
    fn format_for_context() {
        let store = MemoryStore::new_empty();
        store.remember(make_entry("1", "Fixed something", "fix")).unwrap();
        let results = store.recall_keyword("something", 1);
        let formatted = store.format_for_context(&results);
        assert!(formatted.contains("Fixed something"));
        assert!(formatted.contains("[fix]"));
    }

    #[test]
    fn top_k_respects_limit() {
        let store = MemoryStore::new_empty();
        for i in 0..5 {
            store.remember(make_entry(
                &format!("{i}"),
                &format!("memory number {i}"),
                "finding",
            )).unwrap();
        }
        let results = store.recall_keyword("memory", 2);
        assert_eq!(results.len(), 2);
    }
}
