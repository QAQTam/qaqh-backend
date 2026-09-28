# V2 文件变更事件 Spec（FileMutationDelta）+ edit/apply_patch 内核合一

> 状态：spec 冻结稿，待执行。依据 2026-09-27 代码实证。
> 关联：`plan-context-ownership-v2.md`（file_state 归位为 ContextPlugin 的宿主）、
> `plan-permission-extraction-v2.md`（无交集，可并行）。

## 0. 实证起点

1. 三套内容锚定匹配并存：`edit/core.rs`（LF 视图精确匹配）、
   `copy_range::locate_exact`、`apply_patch_engine/seek_sequence`（4 级容差，
   移植自 codex-rs，最完善）→ **收敛目标 = seek_sequence**。
2. 引擎内部已算出行号：`file_update::compute_replacements` 返回
   `(start_index, old_len, new_lines)`；但 `ApplyOutcome` 只吐聚合结果，
   **行号在 API 边界被丢弃**。
3. `file_state` 偏移链由 4 个工具各自维护（edit / file_mutate / copy_range /
   apply_patch），绕过即漏。
4. edit 的模型面保留理由（不可被 apply_patch 替代）：str_replace 是跨厂商
   通用编辑方言；失败反馈回路短（old_str→实际内容 diff 自纠）；单点改动
   token 开销小；`replace_all` patch 无法表达。

## 1. 冻结契约：FileMutationDelta

```rust
/// 文件变更事件的唯一 schema。前端、journal、file_state 三方消费。
/// 行号一律为「LF 规范视图、1 起始、原文件坐标系」。
pub struct FileMutationDelta {
    pub path: String,            // workspace 相对路径
    pub op: PathOp,              // Write/Edit/Patch/Delete（复用 tool_api::display::PathOp）
    pub hunks: Vec<HunkDelta>,
    pub added: u32,              // Σ new_lines（Delete 时为 0）
    pub removed: u32,            // Σ old_lines
}

pub struct HunkDelta {
    pub old_start: u64,   // 原文件中该 hunk 起始行（1-based）
    pub old_lines: u64,   // 被替换行数（删除/替换 >0，纯插入 =0）
    pub new_start: u64,   // 新文件中对应起始行
    pub new_lines: u64,   // 替换后行数（纯删除 =0）
}
```

事件通道与语义：
- 载体：工具结果 projection（`ToolContentBlock` 结构化块）+ timeline 事件，
  复用现有 ToolCallId 关联；
- **edit = 单事件原子**；**apply_patch = 1 个 resolved 事件 + N 个 applied 事件 +
  1 个终态事件**；前端按 `tool_call_id` 聚合，不假设一次一发；
- apply_patch 分步期间任一 hunk 失败：已发 resolved/applied 的 hunk 由终态事件
  标注 `failed`（写盘语义保持全-or-nothing 不变，前端只做动画分步）；
- 本 schema 属于 wire 契约：**冻结后字段只增不改**。

## 2. edit 侧实现清单

1. delta 构造：唯一命中 → `LineIndex` 直接得 `old_start`；
   `old_lines = old_str 行数`、`new_lines = new_str 行数`（LF 视图计数）；
2. `replace_all`：展开为 N 个 hunk，单事件携带（非重叠语义不变）；
3. 发 FileMutationDelta；journal 追加与 file_state 偏移链更新
   **改为消费同一 delta**（消灭手工双记账）；
4. 失败 UX 不变（NOT_FOUND 自纠 diff / AMBIGUOUS_MATCH 列命中位）。

## 2.5 write（file_mutate）侧实现清单

- **退役 `code_delta.rs` 的参数猜数层**：现行实现 `file.write` 硬编码
  `lines_removed: 0`（覆盖写 500 行改 1 行显示 `+500 -0`），edit 只有数量
  无行号。`CodeDeltaRecord`（domain）改为从 FileMutationDelta 聚合派生。
- write = 写前读旧内容（atomic_write 流程已有旧字节）→ 与 args.content
  做**行级 diff（Myers/LCS，纯函数）** → 产出真实 hunks → 单事件原子发
  （与 edit 同语义）。新文件 = 单 hunk 全量插入。
- 边界：文件 >5k 行或改动比例 >60% 降级为单 hunk 整文件 hunk（chips 仍准确）；
  二进制内容 hunks 置空只留聚合；diff 只在 LF 规范视图上做。
- 定位说明：此处的 diff 是**比对引擎**（两份内容求差异），与 seek_sequence
  的**定位引擎**（内容锚定匹配）无交集，不参与 §4 的合一。

## 3. apply_patch / 引擎侧实现清单

1. **漏行号**：`ApplyOutcome` 增加 `resolved: Vec<ResolvedHunk>`，
   `{path, start_index(0-based→转 1-based), old_len, new_len}`——
   数据来自 `compute_replacements`，纯结构改动；
2. **resolve-only 通道**：`dry_run_patch_engine` 复用为 resolve 阶段，
   返回带行号的 ResolvedHunk（现返回聚合，需同上结构改动）；
3. **per-hunk sink**：`apply_patch_engine` 增加可选回调参数
   `on_hunk: impl FnMut(HunkDelta)`，定位成功即发；默认 no-op 不破坏现有调用；
4. 多文件聚合：per-file added/removed（前端 chips 数据源）；
5. 补协议边界测试：内容行以 `+`/`-`/`***` 开头的转义、空 hunk、
   `*** End of File` 锚、CRLF 文件 hunk 行号。

## 4. 内核合一（edit 迁移到 seek_sequence）

- edit 的匹配改为调 `seek_sequence`（4 级容差）+ **容差策略可配**：
  edit 默认 tier1（精确）命中即止、歧义即拒（保持现有严格语义）；
  `old_str` 匹配不到时才走 rstrip/trim 提示「近似命中」进自纠 diff——
  **不静默采纳模糊匹配**；
- `text_file.rs`（行尾保真抽象）与 `file_shared` 的 LF 视图/写回还原二选一：
  保留 `text_file`，`file_shared::normalize_newlines` 降级为它的调用方；
- `copy_range`：**先拿调用数据裁决**。近两周会话日志调用量 ≈0 → 退役；
  若保留，其 locate_exact 同样迁 seek_sequence。

## 5. 执行顺序与守门

| 步 | 内容 | 风险 |
|---|---|---|
| 1 | 冻结 §1 schema + 前端对齐（wire 字段名确认） | 契约，冻结后不改 |
| 2 | edit 实现 delta（§2） | 低；行号直接可得 |
| 3 | 引擎漏 start_index + per-hunk sink（§3.1–3.4） | 低；机械改动 |
| 4 | journal/file_state 统一由 delta 驱动 | 中；先跑 tool_crash_recovery /
| | | tool_ledger_* 测试基线 |
| 5 | edit 匹配迁 seek_sequence（§4） | 中；严格语义回归 |
| 6 | copy_range 数据裁决 | 决策项，非技术项 |

守门点：
- CRLF：行号只认 LF 规范视图；写回还原路径唯一（text_file）；
- `restart_prefix_cache` / journal 重放契约：delta 事件是新增投影，
  不得改变既有持久化字节格式；
- audit/quarantine 链（AUDIT_QUARANTINED → Indeterminate）在执行监督侧，
  与本 spec 无交，勿牵连；
- 每步收尾 `cargo check --workspace --all-targets`；测试补齐节奏后置
  （Step 4/5 落地前必须先补对应基线）。
