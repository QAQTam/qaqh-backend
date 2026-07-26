use crate::error::VectorResult;

/// Converts text into fixed-length float vectors (embeddings).
///
/// This is the central abstraction for all embedding backends:
/// local Candle models, ONNX Runtime, remote APIs, or fallback strategies.
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync`. Model inference is read-only
/// once loaded, so multiple threads can embed concurrently.
///
/// # Lifecycle
///
/// Models are lazily loaded on first use (see `m12-lifecycle`).
/// The embedder can be dropped to free model memory.
pub trait Embedder: Send + Sync {
    /// Dimensionality of the embedding vectors produced.
    fn dim(&self) -> usize;

    /// Name of the underlying model (for diagnostics).
    fn model_name(&self) -> &str;

    /// Embed a single piece of text into a vector.
    fn embed(&self, text: &str) -> VectorResult<Vec<f32>>;

    /// Embed multiple texts in a single batch.
    ///
    /// Default implementation calls `embed` for each text individually.
    /// Implementations should override this for batch efficiency.
    fn embed_batch(&self, texts: &[&str]) -> VectorResult<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
}

// ─── Utility Functions ──────────────────────────────��───────────────

/// Compute the cosine similarity between two vectors.
///
/// Returns a value in [-1.0, 1.0], where 1.0 means identical direction.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "vectors must have same dimension");
    let (dot, norm_a, norm_b) = a
        .iter()
        .zip(b.iter())
        .fold((0.0f32, 0.0f32, 0.0f32), |(dot, na, nb), (&x, &y)| {
            (dot + x * y, na + x * x, nb + y * y)
        });
    let denom = (norm_a * norm_b).sqrt();
    if denom < f32::EPSILON {
        0.0
    } else {
        dot / denom
    }
}

/// L2-normalize a vector in-place.
pub fn normalize(vec: &mut [f32]) {
    let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in vec.iter_mut() {
            *x /= norm;
        }
    }
}

// ─── Noop Embedder ───────────────────────────────────────────────────

/// A no-op embedder that returns zero vectors. Used for testing or
/// when embedding is disabled.
pub struct NoopEmbedder {
    dim: usize,
}

impl NoopEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }
}

impl Embedder for NoopEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn model_name(&self) -> &str {
        "noop"
    }

    fn embed(&self, _text: &str) -> VectorResult<Vec<f32>> {
        Ok(vec![0.0f32; self.dim])
    }
}
