# PLAN-EDIT-FILE-V2：edit_file_v2 差分设计（修订版）

- 状态：设计草案 v2（差分版，替换初版"融合超集"方案）
- 日期：2026-08-14
- 上游输入：[edit-tool-design-spec.md](../../edit-tool-design-spec.md)
- 现状基线：`crates/deepx-workspace/src/file_edit.rs`（v1，已上线）
- 修订记录：初版把 v1 能力全集搬进 v2 再加增量（超集方案），评审否决；本版改为
  **差分方案**——v2 只实现 v1 没有的部分，v1 保持原样不动。

---

## 1. 方案决策：差分 vs 完全独立

评审给出两个候选方向，评估如下：

**方案 A：差分设计（v2 只补 v1 没有的部分）**

- 优点：实现最小、落地最快；v1/v2 分工清晰，各自描述可互相引用；
  不重复维护已稳定逻辑；A/B 对照聚焦在"匹配哲学"（v1 严格 vs v2 智能）上。
- 代价：v2 自身不完整（无行号、无 replace_all/regex），模型需在两工具间选择——
  靠两份 tool description 明确分工解决。
- 关键边界：**有四件事物理上无法分叉**——CRLF 契约（read 的展示/hash 基于 LF 视图）、
  hash 协议（expected_hash 必须与 read_file 同算法同视图）、原子写（同目录临时文件 +
  MoveFileExW）、审批路径（extract_target_paths）。复用这四样不算"重复实现 v1 的能力"，
  它们是仓库级基础设施；不复用则 v2 与 read_file 链路直接断裂。

**方案 B：完全独立（不复用或只参考部分设计）**

- 优点：最干净，无历史包袱；严格对照实验。
- 缺点："完全独立"是伪命题——上述四件事必须复制且行为逐字节一致，复制出来的只是
  双倍维护成本；匹配引擎才是真正可独立对照的部分。
- 结论：不推荐。若目的是做匹配哲学对照实验，方案 A 已达成（匹配引擎独立自写）。

**决策：方案 A，且差分收紧到如下粒度：**

- **v2 新写（v1 没有的）**：spec 的 hunk 枚举协议、四层匹配流水线（含 Tier3 相似度
  评分 + margin 自动采纳）、单快照两阶段全事务应用、OverlappingHunks 重叠检测、
  ropey 承载、HashMismatch 返回 current_content、Top3 候选列表。
- **v2 复用（基础设施白名单，仅 4 项）**：`file_shared::content_hash`（sha256 协议）、
  `normalize_newlines`（CRLF 契约）、`atomic_write`（原子写）、`unified_diff`
  （diff 预览）。全部是纯函数，无状态。
- **v2 明确不实现（留在 v1）**：`start_line`/`end_line` 行号定位（评审明确要求去掉）、
  `replace_all`、`regex`、工具侧账本（`mode: partial` 后补，见 10c）
  （file_state）、v1 诊断体系（closest_line / DID YOU MEAN / 转义 HINT）。

这样 v1 + v2 合起来才是完整能力面：

| 能力 | v1 (edit_file) | v2 (edit_file_v2) |
|---|---|---|
| 精确替换（old/new + context 消歧） | ✅ | ✅（Tier1） |
| 行号定位 / replace_all / regex | ✅ | ❌（评审要求去掉） |
| 每 op 独立事务、多文件 files | ✅ | ❌（v2 只做全事务） |
| 插入/前置/追加 hunk 类型 | ❌（靠空 old+context 绕） | ✅ InsertAfter/InsertBefore/PrependFile/AppendFile |
| 缩进形状匹配 | 部分（L2 偏移补偿） | ✅（spec Tier2 形状匹配） |
| 相似度评分自动采纳 + margin | ❌（严格拒绝） | ✅（Tier3） |
| 多 hunk 单快照全事务 + 重叠检测 | ❌（顺序应用） | ✅ |
| HashMismatch 返回 current_content | ❌（只提示重读） | ✅ |
| 结构化候选列表（score/tier/snippet） | ❌ | ✅ |
| rope 承载 / 大文件性能 | ❌（String 拼接） | ✅ |

---

## 2. 协议设计

请求（spec 原样形态，无 v1 sugar、无行号、无 replace_all/regex）：

```jsonc
{
  "path": "src/foo.rs",
  "expected_hash": "sha256:9f86d0…",   // 可选；有→校验，无→直接编辑（见 10d）
  "hunks": [
    {
      "kind": "replace",
      "old": "…",                        // 空字符串 = 纯插入（此时 context 至少一侧非空）
      "new": "…",
      "context_before": "…",             // 字符串字段，JSON 层自动转义换行
      "context_after": "…"
    },
    { "kind": "insert_after",  "anchor": "…", "new": "…" },
    { "kind": "insert_before", "anchor": "…", "new": "…" },
    { "kind": "prepend_file", "new": "…" },
    { "kind": "append_file",  "new": "…" }
  ]
}
```

字段命名与 v1 刻意区分（`old`/`new`/`anchor` 而非 `old_string`/`old_lines`），
让模型明白这是**不同协议的独立工具**，避免按 v1 习惯传行号字段（会被
`additionalProperties: false` 拒绝并提示）。

响应：

```jsonc
{
  "status": "ok",                        // ok | partial | error（mode 控制，见 10c）
  "new_hash": "sha256:…",                // 编辑后 LF 视图 hash，可直接续接 expected_hash
  "diff_preview": "…",                   // unified diff（人类/UI 展示）
  "applied_hunks": 3,
  "notes": ["hunk[1]: Tier2 缩进形状命中", "检测到 CRLF 已归一化为 LF"],
  "hunks": [                             // 每个 hunk 的匹配元数据
    { "index": 0, "status": "ok", "tier": 1, "line_range": [42, 45] }
  ],
  "error": null                          // 失败时：见第 5 节错误模型
}
```

---

## 3. 匹配流水线（spec 四层原样）

每个需要定位的 hunk（`replace` 的 `old`、`insert_*` 的 `anchor`）独立走流水线，
**所有 hunk 在同一份未修改快照上定位**，命中即停：

**Tier 1 — 精确匹配**：全文查找 `old` 的所有精确出现。唯一 → 直接采用（score=1.0）。
多处 → 用 `context_before`/`context_after` 核对紧邻文本筛出唯一；核对后仍不唯一 →
`AmbiguousMatch`（不降级猜，候选照实返回）。

**Tier 2 — 缩进形状匹配**：把 `old` 按行拆分，剥离公共最小缩进得到"形状"；用相同
行数的滑动窗口扫描文件，同样剥离后逐行比较。容忍模型记错整段基准缩进（如忘了这段
在 impl 块里多缩两格），保留段内相对缩进。命中且唯一 → 采用；否则降级。
（注：与 v1 L2 的"偏移补偿"实现有语义差别——spec 是双向形状剥离，v2 按 spec 实现，
不复制 v1 代码。）

**Tier 3 — 相似度评分匹配**：行数 ±1~2 容差的滑动窗口，对每个候选：

```
score = 0.6 * ratio(old, window)
      + 0.2 * ratio(context_before, 窗口前文)
      + 0.2 * ratio(context_after, 窗口后文)
```

`ratio()` = `similar::TextDiff::from_chars(a, b).ratio()`（0.0~1.0，difflib 字符级
SequenceMatcher 语义，为何不用 from_lines 见 10b 第 1 条）。取最高分候选；**仅当
最高分 ≥ 0.90 且与次高分差距 ≥ 0.15** 才自动采纳，否则视为不确定 → 降级 Tier 4。
阈值与 margin 是保守起步值（spec 建议 0.85/0.10），后续做成配置项
（`edit_v2.t3_threshold` / `edit_v2.t3_margin`），灰度期用真实误改案例回调。

**Tier 4 — 拒绝 + 候选**：返回 Top3 候选（`line_range` 仅展示、`snippet`、`score`、
`tier`），让模型看着候选重给更精确的 `old`/`context`，或改用 rg 重新定位。绝不
"选一个凑合用"。

**空 `old` / 纯插入**：`context_before`、`context_after` 各走 Tier1~3 定位，取交界处
插入；只给一侧以该侧为锚；两侧全空 → `Underspecified`，提示改用 `insert_after` /
`prepend_file` / `append_file`。

---

## 4. 应用与写入（两阶段，全事务）

1. **阶段 1（定位）**：同一份未修改 rope 快照上解析全部 hunk 的 char 区间。
   即使某 hunk 失败也先把全部 hunk 定位完，一次性汇总错误。
2. **重叠检测**：全部解析区间两两比较，有重叠 → `OverlappingHunks` 整体拒绝。
3. **事务性**：任一 hunk 失败（NoMatch/Ambiguous/Underspecified/重叠）→ **整体拒绝，
   文件零改动**（strict）；`mode: "partial"` 时成功 hunk 照常应用、失败 hunk 报详情
   （见 10c——单快照定位 + 部分应用，无 v1 的定位漂移问题）。
4. **阶段 2（应用）**：按区间起始位置从大到小对 rope 执行 `remove(range)` +
   `insert(start, new)`，从文件末尾往前改，无需偏移量表。
5. **换行归一化**：`old`/`new`/`context`/`anchor` 中混入 `\r\n`/`\r` → 统一归一化为
   `\n`（沿用 LF 规范视图契约），`notes` 注明；读入时整体归一化（复用
   `normalize_newlines`），写回按 `was_crlf` 还原原始换行（最小 diff）。
6. **原子写**：复用 `file_shared::atomic_write`（同目录临时文件 +
   Windows `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`）。
7. **hash 与 diff**：最终内容算 sha256 作 `new_hash`（协议兼容 read_file，见第 6 节）；
   用 `similar` 生成 unified diff 作 `diff_preview`（复用 `unified_diff`）。

---

## 5. 错误模型

失败时整体 `status: "error"` + 结构化 `error` 字段（spec 枚举）：

| 错误 | 触发 | 附带信息 |
|---|---|---|
| `FileNotFound` | 路径不存在 | — |
| ~~`FileAlreadyExists`~~ | ~~expected_hash=null 但文件已存在~~ | 已移除（见 10d） |
| `NotUtf8Text` | 非 UTF-8（视为二进制） | — |
| `HashMismatch` | expected_hash 与当前 LF 视图 hash 不符 | `current_hash` + `current_content`（**截断 64 KB** + 尾部行号提示，防撑爆响应） |
| `NoMatch` | 四层全miss | `hunk_index` + `candidates`（Top3） |
| `AmbiguousMatch` | Tier1 多处且 context 消歧失败 | `hunk_index` + `candidates` |
| `OverlappingHunks` | 区间重叠 | `hunk_a` + `hunk_b` |
| `Underspecified` | old 空且 context 全空 | `hunk_index` |

同时保留 v1 风格的逐 hunk 结果数组（`hunks[]`），成功与失败的 hunk 各带
`status`/`tier`/`line_range` 或 `error` 详情——全事务拒绝的是**写入**，不是**报告**：
模型一次拿到所有失败详情，一次性修正重发。

刻意**不引入** v1 的 closest_line / DID YOU MEAN / 转义 HINT——那是 v1 的资产，
v2 的自我纠错路径是候选列表（candidates）本身。需要诊断细节时模型用 v1 或 rg。

---

## 6. hash 与并发（对 spec 的一处有意分歧）

**协议 hash 用 sha256，不用 blake3。** `expected_hash` 是与 read_file 共享的协议字段，
换算法要同步改 read/write/账本/前端，迁移成本远超收益；sha256 对源码文件量级不构成
瓶颈（spec 自己也把 blake3 列为优化项而非正确性项）。v2 不接 file_state 账本——
无行号定位后账本失去存在的必要（那是 v1 为行号盲定位做的防漂移），v2 的并发保护
`expected_hash` 为**可选乐观锁**（见 10d：模型实践中拿不到 read 的 hash，
门禁降级为可选校验）：

- 写入前重读文件、算 LF 视图 hash，与 `expected_hash` 不一致 → 拒绝并返回
  `current_hash` + `current_content`（模型不需要再单独调一次 read）。
- 并发 subagent 场景：后提交方校验必败，被迫重读而非静默覆盖；harness 重试同一请求
  因 hash 已变而失败，不会重复应用。
- 编辑成功后返回 `new_hash`，下次编辑直接续接，不必重读全文（spec 决策 6 显式化）。

---

## 7. 边界情况

- 空文件插入 / 全新文件创建（`expected_hash: null` + `prepend_file`）
- 路径存在但是目录 / 不存在
- 非 UTF-8 → `NotUtf8Text`，不做任何"尝试解读"
- `old` 空且 `context_before`/`context_after` 全空 → `Underspecified`
- 多 hunk 区间重叠 → `OverlappingHunks`，整体拒绝零改动
- 文件原本无结尾换行 / hunk 改到最后一行 → 沿用原文件换行约定，不引入新约定
- 请求内容混入 CRLF → 静默归一化 + `notes` 注明
- 同段代码重复出现多次 → context 消歧；消歧不了 → `AmbiguousMatch`（不猜）
- 大文件（数千行+）→ Tier3 滑动窗口线性增长，必要时用 `strsim` 粗筛候选窗口
  （`strsim` 已在依赖树中，v1 的 `best_similar_window` 验证过 jaro_winkler 路径）

---

## 8. 模块划分

新增单文件（差分版体量小，不需要目录）：

```
crates/deepx-workspace/src/file_edit_v2.rs
  // EditHunk 解析 + 校验（Underspecified/FileAlreadyExists 等）
  // match_tiers: Tier1-4 流水线（精确 → 缩进形状 → 相似度评分+margin → 候选）
  // apply: 快照定位 → 重叠检测 → 倒序 apply（ropey）→ 原子写 → hash/diff
  // register: 工具键 edit_file_v2，ToolRisk::Write，审批复用 extract 同逻辑
```

复用（仅 4 个纯函数，来自 `file_shared.rs`）：

```rust
use crate::file_shared::{atomic_write, content_hash, normalize_newlines, unified_diff};
```

新增依赖：`ropey`（工作区当前未引入，需加进 deepx-workspace 的 Cargo.toml）；
`similar`/`strsim`/`tempfile` 已在依赖树中。

注册信息（独立工具，与 v1 并存，v1 一行不改）：

- 键：`edit_file_v2`
- `PLAN_BLOCKED` 列表加入 `edit_file_v2`
- description 明确分工话术（见下），`additionalProperties: false`

description 分工话术（草案）：

> Structured hunk editor (v2, complements edit_file): kind-tagged hunks
> (replace / insert_after / insert_before / prepend_file / append_file), four-tier
> matching (exact → indent-shape → similarity scoring with margin → top-3 candidates),
> all-or-nothing transaction on a single snapshot, no line numbers. Use for inserts,
> multi-point edits, or when your recollection of the old text is approximate
> (Tier3 auto-applies if score ≥ threshold and margin is clear). For line-number
> edits, replace_all, regex, or per-op independent transactions use edit_file (v1).

---

## 9. 测试清单

spec 第 9 节 14 用例全量覆盖（编号对齐），外加 v2 独有回归：

| # | 用例 | 覆盖点 |
|---|---|---|
| 1 | 精确匹配唯一命中 | Tier1 |
| 2 | 缩进基准不同 → Tier2 形状命中 | Tier2 |
| 3 | 细微出入（变量名错一字）→ Tier3 达标采纳 | Tier3 + margin |
| 4 | Tier3 双候选 margin 不足 → 拒绝 | Tier3 拒绝路径 |
| 5 | 同模式 ×3 + context_before 消歧 | Tier1 消歧 |
| 6 | 两 hunk 不重叠各自命中 | 多 hunk 应用 |
| 7 | 两 hunk 重叠 → 整体拒绝零改动 | OverlappingHunks |
| 8 | 空文件 PrependFile / AppendFile | 文件级 hunk |
| 9 | 无结尾换行 AppendFile 沿用原约定 | 边界 |
| 10 | CRLF 混入 → 归一化 + notes | 归一化 |
| 11 | expected_hash 失配 → 拒绝 + current_content | hash gate |
| 12 | 无 hash 编辑已有文件 → 直接编辑成功 | hash 门禁（10d） |
| 13 | 非 UTF-8 → NotUtf8Text | 二进制 |
| 14 | old 空 + context 全空 → Underspecified | 校验 |
| 15 | InsertAfter/InsertBefore 锚点命中与多锚点歧义 | 插入 hunk |
| 16 | CRLF 文件编辑后写回保持 CRLF | CRLF 契约 |
| 17 | new_hash 续接：编辑返回的 hash 直接当下次 expected_hash | hash 链 |
| 18 | 多 hunk 失败时错误报告含全部失败详情（非只报第一个） | 报告完整性 |
| 19 | 行号字段传入 → additionalProperties 拒绝 + 指向 v1 的提示 | 协议边界 |
| 20 | 倒序应用：三 hunk 从文件头到尾各改一处，结果正确 | 偏移无关性 |

---

## 10. 与 edit-tool-design-spec.md 的差异点（按规格 10.4 要求）

1. **hash 算法**：规格建议 blake3，v2 协议层用 sha256——`expected_hash` 与 read_file
   共享协议是硬约束；blake3 的收益（毫秒级）不值得全链路迁移。
2. **匹配层级**：规格四层，v2 原样四层（v1 的"首空行丢弃/trim 容错"等额外层**不搬入
   v2**，v2 的容错统一由 Tier3 兜底，保持层级语义纯净）。
3. **事务**：默认 strict 全事务（规格语义）；`mode: "partial"` 为后补选项（10c）。
4. **行号**：规格排除行号，v2 遵守（评审明确要求）。
5. **确认机制**（规格第 7 节）：暂不实现，列为后续可选（协议上无字段预留——
   差分版不背 Phase 2 包袱；需要时再加 `auto_apply` 字段）。
6. **依赖**：`similar`/`strsim`/`tempfile` 用 workspace 现有版本；`ropey` 新增，
   版本以 Cargo.lock 解析为准。
7. **诊断**：规格只有候选列表，v2 只有候选列表（v1 诊断体系不搬入，分工明确）。

## 10c. partial 模式与错误详情增强（2026-08-14 补录，响应"不再重写全部"诉求）

**背景**：strict 全事务下，10 个 hunk 失败 1 个 → 9 个成功的也白做、整包重发。
用户要求"出错直接修改草稿，不再重写全部"。评估后分两步走：先做无状态版草稿
（partial——状态就是磁盘上的文件），服务端 draft 会话暂缓（状态生命周期/并发/
模型心智负担代价大，待灰度数据说话）。

**partial 语义**（`mode: "partial"`，默认 `strict` 不变）：
- 全部 hunk 仍在同一快照上定位（单快照不变，无 v1 顺序应用的漂移）；
- 成功的 hunk 应用 + 一次原子写盘；失败的 hunk 报详情；响应 status = `partial`，
  code = 首个失败码，`new_hash` = 部分应用后的 hash；
- 文本指引模型下一步：**只重发失败的 hunk + `expected_hash` = 返回的 new_hash**；
- 成功 hunk 之间的重叠仍拒绝；全部失败 → 零改动（与 strict 等价）。

**错误详情增强**（对应"错误写详细点，模型才会用"）：
1. NO_MATCH 携带**拒绝原因**：差阈值（best 0.82 < 0.90）/ 差 margin（best 与
   second 太接近，提示加 context）/ 完全不像；
2. 每个候选携带 **pattern vs 文件的 -/+ 行对照**（模型一眼看出 typo 在哪）；
3. AMBIGUOUS 的 detail 明确指示"挑一个候选，把它的周边行填进 context"；
4. 渲染文本含可操作指引（read_file 哪个区间、下一步发什么）。

**测试**：34 用例（新增 partial 应用/全失败/重叠拒绝/执行链续接、诊断三类分支、
候选 diff 内容断言、未知 mode 拒绝）。

## 10d. hash 门禁降级为可选校验（2026-08-14，响应"read 不回 hash"）

**事实查证**：read_file 的 `data` 元数据里有 `hash` 字段，但模型可见的正文
（L<number> 行内容）里没有——模型实践中拿不到 hash（`project_for_model` 虽含
`data`，但 hash 藏在元数据里，模型注意力不在那里；且部分 provider adapter 只透传
正文）。v2 原门禁"无 hash 禁止编辑已有文件"（FILE_ALREADY_EXISTS）因此把编辑
已有文件的路径完全堵死。

**改动**：
- `expected_hash` 降级为**可选加强**：有 → 校验（失配 → HASH_MISMATCH +
  current_content）；无 → **直接编辑**。
- 删除 FILE_ALREADY_EXISTS 错误码与门禁逻辑。
- 安全论证与 v1 内容定位一致：v2 全部是内容定位（无行号盲定位），**命中即安全**
  ——匹配到才改，模型对目标的描述本身就是校验；hash 是额外的并发乐观锁，
  有则用，无则不阻塞。多 subagent 并发静默覆盖的风险变为：仅当两个 agent 都
  不传 hash 且编辑同一区域时存在（v1 同样的暴露面）。
- 治本项（可选，未做）：让 read 的模型可见正文附带 hash 行；改 read 影响面大，
  暂缓。

## 10b. 实现阶段发现的新差异（2026-08-14 补录）

按规格 10.4 约定，实现中以下假设与 crate 实际行为不符，已以编译/测试结果为准修正：

1. **`similar::TextDiff::from_lines().ratio()` 不是字符级相似度**：它是**行粒度**
   比较（每行是原子单元），单行 typo 场景（`"let fo0 = 1;"` vs `"let foo = 1;"`）
   直接判 0，Tier3 完全失效。改用 `TextDiff::from_chars(a, b).ratio()`（difflib
   SequenceMatcher 字符级语义，1.0=相同）。**注意**：from_chars 是 Myers diff，
   大窗口成本高于行级；大文件 + 大 pattern 场景后续需接 strsim 粗筛。
2. **ropey 的 `remove`/`insert` 是 in-place 修改（返回 `()`）**，非规格假设的
   "返回新 rope"。倒序应用同样安全（后面的编辑不影响前面区间的 char index）。
3. **Tier3 评分前剥公共缩进**：规格公式用原始文本 ratio；实测"忘了缩进 + 打错
   一个字符"（最常见的 LLM 失误组合）会让前导空白把 ratio 拉到 0.85 以下而失败。
   v2 在评分前对 pattern/窗口/context 统一剥缩进，缩进偏差完全交给 Tier2 语义吸收。
4. **Tier2/3 命中时 new 补回基准缩进**：窗口按剥壳形状命中后，替换文本若原样
   落盘会丢掉文件里的缩进。v2 把 `new` 先剥自身公共缩进、再逐非空行补回文件窗口
   的基准缩进（相对缩进结构保留；`new` 带不带缩进都能给对）。Tier1 精确命中不补偿。
5. **尾换行语义**：替换区间是否含末行尾换行由 `old` 决定——`old` 以 `\n` 结尾才
   纳入区间（模型显式声明）；否则不含。好处：`old`/`new` 对称（都不带尾换行），
   单行替换 `old="b", new="B"` 不会把 B 与下一行粘住；整段删除时模型在 `old`
   末尾加 `\n` 即可干净删除。

---

## 11. 开放问题（评审时拍板）

1. ~~**T3 门槛初值**：0.85 / 0.10 还是 0.90 / 0.15？~~ → **已定为 0.90 / 0.15**
   （2026-08-14：保守起步，灰度期用真实误改案例回调后再调整）。
2. **hunk 数上限**：单次调用 hunks 上限是否显式限定（建议 64）？
3. **current_content 截断**：64 KB 是否合适（配置项 or 固定）？
4. **工具键名**：`edit_file_v2` 定稿？还是更描述性的键（如 `structured_edit`）？
5. **多文件**：v2 是否保留 files 批量入口（差分精神 → 不保留；若模型反馈不便再补）？

---

## 12. 读路径扩展：行号范围读 + 锚定读（2026-08-15）

**背景**：edit 兼读（空 hunks = 读）落地后，工作流从 `read+edit`（read 全文件 → edit 定位）
升级为 `glob+grep+edit`（grep 定位 → edit 精准读 → edit 同锚改）。读路径需要两种与 grep
直连的形态。

### 12a. 行号范围读（start_line / end_line）
- 1-based 闭区间，语义与 read_file 完全对齐：0 → PARSE_ERROR；end < start → PARSE_ERROR；
  start/end 越界 → LINE_OUT_OF_RANGE（data 带 total_lines + hash）；行数 > 400 → RANGE_TOO_LARGE；
  展示字符超 24K → RANGE_TOO_LARGE。
- 展示每行带 `L<number>:` 前缀（与 grep `path:line:` 输出、read_file 展示同格式）。
- 只给 start_line → 读到文件尾；hash 仍为**全文件** hash（expected_hash 契约不变，片段读
  不引入"片段 hash"概念）。
- 与整文件读（无前缀）并存：整文件读是旧 read 语义（模型要全文），范围读是 grep 衔接
  语义（行号对齐）。schema 的 description 已注明差异。

### 12b. 锚定读（anchor + context_before / context_after）
- **复用 hunk 的四层定位引擎（locate_anchor）**：读时的 anchor 命中 = 改时的 hunk 定位，
  读到的上下文就是将要编辑的上下文——读改一致性最大化。
- 窗口 = anchor 命中行 ± context_before/context_after（默认各 10，上限 100/侧），clamp 到
  文件边界；meta 返回 anchor_line / anchor_lines / tier / score / note。
- 模糊锚（多处命中）→ ANCHOR_AMBIGUOUS + 候选列表（candidate_json 与 hunk 候选同构），
  模型加 context 消歧或改用行号范围读——**读阶段解决歧义，改阶段不再失败**。
- 未命中 → NO_MATCH + 候选；空 anchor → UNDERSPECIFIED。
- 参数互斥：start_line/end_line 与 anchor/context 二选一（PARSE_ERROR）；context 必须配 anchor。

### 12c. 协作协议（glob + grep + edit 闭环）

```
grep {path:line} → edit 读 {start_line/end_line 或 anchor} → edit 改 {同一 anchor/old + expected_hash}
```

- 行号来源是 grep（不是模型数行号）；anchor 读改复用（不是先读全文再翻译成 hunk）。
- 权限：permission.rs 按"空 hunks"分类 Read——范围/锚定读均不触发写确认（注释已更新）。

### 12d. 变更清单

- file_edit_v2.rs：read_path / read_range / read_anchored / render_read_candidates /
  candidate_json；常量 READ_MAX_LINES=400 / READ_MAX_CHARS=24K / READ_MAX_CONTEXT=100；
  schema（5 个新参数 + required 收窄为 ["path"]）；description 更新。
- permission.rs：注释更新（读形态三态）。
- backend_prompt.md：TOOL DIRECTORY / 详解段 / 执行闭环（read+edit → grep+edit）三处教学更新。
- 测试：R5-R18（+14），workspace 258/258 全绿。

---

## 13. replace_inline：sed `s///` 语义吸收（2026-08-15）

**背景**：edit 的四层定位是整行级匹配，超长行（数百字符单行）只能整行替换；
backend_prompt.md 教学更新时暴露此边界。讨论后决定**不引入 sed 引擎**（命令式变换
与快照定位+事务+账本哲学冲突、正则方言/原地写风险），而是把 sed 真正独门的
`s///` 行内替换语义吸收为 hunk kind。

### 13a. 语义

```json
{"kind": "replace_inline", "anchor": "<整行锚>", "old": "<行内子串/正则>",
 "new": "<替换文本>", "replace_all": false, "regex": false}
```

- **anchor 限定窗口**：复用 locate_anchor（四层定位）→ 只在命中窗口内替换；
  sed 的"地址定址"由既有定位引擎接管，不引入第二套地址语法。
- 不跨行、不改行结构（逐行处理）；`replace_all=false` 只替换第一处（按行序）。
- `regex=true`：regex crate 语法（与 grep 同引擎族），大小写敏感（与 hunk 精确
  语义一致，不用 CLI 的 smart-case）；new 支持 `$1`/`$2` 捕获组展开（sed `\1` 对应）。
- 窗口内无 old → NO_MATCH（detail 标注窗口行号）；非法正则 → INVALID_REGEX。
- 走完整管线：快照定位、事务、expected_hash、原子写、code_delta（old/new 单行
  → 删 1 增 1，恰好表达"一行内容修改"）、permission（Write）。
- 典型场景：超长行内子串替换（配置/文档单行）、同构代码批量改名（replace_all）。

### 13b. 变更清单

- file_edit_v2.rs：Hunk::ReplaceInline + parse + kind_name；LocateError::InvalidRegex；
  locate_hunk（窗口内命中验证）；inline_replace（split_inclusive 逐行 + 子串/regex
  分支）；resolve（窗口 char range 整窗替换）；run_edit 错误分支；hint；schema/description。
- 测试：R19-R27（9 个：基本/全部/超长行/窗口无命中/regex 捕获组/非法正则/模糊锚/
  混用事务/hash 链），workspace 267/267。
- 实现教训：`done = !replace_all` 初始值写反（false 时永不替换）；手动拼接路径
  不展开捕获组 → 改用 `re.replace`（支持 $1 且只换第一处）。

### 13c. 教学迁移：backend_prompt.md → skill（不再撑爆 prompt）

- 新建 `skills/deepx-edit-file/SKILL.md`：edit_file_v2 完整操作手册（hunk 表 /
  读模式三态 / 四层匹配与错误码 / grep+glob 协作协议 / 陷阱）——按需 `skills
  action=activate` 注入，不进系统提示。
- backend_prompt.md：L18/L35 压缩为摘要 + 指向 skill；文件从 ~13KB 降至 ~6KB。
- 动机：极限模式（3-mode）需要特定提示词触发——深度教学从常驻 prompt 迁到
  按需 skill，标准模式保持轻量，极限/创造模式激活 skill 获取完整能力。

## 14. 宽松行号 hint_line：行号作提示、内容作通行证（2026-08-15 设计确认）

**动机**：模型从 read 结果复制行号是常态，但文件可能被其他工具改过（行号漂移
≤10 行）。v2 字段刻意无行号（v1 教训：行号定位 = 漂移即错改）——hint_line 把
行号降级为"搜索窗口提示"，内容匹配仍是唯一通行证。

### 14a. 设计（四条铁律）

```text
hunk 加可选字段 hint_line: u32（1-based）

1. 正常路径不变：全文件四层匹配，行号不参与定位 → 命中即正常应用
2. Tier4 拒绝（NO_MATCH）时，若 hint_line 存在：
   [hint_line-10, hint_line+10] 窗口内重跑四层 → 唯一命中 → 应用，
   响应回传 { used_hint: true, actual_line, offset }（偏差透明）
3. 窗口内歧义/不命中 → 拒绝，候选优先列窗口内内容（现代版 DID YOU MEAN）
4. 偏差 >10 行 → 拒绝（模型行号概念已失效，应重读而非猜测）
```

### 14b. 安全约束

- 行号永不单独定位（窗口内必须走完整 Tier1-4 阈值）
- 窗口命中必须唯一（歧义即拒绝，不猜）
- 默认路径零变化（不传 hint_line 行为与现状完全一致，向后兼容）

### 14c. 实现成本

- hunk schema + 解析加可选字段；匹配引擎加窗口参数（复用四层，仅搜索范围
  从全文件缩到行窗口）；响应加 used_hint/actual_line/offset；测试 R28-R35
  （窗口命中 / 窗口歧义 / 超窗拒绝 / 不传 hint 行为不变）

## 15. 二次确认与省 token：现状确认 + 非 toolcall 自循环（2026-08-15）

### 15a. 现状确认（已存在，无需新做）

```text
dry_run=true → 只定位+计算不写盘 → pending_id + diff 预览
→ 模型看 diff：满意 → confirm_apply {pending_id, action: apply} 落盘
             不满意 → 同 turn 重新 edit_file_v2 修正（无额外开销）
→ confirm_apply 落盘【不重发 hunks】（pending 内存暂存）——省 token 现成答案
```

**Phase 0（零代码）**：把 dry_run→pending→confirm 工作流写进
skills/deepx-edit-file/SKILL.md（模型养成"先预览后落盘"习惯）。

### 15b. 非 toolcall 自循环（三阶段，用户提议）

**动机**：toolcall JSON 协议冗长（schema 回显 + 参数重发）；模型天生擅长输出
diff/hunk。成熟先例：Claude Code（```apply-patch）、Aider（```diff）。

| 阶段 | 内容 | 状态 |
|---|---|---|
| Phase 0 | dry_run→pending→confirm 工作流文档化（零代码） | 待做 |
| Phase 1 | ```` ```edit ```` fence：assistant 文本检测 fence → 提取 hunk JSON →
  走【现有 execute_authorized 链】（身份/权限/admission/账本/audit 零绕过）→
  结果注入下一轮。白名单工具（先仅 edit_file_v2）+ 解析失败回退提示工具 | 待做 |
| Phase 2 | ```` ```diff ```` 直接解析（unified diff → hunk），更自然、解析成本高 | 观察 Phase 1 解析率再定 |

**铁律**：fence 解析后必须走与 toolcall 完全相同的执行链——只是入口从
"JSON toolcall"换成"文本解析"，内核零绕过（审计身份 = assistant 消息归属 session）。

## 16. 读/编辑引擎一致性确认（2026-08-15）

**结论：行号口径一致（1-based 文本行计数），锚定能力只在 edit 读模式。**

| 能力 | read_file（独立工具，file_query.rs） | edit_file_v2 读模式（同文件） |
|---|---|---|
| 行号计算 | 文本行计数（split lines） | 同口径（start_line/end_line 闭区间） |
| 行号展示 | `L<n>:` 前缀 | 同（读模式三态之一） |
| 范围语义 | 1-based 闭区间、越界 LINE_OUT_OF_RANGE | 对齐 read_file（0→PARSE_ERROR 等） |
| 范围上限 | 400 行 / 24K 字符 → RANGE_TOO_LARGE | **同上限**（刻意对齐） |
| 结果折叠 | `[details folded]` 是**平台层通用机制**（所有工具大结果折叠），非 read 独有 | 同（通用机制） |
| **续读 continuation** | **有**：超预算时回传可直接执行的续读参数（start_line 自动推进，L245-248） | **无**（仅截断标注 64KB/truncated） |
| 内容锚定 | **无**（纯行计数） | **有**（anchor + 四层引擎，读-改同锚） |
| 批量读 | 一次 8 文件 | 单文件 |

**衔接设计**：read_file 的行号可直接喂给 edit 行号范围读（同口径）；edit 的
锚定读解决"行号漂移"（anchor 与编辑共用四层引擎）。

**展示层缺口（移除 read 前需补齐）**：
1. **continuation 续读**——read 独有、edit 读缺失；模型读超预算时的续读体验会退化
2. **8 文件批量读**——read 独有；需决策 edit 补批量或接受单文件轮询
3. 折叠本身无需补（平台层通用，edit 读同样受益）

**教学现状**：edit_file_v2 的 tool description 已含读模式说明（"Omit hunks (or
send []) to READ the file…"），模型看 schema 即知。待办：SKILL.md 强化
"读优先用 edit 读模式（同锚）"指引；注意 read_file 的 8 文件批量读是 edit
读模式没有的能力——移除 read 前需决策：edit 补批量读或接受单文件轮询。

## 17. 补强三件套：宽容化 / 账本行号修正 / hint_line（2026-08-15 实施完成）

**动机**：行号漂移是模型盲定位错误的主要来源。三件套分别从"读的容错"、
"行号自动补偿"、"编辑定位消歧"三个方向收敛，全部**默认路径零变化**。

### 17.1 read 宽容化（file_query.rs，实施完成 5/5 测试）

| 输入 | 旧行为 | 新行为 |
|---|---|---|
| `start_line=0` / `end_line=0` | 拒绝 `read line numbers start at 1` | 视为 1（LSP 0-based 混用宽容） |
| `end_line` 越界 | `LINE_OUT_OF_RANGE` 拒绝 | **截断**到 `total_lines`（读少不读错，truncated 语义照旧） |
| `start_line` 越界 | 拒绝 | **仍拒绝**（行号概念失效，data 带 total_lines 可立即重试） |
| `end < start` / 超 MAX_LINES | 拒绝 | 不变 |

### 17.2 账本行号修正（file_state.rs + file_query.rs + file_edit_v2.rs，8/8 测试）

**机制**：read 建立行号基线（清空偏移链）→ edit 成功时按 hunk 记录
`(before_line, delta)` 偏移 → read 再次请求旧行号时自动补偿。

- `file_state::record_edit_with_shifts(path, content, shifts)`：edit 落盘后调用，
  追加偏移链；`record_write`/`record_read` 清链（全覆盖/新基线语义）；
  `record_grep(path)` 清链（grep 输出实时行号，防误修正）。
- `apply_ops` 返回 `(String, Vec<(usize, i64)>)`：每 hunk 的
  `(编辑前 1-based 起始行, 行号增量 = 新增换行数 - 移除换行数)`，单快照计算。
- `read_one`：偏移链非空时对 start/end 逐行补偿（首尾偏移一致才修正），
  meta 透明回传 `corrected/original_lines/line_offset`。
- **修正触发条件 = 偏移链非空**（不依赖 hash 失配——账本 edit 后 hash 已更新，
  失配检测会漏）；grep/write/read 清链保证实时行号与全覆盖场景不误伤。
- 外部修改（不经账本）→ 链空 → 不修正，`expected_hash` 兜底。
- 已知边界：`read → edit → grep → edit → read(用 grep 行号)` 链中 grep 清链
  后只修正第二个 edit 的偏移，语义正确；grep 与 read 的账本 key 形态不同时
  修正不触发（保守安全）。

### 17.3 hint_line 宽松行号（file_edit_v2.rs，R30-R35 共 6 测试）

**语义**：行号 = 提示/窗口（±10 行），内容 = 唯一通行证，唯一命中才应用。

- hunk 可选字段 `hint_line`（1-based；0 视为 1），支持 replace /
  insert_after / insert_before / replace_inline（replace_all 除外）。
- **触发入口 = AMBIGUOUS_MATCH**（多命中消歧，设计修正：NO_MATCH 场景窗口内
  Tier1 必然也无命中，无意义）：`[hint-10, hint+10]` 窗口内 Tier1 精确匹配，
  0 命中 → 保留原 Ambiguous；1 命中 → 应用（tier=4）；>1 → 新 Ambiguous。
- 透明回传：report JSON 加 `used_hint/actual_line/line_offset`（偏差 = 实际 - 提示）。
- **默认路径零变化**：hint_line 缺省时行为与 §8 原引擎完全一致（既有 82 测试全过）。

### 17.4 实施验证

- `cargo test --workspace` 全绿（新增 17 测试：read 3 + 账本 3 + 偏移 3 + hint 6 + 竞态加固 2）。
- `cargo build --workspace` 零警告零错误。
- 顺带修复：file_state `pending_roundtrip_applies_remote_ledger` 的并行竞态
  （全局 pending 队列互相抢增量 → 重试 3 次加固）。
