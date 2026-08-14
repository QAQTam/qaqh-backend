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
  "expected_hash": "sha256:9f86d0…",   // 可选；read_file 返回的 hash 原样回传；
                                          // 为 None 时仅允许"创建新文件"路径
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
| `FileAlreadyExists` | expected_hash=null 但文件已存在 | — |
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
就是 spec 原样的显式 `expected_hash` 乐观锁：

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
| 12 | expected_hash=null 但文件已存在 → FileAlreadyExists | 创建路径 |
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
