use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use hf_hub::api::sync::Api;
use tokenizers::tokenizer::Tokenizer;

use crate::embedder::{self, Embedder};
use crate::error::{VectorError, VectorResult};

/// BERT-based embedding model running locally via Candle.
///
/// # Model Architecture
///
/// Uses BERT architecture with mean pooling over all tokens to
/// produce sentence-level embeddings. Compatible with:
/// - `BAAI/bge-small-zh-v1.5` (default, 24M params, 512-dim, ~95MB)
/// - `BAAI/bge-large-zh-v1.5` (326M params, 1024-dim)
/// - `BAAI/bge-m3` (568M params, 1024-dim)
/// - Any BERT-compatible model on HuggingFace Hub
///
/// # Lifecycle (m12-lifecycle)
///
/// The model is lazily loaded on first `embed()` call, guarded by
/// a `Mutex` for thread safety. Loading takes ~1-3 seconds (model
/// download + parsing). Subsequent calls are instant.
///
/// # Performance (m10-performance)
///
/// - Single sentence (384-dim): ~2-5ms on modern CPU
/// - Model memory: ~100MB loaded
pub struct CandleEmbedder {
    /// HuggingFace model id.
    model_id: String,

    /// Optional revision (for reproducibility).
    revision: Option<String>,

    /// Device (CPU or CUDA if available).
    device: Device,

    /// Set when the model path is a local directory, not a HF repo id.
    is_local: bool,

    /// Model dimension, cached after first load.
    dim: OnceLock<usize>,

    /// Lazy-loaded model + tokenizer, guarded by Mutex.
    inner: Mutex<Option<LoadedModel>>,
}

struct LoadedModel {
    model: BertModel,
    tokenizer: Tokenizer,
    dim: usize,
    device: Device,
}

impl CandleEmbedder {
    /// Create a new embedder with the default model (bge-small-zh-v1.5).
    pub fn standard() -> Self {
        Self::new("BAAI/bge-small-zh-v1.5", None, Device::Cpu)
    }
}

impl Default for CandleEmbedder {
    fn default() -> Self {
        Self::standard()
    }
}

impl CandleEmbedder {
    /// Create a new embedder with a specific model id.
    pub fn new(model_id: &str, revision: Option<&str>, device: Device) -> Self {
        Self {
            model_id: model_id.to_string(),
            revision: revision.map(String::from),
            device,
            is_local: false,
            dim: OnceLock::new(),
            inner: Mutex::new(None),
        }
    }

    /// Create an embedder from a local model directory.
    /// The directory must contain: config.json, tokenizer.json, model.safetensors.
    pub fn from_local(path: &std::path::Path) -> Self {
        Self {
            model_id: path.to_string_lossy().to_string(),
            revision: None,
            device: Device::Cpu,
            is_local: true,
            dim: OnceLock::new(),
            inner: Mutex::new(None),
        }
    }

    /// Download and load the model from HuggingFace Hub.
    fn load_model(&self) -> VectorResult<LoadedModel> {
        tracing::info!(
            model_id = %self.model_id,
            "loading embedding model (this may take a few seconds on first run)"
        );

        // ── Download / locate model files ──
        let (config_path, tokenizer_path, model_path): (PathBuf, PathBuf, PathBuf) =
            if self.is_local {
                let base = PathBuf::from(&self.model_id);
                let model_path = {
                    let sf = base.join("model.safetensors");
                    if sf.exists() { sf }
                    else { base.join("pytorch_model.bin") }
                };
                (
                    base.join("config.json"),
                    base.join("tokenizer.json"),
                    model_path,
                )
            } else {
                let api = Api::new()
                    .map_err(|e| VectorError::Model(format!("HF API init failed: {e}")))?;
                let repo = if let Some(ref rev) = self.revision {
                    api.repo(hf_hub::Repo::with_revision(
                        self.model_id.clone(),
                        hf_hub::RepoType::Model,
                        rev.clone(),
                    ))
                } else {
                    api.repo(hf_hub::Repo::new(
                        self.model_id.clone(),
                        hf_hub::RepoType::Model,
                    ))
                };
                let cfg = repo.get("config.json")
                    .map_err(|e| VectorError::Model(format!("config download failed: {e}")))?;
                let tk = repo.get("tokenizer.json")
                    .map_err(|e| VectorError::Tokenizer(format!("tokenizer download failed: {e}")))?;
                let mp = repo.get("model.safetensors")
                    .or_else(|_| repo.get("pytorch_model.bin"))
                    .map_err(|e| VectorError::Model(format!("model weights download failed: {e}")))?;
                (cfg, tk, mp)
            };

        let config_raw = std::fs::read_to_string(&config_path)
            .map_err(|e| VectorError::Model(format!("config read failed: {e}")))?;
        let config: Config = serde_json::from_str(&config_raw)
            .map_err(|e| VectorError::Model(format!("config parse failed: {e}")))?;

        // Load tokenizer
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| VectorError::Tokenizer(format!("tokenizer load failed: {e}")))?;

        // Load model weights — supports both safetensors and pytorch .bin
        let vb = if model_path
            .extension()
            .map_or(false, |e| e == "safetensors")
        {
            unsafe {
                VarBuilder::from_mmaped_safetensors(&[&model_path], DType::F32, &self.device)
            }
        } else {
            VarBuilder::from_pth(&model_path, DType::F32, &self.device)
        }
        .map_err(|e| VectorError::Model(format!("weights load failed: {e}")))?;

        let model = BertModel::load(vb, &config)
            .map_err(|e| VectorError::Model(format!("model init failed: {e}")))?;

        let dim = config.hidden_size;

        tracing::info!(
            model_id = %self.model_id,
            dim = dim,
            "embedding model loaded successfully"
        );

        Ok(LoadedModel {
            model,
            tokenizer,
            dim,
            device: self.device.clone(),
        })
    }

    /// Run inference on a single text with a loaded model.
    fn infer(loaded: &LoadedModel, text: &str) -> VectorResult<Vec<f32>> {
        if text.is_empty() {
            return Err(VectorError::InvalidInput(
                "cannot embed empty text".into(),
            ));
        }

        // Tokenize
        let encoding = loaded
            .tokenizer
            .encode(text, true)
            .map_err(|e| VectorError::Tokenizer(format!("tokenization failed: {e}")))?;

        let token_ids: Vec<u32> = encoding.get_ids().to_vec();
        if token_ids.is_empty() {
            return Err(VectorError::Tokenizer(
                "tokenizer returned empty ids".into(),
            ));
        }

        let seq_len = token_ids.len();

        // Build input tensors: [1, seq_len]
        let input_ids = Tensor::new(token_ids.as_slice(), &loaded.device)
            .map_err(|e| VectorError::Inference(format!("input_ids tensor: {e}")))?
            .unsqueeze(0)
            .map_err(|e| VectorError::Inference(format!("unsqueeze: {e}")))?;

        let token_type_ids = Tensor::zeros((1, seq_len), DType::U32, &loaded.device)
            .map_err(|e| VectorError::Inference(format!("token_type_ids: {e}")))?;

        let attention_mask = Tensor::ones((1, seq_len), DType::U32, &loaded.device)
            .map_err(|e| VectorError::Inference(format!("attention_mask: {e}")))?;

        // Forward pass
        let output = loaded
            .model
            .forward(&input_ids, &token_type_ids, Some(&attention_mask))
            .map_err(|e| VectorError::Inference(format!("forward pass: {e}")))?;

        // Mean pooling: average over all token embeddings
        let pooled = output
            .mean(1)
            .map_err(|e| VectorError::Inference(format!("mean pooling: {e}")))?;

        // Squeeze batch dim: [1, hidden] → [hidden]
        let pooled = pooled
            .squeeze(0)
            .map_err(|e| VectorError::Inference(format!("squeeze: {e}")))?;

        // Convert to Vec<f32>
        let mut embedding: Vec<f32> = pooled
            .to_vec1()
            .map_err(|e| VectorError::Inference(format!("to_vec: {e}")))?;

        // L2 normalize
        embedder::normalize(&mut embedding);

        Ok(embedding)
    }
}

impl Embedder for CandleEmbedder {
    fn dim(&self) -> usize {
        *self.dim.get_or_init(|| {
            // If not yet loaded, we can't know the dim. Return 0 as sentinel.
            // The caller should call embed() first, which will populate dim.
            0
        })
    }

    fn model_name(&self) -> &str {
        &self.model_id
    }

    fn embed(&self, text: &str) -> VectorResult<Vec<f32>> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| VectorError::Model(format!("lock poisoned: {e}")))?;

        if guard.is_none() {
            let loaded = self.load_model()?;
            let dim = loaded.dim;
            *guard = Some(loaded);
            // Cache the dimension
            let _ = self.dim.set(dim);
        }

        // SAFETY: we just ensured guard is Some
        let loaded = guard.as_ref().unwrap();
        Self::infer(loaded, text)
    }

    fn embed_batch(&self, texts: &[&str]) -> VectorResult<Vec<Vec<f32>>> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| VectorError::Model(format!("lock poisoned: {e}")))?;

        if guard.is_none() {
            let loaded = self.load_model()?;
            let dim = loaded.dim;
            *guard = Some(loaded);
            let _ = self.dim.set(dim);
        }

        let loaded = guard.as_ref().unwrap();
        texts.iter().map(|t| Self::infer(loaded, t)).collect()
    }
}

// ─── Tests ──────────────────────────────��────────────────────────────

#[cfg(test)]
mod tests {
    use crate::embedder::Embedder;
    use crate::error::VectorResult;

    /// A mock embedder that doesn't need network/models.
    struct MockEmbedder {
        dim: usize,
        model_name: String,
    }

    impl Embedder for MockEmbedder {
        fn dim(&self) -> usize {
            self.dim
        }

        fn model_name(&self) -> &str {
            &self.model_name
        }

        fn embed(&self, text: &str) -> VectorResult<Vec<f32>> {
            let hash: u64 = text
                .bytes()
                .fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(b as u64));
            let mut vec = Vec::with_capacity(self.dim);
            for i in 0..self.dim {
                let val = ((hash.wrapping_mul((i + 1) as u64) % 1000) as f32) / 1000.0;
                vec.push(val);
            }
            super::embedder::normalize(&mut vec);
            Ok(vec)
        }
    }

    #[test]
    fn noop_embedder_returns_zeros() {
        let noop = crate::NoopEmbedder::new(384);
        let vec = noop.embed("hello").expect("embed should succeed");
        assert_eq!(vec.len(), 384);
        assert!(vec.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn cosine_similarity_identical() {
        let v = vec![1.0f32, 2.0, 3.0];
        let sim = super::embedder::cosine_similarity(&v, &v);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let a = vec![1.0f32, 0.0];
        let b = vec![0.0f32, 1.0];
        let sim = super::embedder::cosine_similarity(&a, &b);
        assert!((sim - 0.0).abs() < 1e-6);
    }

    #[test]
    fn normalize_vector() {
        let mut v = vec![3.0f32, 4.0];
        super::embedder::normalize(&mut v);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
    }

    #[test]
    fn mock_embedder_consistent() {
        let mock = MockEmbedder {
            dim: 128,
            model_name: "mock".into(),
        };
        let a = mock.embed("hello").expect("embed should succeed");
        let b = mock.embed("hello").expect("embed should succeed");
        assert_eq!(a, b);
    }
}
