//! Qwen3-Embedding via GGUF quantized models.
//!
//! Loads Qwen3-based text embedding models from GGUF files using
//! Candle's quantized tensor infrastructure (`QMatMul`).
//!
//! Adapted from `candle-transformers::models::quantized_qwen3`.
//! Key differences: no KV-cache, bidirectional attention, mean pooling.
//!
//! # Model Compatibility
//!
//! - `Qwen/Qwen3-Embedding-0.6B` (Q4_K_M ~300MB)
//! - `Qwen/Qwen3-Embedding-4B`  (Q4_K_M ~2.5GB)
//! - `Qwen/Qwen3-Embedding-8B`  (Q4_K_M ~5GB)

use std::io::{Read, Seek};
use std::path::Path;
use std::sync::{Arc, Mutex};

use candle_core::quantized::gguf_file;
use candle_core::quantized::QMatMul;
use candle_core::{DType, Device, Module, Result as CandleResult, Tensor};

use crate::embedder::{self, Embedder};
use crate::error::{VectorError, VectorResult};

// ─── GGUF loader ──────────────────────────────��─────────────────────

struct Gguf<R: Read + Seek> {
    ct: gguf_file::Content,
    reader: R,
    device: Device,
}

impl<R: Read + Seek> Gguf<R> {
    fn new(ct: gguf_file::Content, reader: R, device: &Device) -> Self {
        Self { ct, reader, device: device.clone() }
    }

    fn tensor(&mut self, name: &str, device: &Device) -> CandleResult<Tensor> {
        let qt = gguf_file::Content::tensor(&self.ct, &mut self.reader, name, device)?;
        qt.dequantize(device)
    }

    fn qmatmul(&mut self, name: &str) -> CandleResult<QMatMul> {
        let qt = gguf_file::Content::tensor(&self.ct, &mut self.reader, name, &self.device)?;
        Ok(QMatMul::from_qtensor(qt)?)
    }

    fn rms_norm(&mut self, name: &str, eps: f64, device: &Device) -> CandleResult<RmsNorm> {
        let w = self.tensor(name, device)?;
        Ok(RmsNorm { weight: w, eps })
    }
}

// ─── RMS Norm ──────────────────────────────��─────────────────────────

struct RmsNorm {
    weight: Tensor,
    eps: f64,
}

impl RmsNorm {
    fn forward(&self, x: &Tensor) -> CandleResult<Tensor> {
        let x_dtype = x.dtype();
        let w_dtype = self.weight.dtype();
        let x = if x_dtype != w_dtype { x.to_dtype(w_dtype)? } else { x.clone() };
        let rms = (x.sqr()?.mean_keepdim(2)? + self.eps as f64)?.sqrt()?;
        x.broadcast_div(&rms)?.broadcast_mul(&self.weight)
    }
}

// ─── RoPE ──────────────────────────────��─────────────────────────────

struct RotaryEmbedding {
    cos: Tensor,
    sin: Tensor,
}

impl RotaryEmbedding {
    fn new(dim: usize, max_seq_len: usize, rope_theta: f64, dev: &Device) -> CandleResult<Self> {
        let inv_freq: Vec<f32> = (0..dim)
            .step_by(2)
            .map(|i| 1f32 / (rope_theta as f32).powf(i as f32 / dim as f32))
            .collect();
        let inv_freq = Tensor::from_vec(inv_freq, (1, dim / 2), dev)?;
        let t = Tensor::arange(0f32, max_seq_len as f32, dev)?
            .reshape((max_seq_len, 1))?
            .to_dtype(DType::F32)?;
        let freqs = t.matmul(&inv_freq)?;
        Ok(Self { sin: freqs.sin()?, cos: freqs.cos()? })
    }

    fn apply(&self, q: &Tensor, k: &Tensor, offset: usize) -> CandleResult<(Tensor, Tensor)> {
        let (_, _, seq_len, _) = q.dims4()?;
        let cos = self.cos.narrow(0, offset, seq_len)?;
        let sin = self.sin.narrow(0, offset, seq_len)?;
        let q_embed = candle_nn::rotary_emb::rope(&q.contiguous()?, &cos, &sin)?;
        let k_embed = candle_nn::rotary_emb::rope(&k.contiguous()?, &cos, &sin)?;
        Ok((q_embed, k_embed))
    }
}

// ─── Attention ──────────────────────────────────────────────────────

struct AttentionWeights {
    q_proj: QMatMul,
    k_proj: QMatMul,
    v_proj: QMatMul,
    o_proj: QMatMul,
    q_norm: RmsNorm,
    k_norm: RmsNorm,
    num_heads: usize,
    num_kv_heads: usize,
    num_kv_groups: usize,
    head_dim: usize,
    rotary_emb: Arc<RotaryEmbedding>,
}

impl AttentionWeights {
    fn new<R: Read + Seek>(
        gg: &mut Gguf<R>,
        num_heads: usize,
        num_kv_heads: usize,
        head_dim: usize,
        rms_norm_eps: f64,
        rotary_emb: Arc<RotaryEmbedding>,
        prefix: &str,
    ) -> CandleResult<Self> {
        let dev = gg.device.clone();
        let num_kv_groups = num_heads / num_kv_heads;
        let q_proj = gg.qmatmul(&format!("{prefix}.attn_q.weight"))?;
        let k_proj = gg.qmatmul(&format!("{prefix}.attn_k.weight"))?;
        let v_proj = gg.qmatmul(&format!("{prefix}.attn_v.weight"))?;
        let o_proj = gg.qmatmul(&format!("{prefix}.attn_output.weight"))?;
        let q_norm = gg.rms_norm(&format!("{prefix}.attn_q_norm.weight"), rms_norm_eps, &dev)?;
        let k_norm = gg.rms_norm(&format!("{prefix}.attn_k_norm.weight"), rms_norm_eps, &dev)?;
        Ok(Self { q_proj, k_proj, v_proj, o_proj, q_norm, k_norm, num_heads, num_kv_heads, num_kv_groups, head_dim, rotary_emb })
    }

    fn forward(&mut self, x: &Tensor, mask: Option<&Tensor>, offset: usize) -> CandleResult<Tensor> {
        let (b, l, _) = x.dims3()?;
        let q = self.q_proj.forward(x)?;
        let k = self.k_proj.forward(x)?;
        let v = self.v_proj.forward(x)?;

        let q = q.reshape((b, l, self.num_heads, self.head_dim))?.transpose(1, 2)?;
        let k = k.reshape((b, l, self.num_kv_heads, self.head_dim))?.transpose(1, 2)?;
        let v = v.reshape((b, l, self.num_kv_heads, self.head_dim))?.transpose(1, 2)?;

        let q = self.q_norm.forward(&q)?;
        let k = self.k_norm.forward(&k)?;

        let (q, k) = self.rotary_emb.apply(&q, &k, offset)?;

        let k = if self.num_kv_groups > 1 { k.repeat((1, self.num_kv_groups, 1, 1))? } else { k };
        let v = if self.num_kv_groups > 1 { v.repeat((1, self.num_kv_groups, 1, 1))? } else { v };

        let attn = q.matmul(&k.t()?)?;
        let attn = (attn / (self.head_dim as f64).sqrt())?;
        let attn = match mask {
            Some(m) => attn.broadcast_add(m)?,
            None => attn,
        };
        let attn = candle_nn::ops::softmax_last_dim(&attn)?;
        let attn = attn.matmul(&v)?;
        let attn = attn.transpose(1, 2)?.reshape((b, l, self.num_heads * self.head_dim))?;
        self.o_proj.forward(&attn)
    }
}

// ─── MLP ─────────────────────────────────────────────────────────────

struct MlpWeights {
    gate_proj: QMatMul,
    up_proj: QMatMul,
    down_proj: QMatMul,
}

impl MlpWeights {
    fn new<R: Read + Seek>(gg: &mut Gguf<R>, prefix: &str) -> CandleResult<Self> {
        Ok(Self {
            gate_proj: gg.qmatmul(&format!("{prefix}.ffn_gate.weight"))?,
            up_proj: gg.qmatmul(&format!("{prefix}.ffn_up.weight"))?,
            down_proj: gg.qmatmul(&format!("{prefix}.ffn_down.weight"))?,
        })
    }

    fn forward(&self, x: &Tensor) -> CandleResult<Tensor> {
        let gate = candle_nn::ops::silu(&self.gate_proj.forward(x)?)?;
        let up = self.up_proj.forward(x)?;
        let prod = gate.broadcast_mul(&up)?;
        self.down_proj.forward(&prod)
    }
}

// ─── Decoder Layer ──────────────────────────────��───────────────────

struct DecoderLayerWeights {
    attn: AttentionWeights,
    mlp: MlpWeights,
    input_layernorm: RmsNorm,
    post_attn_norm: RmsNorm,
}

impl DecoderLayerWeights {
    fn new<R: Read + Seek>(
        gg: &mut Gguf<R>,
        num_heads: usize,
        num_kv_heads: usize,
        head_dim: usize,
        rms_norm_eps: f64,
        rotary_emb: Arc<RotaryEmbedding>,
        prefix: &str,
    ) -> CandleResult<Self> {
        let dev = gg.device.clone();
        Ok(Self {
            attn: AttentionWeights::new(gg, num_heads, num_kv_heads, head_dim, rms_norm_eps, rotary_emb, prefix)?,
            mlp: MlpWeights::new(gg, prefix)?,
            input_layernorm: gg.rms_norm(&format!("{prefix}.attn_norm.weight"), rms_norm_eps, &dev)?,
            post_attn_norm: gg.rms_norm(&format!("{prefix}.ffn_norm.weight"), rms_norm_eps, &dev)?,
        })
    }

    fn forward(&mut self, x: &Tensor, mask: Option<&Tensor>, offset: usize) -> CandleResult<Tensor> {
        let residual = x.clone();
        let x = self.input_layernorm.forward(x)?;
        let x = self.attn.forward(&x, mask, offset)?;
        let x = (x + residual)?;
        let residual = x.clone();
        let x = self.post_attn_norm.forward(&x)?;
        let x = self.mlp.forward(&x)?;
        x.broadcast_add(&residual)
    }
}

// ─── Model ──────────────────────────────��────────────────────────────

struct Qwen3Model {
    layers: Vec<DecoderLayerWeights>,
    output_norm: RmsNorm,
}

impl Qwen3Model {
    fn new<R: Read + Seek>(
        gg: &mut Gguf<R>,
        num_layers: usize,
        num_heads: usize,
        num_kv_heads: usize,
        head_dim: usize,
        rms_norm_eps: f64,
        rotary_emb: Arc<RotaryEmbedding>,
    ) -> CandleResult<Self> {
        let mut layers = Vec::with_capacity(num_layers);
        for i in 0..num_layers {
            layers.push(DecoderLayerWeights::new(
                gg, num_heads, num_kv_heads, head_dim, rms_norm_eps,
                rotary_emb.clone(),
                &format!("blk.{i}"),
            )?);
        }
        let dev = gg.device.clone();
        let output_norm = gg.rms_norm("output_norm.weight", rms_norm_eps, &dev)?;
        Ok(Self { layers, output_norm })
    }

    fn forward_bidirectional(&mut self, x: &Tensor) -> CandleResult<Tensor> {
        let mut h = x.clone();
        for layer in &mut self.layers {
            h = layer.forward(&h, None, 0)?;
        }
        self.output_norm.forward(&h)
    }
}

// ─── Qwen3Embedder ──────────────────────────────��───────────────────

/// Qwen3-Embedding model loaded from a GGUF file.
pub struct Qwen3Embedder {
    gguf_path: String,
    device: Device,
    loaded: Mutex<Option<Loaded>>,
}

struct Loaded {
    dim: usize,
    token_emb: Tensor,
    model: Qwen3Model,
}

impl Qwen3Embedder {
    /// Create a new embedder for a GGUF file on disk.
    pub fn new(gguf_path: &Path, device: Device) -> Self {
        Self {
            gguf_path: gguf_path.to_string_lossy().into_owned(),
            device,
            loaded: Mutex::new(None),
        }
    }

    fn load(&self) -> VectorResult<Loaded> {
        tracing::info!(path = %self.gguf_path, "loading Qwen3-Embedding GGUF");

        let mut file = std::fs::File::open(&self.gguf_path)
            .map_err(|e| VectorError::Model(format!("open: {e}")))?;
        let ct = gguf_file::Content::read(&mut file)
            .map_err(|e| VectorError::Model(format!("read GGUF: {e}")))?;

        let get_u = |key: &str| {
            ct.metadata.get(key)
                .and_then(|v| v.to_u32().ok())
                .unwrap_or(0) as usize
        };
        let get_f = |key: &str, default: f64| {
            ct.metadata.get(key)
                .and_then(|v| v.to_f64().ok())
                .unwrap_or(default)
        };

        let h0 = get_u("qwen3.attention.head_count");
        let h1 = get_u("qwen3.attention.head_count_kv");
        let h1 = if h1 == 0 { h0 } else { h1 };
        let hidden = get_u("qwen3.embedding_length");
        let num_layers = get_u("qwen3.block_count");
        let rms_norm_eps = get_f("qwen3.attention.layer_norm_rms_epsilon", 1e-6);
        let rope_theta = get_f("qwen3.rope.freq_base", 1_000_000.0);
        // head_dim = key_length (Q/K norm dimension), NOT hidden/num_heads
        let mut head_dim = get_u("qwen3.attention.key_length");
        if head_dim == 0 {
            head_dim = hidden / h0;
        }
        let dim = hidden;

        if h0 == 0 || hidden == 0 || num_layers == 0 {
            return Err(VectorError::Model("missing GGUF metadata".into()));
        }

        // Re-open for tensor reading
        let mut file = std::fs::File::open(&self.gguf_path)
            .map_err(|e| VectorError::Model(format!("reopen: {e}")))?;
        let ct = gguf_file::Content::read(&mut file)
            .map_err(|e| VectorError::Model(format!("re-read GGUF: {e}")))?;
        let mut gg = Gguf::new(ct, file, &self.device);

        let token_emb = gg.tensor("token_embd.weight", &self.device)
            .map_err(|e| VectorError::Model(format!("token_embd: {e}")))?;

        let rotary_emb = Arc::new(
            RotaryEmbedding::new(head_dim, 32768, rope_theta, &self.device)
                .map_err(|e| VectorError::Model(format!("rope: {e}")))?,
        );

        let model = Qwen3Model::new(
            &mut gg, num_layers, h0, h1, head_dim, rms_norm_eps, rotary_emb,
        ).map_err(|e| VectorError::Model(format!("model: {e}")))?;

        tracing::info!(h0, h1, hidden, num_layers, head_dim, "Qwen3-Embedding loaded");

        Ok(Loaded { dim, token_emb, model })
    }
}

impl Embedder for Qwen3Embedder {
    fn dim(&self) -> usize {
        self.loaded.lock().ok()
            .and_then(|g| g.as_ref().map(|l| l.dim))
            .unwrap_or(0)
    }

    fn model_name(&self) -> &str {
        "qwen3-embedding-gguf"
    }

    fn embed(&self, text: &str) -> VectorResult<Vec<f32>> {
        if text.is_empty() {
            return Err(VectorError::InvalidInput("empty text".into()));
        }

        let mut guard = self.loaded.lock()
            .map_err(|e| VectorError::Model(format!("lock: {e}")))?;
        if guard.is_none() {
            *guard = Some(self.load()?);
        }
        let loaded = guard.as_mut().unwrap();

        // Simple byte-level tokenization (GGUF embeds the tokenizer,
        // but extracting it requires more work — TODO: use proper tokenizer)
        let tokens: Vec<u32> = text.bytes().map(|b| b as u32).collect();

        let token_ids = Tensor::new(tokens.as_slice(), &self.device)
            .map_err(|e| VectorError::Inference(format!("ids: {e}")))?;
        let input_emb = loaded.token_emb.embedding(&token_ids)
            .map_err(|e| VectorError::Inference(format!("embed: {e}")))?;
        let input_emb = input_emb.unsqueeze(0)
            .map_err(|e| VectorError::Inference(format!("unsqueeze: {e}")))?;

        let hidden = loaded.model.forward_bidirectional(&input_emb)
            .map_err(|e| VectorError::Inference(format!("forward: {e}")))?;

        // Mean pooling
        let pooled = hidden.mean(1)
            .map_err(|e| VectorError::Inference(format!("mean: {e}")))?;
        let pooled = pooled.squeeze(0)
            .map_err(|e| VectorError::Inference(format!("squeeze: {e}")))?;

        let mut embedding: Vec<f32> = pooled.to_vec1()
            .map_err(|e| VectorError::Inference(format!("to_vec: {e}")))?;

        embedder::normalize(&mut embedding);
        Ok(embedding)
    }
}

// ─── Diagnostic ──────────────────────────────��──────────────────────

/// Diagnostic: print tensor names and key metadata (skips tokenizer vocab).
pub fn diagnose_gguf(path: &Path) -> VectorResult<()> {
    use candle_core::quantized::gguf_file;

    let mut file = std::fs::File::open(path)
        .map_err(|e| VectorError::Model(format!("open: {e}")))?;
    let ct = gguf_file::Content::read(&mut file)
        .map_err(|e| VectorError::Model(format!("read: {e}")))?;

    println!("=== Key Metadata ===");
    let keys = [
        "qwen3.attention.head_count",
        "qwen3.attention.head_count_kv",
        "qwen3.attention.key_length",
        "qwen3.embedding_length",
        "qwen3.feed_forward_length",
        "qwen3.block_count",
        "qwen3.context_length",
        "qwen3.attention.layer_norm_rms_epsilon",
        "qwen3.rope.freq_base",
        "qwen3.pooling_type",
        "general.file_type",
        "tokenizer.ggml.model",
    ];
    for key in &keys {
        if let Some(v) = ct.metadata.get(*key) {
            println!("  {key}: {v:?}");
        }
    }

    println!("\n=== Tensor Name + Shape (sample) ===");
    let mut printed = 0;
    for (name, info) in &ct.tensor_infos {
        if printed > 15 { break; }
        if name.starts_with("tokenizer") { continue; }
        println!("  {name}: {:?}", info.shape);
        printed += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn real_embed_test() {
        let path = std::path::Path::new(
            r"C:\Users\QAQTam\Downloads\Qwen3-Embedding-4B-Q4_K_M.gguf"
        );
        if !path.exists() {
            println!("GGUF file not found, skipping");
            return;
        }
        let embedder = Qwen3Embedder::new(path, Device::Cpu);
        match embedder.embed("你好世界") {
            Ok(vec) => {
                println!("Embedding dim: {}", vec.len());
                println!("First 10 values: {:?}", &vec[..10.min(vec.len())]);
                println!("Norm: {}", vec.iter().map(|x| x*x).sum::<f32>().sqrt());
            }
            Err(e) => println!("Embedding failed: {e}"),
        }
    }

    #[test]
    #[ignore]
    fn diagnose_gguf_test() {
        let path = std::path::Path::new(
            r"C:\Users\QAQTam\Downloads\Qwen3-Embedding-4B-Q4_K_M.gguf"
        );
        if path.exists() {
            diagnose_gguf(path).unwrap();
        } else {
            println!("GGUF file not found at {:?}, skipping", path);
        }
    }
}
