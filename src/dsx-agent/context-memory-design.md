# DSX Agent: 上下文构建与分层记忆子系统设计

**Team C / C02** — 上下文构建 + 分层记忆
**依赖关系**: 本设计同时回应 A01 发现的循环引用问题

---

## 1. 系统总览

dsx-agent 的上下文+记忆子系统由三个核心组件构成：

```
记忆子系统 (memory/)                   上下文构建 (context.rs)
  ┌──────────────┐                    ┌────────────────────┐
  │ Semantic     │ ──render()──▶     │ ContextBuilder     │ 4层组装
  │ LongTerm     │ ──render()──▶     │   System           │ ← 不变
  │ ShortTerm    │ ──render()──▶     │   Semantic         │ ← 文件知识
  │ PitfallGuide │ ──render()──▶     │   Backbone(LTM)    │ ← 项目骨架
  └──────┬───────┘                    │   Episodic(对话)   │ ← 压缩后历史
         │                            └────────┬───────────┘
         │ 持久化接口                            │
         ▼                                      ▼
  ┌────────────────┐                  ┌────────────────────┐
  │ session/       │                  │ ContextAssembler   │
  │ memory_io.rs   │                  │ Turn+Step 模型     │
  │ (C03 共享边界)  │                  │ 类型级交替保证     │
  └────────────────┘                  └────────────────────┘
```

### 1.1 数据流

```
用户输入
  │
  ▼
ContextAssembler.push_user() ──▶ Turn 新增
  │
  ▼
API 调用 ──▶ assistant 响应 ──▶ ContextAssembler.push_assistant()
  │                                      │
  ▼                                      ▼
Tool 执行 ──▶ ContextAssembler.push_tool_result()
  │
  ▼  (KnowledgeExtractor 从工具结果中提取知识)
SemanticMemory.upsert() ──▶ 持久化至 semantic-mem.json
LongTermMemory.sync_from_semantic() ──▶ 持久化至 long-mem.md

下一次 API 调用:
ContextBuilder.build()
  ├─ System prompt (固定)
  ├─ SemanticMemory.render() (文件知识)
  ├─ LongTermMemory.render_backbone_compact() (项目骨架)
  └─ compact_context(对话历史) (压缩后轮次)
```

---

## 2. ContextBuilder — 4 层渐进式组装

### 2.1 层顺序

组装顺序是**稳定的前缀**，跨轮次不变：

```
Layer 0: System Prompt         ← 永远不变
Layer 1: Long-term Backbone    ← 项目架构 + 不变量 (几乎不变)
Layer 2: Semantic Block        ← 文件知识 (当读/写文件时变化)
Layer 3: Episodic History      ← 对话历史 (每轮变化, 通过 compact 控制大小)
```

**为什么要 backbone 在 semantic 之前?** 项目架构信息对模型理解整体结构比单个文件知识更基础。backbone 的 cache key 几乎不变，有助于 API 层 prompt caching。

### 2.2 插入逻辑 (context.rs:397-460)

```rust
// 伪代码 — 对应 context.rs ContextBuilder::build()
fn build():
    let mut messages = clone_and_compact(messages, ...)

    // Layer 1: backbone (系统消息 #1)
    if long_mem_block 非空:
        messages.insert(1, system(long_mem_block))

    // Layer 2: semantic (系统消息 #2 或 #1)
    if semantic_block 非空:
        messages.insert( backbone 后方 ), system(semantic_block)

    return messages
```

### 2.3 CompactContext — 4 级压缩

阈值参考 V4 模型退化观察：

| 压缩级别 | 触发阈值 | 操作 | 效果 |
|---------|---------|------|------|
| L1 | >40% | 截断长输出 (工具结果保留首尾各 50 行) | 零损失清理 |
| L2 | >50% | 低价值轮次 → 结构化卡片 (保留最近 N 轮全文) | ~40% 压缩 |
| L3 | >65% | 仅保留最近 3 轮全文，其余压缩为一张合并卡片 | ~60% 压缩 |
| L4 | >80% | 仅保留最近 1 轮全文 + 一张汇总卡片 | ~75% 压缩 |

关键设计决策:
- **L1 永远执行**: 截断过长的 tool output 和 file read 结果，但不 strip reasoning (Anthropic API 要求保留 thinking blocks)
- **分区压缩**: 不会被压缩的轮次 = 系统消息 + 最近 N 轮全文 + 分步卡片
- **已知文件优化**: `known_files` (来自 SemanticMemory) 使已记录文件仅输出路径，不重复描述
- **Danger 模式 (L3/L4)**: 跳过单次错误、跳过 "(new)" 标注、仅保留出现 ≥2 次的错误信息

#### 2.3.1 轮次分割 (split_rounds)

以 "user 消息" 作为轮次边界。系统消息前缀被剥离后独立处理:

```
[system A] [system B] [user: "hello"] [assistant: "hi"] [user: "fix this"] [assistant(tool_use)] [tool(result)] [assistant: "done"]
                                                                                   ↓
Round 0: [user: "hello"] [assistant: "hi"]
Round 1: [user: "fix this"] [assistant(tool_use)] [tool(result)] [assistant: "done"]
```

#### 2.3.2 轮次评分 (scores)

`turn_scorer.rs` 提供每轮重要性评分（0.0-1.0），用于 L2-L4 决定哪些轮次先被卡片化。

评分依据:
- exec + explore → 0.9 (最高价值，发现项目结构)
- file write/edit → 0.7 (修改了文件)
- exec error → 0.8 (错误信息需要保留)
- read_file → 0.4 (中等价值)
- 纯文本对话 → 0.2 (基础分)

#### 2.3.3 卡片格式

正常模式下的单轮卡片:
```
F: src/main.rs (fn main, struct Config) (new)
F: src/lib.rs (fn parse_args)
E: error[E0308]: mismatched types — src/main.rs:42
```

Danger 模式下的单轮卡片（仅文件路径，无错误）:
```
F: src/main.rs (fn main, struct Config)
F: src/lib.rs (fn parse_args)
```

合并卡片（多轮合并）:
```
F: src/main.rs (fn main, struct Config) (new)
F: src/lib.rs (fn parse_args)
E: error[E0308]: mismatched types — src/main.rs:42
E: error[E0425]: cannot find value — src/lib.rs:15
```

---

## 3. ContextAssembler — Turn+Step 对话模型

### 3.1 为什么需要 Turn+Step 而非 Vec<Message>

原始 `Vec<Message>` 在 Rust 类型层面无法防止:

- 连续两个 user 消息（违反 Anthropic API 要求）
- 无对应 tool_use 的 tool_result
- 无 tool_use 声明的 tool_result 序列

Turn+Step 模型在**类型层面**使这些状态不可表达:

```rust
Conversation
  ├── SystemMessages: Vec<Message>       // role=system, 任意顺序
  ├── Turns: Vec<Turn>
  │    ├── Turn 0:
  │    │    ├── user: Message            // role=user
  │    │    └── steps: Vec<Step>
  │    │         ├── Step 0:
  │    │         │    ├── assistant: Message           // role=assistant
  │    │         │    └── tool_results: Vec<Message>   // role=tool, 验证 tool_call_id
  │    │         └── Step 1:
  │    │              ├── assistant: Message
  │    │              └── tool_results: Vec<Message>
  │    ├── Turn 1:
  │    │    └── ...
```

### 3.2 类型级不变量

| 不变量 | 强制方式 | 违反时返回 |
|-------|---------|-----------|
| user 后必须有 assistant | `push_user` 创建新 Turn; `push_assistant` 检查 Turn 存在 | `NoUserPending` |
| 不允许连续 user | `push_user` 检查上轮 steps 非空 | `TurnIncomplete` |
| tool_result 必须在同一 Turn | `push_tool_result` 检查 step.assistant.tool_calls 包含该 id | `OrphanToolResult` |
| 同一 step 的 tool_use 全部满足后才能开始新的 step | `push_assistant` 检查上一步 all_tools_satisfied | `TurnIncomplete` |
| 同一 turn 的 tool_use 全部满足后才能开始新的 user turn | `push_user` 检查 all_steps_satisfied | `TurnIncomplete` |

### 3.3 多进程安全性

Turn+Step 模型完全运行在 agent 进程内，不跨进程共享。C01 (orchestrator) 通过 `ContextAssembler` 的借用来安全访问:

```rust
// C01 (orchestrator) 中:
let mut ctx = ContextAssembler::new();
ctx.push_user(user_input)?;           // 线程安全 — 单线程 agent
let msg = ctx.to_anthropic_messages();// 唯一输出路径, 保证格式正确

// 异步工具结果:
ctx.push_tool_result_for(id, result)?; // 跨 turn 搜索匹配的 tool_use
```

`to_anthropic_messages()` 是唯一的 Anthropic API 格式输出路径。内部将 Turn+Step 展平为:

```
user("fix this")
  → assistant(tool_use: read_file)
  → user(tool_result: read_file)
  → assistant(tool_use: write_file)
  → user(tool_result: write_file)
user("now add tests")
  → assistant("done")
```

### 3.4 会话恢复

`from_legacy(msgs: Vec<Message>)` 从旧格式的平铺消息重建 Turn+Step。对孤立的 tool_use 注入 `[ERROR]` 伪 tool_result:

```rust
// 重建逻辑 (conversation.rs:512-573):
// 1. 提取 system 消息
// 2. 遍历: user → push_user, assistant → push_assistant_unchecked, tool → push_tool_result_for
// 3. 对孤立 tool_use 注入 [ERROR] → 阻止模型重试
```

---

## 4. 记忆分层设计

### 4.1 四层记忆概览

| 层级 | 数据结构 | 持久化格式 | 生命周期 | 典型大小 | 注入位置 |
|------|---------|-----------|---------|---------|---------|
| Semantic | `BTreeMap<Path, FileSemanticEntry>` | JSON (semantic-mem.json) | 跨会话 | ~2K tokens | Layer 2 |
| Long-Term | `LongTermMemory` | Markdown (long-mem.md) | 跨会话 | ~500 tokens | Layer 1 |
| Short-Term | `ShortTermMemory` | Markdown (short-mem.md) | 单会话 | ~300 tokens | Layer 3 (episodic) |
| Pitfall | `PitfallGuide` | JSON (pitfalls.json) | 跨会话, 全局 | ~500 tokens | Layer 3 (episodic) |

### 4.2 SemanticMemory (semantic.rs)

**用途**: 文件级别的知识积累，跨会话保留。

```rust
pub struct SemanticMemory {
    entries: BTreeMap<String, FileSemanticEntry>,  // 路径 → 条目
    arch_decisions: Vec<ArchDecision>,             // 架构决策 (最多 10)
    error_patterns: Vec<ErrorPattern>,             // 错误模式 (最多 8)
    token_budget: u32,                             // render() 输出预算
    total_updates: u64,                            // 更新计数
}

pub struct FileSemanticEntry {
    path: String,
    symbols: Vec<String>,        // 函数/类型名 (最多 12)
    purpose: Option<String>,     // 文件目的 (来自文档注释)
    last_seen: u64,              // 最后访问时间戳
    touch_count: u32,            // 操作计数
    language: Option<String>,    // 检测到的语言
}
```

**关键方法**:
- `upsert(path, entry)`: 合并符号列表，更新访问时间
- `enforce_budget(max_tokens)`: 按 (频率×0.4 + 新鲜度×0.6) 评分，淘汰低分条目
- `render()`: BTreeMap 保证稳定顺序 → byte-identical 同文件集
- `add_error_pattern(pattern, fix, file)`: 去重合并，保留最高频 8 条

**知识提取** (KnowledgeExtractor):
- `read_file` → 提取符号 + 目的 (文档注释)
- `write_file` / `edit_file` → 提取符号 + 目的
- `edit_file_diff` → 提取变更类型 + 行数
- `explore` / `project_map` → 提取文件列表 + 语言检测

### 4.3 LongTermMemory (long_term.rs)

**用途**: 项目架构知识，跨会话保留。系统提取，AI 只读。

```rust
pub struct LongTermMemory {
    architecture: Vec<(String, String)>,   // 文件路径 → 职责描述 (最多 30)
    decisions: Vec<(String, String)>,      // 决策摘要 → 理由 (最多 50)
    file_changes: Vec<(String, u32, u32)>, // 路径 → (+行, -行) (最多 50)
    critical_invariants: Vec<String>,      // 关键不变量 (最多 20)
}
```

**渲染模式**:
- `render()`: 完整格式 (模块图 + 不变量 + 决策 + 文件变更)
- `render_backbone_compact()`: 精简格式 (~300-500 tokens, 仅模块图 + 不变量)

**同步**: `sync_from_semantic(sem)` 从 SemanticMemory 同步文件职责到 architecture 列表。

### 4.4 ShortTermMemory (short_term.rs)

**用途**: 单会话内的轮次索引，AI 写 + 读。

```rust
pub struct ShortTermMemory {
    seed: String,
    model: String,
    started_at: u64,
    rounds: Vec<RoundEntry>,         // 轮次摘要 (最多 50)
    active_issues: Vec<String>,      // AI 记录的问题 (最多 10)
    pending_tasks: Vec<String>,      // 待办任务 (最多 10)
}

pub struct RoundEntry {
    num: usize,
    tools: Vec<String>,    // 本轮调用的工具列表
    files: Vec<String>,    // 涉及的文件列表
    summary: String,       // AI 生成的摘要
}
```

### 4.5 PitfallGuide (pitfall.rs)

**用途**: 跨会话自我学习 — AI 从错误中学习，避免重蹈覆辙。

```rust
pub struct PitfallGuide {
    entries: Vec<PitfallEntry>,   // 最多 50 条
    total_updates: u64,
}

pub struct PitfallEntry {
    emotion: String,         // 记录时的 AgentEmotion 标签
    error_kind: String,      // ErrorKind 变体名
    tool: String,            // 涉及的工具
    description: String,     // AI 自述: 发生了什么
    lesson: String,          // AI 自述: 如何避免
    files: Vec<String>,
    frequency: u32,
    first_seen: u64,
    last_seen: u64,
}
```

**匹配机制**: AI 通过 `pitfall_save` tool 记录教训。健康平台在错误率高时调用 `find_matches()` 匹配当前错误，注入上下文。

---

## 5. 与 Session (C03) 的接口定义

### 5.1 接口边界

记忆持久化位于 `dsx-agent::session::memory_io`，与 C03 共享模块。C02 定义数据类型，C03 提供持久化路径和 I/O。

```
C02 定义的数据类型              C03 提供的 I/O
─────────────────              ────────────────
SemanticMemory          ──▶    write_semantic_memory()
                               read_semantic_entry()

LongTermMemory          ──▶    write_memory(seed, "long", ...)
                               read_memory(seed, "long")

ShortTermMemory         ──▶    write_memory(seed, "short", ...)
                               read_memory(seed, "short")
                               write_memory_preserving_notes()

PitfallGuide            ──▶    save_pitfalls()
                               load_pitfalls()
```

### 5.2 文件布局

每个会话目录包含:

```
~/.config/dsx/sessions/{seed}-{date}/
  ├── session.json                 # 会话持久化 (C03 负责)
  ├── session.live.json            # 活跃会话快照 (C03 负责)
  ├── semantic-mem.json            # SemanticMemory JSON (C02 读写)
  ├── long-mem.md                  # LongTermMemory Markdown (C02 读写)
  ├── short-mem.md                 # ShortTermMemory Markdown (C02 读写)
  └── (其他记忆文件)

全局:
  ~/.config/dsx/sessions/pitfalls.json   # PitfallGuide (全局, 跨会话)
  ~/.config/dsx/sessions/index.json      # 会话索引 (C03 负责)
```

### 5.3 解决 A01 循环引用问题

**当前问题**: `dsx-types::SessionFile` 第 406 行引用了 `crate::memory::SemanticMemory`，但 `dsx-types` 中没有 `memory` 模块，且不应依赖 `dsx-agent::memory::semantic`。

**解决方案**: 从 `SessionFile` 中移除 `semantic_memory` 字段。SemanticMemory 作为**独立文件** (`semantic-mem.json`) 持久化，与 `session.json` 分离。加载会话时分别加载:

```rust
// persist.rs — 不再嵌入 semantic_memory 到 session.json
fn save_session(seed, messages, model, effort) {
    let file = SessionFile {
        // ... 无 semantic_memory 字段
    };
    // semantic_memory 单独由 write_semantic_memory() 保存
}
```

**变更影响**:
- `dsx-types::SessionFile`: 移除 `semantic_memory: Option<crate::memory::SemanticMemory>`
- `persist.rs::save_session` / `finalize_session`: 不再接收 `semantic_memory` 参数
- `restore.rs`: 加载会话后通过 `read_semantic_entry()` 按需读取
- `TokenBreakdown` (tokenizer.rs): 需定义在 dsx-types 或 dsx-agent 内，不跨 crate 引用

---

## 6. Context + Token 核算

### 6.1 TokenBreakdown

每个 API 调用前，ContextBuilder 输出 token 核算:

```rust
pub struct TokenBreakdown {
    pub system: u32,     // Layer 0: 系统提示词
    pub semantic: u32,   // Layer 1: SemanticMemory
    pub long_mem: u32,   // Layer 2: LongTermMemory backbone
    pub episodic: u32,   // Layer 3: 对话历史 (压缩后)
    pub total: u32,      // 总和
}
```

### 6.2 压缩触发链

```
ContextBuilder.build()
  │
  ├─ 计算 total (压缩前)
  │
  ├─ ratio = total / context_limit
  │
  ├─ if ratio > 0.40 → L1 (截断长输出)
  ├─ if ratio > 0.50 → L2 (卡片化低分轮次)
  ├─ if ratio > 0.65 → L3 (保留最近 3 轮)
  └─ if ratio > 0.80 → L4 (保留最近 1 轮)
```

### 6.3 强制压缩

C01 (orchestrator) 可在紧凑阈值时调用 `force_compact(messages, level, scores)` 主动压缩，绕过渐进式检查。

---

## 7. 与 C01 (编排引擎) 的集成点

### 7.1 Agent Loop 中的上下文流

```
agent_loop.rs (C01):
  1. 接收用户输入
  2. ContextAssembler.push_user(text)
  3. self.semantic.upsert(...)            // 更新文件知识
  4. ContextBuilder.build():
     a. self.long_term.render_backbone_compact()
     b. self.semantic.render()
     c. compact_context(messages, scores, known_files)
  5. 发送 API 请求
  6. 接收 assistant 响应
  7. ContextAssembler.push_assistant(msg)
  8. 执行 tool
  9. ContextAssembler.push_tool_result(id, result)
  10. KnowledgeExtractor.extract(...)      // 从工具结果提取知识
  11. self.semantic.upsert(...)
  12. goto 4 (下一个循环)
```

### 7.2 关键集成 API

```rust
// C01 使用时无需了解 Compaction 内部细节:
impl ContextBuilder {
    pub fn build(&self) -> (Vec<Message>, TokenBreakdown);
}

impl ContextAssembler {
    pub fn push_user(&mut self, text: &str) -> Result<(), AssemblerError>;
    pub fn push_assistant(&mut self, msg: Message) -> Result<(), AssemblerError>;
    pub fn push_tool_result(&mut self, id: &str, result: &str) -> Result<(), AssemblerError>;
    pub fn to_anthropic_messages(&self) -> (Option<String>, Vec<AnthropicMessage>);
    pub fn from_legacy(msgs: Vec<Message>) -> (Self, Vec<String>);
    pub fn has_pending_tools(&self) -> bool;
    pub fn pending_tool_call_ids(&self) -> Vec<String>;
    pub fn is_dirty(&self) -> bool;
}

// 记忆 + 知识提取:
impl KnowledgeExtractor {
    pub fn extract(tool_name, args, output) -> Vec<FileSemanticEntry>;
    pub fn extract_project_map(raw) -> Vec<FileSemanticEntry>;
}
```

---

## 8. 改动清单

### 8.1 需要修改的文件

| 文件 | 改动 |
|------|------|
| `dsx-types/mod.rs` | 移除 `SessionFile.semantic_memory` 字段 |
| `dsx-agent/session/persist.rs` | 移除 `semantic_memory` 参数; 保存时不再嵌入 |
| `dsx-agent/context.rs` | ContextBuilder 已就绪, 确认 `TokenBreakdown` 导入路径 |
| `dsx-agent/memory/mod.rs` | 保持当前 re-export |
| `dsx-agent/memory/semantic.rs` | 确认 `tokenizer` 引用路径 (当前使用 `crate::tokenizer`) |
| `dsx-agent/memory/pitfall.rs` | 确认 `health` 引用路径 (当前使用 `crate::health`) |

### 8.2 无需修改的文件

| 文件 | 原因 |
|------|------|
| `memory/long_term.rs` | 独立, 无循环依赖 |
| `memory/short_term.rs` | 独立, 无循环依赖 |
| `session/memory_io.rs` | 接口定义清晰, 仅导入 crate::memory 类型 |
| `conversation.rs` | 独立, 仅依赖 crate::types |
| `turn_scorer.rs` | 独立评分逻辑 |

### 8.3 循环引用修复 (A01)

```
Before:
  dsx-types::SessionFile ──→ (无) crate::memory::SemanticMemory
  dsx-agent::session::persist ──→ crate::memory::SemanticMemory

After:
  dsx-types::SessionFile ──→ (无 semantic_memory 字段)
  dsx-agent::session::memory_io ──→ crate::memory::SemanticMemory (同 crate, OK)
  SemanticMemory 持久化为独立文件 semantic-mem.json, 不经过 SessionFile
```
