//! Cross-encoder reranker for fine-grained relevance scoring.
//!
//! Unlike bi-encoders (embedding models), a cross-encoder processes
//! the query and document together through the full transformer stack,
//! producing a direct relevance score. This is more accurate but
//! slower — typically used to re-rank the top-K candidates from a
//! faster embedding-based search.

use crate::error::VectorResult;

/// Re-ranks search results with a cross-encoder model.
///
/// # How it differs from `Embedder`
///
/// - **Embedder**: query → vector, document → vector, compare with cosine.
///   Fast but coarse (bi-encoder).
/// - **Reranker**: (query, document) pair → score directly through full
///   transformer. Accurate but slower (cross-encoder).
///
/// # Typical workflow
///
/// 1. Embedding search returns top-20 candidates
/// 2. Reranker re-scores those 20 candidates
/// 3. Return top-3 for the LLM context
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync`.
pub trait Reranker: Send + Sync {
    /// Score a single (query, document) pair.
    ///
    /// Returns a relevance score. Higher = more relevant.
    /// Most implementations return scores in [0.0, 1.0] after sigmoid.
    fn score(&self, query: &str, document: &str) -> VectorResult<f32>;

    /// Score multiple documents against the same query.
    ///
    /// Default implementation calls `score` for each document.
    /// Implementations should override for batch efficiency.
    fn score_batch(&self, query: &str, documents: &[&str]) -> VectorResult<Vec<f32>> {
        documents
            .iter()
            .map(|doc| self.score(query, doc))
            .collect()
    }

    /// Score and return the top-K document indices and scores.
    fn rerank_top_k(
        &self,
        query: &str,
        documents: &[&str],
        top_k: usize,
    ) -> VectorResult<Vec<(usize, f32)>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }

        let scores = self.score_batch(query, documents)?;
        let mut indexed: Vec<(usize, f32)> = scores.into_iter().enumerate().collect();
        indexed.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let k = top_k.min(indexed.len());
        Ok(indexed[..k].to_vec())
    }

    /// Name of the underlying model (for diagnostics).
    fn model_name(&self) -> &str;
}

/// A no-op reranker that returns 0.5 for all documents (neutral score).
/// Used when reranking is disabled or as a placeholder.
pub struct NoopReranker;

impl Reranker for NoopReranker {
    fn score(&self, _query: &str, _document: &str) -> VectorResult<f32> {
        Ok(0.5)
    }

    fn model_name(&self) -> &str {
        "noop"
    }
}

// ─── Tests ──────────────────────────────��────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock reranker that scores based on keyword overlap.
    struct MockReranker;

    impl Reranker for MockReranker {
        fn score(&self, query: &str, document: &str) -> VectorResult<f32> {
            let q_words: Vec<&str> = query.split_whitespace().collect();
            let d_words: Vec<&str> = document.split_whitespace().collect();
            let overlap = q_words.iter().filter(|w| d_words.contains(w)).count();
            let score = overlap as f32 / q_words.len().max(1) as f32;
            Ok(score)
        }

        fn model_name(&self) -> &str {
            "mock"
        }
    }

    #[test]
    fn noop_returns_neutral() {
        let r = NoopReranker;
        let s = r.score("query", "doc").unwrap();
        assert!((s - 0.5).abs() < 1e-6);
    }

    #[test]
    fn mock_exact_match() {
        let r = MockReranker;
        let s = r.score("hello world", "hello world").unwrap();
        assert!((s - 1.0).abs() < 1e-6);
    }

    #[test]
    fn mock_partial_match() {
        let r = MockReranker;
        let s = r.score("hello world", "hello rust").unwrap();
        assert!((s - 0.5).abs() < 1e-6);
    }

    #[test]
    fn mock_no_match() {
        let r = MockReranker;
        let s = r.score("hello", "rust programming").unwrap();
        assert!((s - 0.0).abs() < 1e-6);
    }

    #[test]
    fn rerank_top_k_ordering() {
        let r = MockReranker;
        let docs = vec!["rust programming", "hello world", "hello rust"];
        let results = r.rerank_top_k("hello", &docs, 2).unwrap();
        assert_eq!(results.len(), 2);
        // "hello world" and "hello rust" should be top (both have "hello")
        assert!(results[0].1 >= results[1].1);
    }

    #[test]
    fn rerank_top_k_empty() {
        let r = MockReranker;
        let results = r.rerank_top_k("query", &[], 5).unwrap();
        assert!(results.is_empty());
    }
}
