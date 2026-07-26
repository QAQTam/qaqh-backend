//! BM25 keyword-based retrieval — the final fallback when no embedding
//! model is available.
//!
//! BM25 is a probabilistic ranking function that scores documents based
//! on term frequency and inverse document frequency. It requires no ML
//! model and runs entirely on CPU with zero memory overhead beyond the
//! indexed documents themselves.
//!
//! ## When to use
//!
//! This is the **last-resort** backend in the degradation chain. It
//! provides keyword matching but no semantic understanding. Use it
//! only when all embedding model backends (Candle, ONNX, remote API)
//! have failed or been disabled.
//!
//! ## Parameters
//!
//! - `k1 = 1.5`: term frequency saturation
//! - `b = 0.75`: length normalization

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::{VectorError, VectorResult};

/// BM25 configuration parameters.
const K1: f32 = 1.5;
const B: f32 = 0.75;

/// A single document in the BM25 index.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Bm25Document {
    id: String,
    metadata: String,
    /// Tokenized terms with their frequencies.
    term_freqs: HashMap<String, usize>,
    /// Total number of tokens in this document.
    doc_len: usize,
}

/// BM25 keyword-based search index.
///
/// # Thread Safety
///
/// The index is `Send + Sync`. Search is read-only and can be
/// performed concurrently. Inserts require `&mut self`.
pub struct Bm25Index {
    documents: Vec<Bm25Document>,
    /// Document frequency: how many documents contain each term.
    doc_freqs: HashMap<String, usize>,
    /// Average document length (cached for scoring).
    avg_doc_len: f32,
}

impl Bm25Index {
    /// Create a new empty index.
    pub fn new() -> Self {
        Self {
            documents: Vec::new(),
            doc_freqs: HashMap::new(),
            avg_doc_len: 0.0,
        }
    }

    /// Number of documents in the index.
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// Add a document to the index.
    pub fn insert(&mut self, id: &str, text: &str, metadata: &str) {
        let tokens = tokenize(text);
        let doc_len = tokens.len();
        let mut term_freqs: HashMap<String, usize> = HashMap::new();

        for token in tokens {
            *term_freqs.entry(token).or_insert(0) += 1;
        }

        // Update document frequencies: count how many documents contain each term
        for term in term_freqs.keys() {
            self.doc_freqs
                .entry(term.clone())
                .and_modify(|c| *c += 1)
                .or_insert(1);
        }

        self.documents.push(Bm25Document {
            id: id.to_string(),
            metadata: metadata.to_string(),
            term_freqs,
            doc_len,
        });

        // Recalculate average document length
        let total_len: usize = self.documents.iter().map(|d| d.doc_len).sum();
        self.avg_doc_len = total_len as f32 / self.documents.len() as f32;
    }

    /// Remove a document by id.
    pub fn delete(&mut self, id: &str) -> VectorResult<()> {
        let pos = self
            .documents
            .iter()
            .position(|d| d.id == id)
            .ok_or_else(|| VectorError::Store(format!("document not found: {id}")))?;

        // Remove term frequencies
        let doc = &self.documents[pos];
        for term in doc.term_freqs.keys() {
            if let Some(count) = self.doc_freqs.get_mut(term) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.doc_freqs.remove(term);
                }
            }
        }

        self.documents.remove(pos);

        // Recalculate average document length
        if !self.documents.is_empty() {
            let total_len: usize = self.documents.iter().map(|d| d.doc_len).sum();
            self.avg_doc_len = total_len as f32 / self.documents.len() as f32;
        } else {
            self.avg_doc_len = 0.0;
        }

        Ok(())
    }

    /// Search the index with a BM25 query, returning top-K results.
    ///
    /// Returns `(id, metadata, score)` tuples sorted by descending score.
    pub fn search(&self, query: &str, top_k: usize) -> Vec<(String, String, f32)> {
        if self.documents.is_empty() || query.is_empty() {
            return Vec::new();
        }

        let query_terms = tokenize(query);
        let n_docs = self.documents.len() as f32;

        // Score each document
        let mut scored: Vec<(usize, f32)> = self
            .documents
            .iter()
            .enumerate()
            .map(|(idx, doc)| {
                let score: f32 = query_terms
                    .iter()
                    .map(|term| {
                        // IDF
                        let df = *self.doc_freqs.get(term).unwrap_or(&0) as f32;
                        let idf = if df > 0.0 {
                            ((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln()
                        } else {
                            0.0
                        };

                        // TF component
                        let tf = *doc.term_freqs.get(term).unwrap_or(&0) as f32;
                        let tf_norm = (tf * (K1 + 1.0))
                            / (tf + K1 * (1.0 - B + B * doc.doc_len as f32 / self.avg_doc_len));

                        idf * tf_norm
                    })
                    .sum();
                (idx, score)
            })
            .collect();

        // Sort by score descending, take top-K (filtering zero scores)
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.retain(|(_, score)| *score > 0.0);
        let k = top_k.min(scored.len());

        scored[..k]
            .iter()
            .map(|(idx, score)| {
                let doc = &self.documents[*idx];
                (doc.id.clone(), doc.metadata.clone(), *score)
            })
            .collect()
    }
}

impl Default for Bm25Index {
    fn default() -> Self {
        Self::new()
    }
}

/// Simple tokenizer: split on whitespace and punctuation, lowercase.
fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn build_index() -> Bm25Index {
        let mut idx = Bm25Index::new();
        idx.insert("1", "rust programming language systems", r#"{"title":"Rust"}"#);
        idx.insert("2", "python programming language scripting", r#"{"title":"Python"}"#);
        idx.insert("3", "rust memory safety ownership borrow checker", r#"{"title":"Rust Safety"}"#);
        idx
    }

    #[test]
    fn basic_search() {
        let idx = build_index();
        let results = idx.search("rust programming", 2);
        assert_eq!(results.len(), 2);
        // "rust programming language systems" should score highest
        assert_eq!(results[0].0, "1");
    }

    #[test]
    fn keyword_match_single() {
        let idx = build_index();
        let results = idx.search("python", 3);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "2");
    }

    #[test]
    fn no_match_returns_empty() {
        let idx = build_index();
        let results = idx.search("javascript", 3);
        assert!(results.is_empty());
    }

    #[test]
    fn empty_query() {
        let idx = build_index();
        let results = idx.search("", 3);
        assert!(results.is_empty());
    }

    #[test]
    fn delete_and_reindex() {
        let mut idx = build_index();
        assert_eq!(idx.len(), 3);
        idx.delete("2").unwrap();
        assert_eq!(idx.len(), 2);
        let results = idx.search("python", 3);
        assert!(results.is_empty());
    }

    #[test]
    fn delete_nonexistent() {
        let mut idx = build_index();
        assert!(idx.delete("nonexistent").is_err());
    }

    #[test]
    fn top_k_respects_limit() {
        let idx = build_index();
        let results = idx.search("rust", 1);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn tokenizer_splits_correctly() {
        let tokens = tokenize("Hello, World! Rust 2024");
        assert_eq!(tokens, vec!["hello", "world", "rust", "2024"]);
    }

    #[test]
    fn tokenizer_handles_chinese() {
        // Chinese characters without spaces won't be split ideally
        // (this is a known limitation of the simple tokenizer), but
        // mixed Chinese/English should at least separate on punctuation.
        let tokens = tokenize("rust 是系统编程语言");
        assert!(tokens.contains(&"rust".to_string()));
    }
}
