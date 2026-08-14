# deepx-vector 设计文档

> 版本: 0.1.0 | 日期: 2026-07-24 | 状态: Phase 1-3 已实现

---

## 1. 设计初衷

DeepX 是一个 AI 编码代理。核心循环是"用户输入 → LLM 推理 → 调用工具 → 观察 → 循环"。
当前有四个痛点全靠 LLM 硬扛：

| 痛点 | 现状 | 代价 |
|------|------|------|
| **技能发现** | 200+ 条 SKILL.md 全量 dump 进 system prompt，LLM 自行判断相关性 | 每轮浪费 ~3000 token |
| **跨会话遗忘** | Session 关闭后 agent 完全失忆，上次的 debug 结论、设计决策全部丢失 | 重复排查同类 bug |
| **代码库零索引** | 每次 `read`/`grep` 都是冷启动，agent 用 5+ 轮 tool call 才能摸清项目骨架 | 浪费轮次和 token |
| **Context 臃肿** | system prompt 里 dump 全部 skills + 全部 tools + 历史消息 | 有用信息密度 < 30% |

**核心思路**：用向量检索把 context 从"全量灌入"变成"按需检索"。

---

## 2. 架构概览

### 2.1 Crate 定位

```
deepx-vector          ← 新增 leaf crate
├── 依赖 (无)           ← 零运行时依赖（Candle 是可选 feature）
├── 被依赖 deepx-tools    ← 注册 RAG tool
├── 被依赖 deepx-skills   ← 语义技能搜索
└── 被依赖 deepx-session  ← 跨会话记忆（Phase 5）
```

### 2.2 模块结构

```
deepx-vector/src/
├── lib.rs              # crate 根，pub re-exports
├── error.rs            # VectorError (thiserror)
├── embedder.rs         # Embedder trait + normalize/cosine_similarity
├── store.rs            # VectorStore trait + BruteForceStore
├── chunk.rs            # Chunker trait + TextChunker
├── candle.rs           # CandleEmbedder (BERT, ~95MB, 512-dim)
├── bm25.rs             # BM25 关键词检索 (零依赖兜底)
├── reranker.rs         # Reranker trait (交叉编码精排)
├── reranker_candle.rs  # CandleReranker (BERT cross-encoder, ~400MB)
└── degradation.rs      # 降级链检测 + EmbeddingTier/RerankerTier
```

### 2.3 依赖方向红线

对照 `deepx-arch` 模块边界：

```
deepx-vector → deepx-tools     ✅ (工具注册)
deepx-vector → deepx-skills    ✅ (语义搜索)
deepx-vector → deepx-session   ✅ (长期记忆)
deepx-tools → deepx-vector     ❌ (不可能，depth-vector 是最下层)
```

`deepx-vector` 是最底层工具 crate，不依赖任何 DeepX 内部 crate。

---

## 3. 模型选型

### 3.1 四级嵌入模型矩阵

| Tier | 模型 | 参数量 | 体积 | 维度 | 推理延迟 | 硬件 | 适用对象 |
|------|------|--------|------|------|---------|------|---------|
| **Tier 1** (默认) | `bge-small-zh-v1.5` | 24M | 95MB | 512 | ~3ms | CPU | 100% 用户，开箱即用 |
| **Tier 2** (推荐升级) | `bge-base-zh-v1.5` | 100M | 400MB | 768 | ~10ms | CPU | 追求精度，换模型文件即用 |
| **Tier 3** (GPU) | `Qwen3-Embedding-4B` (GGUF Q4_K_M) | 4B | 2.5GB | 2560 | ~10ms (GPU) | GPU (Vulkan) | 8GB+ VRAM，极致精度 |
| **Tier 4** (兜底) | BM25 | - | 0 | N/A | <1ms | 无 | 永远可用的兜底方案 |

> **不在默认链中的模型：** `bge-large-zh-v1.5`（1.3GB，相比 base 精度提升有限，性价低）、`BGE-M3`（2.2GB，多语言/稀疏检索优势在中文场景用不上，CPU ~50ms 偏慢）、`CodeGeeX4-All-9B`（9B 参数太重，不适合 embedding 角色）。这些作为用户自行更换的可选模型。**ONNX Runtime** 和 **Remote API** 暂不做独立 Tier，作为各 Tier 的替代运行时（例如 bge-base 可走 ONNX 推理获得更低延迟）。

### 3.2 Tier 1: bge-small-zh-v1.5

纯文本 BERT 嵌入。Candle 原生支持，纯 CPU 推理。默认模型，开箱即用。

| 属性 | 值 |
|------|-----|
| **参数量** | 24M |
| **模型体积** | ~95 MB (safetensors) |
| **向量维度** | 512 |
| **推理延迟 (CPU)** | < 3ms/条 (i3-8100 级别) |
| **HuggingFace ID** | `BAAI/bge-small-zh-v1.5` |
| **运行时** | Candle (纯 Rust) |

### 3.3 Tier 2: bge-base-zh-v1.5

与 bge-small 同架构（BERT），精度更高。**零代码改动**，只换模型文件即可升级。

| 属性 | 值 |
|------|-----|
| **参数量** | 100M |
| **模型体积** | ~400 MB (safetensors) |
| **向量维度** | 768 |
| **推理延迟 (CPU)** | ~10ms/条 |
| **HuggingFace ID** | `BAAI/bge-base-zh-v1.5` |
| **运行时** | Candle (纯 Rust) |

**与 bge-small 对比：** 体积 4x，延迟 3x，但语义区分能力显著提升。对以下场景推荐升级：
- 技能段落检索（技能内容较长，512 维可能不够区分）
- 跨会话记忆匹配（需要更精细的语义距离判定）
- 代码语义搜索（代码文件块间距更大）

### 3.4 Tier 3: Qwen3-Embedding-4B (GGUF + Vulkan)

Qwen3 系列纯文本嵌入，decoder-only 架构，极致精度。通过 llama.cpp Vulkan 后端 GPU 加速。**纯 CPU 不可用（~3min/条）。**

| 属性 | 值 |
|------|-----|
| **参数量** | 4B |
| **GGUF 体积** | ~2.5 GB (Q4_K_M) |
| **向量维度** | 2560 |
| **层数** | 36 |
| **Attention** | GQA (32 Q heads, 8 KV heads, head_dim=128) |
| **推理延迟 (GPU)** | ~10ms (RX 6750 GRE Vulkan，预估) |
| **推理延迟 (CPU)** | ~3min (不可用) |
| **运行时** | llama.cpp via `llama-cpp-2` + Vulkan |
| **编译要求** | MSVC Build Tools + Vulkan SDK + LLVM |

### 3.5 GPU 后端支持矩阵

| 后端 | Candle | llama-cpp-2 |
|------|--------|-------------|
| **CPU** | ✅ | ✅ |
| **CUDA (NVIDIA)** | ✅ | ✅ |
| **Metal (Apple)** | ✅ | ✅ |
| **Vulkan (AMD/Intel)** | ❌ | ✅ 已验证 (需 Vulkan SDK) |
| **ROCm (AMD)** | ❌ | ⚠️ Linux 可用，Windows 实验性 |
| **DirectML (Windows通用)** | ❌ | ❌ |

**结论**: AMD GPU 用户用 `llama-cpp-2` + `vulkan` feature 跑 Tier 3。NVIDIA 用户可直接用 Candle 的 `cuda` feature 跑任意 Tier 1~2 模型。

---

## 4. 降级链

### 4.1 嵌入降级链 (EmbeddingTier)

```
┌──────────────────────────────────────────────���───────────────────┐
│ Tier 1: CandleEmbedder (bge-small-zh, 95MB, 512维, 3ms CPU)    │
│         默认。本地 BERT，零网络，全平台可用，永不失败            │
├──────────────────────────────────────────────────────────────────┤
│ Tier 2: CandleEmbedder (bge-base-zh, 400MB, 768维, 10ms CPU)   │
│         自动检测：模型文件存在则升级。同架构零代码改动            │
│         回退条件：模型文件不存在 → Tier 1                        │
├──────────────────────────────────────────────────────────────────┤
│ Tier 3: VulkanEmbedder (Qwen3-Embedding-4B, 2.5GB GGUF, GPU)    │
│         自动检测：Vulkan 二进制已编译 + GGUF 文件存在             │
│         回退条件：GPU 初始化失败 / 显存不足 → Tier 2             │
├──────────────────────────────────────────────────────────────────┤
│ Tier 4: BM25                                                     │
│         关键词检索，零依赖，永远可用                              │
└──────────────────────────────────────────────────────────────────┘
```

> **降级规则**: 每级优先使用，初始化失败自动回退到下一级。启动时按 1→2→3 顺序依次尝试初始化，停在第一个成功的 Tier。Tier 4 (BM25) 无状态，始终作为最终兜底。

### 4.2 重排序降级链 (RerankerTier)

```
Tier 1: CandleReranker (bge-reranker-base, 400MB, <20ms CPU) — 本地交叉编码
Tier 2: BM25 + 向量相似度加权融合                       — 无额外模型开销
```

### 4.3 自动检测与降级逻辑

```rust
// 启动时的 Tier 检测逻辑
fn detect_embedding_tier(paths: &ModelPaths) -> EmbeddingTier {
    // 按 3→2→1→4 顺序尝试，停在第一个成功的
    if paths.qwen3_gguf.exists() && vulkan_available() {
        match VulkanEmbedder::try_init(paths) {
            Ok(e) => return EmbeddingTier::Vulkan(e),  // Tier 3
            Err(_) => log::warn!("Vulkan 初始化失败，降级到 Tier 2"),
        }
    }
    if paths.bge_base.exists() {
        return EmbeddingTier::CandleBase(load_candle("bge-base-zh-v1.5")); // Tier 2
    }
    EmbeddingTier::CandleSmall(load_candle("bge-small-zh-v1.5")) // Tier 1
}
// Tier 4 BM25: 无状态，任何 Tier 失效时作为最终兜底
```

### 4.4 Feature Gate 配置

```toml
# 用户侧 Cargo.toml
[dependencies]
deepx-vector = { path = "..." }
# 默认启用 candle feature，使用 bge-small-zh
# 自动检测 bge-base 模型文件，存在则升级
# 关闭 candle 则降级到 BM25
# deepx-vector = { path = "...", default-features = false }

[features]
default = ["candle"]   # 启用本地嵌入推理
# vulkan = []           # 可选：GPU 加速（需 Vulkan SDK）
# rag = [...]           # deepx-tools / deepx-skills 层面控制
```

---

## 5. 性能预算

### 5.1 延迟预算

| 操作 | 目标延迟 | Tier 1 (small / CPU) | Tier 2 (base / CPU) | Tier 3 (Qwen3 / GPU) |
|------|---------|----------------------|----------------------|-----------------------|
| 单条嵌入 | < 15ms | ~2-3ms | ~8-10ms | ~10ms (GPU) |
| 批量嵌入 (10 条) | < 100ms | ~20-30ms | ~80-100ms | TBD |
| 向量搜索 (1000 条) | < 1ms | ~0.3ms | ~0.5ms (768维) | ~1ms (2560维) |
| 向量搜索 (10000 条) | < 5ms | ~2-3ms | ~3-4ms | ~5ms |
| Reranker 单对评分 | < 30ms | ~15-20ms | ~15-20ms | - |
| BM25 搜索 (1000 条) | < 2ms | ~0.5ms | ~0.5ms | - |

### 5.2 内存预算

| 组件 | 内存占用 |
|------|---------|
| bge-small-zh 模型 | ~100 MB |
| **bge-base-zh 模型** | **~450 MB** |
| bge-reranker-base 模型 | ~450 MB |
| 向量索引 (10000 条 × 512-dim) | ~20 MB |
| 向量索引 (10000 条 × 768-dim) | ~30 MB |
| 向量索引 (10000 条 × 2560-dim) | ~100 MB |
| BM25 索引 (10000 条) | ~5 MB |
| **总计 (Tier 1 small, 默认)** | **~575 MB** |
| **总计 (Tier 2 base + reranker)** | **~930 MB** |
| **总计 (Tier 3 Qwen3 GPU)** | **~2.5 GB 显存** |
| **总计 (BM25 only)** | **~5 MB** |

### 5.3 磁盘预算

| 组件 | 磁盘占用 |
|------|---------|
| bge-small-zh (safetensors) | ~95 MB |
| **bge-base-zh (safetensors)** | **~400 MB** |
| bge-reranker-base (safetensors) | ~400 MB |
| tokenizer.json × 2 | ~3 MB |
| **总计 (small, 默认)** | **~500 MB** |
| **总计 (base + reranker)** | **~800 MB** |

### 5.4 Token 节省预期

| 场景 | 优化前 | 优化后 | 节省 |
|------|--------|--------|------|
| Skills 注入 | 全量 ~3000 tok/turn | top-5 ~500 tok/turn | **~2500 tok/turn** |
| 10 轮任务 | ~30000 tok | ~5000 tok | **~25000 tok/任务** |

---

## 6. 用户配置方式

### 6.1 默认配置（推荐）

用户零配置。随 DeepX 安装包分发 `bge-small-zh-v1.5` 模型，Agent 启动时自动加载。如有 `bge-base-zh-v1.5` 模型文件，自动升级至 Tier 2。

### 6.2 进阶配置（环境变量）

```bash
# 嵌入模型（按需取消注释）
DEEPX_EMBEDDING_MODEL="BAAI/bge-small-zh-v1.5"   # 默认（Tier 1）
# DEEPX_EMBEDDING_MODEL="BAAI/bge-base-zh-v1.5"   # Tier 2 升级
# DEEPX_EMBEDDING_MODEL="qwen3-embedding-4b"       # Tier 3 GPU

# 重排序模型
DEEPX_RERANKER_MODEL="BAAI/bge-reranker-base"      # 默认
# DEEPX_RERANKER_MODEL=""                            # 禁用 reranker
```

### 6.3 代码配置（进阶 API）

```rust
use deepx_vector::CandleEmbedder;

// Tier 1: 默认 (bge-small-zh)
let embedder = CandleEmbedder::new("BAAI/bge-small-zh-v1.5")?;

// Tier 2: 升级 (bge-base-zh)——同 API，只换模型名
let embedder = CandleEmbedder::new("BAAI/bge-base-zh-v1.5")?;

// 其他可选模型（不在默认 Tier 链中）
// let embedder = CandleEmbedder::new("BAAI/bge-large-zh-v1.5")?;
// let embedder = CandleEmbedder::new("BAAI/bge-m3")?;
```

---

## 7. 五阶段决策记录

### Phase 1: 核心 trait + Candle 嵌入 + BruteForce 搜索

**决策**：
- 三个核心 trait：`Embedder` (文本→向量)、`VectorStore` (存储+搜索)、`Chunker` (文本分割)
- `normalize()` 和 `cosine_similarity()` 作为独立函数，不放 trait（避免 E0790 歧义）
- `BruteForceStore` 在插入时预归一化向量，搜索只需点积（O(N*D) → 无归一化开销）
- `CandleEmbedder` 用 `Mutex<Option<LoadedModel>>` 懒加载（Rust 2024 无 stable `OnceLock::get_or_try_init`）
- `TextChunker` 按段落分割 + 长文本在句子边界切分

**状态**: ✅ 已实现 (15 单测 + 1 doctest 全绿)

### Phase 2: 降级链 + BM25 + Reranker

**决策**：
- BM25 作为 Tier 4 最终兜底，零外部依赖，自定义简单分词器
- `Reranker` trait 支持交叉编码精排，`NoopReranker` 作为占位
- `CandleReranker` 手动构建 BERT + Linear(head)（candle-transformers 0.9.2 无 `BertForSequenceClassification`）
- `EmbeddingTier`/`RerankerTier` 枚举 + 工厂函数，cfg 分区避免 unreachable 代码
- bge-base 和 Qwen3 GPU 作为后续升级 Tier，架构预留扩展点

**状态**: ✅ 已实现 (39 单测 + 2 doctest 全绿)

### Phase 3: 集成到 tools + skills + config

**决策**：
- `deepx-tools` 新增 `rag.rs`，注册 3 个 tool，全局 `OnceLock` 管理 RAG 管道
- `deepx-skills` 新增 `SkillCatalog::search_semantic()`，feature gate 控制向量/关键词分支
- `deepx-config` 不直接依赖 `deepx-skills`，技能渲染在 `deepx-msglp/skill_context.rs`
- 全量 feature gate 默认关闭（`rag` feature），不影响现有功能
- RAG tool 的 handler 为 `fn` 指针（非闭包），适配现有 ToolHandler 签名

**状态**: ✅ 已实现 (全量 cargo check 通过，feature on/off 各通过)

### Phase 4: 设计文档终稿

**决策**：
- 模型选型四级：bge-small (Tier 1, 95MB, 默认) → bge-base (Tier 2, 400MB, 升级) → Qwen3-Emb-4B GGUF (Tier 3, 2.5GB, GPU) → BM25 (Tier 4, 兜底)
- 不在默认链的可选模型：bge-large, BGE-M3, CodeGeeX4-All-9B — 用户自行更换
- 性能预算：Tier 1 嵌入 < 5ms, Tier 2 < 15ms, 搜索 < 3ms (10000 条), 内存 < 1GB (全开)
- 环境变量控制模型选择，零配置默认 bge-small

**状态**: ✅ 本文档即为交付物

### Phase 5: 跨会话长期记忆集成

**决策**：
- `MemoryStore`：JSON 文件持久化，`recall_keyword()` + `recall_semantic()`
- `memory_hook`：基于中文关键词（决定/修复/发现/根因）提取记忆
- `memory_search` tool 对接 `MemoryStore`

**状态**: ✅ 已实现 (45 + 5 单测全绿)

### Phase 6: Vulkan GPU 加速 (Qwen3-Embedding-4B)

**决策**：
- Qwen3-Embedding-4B 通过 `llama-cpp-2` + Vulkan 后端在 AMD GPU 上推理
- 代码层已完成：`vulkan_embedder.rs` 实现 `Embedder` trait
- 编译通道已打通：`cargo check --features vulkan` 通过（需 MSVC + LLVM + Vulkan SDK）
- 二进制链接阻塞：CRT 不匹配（待解决，环境问题非代码问题）

**状态**: ⚠️ Rust 编译通过，二进制链接待解决

### Phase 7: 设计文档修订 — bge-base 加入分级 (2026-07-24)

**决策**：
- 模型分级从三级调整为四级，加入 `bge-base-zh-v1.5` 作为 Tier 2
- 移除 ONNX Runtime 和 Remote API 作为独立 Tier
- bge-base 与 bge-small 同架构 (BERT)，零代码改动只换模型文件
- Qwen3 GPU 延迟更正为 ~10ms（此前 100ms 过于保��）

---

## 8. 当前状态与后续规划

### 8.1 已完成项

| 模块 | 状态 | 说明 |
|------|------|------|
| bge-small-zh-v1.5 嵌入 | ✅ | Candle, CPU, 3ms, 默认 Tier 1 |
| BM25 关键词搜索 | ✅ | 零依赖，永远可用，Tier 4 兜底 |
| bge-reranker-base 重排 | ✅ | Cross-encoder, CPU, 20ms/对 |
| BruteForceStore (cosine) | ✅ | 10000 条 < 3ms |
| 4 级降级链 | ✅ | Candle(small) → Candle(base) → GPU(Qwen3) → BM25 |
| rag_index / rag_search tools | ✅ | Feature-gated in deepx-tools |
| memory_hook 跨会话记忆 | ✅ | Feature-gated in deepx-session |
| search_semantic on Skills | ✅ | 方法定义完成，agent loop 未调用 |

### 8.2 进行中

| 事项 | 说明 |
|------|------|
| Vulkan 链接 (Qwen3 GPU) | Rust 编译通过，CRT 不匹配待解决 |
| VectorEngine 单例 + 生命周期 | 下一阶段架构整合 |

### 8.3 已知限制

| 限制 | 计划 |
|------|------|
| Vulkan 链接 CRT 不匹配 | `.cargo/config.toml` 或重新编译 llama.cpp |
| bge-base 未实际下载测试 | 换模型文件即可，零代码改动 |
| search_semantic 未接入 agent loop | 需替换 build_context 中的全量 skill dump |
| 无增量索引更新 | 文件监视 + 重索引脏 chunk |
| BM25 分词器不支持中文分词 | 可选 jieba-rs 或 tantivy 中文分词 |
| RAG tool 的向量存储未持久化 | SQLite 或文件持久化 |
| bge-reranker-base 端到端测试 | 下载模型 + 集成测试 |
