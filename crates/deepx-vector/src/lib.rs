//! # deepx-vector
//!
//! Vector embeddings and semantic search for the DeepX agent.
//!
//! This crate provides:
//! - **Embedder**: text → vector conversion via local ML models (Candle) or remote APIs
//! - **VectorStore**: persistent or in-memory storage with similarity search
//! - **Chunker**: text segmentation for embedding-appropriate chunk sizes
//! - **Reranker**: cross-encoder for fine-grained relevance scoring
//! - **BM25**: keyword-based fallback when no embedding model is available
//! - **Degradation chain**: auto-detection and graceful fallback across backends
//!
//! ## Architecture (codebase-design)
//!
//! Each component is designed as a **deep module**: a small, stable interface
//! hiding complex implementations. Callers (tools, skills, session) interact
//! through traits without knowing the underlying model or storage backend.
//!
//! ## Quick Start
//!
//! ```no_run
//! use deepx_vector::{CandleEmbedder, BruteForceStore, VectorStore, Embedder};
//!
//! // Load the default embedding model (bge-small-zh, ~95MB)
//! let embedder = CandleEmbedder::standard();
//!
//! // Create an in-memory vector store
//! let mut store = BruteForceStore::new(embedder.dim());
//!
//! // Embed and store some text
//! let vec = embedder.embed("rust 是一种系统编程语言").unwrap();
//! store.insert("doc1", vec, r#"{"text":"rust docs"}"#).unwrap();
//!
//! // Search by meaning
//! let query = embedder.embed("编程语言").unwrap();
//! let results = store.search(&query, 3).unwrap();
//! ```

pub mod bm25;
pub mod chunk;
pub mod degradation;
pub mod embedder;
pub mod error;
pub mod memory;
pub mod reranker;
pub mod store;

// Conditional compilation: Candle backend
#[cfg(feature = "candle")]
pub mod candle;

#[cfg(feature = "candle")]
pub mod reranker_candle;

#[cfg(feature = "qwen3")]
pub mod qwen3_embedding;

#[cfg(feature = "vulkan")]
pub mod vulkan_embedder;

// 向量引擎（依赖 Candle 推理，cfg(feature = "candle") 控制）
#[cfg(feature = "candle")]
pub mod engine;
#[cfg(feature = "candle")]
pub use engine::{VectorConfig, VectorEngine, SkillChunk};

// Re-exports
pub use bm25::Bm25Index;
pub use chunk::{Chunker, TextChunker};
pub use degradation::{
    auto_detect, create_bm25, create_embedder, create_reranker, detect_reranker_tier, detect_tier,
    has_semantic, AutoDetectedPipeline, EmbeddingTier, RerankerTier,
};
pub use embedder::{cosine_similarity, normalize, Embedder, NoopEmbedder};
pub use error::{VectorError, VectorResult};
pub use memory::{MemoryEntry, MemoryStore};
pub use reranker::{NoopReranker, Reranker};
pub use store::{BruteForceStore, SearchResult, VectorStore};

#[cfg(feature = "candle")]
pub use candle::CandleEmbedder;

#[cfg(feature = "candle")]
pub use reranker_candle::CandleReranker;

#[cfg(feature = "qwen3")]
pub use qwen3_embedding::Qwen3Embedder;

#[cfg(feature = "vulkan")]
pub use vulkan_embedder::VulkanEmbedder;
