//! Candle-based cross-encoder reranker using BERT.
//!
//! Wraps a BERT model with a linear classification head to score
//! (query, document) pairs. Uses the same Candle infrastructure as
//! `CandleEmbedder`.

use std::sync::Mutex;

use candle_core::{DType, Device, Module, Tensor};
use candle_nn::{linear, Linear, VarBuilder};
use candle_transformers::models::bert::{BertModel, Config};
use hf_hub::api::sync::Api;
use tokenizers::tokenizer::Tokenizer;

use crate::error::{VectorError, VectorResult};
use crate::reranker::Reranker;

/// Candle-based BERT cross-encoder for re-ranking.
///
/// # Architecture
///
/// - BERT backbone (same as embedding model)
/// - Linear classification head: `hidden_size → 1`
/// - Takes `[CLS] query [SEP] document [SEP]` as input
/// - CLS token output → linear → sigmoid → relevance score in [0, 1]
///
/// # Model Compatibility
///
/// Compatible with any BERT-based reranker on HuggingFace Hub:
/// - `BAAI/bge-reranker-base` (default, ~400MB, 768-dim)
/// - `BAAI/bge-reranker-v2-m3` (multilingual, ~2.2GB)
///
/// # Lifecycle (m12-lifecycle)
///
/// Lazy-loaded on first `score()` call, guarded by `Mutex`.
pub struct CandleReranker {
    model_id: String,
    revision: Option<String>,
    device: Device,
    loaded: Mutex<Option<LoadedReranker>>,
}

struct LoadedReranker {
    model: BertModel,
    classifier: Linear,
    tokenizer: Tokenizer,
    device: Device,
}

impl CandleReranker {
    /// Create a reranker with the default model (bge-reranker-base).
    pub fn standard() -> Self {
        Self::new("BAAI/bge-reranker-base", None, Device::Cpu)
    }

    /// Create a reranker with a specific model id.
    pub fn new(model_id: &str, revision: Option<&str>, device: Device) -> Self {
        Self {
            model_id: model_id.to_string(),
            revision: revision.map(String::from),
            device,
            loaded: Mutex::new(None),
        }
    }

    fn load_model(&self) -> VectorResult<LoadedReranker> {
        tracing::info!(
            model_id = %self.model_id,
            "loading reranker model"
        );

        let api = Api::new()
            .map_err(|e| VectorError::Model(format!("HF API init: {e}")))?;

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

        // Download config
        let config_path = repo
            .get("config.json")
            .map_err(|e| VectorError::Model(format!("config download: {e}")))?;
        let config_raw = std::fs::read_to_string(&config_path)
            .map_err(|e| VectorError::Model(format!("config read: {e}")))?;
        let config: Config = serde_json::from_str(&config_raw)
            .map_err(|e| VectorError::Model(format!("config parse: {e}")))?;

        // Download tokenizer
        let tokenizer_path = repo
            .get("tokenizer.json")
            .map_err(|e| VectorError::Tokenizer(format!("tokenizer download: {e}")))?;
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| VectorError::Tokenizer(format!("tokenizer load: {e}")))?;

        // Download model weights
        let model_path = repo
            .get("model.safetensors")
            .or_else(|_| repo.get("pytorch_model.bin"))
            .map_err(|e| VectorError::Model(format!("weights download: {e}")))?;

        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[&model_path], DType::F32, &self.device)
        }
        .map_err(|e| VectorError::Model(format!("weights load: {e}")))?;

        let model = BertModel::load(vb.pp("bert"), &config)
            .or_else(|_| BertModel::load(vb.clone(), &config))
            .map_err(|e| VectorError::Model(format!("bert load: {e}")))?;

        // Classification head: hidden_size → 1
        let classifier = linear(
            config.hidden_size,
            1,
            vb.pp("classifier"),
        )
        .map_err(|e| VectorError::Model(format!("classifier load: {e}")))?;

        Ok(LoadedReranker {
            model,
            classifier,
            tokenizer,
            device: self.device.clone(),
        })
    }

    fn forward(loaded: &LoadedReranker, query: &str, document: &str) -> VectorResult<f32> {
        if query.is_empty() || document.is_empty() {
            return Ok(0.0);
        }

        // Tokenize as cross-encoder: [CLS] query [SEP] document [SEP]
        let encoding = loaded
            .tokenizer
            .encode((query, document), true) // add_special_tokens=true
            .map_err(|e| VectorError::Tokenizer(format!("encode: {e}")))?;

        let token_ids: Vec<u32> = encoding.get_ids().to_vec();
        if token_ids.is_empty() {
            return Err(VectorError::Tokenizer("empty token ids".into()));
        }

        let seq_len = token_ids.len();

        // Build tensors
        let input_ids = Tensor::new(token_ids.as_slice(), &loaded.device)
            .map_err(|e| VectorError::Inference(format!("input_ids: {e}")))?
            .unsqueeze(0)
            .map_err(|e| VectorError::Inference(format!("unsqueeze: {e}")))?;

        let token_type_ids = Tensor::zeros((1, seq_len), DType::U32, &loaded.device)
            .map_err(|e| VectorError::Inference(format!("token_type_ids: {e}")))?;

        let attention_mask = Tensor::ones((1, seq_len), DType::U32, &loaded.device)
            .map_err(|e| VectorError::Inference(format!("attention_mask: {e}")))?;

        // Forward through BERT
        let output = loaded
            .model
            .forward(&input_ids, &token_type_ids, Some(&attention_mask))
            .map_err(|e| VectorError::Inference(format!("forward: {e}")))?;

        // Extract CLS token (position 0): output shape [1, seq_len, hidden]
        let cls = output
            .get_on_dim(1, 0)
            .map_err(|e| VectorError::Inference(format!("cls extract: {e}")))?;
        // cls shape: [1, hidden] — squeeze batch dim
        let cls = cls
            .squeeze(0)
            .map_err(|e| VectorError::Inference(format!("squeeze: {e}")))?;

        // Pass through classifier
        let logit = loaded
            .classifier
            .forward(&cls)
            .map_err(|e| VectorError::Inference(format!("classifier: {e}")))?;

        // sigmoid
        let score = candle_nn::ops::sigmoid(&logit)
            .map_err(|e| VectorError::Inference(format!("sigmoid: {e}")))?;

        // Extract scalar
        let val: f32 = score
            .to_vec0()
            .map_err(|e| VectorError::Inference(format!("to_scalar: {e}")))?;

        Ok(val)
    }
}

impl Reranker for CandleReranker {
    fn score(&self, query: &str, document: &str) -> VectorResult<f32> {
        let mut guard = self
            .loaded
            .lock()
            .map_err(|e| VectorError::Model(format!("lock: {e}")))?;

        if guard.is_none() {
            *guard = Some(self.load_model()?);
        }

        Self::forward(guard.as_ref().unwrap(), query, document)
    }

    fn model_name(&self) -> &str {
        &self.model_id
    }

    fn score_batch(&self, query: &str, documents: &[&str]) -> VectorResult<Vec<f32>> {
        let mut guard = self
            .loaded
            .lock()
            .map_err(|e| VectorError::Model(format!("lock: {e}")))?;

        if guard.is_none() {
            *guard = Some(self.load_model()?);
        }

        let loaded = guard.as_ref().unwrap();
        documents
            .iter()
            .map(|doc| Self::forward(loaded, query, doc))
            .collect()
    }
}

impl Default for CandleReranker {
    fn default() -> Self {
        Self::standard()
    }
}
