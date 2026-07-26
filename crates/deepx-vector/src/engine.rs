//! 向量引擎 —— embedding + 搜索 + 记忆 的统一入口。
//!
//! `VectorEngine` 是 crate 的顶层编排：
//! - 管理嵌入器（Embedder）的生命周期
//! - 维护技能索引 + 文档索引两个独立的向量存储
//! - 代理 `MemoryStore` 的跨会话记忆读写
//!
//! # 使用示例
//!
//! ```ignore
//! let mut engine = VectorEngine::init(VectorConfig::default())?;
//! engine.index_skills(&[("skill1".into(), "skill body...".into())])?;
//! let chunks = engine.search_skills("查询语句")?;
//! let ctx = engine.format_skill_context(&chunks);
//! ```

use std::path::PathBuf;

use crate::{
    BruteForceStore, CandleEmbedder, Embedder, MemoryEntry, MemoryStore, SearchResult,
    TextChunker, VectorError, VectorResult,
};
use crate::chunk::Chunker;
use crate::store::VectorStore;

// ─── VectorConfig ───────────────────────────────────────────────────────

/// 向量引擎配置
#[derive(Debug, Clone)]
pub struct VectorConfig {
    /// 是否启用向量引擎（false 时不加载模型，AgentState.vector = None）
    pub enabled: bool,
    /// HuggingFace 模型 ID
    pub model_id: String,
    /// 嵌入向量维度（bge-small = 512, bge-base = 768, qwen3 = 2560）
    pub embed_dim: usize,
    /// 数据存储根目录（向量索引、记忆文件）
    pub store_dir: PathBuf,
    /// 跨会话记忆目录（默认 store_dir/memory）
    pub memory_dir: PathBuf,
    /// 技能检索 top-K
    pub skill_top_k: usize,
    /// 记忆检索 top-K
    pub memory_top_k: usize,
    /// 文本分块最大字符数
    pub max_chunk_size: usize,
    /// 文本分块最小字符数
    pub min_chunk_size: usize,
    /// 本地模型目录（设置后跳过 HF Hub 下载，直接从本地加载）
    pub local_model: Option<std::path::PathBuf>,
}

impl Default for VectorConfig {
    fn default() -> Self {
        let home = home_dir();
        let base = PathBuf::from(home).join(".deepx").join("vector");
        Self {
            enabled: true,
            model_id: "BAAI/bge-small-zh-v1.5".into(),
            embed_dim: 512,
            store_dir: base.clone(),
            memory_dir: base.join("memory"),
            skill_top_k: 5,
            memory_top_k: 3,
            max_chunk_size: 500,
            min_chunk_size: 50,
            local_model: None,
        }
    }
}

fn home_dir() -> String {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".into())
}

// ─── SkillChunk ─────────────────────────────────────────────────────────

/// 技能文本块 — 语义搜索的返回单元
#[derive(Debug, Clone)]
pub struct SkillChunk {
    /// 所属技能名称
    pub skill_name: String,
    /// 块在该技能中的序号（从 0 开始）
    pub chunk_index: usize,
    /// 文本内容
    pub text: String,
    /// 与查询的余弦相似度 (0..1)
    pub score: f32,
}

// ─── VectorEngine ──────────────────────────────────────────────────────

/// 向量引擎
///
/// 线程安全：内部使用 `Box<dyn Embedder>` (CandleEmbedder 内部有 Mutex)。
/// 多线程共享时用 `Arc<Mutex<VectorEngine>>` 包裹。
pub struct VectorEngine {
    embedder: Box<dyn Embedder>,
    /// 技能内容向量索引（按 chunk 存储）
    skill_store: BruteForceStore,
    /// 用户文档向量索引
    doc_store: BruteForceStore,
    /// 跨会话持久记忆
    memory: MemoryStore,
    /// 文本分块器
    chunker: TextChunker,
    /// 配置副本
    #[allow(dead_code)]
    config: VectorConfig,
}

impl std::fmt::Debug for VectorEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VectorEngine")
            .field("skill_store_len", &self.skill_store.len())
            .field("doc_store_len", &self.doc_store.len())
            .field("store_dir", &self.config.store_dir)
            .finish()
    }
}

impl VectorEngine {
    // ── 初始化 ──────────────────────────────��────────────────────────

    /// 启动时初始化引擎。
    ///
    /// - 创建 CandleEmbedder（模型首次 `embed()` 时从 HF Hub 下载）
    /// - 创建独立的 skill / doc 向量存储
    /// - 打开或创建 MemoryStore
    ///
    /// 若 `config.enabled == false`，返回错误，由调用方决定跳过。
    pub fn init(config: VectorConfig) -> VectorResult<Self> {
        if !config.enabled {
            return Err(VectorError::Store("向量引擎未启用".into()));
        }

        let embedder: Box<dyn Embedder> = if let Some(ref local_path) = config.local_model {
            if local_path.is_dir() {
                Box::new(CandleEmbedder::from_local(local_path))
            } else {
                return Err(VectorError::Store(format!(
                    "本地模型目录不存在: {}",
                    local_path.display()
                )));
            }
        } else {
            Box::new(CandleEmbedder::new(
                &config.model_id,
                None,
                candle_core::Device::Cpu,
            ))
        };
        let memory = MemoryStore::open(&config.memory_dir)?;

        Ok(Self {
            embedder,
            skill_store: BruteForceStore::new(config.embed_dim),
            doc_store: BruteForceStore::new(config.embed_dim),
            memory,
            chunker: TextChunker::new(config.max_chunk_size, config.min_chunk_size),
            config,
        })
    }

    // ── 技能索引 / 搜索 ──────────────────────────────────────────────

    /// 索引技能内容：分块 → 嵌入 → 存入 skill_store。
    ///
    /// `skills` 中的每个元素是 (技能名称, 技能 body 文本)。
    pub fn index_skills(&mut self, skills: &[(String, String)]) -> VectorResult<()> {
        for (name, body) in skills {
            let chunks = self.chunker.chunk(body)?;
            for (i, chunk_text) in chunks.iter().enumerate() {
                let id = format!("{name}:{i}");
                let vec = self.embedder.embed(chunk_text)?;
                let meta = serde_json::json!({
                    "skill": name,
                    "chunk_index": i,
                    "text": chunk_text,
                });
                self.skill_store
                    .insert(&id, vec, &meta.to_string())
                    .map_err(|e| VectorError::Store(format!("索引技能块失败: {e}")))?;
            }
        }
        Ok(())
    }

    /// 清空并重新索引所有技能（用于技能列表变更时）。
    pub fn reindex_skills(&mut self, skills: &[(String, String)]) -> VectorResult<()> {
        self.skill_store = BruteForceStore::new(self.config.embed_dim);
        self.index_skills(skills)
    }

    /// 语义搜索技能内容，返回 top-K 相关块。
    pub fn search_skills(&self, query: &str) -> VectorResult<Vec<SkillChunk>> {
        let query_vec = self.embedder.embed(query)?;
        let top_k = self.config.skill_top_k.min(self.skill_store.len());
        if top_k == 0 {
            return Ok(vec![]);
        }
        let results = self.skill_store.search(&query_vec, top_k)?;

        Ok(results
            .into_iter()
            .filter_map(|r| {
                let meta: serde_json::Value = serde_json::from_str(&r.metadata).ok()?;
                Some(SkillChunk {
                    skill_name: meta.get("skill")?.as_str()?.to_string(),
                    chunk_index: meta.get("chunk_index")?.as_u64()? as usize,
                    text: meta.get("text")?.as_str()?.to_string(),
                    score: r.score,
                })
            })
            .collect())
    }

    /// 将检索到的技能块格式化为 LLM 上下文字符串。
    pub fn format_skill_context(&self, chunks: &[SkillChunk]) -> String {
        if chunks.is_empty() {
            return String::new();
        }
        let mut out = String::from("## 相关技能知识（语义检索）\n\n");
        for (i, chunk) in chunks.iter().enumerate() {
            out.push_str(&format!(
                "**[{}]** (技能: {}, 相关度: {:.2})\n{}\n\n",
                i + 1,
                chunk.skill_name,
                chunk.score,
                chunk.text
            ));
        }
        out
    }

    // ── 文档索引 / 搜索 ──────────────────────────────────────────────

    /// 索引用户文档（对工具 `rag_index` 暴露）。
    pub fn index_docs(&mut self, docs: &[(String, String)]) -> VectorResult<()> {
        for (id, text) in docs {
            let chunks = self.chunker.chunk(text)?;
            for (i, chunk_text) in chunks.iter().enumerate() {
                let chunk_id = format!("{id}:{i}");
                let vec = self.embedder.embed(chunk_text)?;
                self.doc_store.insert(&chunk_id, vec, chunk_text)?;
            }
        }
        Ok(())
    }

    /// 语义搜索已索引的文档。
    pub fn search_docs(&self, query: &str, top_k: usize) -> VectorResult<Vec<SearchResult>> {
        let query_vec = self.embedder.embed(query)?;
        self.doc_store.search(&query_vec, top_k)
    }

    /// 文档索引中的条目数。
    pub fn doc_count(&self) -> usize {
        self.doc_store.len()
    }

    /// 记忆存储目录（供外部读取，避免路径重复计算）
    pub fn memory_dir(&self) -> &std::path::Path {
        &self.config.memory_dir
    }

    /// 技能索引中的条目数（chunk 数量）。
    pub fn skill_count(&self) -> usize {
        self.skill_store.len()
    }

    // ── 跨会话记忆 ──────────────────────────────��────────────────────

    /// 语义检索跨会话记忆。
    pub fn recall_memory(
        &self,
        query: &str,
        limit: usize,
    ) -> VectorResult<Vec<MemoryEntry>> {
        self.memory.recall_semantic(self.embedder.as_ref(), query, limit)
    }

    /// 关键字检索跨会话记忆。
    pub fn recall_memory_keyword(&self, keyword: &str, limit: usize) -> Vec<MemoryEntry> {
        self.memory.recall_keyword(keyword, limit)
    }

    /// 归档一条记忆。
    pub fn archive_memory(&mut self, entry: MemoryEntry) -> VectorResult<()> {
        self.memory.remember(entry)
    }

    /// 手动写入一条记忆（带自动 id 和 session 标记）。
    /// 手动写入一条记忆。
    pub fn write_memory(
        &self,
        content: &str,
        memory_type: &str,
    ) -> VectorResult<MemoryEntry> {
        let id = format!(
            "manual_{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string();
        let entry = MemoryEntry {
            id,
            session_id: "manual".to_string(),
            content: content.to_string(),
            memory_type: memory_type.to_string(),
            timestamp: String::new(),
            metadata: ts,
        };
        self.memory.remember(entry.clone())?;
        Ok(entry)
    }

    /// 删除一条记忆。
    pub fn delete_memory(&self, id: &str) -> VectorResult<bool> {
        self.memory.forget(id)
    }

    /// 将记忆条目格式化为 LLM 友好的上下文。
    pub fn format_memory_context(entries: &[MemoryEntry]) -> String {
        if entries.is_empty() {
            return String::new();
        }
        let mut out = String::from("## 历史相关记忆\n\n");
        for (i, entry) in entries.iter().enumerate() {
            out.push_str(&format!(
                "**[{}]** (会话: {}, 类型: {})\n>{}\n\n",
                i + 1,
                &entry.session_id[..entry.session_id.len().min(12)],
                entry.memory_type,
                entry.content
            ));
        }
        out
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_config() -> VectorConfig {
        // Use temp dir to avoid polluting real data.
        let tmp = std::env::temp_dir().join(format!("deepx_vector_test_{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp);
        VectorConfig {
            enabled: true,
            model_id: "BAAI/bge-small-zh-v1.5".into(),
            embed_dim: 512,
            store_dir: tmp.clone(),
            memory_dir: tmp.join("memory"),
            skill_top_k: 3,
            memory_top_k: 2,
            max_chunk_size: 200,
            min_chunk_size: 20,
            local_model: None,
        }
    }

    #[test]
    fn config_default_uses_bge_small() {
        let cfg = VectorConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.model_id, "BAAI/bge-small-zh-v1.5");
        assert_eq!(cfg.embed_dim, 512);
        assert_eq!(cfg.skill_top_k, 5);
        assert_eq!(cfg.memory_top_k, 3);
    }

    #[test]
    fn engine_disabled_returns_error() {
        let mut cfg = test_config();
        cfg.enabled = false;
        assert!(VectorEngine::init(cfg).is_err());
    }

    #[test]
    #[cfg(feature = "candle")]
    fn engine_init_ok_with_candle() {
        let cfg = test_config();
        let engine = VectorEngine::init(cfg);
        // Candle model may not be available in CI, so skip if model download fails.
        match engine {
            Ok(_) => {} // success
            Err(e) => eprintln!("(expected in CI) {e}"),
        }
    }

    #[test]
    fn format_skill_context_empty() {
        let cfg = test_config();
        let engine = VectorEngine::init(cfg).unwrap_or_else(|_| {
            // Create minimal engine without real model for formatting test.
            // We can't easily construct without Candle, but the format method is pure.
            panic!("Need candle feature for this test")
        });
        let result = engine.format_skill_context(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn format_skill_context_non_empty() {
        let cfg = test_config();
        let engine = VectorEngine::init(cfg).unwrap_or_else(|_| {
            panic!("Need candle feature for this test")
        });
        let chunks = vec![SkillChunk {
            skill_name: "test-skill".into(),
            chunk_index: 0,
            text: "这是一段测试文本".into(),
            score: 0.95,
        }];
        let result = engine.format_skill_context(&chunks);
        assert!(result.contains("test-skill"));
        assert!(result.contains("这是一段测试文本"));
        assert!(result.contains("0.95"));
    }

    #[test]
    fn format_memory_context_entries() {
        let entries = vec![MemoryEntry {
            id: "mem-1".into(),
            session_id: "sess-1".into(),
            memory_type: "修复".into(),
            content: "修复了 Vulkan CRT 链接问题".into(),
            timestamp: "2026-01-01T00:00:00Z".into(),
            metadata: String::new(),
        }];
        let result = VectorEngine::format_memory_context(&entries);
        assert!(result.contains("修复了 Vulkan CRT 链接问题"));
        assert!(result.contains("修复"));
        assert!(result.contains("sess-1"));
    }

    #[test]
    fn search_skills_empty_store() {
        let cfg = test_config();
        let engine = VectorEngine::init(cfg).unwrap();
        let results = engine.search_skills("test query").unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn doc_count_initially_zero() {
        let cfg = test_config();
        let engine = VectorEngine::init(cfg).unwrap();
        assert_eq!(engine.doc_count(), 0);
    }
}
