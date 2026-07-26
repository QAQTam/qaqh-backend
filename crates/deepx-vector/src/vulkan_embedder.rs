//! Vulkan-accelerated embedding via llama.cpp (llama-cpp-2).
//!
//! Uses `llama-cpp-2` with `vulkan` feature for GPU-accelerated
//! GGUF model inference on AMD GPUs.
//!
//! # Lifecycle (m12-lifecycle)
//!
//! The model+backend are leaked as `&'static` references after loading,
//! since llama-cpp-2's `LlamaContext` borrows the model with a lifetime.
//! This is safe because the model lives for the program's entire lifetime.

use std::path::Path;
use std::sync::Mutex;

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};

use crate::embedder::{self, Embedder};
use crate::error::{VectorError, VectorResult};

/// Vulkan-accelerated embedding model via llama.cpp.
pub struct VulkanEmbedder {
    model: &'static LlamaModel,
    ctx: Mutex<LlamaContext<'static>>,
    dim: usize,
}

impl VulkanEmbedder {
    /// Create a new Vulkan embedder. `n_gpu_layers` controls GPU offloading
    /// (e.g. 36 for a 36-layer model = full GPU, 0 = CPU-only).
    pub fn new(gguf_path: &Path, n_gpu_layers: u32) -> VectorResult<Self> {
        tracing::info!(path = %gguf_path.display(), n_gpu_layers, "loading Vulkan model");

        // Init backend — leak for 'static lifetime
        let backend = Box::leak(Box::new(
            LlamaBackend::init()
                .map_err(|e| VectorError::Model(format!("backend: {e}")))?
        ));

        // Load model — leak for 'static lifetime
        let model_params = LlamaModelParams::default()
            .with_n_gpu_layers(n_gpu_layers);

        let model = Box::leak(Box::new(
            LlamaModel::load_from_file(backend, gguf_path, &model_params)
                .map_err(|e| VectorError::Model(format!("load: {e}")))?
        ));

        let dim = usize::try_from(model.n_embd())
            .map_err(|e| VectorError::Model(format!("n_embd: {e}")))?;

        // Create context with embedding support
        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(std::num::NonZeroU32::new(4096))
            .with_n_batch(512)
            .with_embeddings(true);

        let ctx = model.new_context(backend, ctx_params)
            .map_err(|e| VectorError::Model(format!("context: {e}")))?;

        tracing::info!(dim, "Vulkan model loaded");

        Ok(Self {
            model,
            ctx: Mutex::new(ctx),
            dim,
        })
    }
}

impl Embedder for VulkanEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn model_name(&self) -> &str {
        "vulkan-gguf"
    }

    fn embed(&self, text: &str) -> VectorResult<Vec<f32>> {
        if text.is_empty() {
            return Err(VectorError::InvalidInput("empty text".into()));
        }

        let mut ctx = self.ctx.lock()
            .map_err(|e| VectorError::Model(format!("lock: {e}")))?;

        // Tokenize
        let tokens = self.model.str_to_token(text, AddBos::Always)
            .map_err(|e| VectorError::Tokenizer(format!("tokenize: {e}")))?;

        if tokens.is_empty() {
            return Err(VectorError::Tokenizer("empty tokens".into()));
        }

        let n_tokens = tokens.len();

        // Create batch
        let mut batch = LlamaBatch::new(n_tokens, 1);

        // Add tokens as sequence 0
        batch.add_sequence(&tokens, 0, false)
            .map_err(|e| VectorError::Inference(format!("batch: {e}")))?;

        // Decode
        ctx.decode(&mut batch)
            .map_err(|e| VectorError::Inference(format!("decode: {e}")))?;

        // Get embedding for sequence 0 (last token embedding)
        let emb = ctx.embeddings_seq_ith(0)
            .map_err(|e| VectorError::Inference(format!("embeddings: {e}")))?;

        let mut embedding = emb.to_vec();
        embedder::normalize(&mut embedding);

        Ok(embedding)
    }
}

// Safety: llama-cpp-2 model and context are thread-safe (guarded by Mutex)
unsafe impl Send for VulkanEmbedder {}
unsafe impl Sync for VulkanEmbedder {}
