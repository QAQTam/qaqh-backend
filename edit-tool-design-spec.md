# Rust 编辑工具设计规格（edit_file）

配合已有的 `glob`（定位文件）+ `rg`（定位内容行）工具，本工具补上"改内容"这一环，形成一条完整工具链：glob 缩小文件范围 → rg 定位具体内容 → edit_file 完成修改。

## 0. 约束

- 调用方是 LLM：不擅长精确数行号，但擅长复现"看到过的文本片段"
- 本地优先、Rust 实现，尽量少引外部重依赖
- 转义问题必须在协议层根治，不能要求模型自己转义特殊字符
- 项目统一使用 LF 换行
- 要扛住并发编辑、CRLF 混入、大文件、重复代码块等边界情况

## 1. 核心设计决策（相对常见方案的新意）

1. **多段式 hunk，而非整块 old_str/new_str**：一次调用可含多个不连续修改点，每个 hunk 只需给"变化的那几行 + 少量上下文"，不必复现整个函数体——省 token，也降低复现出错概率。
2. **结构化 JSON 字段，而非文本 patch DSL**：不采用 unified diff 那种 `@@`/`-`/`+` 行前缀语法（如 Codex 的 apply_patch）。原因：一旦源码本身有行以 `-`、`+`、空格开头，文本 DSL 就会和内容语法冲突、需要转义。`old`/`new`/`context_before`/`context_after` 各自是独立的 JSON 字符串字段，JSON 序列化层自动处理转义（换行、引号、反斜杠等），模型不需要手动转义任何字符——这是"确保转义正常"的根本解法，不是靠约定或提醒模型小心。
3. **四层递进模糊匹配**，而非"精确匹配/直接拒绝"两级：精确 → 缩进归一化 → 相似度评分(+context加权) → 拒绝并返回候选。每层降级都可解释，不是模糊地"尽量猜"。
4. **相似度评分引入 margin（胜出边际）规则**：不是"分数过阈值就采纳"，而是"最高分要显著超过次高分"才自动采纳，避免在两个差不多像的候选里赌一个。
5. **rope 结构承载文件内容**：多 hunk 一次应用、大文件场景下，避免每次编辑都整份重新分配字符串。
6. **hash 续接而非强制重新 view**：每次编辑成功后直接返回新 hash，下次编辑可直接拿这个 hash 当 `expected_hash`，不必重新读一遍全文——省 token，同时保留"陈旧视图检测"的安全性。
7. **明确排除的方向**：Cursor/Morph 式的"专用 apply 模型二次合并"方案。这需要额外的模型调用和网络往返，与本地优先的定位冲突，也不属于"Rust 库"范畴，故本设计不采用，全部逻辑用确定性算法在本地完成。

## 2. 数据结构

```rust
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EditHunk {
    /// 替换一段内容；old 为空字符串时表示"仅在此处插入"，
    /// 此时 context_before / context_after 至少一个不能为空。
    Replace {
        context_before: String,
        old: String,
        new: String,
        context_after: String,
    },
    /// 在匹配到 anchor 的文本之后/之前插入 new；anchor 走与 old 相同的匹配流水线
    InsertAfter { anchor: String, new: String },
    InsertBefore { anchor: String, new: String },
    /// 文件头/尾插入，无需匹配，天然无歧义
    PrependFile { new: String },
    AppendFile { new: String },
}

#[derive(Deserialize)]
pub struct EditFileRequest {
    pub path: String,
    /// 上一次 view/edit 返回的 hash；新建文件时传 None（且要求路径当前不存在）
    pub expected_hash: Option<String>,
    pub hunks: Vec<EditHunk>,
}

#[derive(Serialize)]
pub struct EditFileResult {
    pub new_hash: String,
    pub diff_preview: String, // unified diff 文本，供人类/UI展示
    pub applied_hunks: usize,
    pub notes: Vec<String>,   // 如 "hunk[1]: 经缩进归一化匹配命中" / "检测到 CRLF 已归一为 LF"
}

#[derive(Serialize)]
#[serde(tag = "error")]
pub enum EditFileError {
    FileNotFound,
    FileAlreadyExists, // expected_hash=None 但文件已存在
    NotUtf8Text,
    HashMismatch { current_hash: String, current_content: String },
    NoMatch { hunk_index: usize, candidates: Vec<MatchCandidate> },
    AmbiguousMatch { hunk_index: usize, candidates: Vec<MatchCandidate> },
    OverlappingHunks { hunk_a: usize, hunk_b: usize },
    Underspecified { hunk_index: usize }, // old/context/anchor 全空
}

#[derive(Serialize)]
pub struct MatchCandidate {
    pub line_range: (usize, usize), // 仅展示用，1-indexed
    pub snippet: String,
    pub score: f32, // Tier1 = 1.0
    pub tier: u8,
}
```

> 以上签名为示意结构，字段命名/派生宏可按实现习惯调整；重点是"结构化字段"这个协议形状本身。

## 3. 匹配算法（四层）

对每个需要定位的 hunk（`Replace`/`InsertAfter`/`InsertBefore`），按顺序尝试，命中即停：

**Tier 1 —— 精确匹配**
在全文查找 `old`（或 `anchor`）的所有精确出现位置。唯一则直接采用（score=1.0）。多处时，用 `context_before`/`context_after` 核对紧邻文本筛出真正匹配的那一处；核对后仍不唯一则降级。

**Tier 2 —— 缩进/空白归一化匹配**
把 `old` 按行拆分，剥离"公共最小缁进"得到"形状"；用相同行数的滑动窗口扫描文件，同样剥离每个窗口的公共最小缩进后逐行比较。这样能容忍模型记错整段代码的基准缩进（比如忘了这段在 impl 块里要多缩两格），同时保留段内的相对缩进结构（相对缩进往往是语法有效性所依赖的）。命中且唯一则采用，否则降级。

**Tier 3 —— 相似度评分匹配**
用同样行数（±1~2 行容差）的滑动窗口，对每个候选计算综合分数：

```
score = 0.6 * ratio(old, window)
      + 0.2 * ratio(context_before, 窗口前文)
      + 0.2 * ratio(context_after, 窗口后文)
```

`ratio()` 用 `similar::TextDiff::from_lines(a, b).ratio()`（已核实此 API 存在，行为等价于 Python difflib 的 SequenceMatcher.ratio()，返回 0.0~1.0）。取分数最高的候选；仅当「最高分 ≥ 0.85」且「与次高分差距 ≥ 0.10」才自动采纳，否则视为不确定，降级到 Tier 4。这两个数字是起点，建议实现后用真实误改案例回调。

**Tier 4 —— 失败，返回候选**
返回 Top 3 候选（不论来自 Tier2 还是 Tier3），附带 `line_range`（仅展示）、`snippet`、`score`、`tier`，让调用方看着候选重新给出更精确的 `old`/`context`，或改用 rg 重新定位。绝不"选一个凑合用"。

**空 `old` / 纯插入**：`context_before`、`context_after` 走同样的 Tier1~3 流程分别定位，取两者交界处插入；只提供一侧就以那一侧为锚点；两侧都为空则返回 `Underspecified`，提示改用 `InsertAfter`/`PrependFile`/`AppendFile`。

## 4. 应用与写入

1. 用同一份**未修改的原始 rope 快照**解析全部 hunk 的目标 char 区间——即使某个 hunk 失败，也先把全部 hunk 定位完，一次性汇总错误，不要改到一半才发现后面有问题。
2. 检查所有已解析区间两两是否重叠 → 有重叠则整体拒绝（`OverlappingHunks`），不做部分应用；整次调用是事务性的，要么全部成功要么完全不改。
3. 按区间起始位置从大到小排序，依次对 rope 执行 `remove(range)` + `insert(start, new)`——从文件末尾往前改，前面 hunk 的位置不会被后面的编辑影响，不需要维护偏移量表。
4. `new`/`old`/`context` 中若混入 `\r\n`/`\r`，统一归一化为 `\n`（对应"统一LF"约定），并在 `notes` 里注明；原文件若意外含 CRLF，读入时先整体归一化。
5. 用 `tempfile::NamedTempFile::new_in(同目录)` 写入 + `persist(path)` 做原子替换（临时文件必须和目标文件同目录，否则 rename 可能跨文件系统失败；Windows 上对应 `MoveFileExW` 的替换语义，`tempfile` crate 已处理好跨平台细节）。
6. 计算最终内容的 `blake3` hash 作为 `new_hash`；用 `similar` 生成 unified diff 字符串作为 `diff_preview`（这也是 UI 展示"agent 刚才改了什么"的现成素材）。

## 5. 并发与陈旧视图（hash gate）

`expected_hash` 是乐观并发控制：写入前重新读取文件、算 hash，和 `expected_hash` 不一致就直接拒绝，把 `current_hash` + `current_content` 一起返回（模型不需要再单独调一次 view）。这对多 subagent 并发改同一文件的场景很重要——后提交的一方 hash 校验必然失败，被迫重新读取而不是静默覆盖前一个的修改；同理，若 harness 因网络抖动重试了同一个编辑请求，第二次请求会因 hash 已变化而失败，不会重复应用同一改动。

## 6. 依赖库清单

| Crate | 用途 | 备注 |
|---|---|---|
| `ropey` | 文件内容的核心数据结构 | O(log N) 编辑，操作以 char index 为单位，天然避免切断 UTF-8；已核实 API：`Rope::from_str` / `line_to_char` / `remove` / `insert` / `slice` |
| `similar` | Tier3 相似度评分、生成 diff 预览 | 已核实 `TextDiff::from_lines(a,b).ratio()` 返回 0.0~1.0 相似度；若只需要好看的 diff 文本，也可以用 `diffy` crate（专门产出 Unified Format 的 Patch，支持带颜色渲染）替代/搭配 |
| `blake3` | 内容哈希，做 `expected_hash` 乐观锁 | 比 sha256 快很多，适合每次编辑都算一次的场景 |
| `tempfile` | 原子写入（临时文件 + rename） | 需与目标文件同目录创建临时文件 |
| `serde` / `serde_json` | 工具调用参数/结果的结构化序列化 | 转义问题在这一层被自动解决 |
| `strsim`（可选） | 超大文件场景下，先用便宜的 Levenshtein 粗筛候选窗口，再对幸存者跑较贵的 `similar::ratio` | 非必需，普通源码文件量级下 `similar` 直接跑够快 |
| `tree-sitter`（可选，Phase 2） | 用语法结构而非文本锚定（比如"函数 parse 的函数体"），彻底摆脱格式/缩进敏感性 | 代价是要管理各语言 grammar，工程量明显更大，先不纳入核心方案 |

## 7. 可选安全阀：置信度分级自动应用

考虑到上层可能是有 UI 的桌面应用，可以加一个开关：只有 Tier1（精确匹配）自动应用；Tier2/Tier3 命中时先返回 `diff_preview` 让上层确认，再用一个 `confirm_edit(request_id)` 调用真正落盘。纯后台无人值守场景可以关掉这个开关，全部 tier 自动应用。这是可配置项，不写死。

## 8. 边界情况清单

- 空文件插入 / 全新文件创建（`expected_hash: None` + `PrependFile`）
- 路径存在但是目录，或路径不存在
- 文件不是合法 UTF-8 → 当作二进制直接拒绝（`NotUtf8Text`），不做任何"尝试解读"
- `old` 为空且 `context_before`/`context_after` 也全空 → `Underspecified`
- 一次调用里多个 hunk 解析出的区间重叠 → 整体拒绝，不做部分应用
- 文件原本无结尾换行 / hunk 改到最后一行时，沿用原文件的换行约定，不引入新约定
- 请求里的 `old`/`context`/`new` 混入 `\r\n` → 静默归一化为 `\n`，`notes` 里注明
- 同一段代码在文件里重复出现多次（比如相似的 match 分支）→ 依赖 `context_before`/`context_after` 消歧；两侧都消歧不了则老实报 `AmbiguousMatch`
- 大文件（数千行以上）性能兜底：Tier3 滑动窗口比较随文件行数线性增长，必要时接入 `strsim` 做前置粗筛

## 9. 测试用例清单

1. 精确匹配，唯一命中
2. 内容相同但缩进基准不同 → Tier2 命中
3. 内容有细微出入（比如变量名打错一个字母）→ Tier3 命中，且分数/margin 都达标
4. Tier3 两个候选分数接近（margin 不够）→ 应该拒绝而不是瞎猜
5. 同一模式在文件中出现 3 次，仅靠 `old` 无法消歧，加上 `context_before` 后能唯一定位
6. 一次调用里两个 hunk 互不重叠，各自命中不同位置，应用后两处都正确且互不干扰
7. 一次调用里两个 hunk 区间重叠 → 整体拒绝，文件不应有任何改动
8. 空文件场景下 `PrependFile`/`AppendFile`
9. 文件末尾没有换行符时的 `AppendFile` 行为
10. 传入内容含 `\r\n` → 自动归一化为 `\n`，且 `notes` 有记录
11. `expected_hash` 不匹配 → 拒绝，返回当前 hash + 当前内容
12. `expected_hash: None` 但文件已存在 → `FileAlreadyExists`
13. 非 UTF-8 二进制文件 → 直接拒绝，不做任何写入尝试
14. `old` 长度为 0 且 `context_before`/`context_after` 也为空 → `Underspecified`

## 10. 交付物要求

请基于以上设计产出：

1. 完整可编译的 `Cargo.toml`（依赖用当前稳定版）+ 核心模块代码（数据结构、四层匹配、应用与写入、hash gate）
2. 覆盖第 9 节全部用例的单元测试（用真实的小型文本样例构造 fixture，不要只测 happy path）
3. 一份简短 README，说明本工具如何与 `glob`/`rg` 配合成"定位文件 → 定位内容 → 修改内容"的工具链
4. 若实现中发现某环节（尤其是 Tier3 阈值/margin 的具体数值、各 crate 的具体 API 签名）与本文档描述有出入，以 crate 实际文档/编译结果为准，并在 README 里注明与本设计文档的差异点
