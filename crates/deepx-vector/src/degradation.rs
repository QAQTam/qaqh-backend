//! Degradation chain: automatic backend selection with graceful fallback.
//!
//! When an embedding model fails to load (missing dependencies, no GPU,
//! download error), the system automatically falls back to the next
//! available tier. This ensures DeepX always has at least keyword-based
//! search capability.
//!
//! ## Tier hierarchy
//!
//! ```text
//! Tier 1: CandleEmbedder (local BERT, ~95MB)
//!     | fails → Tier 2
//! Tier 2: ONNX Runtime      (TODO: future)
//!     | fails → Tier 3
//! Tier 3: Remote API         (TODO: future)
//!     | fails → Tier 4
//! Tier 4: BM25               (keyword-only, always available)
//! ```
//!
//! ## Usage
//!
//! ```no_run
//! use deepx_vector::degradation::{detect_tier, create_embedder, EmbeddingTier};
//!
//! let tier = detect_tier();
//! let embedder = create_embedder(tier);
//! // Use embedder.embed(...) or fall back to BM25 search
//! ```

use crate::bm25::Bm25Index;
use crate::embedder::{Embedder, NoopEmbedder};

#[cfg(feature = "candle")]
use crate::candle::CandleEmbedder;

#[cfg(feature = "candle")]
use crate::reranker_candle::CandleReranker;

use crate::reranker::{NoopReranker, Reranker};

/// Available embedding backends, ordered by preference (best first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingTier {
    /// Local BERT embedding via Candle (bge-small-zh, ~95MB).
    /// Best semantic quality, zero network dependency after first download.
    Candle,

    /// ONNX Runtime backend (planned).
    /// Larger model compatibility, cross-platform ONNX format.
    Onnx,

    /// OpenAI-compatible remote embedding API.
    /// Best quality, requires network and API key.
    Remote,

    /// BM25 keyword-based retrieval.
    /// No semantic understanding, but always works with zero dependencies.
    Bm25,

    /// Embedding is explicitly disabled.
    None,
}

/// Reranker backends, ordered by preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerankerTier {
    /// Local BERT cross-encoder via Candle (bge-reranker-base, ~400MB).
    Candle,

    /// Remote reranker API (planned).
    Remote,

    /// No reranking.
    None,
}

/// Auto-detect the best available embedding backend.
///
/// Tries each tier in order and returns the first one that can be
/// successfully initialized. BM25 is always available as the
/// ultimate fallback.
pub fn detect_tier() -> EmbeddingTier {
    #[cfg(feature = "candle")]
    {
        // Candle is compiled in — it's available
        EmbeddingTier::Candle
    }

    #[cfg(not(feature = "candle"))]
    {
        // TODO: Try ONNX
        // TODO: Try Remote API

        // Fallback: BM25 always works
        EmbeddingTier::Bm25
    }
}

/// Auto-detect the best available reranker backend.
pub fn detect_reranker_tier() -> RerankerTier {
    #[cfg(feature = "candle")]
    {
        RerankerTier::Candle
    }

    #[cfg(not(feature = "candle"))]
    {
        RerankerTier::None
    }
}

/// Create an embedder for the given tier.
///
/// Returns `None` for `EmbeddingTier::None`. For `Bm25`, the embedder
/// is a `NoopEmbedder` — BM25 search is done separately, not through
/// the `Embedder` trait (since BM25 doesn't produce vectors).
pub fn create_embedder(tier: EmbeddingTier) -> Option<Box<dyn Embedder>> {
    match tier {
        #[cfg(feature = "candle")]
        EmbeddingTier::Candle => Some(Box::new(CandleEmbedder::standard())),

        EmbeddingTier::Onnx => {
            tracing::warn!("ONNX backend not yet implemented, falling back");
            None
        }
        EmbeddingTier::Remote => {
            tracing::warn!("remote API backend not yet implemented, falling back");
            None
        }
        EmbeddingTier::Bm25 => {
            // BM25 doesn't produce embeddings; return a no-op embedder.
            // The caller should use Bm25Index directly for search.
            Some(Box::new(NoopEmbedder::new(0)))
        }
        EmbeddingTier::None => None,
    }
}

/// Create a reranker for the given tier.
pub fn create_reranker(tier: RerankerTier) -> Option<Box<dyn Reranker>> {
    match tier {
        #[cfg(feature = "candle")]
        RerankerTier::Candle => Some(Box::new(CandleReranker::standard())),

        RerankerTier::Remote => {
            tracing::warn!("remote reranker not yet implemented");
            None
        }
        RerankerTier::None => Some(Box::new(NoopReranker)),
    }
}

/// Create a BM25 index (always available, independent of tier).
pub fn create_bm25() -> Bm25Index {
    Bm25Index::new()
}

/// Result of auto-detection: (embedder, reranker, bm25-fallback).
pub type AutoDetectedPipeline = (
    Option<Box<dyn Embedder>>,
    Option<Box<dyn Reranker>>,
    Bm25Index,
);

/// Full auto-detect: returns the best available embedder, reranker,
/// and a BM25 fallback in one call.
pub fn auto_detect() -> AutoDetectedPipeline {
    let tier = detect_tier();
    let reranker_tier = detect_reranker_tier();
    let embedder = create_embedder(tier);
    let reranker = create_reranker(reranker_tier);
    let bm25 = create_bm25();

    tracing::info!(
        embedder_tier = ?tier,
        reranker_tier = ?reranker_tier,
        "degradation chain initialized"
    );

    (embedder, reranker, bm25)
}

/// Check if the given tier has semantic (vector) embedding capability.
pub fn has_semantic(tier: EmbeddingTier) -> bool {
    matches!(tier, EmbeddingTier::Candle | EmbeddingTier::Onnx | EmbeddingTier::Remote)
}

// ─── Tests ────────────────────────────────��──────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_tier_returns_valid() {
        let tier = detect_tier();
        // When candle feature is enabled, should be Candle; otherwise Bm25
        #[cfg(feature = "candle")]
        assert_eq!(tier, EmbeddingTier::Candle);
        #[cfg(not(feature = "candle"))]
        assert_eq!(tier, EmbeddingTier::Bm25);
    }

    #[test]
    fn has_semantic_detection() {
        assert!(has_semantic(EmbeddingTier::Candle));
        assert!(has_semantic(EmbeddingTier::Onnx));
        assert!(has_semantic(EmbeddingTier::Remote));
        assert!(!has_semantic(EmbeddingTier::Bm25));
        assert!(!has_semantic(EmbeddingTier::None));
    }

    #[test]
    fn create_embedder_bm25_returns_noop() {
        let e = create_embedder(EmbeddingTier::Bm25).expect("should get embedder");
        assert_eq!(e.dim(), 0);
    }

    #[test]
    fn create_embedder_none_returns_none() {
        let e = create_embedder(EmbeddingTier::None);
        assert!(e.is_none());
    }

    #[test]
    fn create_reranker_none_returns_noop() {
        let r = create_reranker(RerankerTier::None).expect("should get reranker");
        assert_eq!(r.model_name(), "noop");
    }

    #[test]
    fn detect_reranker_tier_returns_valid() {
        let tier = detect_reranker_tier();
        #[cfg(feature = "candle")]
        assert_eq!(tier, RerankerTier::Candle);
        #[cfg(not(feature = "candle"))]
        assert_eq!(tier, RerankerTier::None);
    }

    #[test]
    fn auto_detect_returns_triple() {
        let (embedder, reranker, bm25) = auto_detect();
        #[cfg(feature = "candle")]
        assert!(embedder.is_some());
        #[cfg(not(feature = "candle"))]
        assert!(embedder.is_some()); // BM25 gives NoopEmbedder
        assert!(reranker.is_some());
        assert_eq!(bm25.len(), 0);
    }

    #[test]
    fn embedder_tier_is_copy() {
        let tier = EmbeddingTier::Bm25;
        let _copy = tier; // should compile
        assert_eq!(tier, _copy);
    }

    #[test]
    fn reranker_tier_is_copy() {
        let tier = RerankerTier::None;
        let _copy = tier;
        assert_eq!(tier, _copy);
    }
}
