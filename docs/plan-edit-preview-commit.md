# PLAN：编辑工具两阶段协议 —— 预览写入 → 提交（Edit Preview/Commit）

> 状态：Approved（决策已确认）  ·  日期：2026-08-04  ·  范围：apply_patch / edit / edit_block / write

## 0.2 实施状态（2026-08-04）

| 阶段 | 状态 | 证据 |
|------|------|------|
| 0 存储骨架（`staged_plan.rs`） | ✅ 完成 | TTL/配额/LRU sweep/内容寻址校验/workspace 归属；5 个单测 |
| 1 `apply_patch` 两阶段 | ✅ 完成 | `commit`/`abort` 参数、dry_run 落盘返回 `preview_id`、逐文件校验、一次性语义；4 个新单测 + CLI E2E 全链路通过（preview→staged→commit→清理→重复提交 404） |
| 2 `edit` / `edit_block` 两阶段 | ⬜ 待实施 | |
| 3 `write` 纳入 + `status` 查询 | ⬜ 待实施 | |

## 0.1 实施位置（已确认）

工具引擎位于本仓库 `crates/deepx-workspace`（workspace.exe 本体，CLI + HTTP serve 双入口，`main.rs`）：
- `apply_patch.rs`：已有完整两阶段雏形——`dry_run` 生成 `plan_hash`（`PatchPlan::plan_hash`，sha256 over changes），非 dry_run 校验 `plan_hash` 参数，但**应用时必须重传完整 patch 重建 plan**（减负点）；
- `file_mutate.rs`：`exec_edit_file` 已有 `dry_run`（仅预览不返回句柄）；`exec_write_file` 无 dry_run；
- `file_state.rs` / `file_shared.rs`：文件 hash/原子写基础件。
落盘目录：`<workspace>/.deepx/staged-plans/`（与现有 `.deepx/trash/` 同层级）。

## 0. 已确认决策

1. `commit` / `abort` **作为工具参数**（`{ "commit": preview_id }`），不做独立命令——独立命令增加调用负担与心智成本；
2. 旧单次调用（`dry_run=false` + 完整内容）**永久保留**——简单改动一次调用更省；
3. undo / revert **本期不做**——留待沙箱阶段做体系化编辑工具时统一设计（集中 undo + 读路径 overlay）；
4. `write` **纳入本期**——三个工具（apply_patch/edit/edit_block）升级时同步统一，全工具协议一致；
5. **冗余防御（2026-08-04 补充）**：LLM 若在 commit 时仍按旧习惯附带完整 patch——附带 patch 重建的计划与 `commit_id` **一致 → 容忍并忽略冗余**（正常提交）；不一致/无法解析 → `COMMIT_WITH_PATCH` 拒绝。旧调用习惯不会无故断裂；
6. **todo 命名统一（2026-08-04）**：主工具名由 `task` 统一为 **`todo`**（与 prompt/文档一致）；`task` 保留为兼容别名（同一 handler、schema 标注 deprecated），宿主侧全面切换后移除。系统 prompt（backend_prompt.md）[TASK MANAGEMENT] 改用 todo + create_batch、[WORKFLOW]/[FILE EDITING] 补充两阶段 commit 示例。

## 1. 问题与目标

**问题**：当前 dry_run 预览后，正式应用必须**再次完整重复输出 patch/edit 内容**，存在两个成本：
1. Token 浪费：大 patch 被传输两次（预览 + 应用）；
2. 不一致风险：模型二次输出时可能手滑改错（锚点漂移、漏行、改串），预览-应用语义被破坏。

**目标**：
- dry_run 提升为"预览写入"：返回 `preview_id`（复用现有 `plan_hash` 机制）与 diff、预期文件 hash；
- 新增 commit 语义：后续**仅凭 `preview_id` 即可提交**，不再携带修改内容；
- 三个编辑工具（apply_patch / edit / edit_block）协议统一；
- 向后兼容：现有"单次调用直接应用"路径永久保留。

## 2. 协议设计（统一语义）

```jsonc
// ① 预览（不写盘，纯计算）
{ "tool": "apply_patch|edit|edit_block", "...": "原有参数", "dry_run": true }
→ 200 {
    "preview_id": "ab12…64hex",
    "diff": "…",
    "expected_hashes": { "<path>": "<sha256>" },   // 提交时的逐文件校验基准
    "expires_at": 1722744000000
  }

// ② 提交（仅带 preview_id，禁止混入 patch 内容）
{ "tool": "<同上>", "commit": "ab12…" }
→ 200 { "status": "committed", "files": [{ "path": "…", "before_hash": "…", "after_hash": "…" }] }

// ③ 放弃
{ "tool": "<同上>", "abort": "ab12…" }
→ 200 { "status": "aborted" }

// ④ 查询（可选）
{ "tool": "<同上>", "status": "ab12…" }
→ 200 { "preview_id": "…", "files": […], "created_at": …, "expires_at": … }
```

- `preview_id` = 现有 `plan_hash` 语义扩展（64-hex，内容寻址，不可猜测）；
- commit 请求若同时携带 patch 内容 → `400 COMMIT_WITH_PATCH`（API 歧义拒绝）；
- 计划绑定 `(session_id, workspace)`，跨会话/跨工作区提交拒绝。

## 3. 暂存计划存储

**结构**（`StagedPlan`）：
```jsonc
{
  "preview_id": "ab12…",
  "tool": "apply_patch",
  "session_id": "…",
  "workspace": "F:\\DeepX",
  "created_at": 1722740000000,
  "expires_at": 1722826400000,
  "diff": "…",                                  // 人类可读预览
  "patch_source": "…",                          // 原始 patch/old_string+new_string（应用时重新走匹配）
  "expected_hashes": { "<path>": "<sha256>" },  // 提交校验基准
  "files": ["<path>", …]
}
```

**载体**：
- 内存 `HashMap<preview_id, StagedPlan>`：本进程热路径；
- 磁盘 `<workspace>/.deepx/staged-plans/{preview_id}.json`：跨重启/多进程（workspace.exe 场景）。

**生命周期（P0，防泄漏）**：
| 项 | 策略 |
|----|------|
| TTL | 24h（可配置），访问时惰性过期 + 启动时全量扫描清理 |
| 配额 | ≤ 500 个计划 / ≤ 50MB，超限按 LRU 淘汰（先 abort 最旧） |
| 会话结束 | 可选：会话级 abort 全部未提交计划 |
| 安全 | `preview_id` 必须匹配 `^[a-f0-9]{64}$`（路径遍历防护）；计划文件只允许存在于 staged-plans 目录内 |

## 4. 提交流程（校验 + 事务）

```
commit(preview_id)
 ├─ 取计划（内存 → 磁盘）；不存在/过期 → 404 PREVIEW_NOT_FOUND / 409 PREVIEW_STALE
 ├─ 所有权校验（session_id + workspace）→ 403 PREVIEW_OWNERSHIP
 ├─ 逐文件 hash 校验：磁盘当前 hash == expected_hashes[path]
 │     └─ 不等 → 409 PREVIEW_CONFLICT（返回冲突文件清单 + 当前 hash），不部分应用
 ├─ 事务应用：
 │     ├─ 全部新内容写入同目录临时文件（*.deepx-tmp）
 │     ├─ 全部就绪后逐个 rename 覆盖
 │     └─ 任一 rename 失败 → 已覆盖文件用写前备份回滚（备份存于计划目录内）
 ├─ 成功后：删除暂存计划（内存 + 磁盘）
 └─ 返回 committed（含 before/after hash，供审计）
```

- 幂等：commit 成功后计划即删除；重复 commit → `410 PREVIEW_ALREADY_COMMITTED`（明确错误，不重放）。
- 事务应用同时修复现状"多文件 patch 部分失败可能部分应用"的隐患。

## 5. 各工具改造点

| 工具 | 现状 | 改造 |
|------|------|------|
| `apply_patch` | dry_run 返回 plan_hash（仅校验用，不落盘） | ① dry_run：计划落盘 + 返回 `preview_id`/`expected_hashes`；② 新增 `commit`/`abort` 参数；③ 保留 `dry_run=false + 完整 patch` 直接应用 |
| `edit` | dry_run 仅预览，不返回句柄 | ① dry_run：计算 `expected_hash_before`（old_string 匹配后目标文件 hash）+ 生成 `preview_id` + 落盘（计划 = path + old/new + hash）；② `commit`/`abort` |
| `edit_block` | 同上（fuzzy 匹配） | 同上；fuzzy 计划存原始行块，提交时重新走模糊匹配（hash 校验先行） |
| `write` | 无 dry_run | 新增 `dry_run` 预览（目标路径存在性 + 内容 hash），与三工具协议统一（决策 4） |

## 6. 错误码（新增）

| 码 | 语义 |
|----|------|
| `404 PREVIEW_NOT_FOUND` | preview_id 不存在/已提交/已 abort |
| `409 PREVIEW_STALE` | 计划已过 TTL |
| `409 PREVIEW_CONFLICT` | 目标文件被外部修改（hash 不匹配），返回冲突清单 |
| `403 PREVIEW_OWNERSHIP` | 跨会话/跨工作区提交 |
| `400 COMMIT_WITH_PATCH` | commit 与 patch 内容混用 |
| `410 PREVIEW_ALREADY_COMMITTED` | 重复提交（幂等保护） |

## 7. 测试计划

**单元**：
- preview → commit 全流程（apply_patch 多文件：新增/删除/移动/更新混合）
- preview 后外部修改目标文件 → commit 409，**不部分应用**
- 重复 commit → 410；abort 后 commit → 404
- TTL 过期 → 404；配额超限 LRU 淘汰
- 跨会话提交 → 403
- edit / edit_block 两阶段（含正则 old_string、fuzzy 场景）
- 路径遍历注入（`../../` 文件名、非 64-hex preview_id）→ 拒绝

**集成**：
- 大 patch（>100 hunks、多文件）preview → commit 全链路
- 事务失败注入（模拟 rename 失败）→ 全量回滚验证
- workspace.exe 场景：计划落盘目录跨进程可见

**回归**：现有单次调用路径（`dry_run=false` 直接应用）全量回归，行为零变化。

## 8. 实施步骤（每阶段独立可交付）

| 阶段 | 内容 | 交付 |
|------|------|------|
| 0 | 存储骨架：`StagedPlan` 结构、staged-plans 目录、TTL/配额/LRU、启动清理 | 可独立测试的生命周期模块 |
| 1 | `apply_patch` 两阶段：preview 落盘 → `commit` → `abort` | apply_patch 减负落地 |
| 2 | `edit` / `edit_block` 两阶段 | 工具协议统一 |
| 3 | `write` 纳入 + `status` 查询 | 全工具协议统一（决策 4） |

> undo 快照与 revert **不在本期**（决策 3）——留待沙箱阶段与集中 undo、读路径 overlay 一并体系化设计。

每阶段：单元 + 集成测试 + 工具描述（system prompt）更新。

## 9. 兼容与迁移

- 单次调用（不带 dry_run）路径**永久保留**，模型可选择直改或两阶段；
- 工具描述更新为推荐流程：`preview → commit`，大改动必须两阶段；
- 现有已形成的"dry_run → 重复 patch 应用"习惯自动迁移到"preview → commit"；
- 旧 `plan_hash` 字段名保留为 `preview_id` 的别名（过渡期兼容），下一版本移除别名。

## 10. 风险与对策

| 风险 | 对策 |
|------|------|
| 磁盘暂存泄漏 | TTL + 配额 + 启动清理，P0 阶段完成 |
| 计划与工作区漂移 | 逐文件 hash 校验，409 不部分应用 |
| 大 patch 磁盘占用 | 配额（50MB）+ LRU；`patch_source` 存原始文本而非展开结构 |
| preview_id 猜测/注入 | 64-hex 校验 + 目录内路径强制 |

## 11. 明确不做（本期）

- undo 快照 / revert（决策 3：留待沙箱阶段与集中 undo、读路径 overlay 一并体系化设计）
- 全量拷贝影子工作区 / 就地 `.old` 备份（沙箱形态另案：增量 overlay + 集中 undo）
- 引入 git2/gix（本需求不需要；锚点匹配机制与 git patch 不兼容）
- 读路径 overlay（暂存区优先视图）——留待 workspace.exe 独立化阶段
