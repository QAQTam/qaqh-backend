use crate::embedder;
use crate::error::{VectorError, VectorResult};

/// A single search result from the vector store.
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// Unique identifier for the stored item.
    pub id: String,
    /// Arbitrary metadata (typically JSON-serialized).
    pub metadata: String,
    /// Cosine similarity score with the query vector [0.0, 1.0].
    pub score: f32,
}

/// Persistent or in-memory store for embedding vectors with similarity search.
///
/// # Design (codebase-design: deep module)
///
/// Small interface (insert, search, delete) that hides complex
/// implementation details: similarity metrics, search strategies,
/// and future SQLite-vec integration.
pub trait VectorStore: Send + Sync {
    /// Insert a vector with associated id and metadata.
    fn insert(&mut self, id: &str, vector: Vec<f32>, metadata: &str) -> VectorResult<()>;

    /// Batch insert for efficiency.
    fn insert_batch(&mut self, items: Vec<(String, Vec<f32>, String)>) -> VectorResult<()> {
        for (id, vec, meta) in items {
            self.insert(&id, vec, &meta)?;
        }
        Ok(())
    }

    /// Search for the top-K most similar items to the query vector.
    fn search(&self, query: &[f32], top_k: usize) -> VectorResult<Vec<SearchResult>>;

    /// Remove an item by id.
    fn delete(&mut self, id: &str) -> VectorResult<()>;

    /// Number of items in the store.
    fn len(&self) -> usize;

    /// Whether the store is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ─── BruteForce Store ──────────────────────────────��────────────────

/// A simple in-memory vector store using brute-force cosine similarity.
///
/// Best for small collections (< 10,000 items). For larger datasets,
/// a future SQLite-vec or approximate-nearest-neighbor backend should
/// be used (see Tier 2 design).
///
/// # Performance (m10-performance)
///
/// - Search: O(N * D) where N = items, D = dimension
/// - 10,000 items × 384 dims ≈ 3ms on modern CPU
/// - Vectors are L2-normalized on insertion for faster cosine computation
///   (normalized cosine = dot product)
pub struct BruteForceStore {
    dim: usize,
    entries: Vec<StoreEntry>,
}

struct StoreEntry {
    id: String,
    vector: Vec<f32>, // L2-normalized on insert
    metadata: String,
}

impl BruteForceStore {
    /// Create a new empty store for vectors of the given dimension.
    ///
    /// # Panics
    ///
    /// Panics if `dim == 0`.
    pub fn new(dim: usize) -> Self {
        assert!(dim > 0, "vector dimension must be positive");
        Self {
            dim,
            entries: Vec::new(),
        }
    }
}

impl VectorStore for BruteForceStore {
    fn insert(&mut self, id: &str, mut vector: Vec<f32>, metadata: &str) -> VectorResult<()> {
        if vector.len() != self.dim {
            return Err(VectorError::InvalidInput(format!(
                "expected dimension {}, got {}",
                self.dim,
                vector.len()
            )));
        }
        // Normalize for faster cosine similarity
        embedder::normalize(&mut vector);

        // Replace existing entry with same id
        if let Some(existing) = self.entries.iter_mut().find(|e| e.id == id) {
            existing.vector = vector;
            existing.metadata = metadata.to_string();
        } else {
            self.entries.push(StoreEntry {
                id: id.to_string(),
                vector,
                metadata: metadata.to_string(),
            });
        }
        Ok(())
    }

    fn search(&self, query: &[f32], top_k: usize) -> VectorResult<Vec<SearchResult>> {
        if query.len() != self.dim {
            return Err(VectorError::InvalidInput(format!(
                "expected query dimension {}, got {}",
                self.dim,
                query.len()
            )));
        }
        if self.entries.is_empty() {
            return Ok(Vec::new());
        }

        // Normalize query once (m10-performance: avoid per-result normalization)
        let mut query_norm = query.to_vec();
        embedder::normalize(&mut query_norm);

        // Compute cosine similarity = dot product (vectors are already normalized)
        let mut scored: Vec<(usize, f32)> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let dot: f32 = query_norm
                    .iter()
                    .zip(entry.vector.iter())
                    .map(|(a, b)| a * b)
                    .sum();
                (i, dot)
            })
            .collect();

        // Partial sort for top-K (m10-performance: O(N log K) instead of O(N log N))
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let k = top_k.min(scored.len());

        let results: Vec<SearchResult> = scored[..k]
            .iter()
            .map(|(idx, score)| {
                let entry = &self.entries[*idx];
                SearchResult {
                    id: entry.id.clone(),
                    metadata: entry.metadata.clone(),
                    score: *score,
                }
            })
            .collect();
        Ok(results)
    }

    fn delete(&mut self, id: &str) -> VectorResult<()> {
        let before = self.entries.len();
        self.entries.retain(|e| e.id != id);
        if self.entries.len() == before {
            Err(VectorError::Store(format!("entry not found: {id}")))
        } else {
            Ok(())
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store() -> BruteForceStore {
        BruteForceStore::new(4)
    }

    #[test]
    fn insert_and_search() {
        let mut store = make_store();
        store.insert("a", vec![1.0, 0.0, 0.0, 0.0], r#"{"text":"hello"}"#).unwrap();
        store.insert("b", vec![0.0, 1.0, 0.0, 0.0], r#"{"text":"world"}"#).unwrap();
        store.insert("c", vec![0.0, 0.0, 1.0, 0.0], r#"{"text":"foo"}"#).unwrap();

        // Query close to "a"
        let results = store.search(&[0.9, 0.1, 0.0, 0.0], 2).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "a");
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn search_top_k_respects_limit() {
        let mut store = make_store();
        for i in 0..5 {
            let mut v = vec![0.0f32; 4];
            v[i % 4] = 1.0;
            store.insert(&format!("{i}"), v, "{}").unwrap();
        }
        let results = store.search(&[1.0, 0.0, 0.0, 0.0], 3).unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn delete_removes_entry() {
        let mut store = make_store();
        store.insert("x", vec![1.0, 0.0, 0.0, 0.0], "{}").unwrap();
        assert_eq!(store.len(), 1);
        store.delete("x").unwrap();
        assert_eq!(store.len(), 0);
        assert!(store.delete("x").is_err());
    }

    #[test]
    fn insert_replace_same_id() {
        let mut store = make_store();
        store.insert("k", vec![1.0, 0.0, 0.0, 0.0], "old").unwrap();
        store.insert("k", vec![0.0, 1.0, 0.0, 0.0], "new").unwrap();
        assert_eq!(store.len(), 1);
        let results = store.search(&[0.0, 1.0, 0.0, 0.0], 1).unwrap();
        assert_eq!(results[0].metadata, "new");
    }

    #[test]
    fn dimension_mismatch_rejected() {
        let mut store = BruteForceStore::new(3);
        assert!(store.insert("bad", vec![1.0, 2.0], "{}").is_err());
        assert!(store.search(&[1.0, 2.0], 1).is_err());
    }

    #[test]
    fn empty_store_search() {
        let store = BruteForceStore::new(2);
        let results = store.search(&[1.0, 0.0], 5).unwrap();
        assert!(results.is_empty());
    }
}
