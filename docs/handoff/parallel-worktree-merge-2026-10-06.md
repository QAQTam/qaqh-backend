# 并行 worktree 合并交接（2026-10-06）

> 状态：**已完成**（本地 main 已整合，待远端 PR 落库）。
> 范围：6 个 worktree 的并行改动归拢到 `main`。

## 1. 背景

同一份仓库曾以 6 个 worktree 并行推进，出现「同一功能在两条线上各改一版」的
分叉形态。本次把全部并行成果合并回 `main`，并显式处理「删除语义」与
「更早基线可能带回旧内容」的风险。

合并前 `main` 与 `origin/main` 的关系：`origin/main = bb483d2`，
本地 `main` 领先 38 个提交（纯快进后继），371 文件变动。

## 2. worktree 清单与去向

| worktree | 分支 | 相对 main | 处置 |
|---|---|---|---|
| `E:\qaqh-backend` | `main` | — | 先落未提交修复，再作合并目标 |
| `.codex\...\workspace-audit-pr3` | `feat/workspace-audit-pr3` | +2 提交 | 合并（3 处冲突语义解） |
| `.qoder-cn\...\36ca23` | detached `9701ca0` | 无独有提交 | 内容已在 main，无需合并 |
| `E:\qaqh-addin-winsandbox` | `addin-winsandbox` | 无提交（落后 7） | 先提交沙箱 spike 再合并 |
| `E:\qaqh-backend-p2` | `refactor/p2-...` | 与 main 同点 | 内容已在 main |
| `E:\qaqh-m0-authz` | `m0-daemon-authz` | +2 提交 | 合并（无冲突） |

## 3. 合并顺序（新语义优先、旧基线最后）

1. **先落 main 自身的未提交改动**（3 个提交）：OHOS rustls ring provider、
   Ringing v2 回执因果链修复、文档 + lockfile。不先清干净，后续 `merge` 会被拒。
2. **`m0-daemon-authz`**（较新基线 `56a8e40`）→ 无冲突，merge commit `6dd2de0`。
3. **`feat/workspace-audit-pr3`**（基线 `669b4cb`）→ 解 3 处冲突，`587adb7`。
4. **`addin-winsandbox`**（**更早**基线 `7f449b8`）→ 最后合并，`c0a9e5c`。
5. 收尾移植 workspace-audit 未提交内容（e2e example + gate 断言），
   `6a338c4` / `8e5b461`。

## 4. 删除语义如何保住

关键前提：`7f449b8` 是 main 的祖先，main 后续的删除提交都在它之上：

- `065f35a` 删 v1 领域事件变体与死访问器；
- `3dc3af6` 删 `index.json` 与 `timeline-v3` 两项 migrate-on-read；
- `c9c77e7` 砍 DeepX 数据根 marker 启动期改写。

沙箱改动的 42 个文件与 main 自其基线以来**改动/删除的文件零交集**，
因此普通合并在结构上不可能把已删内容带回。合并后回潮探测：

- DeepX marker 改写：无匹配；
- `index.json` / `timeline-v3` migrate-on-read：无匹配（只剩 `index.jsonl`）；
- `LegacyToolAdapter` / `PreparedExecutor` / `install_tool_call_context`：无匹配。

## 5. 冲突解算记录

| 文件 | 冲突 | 裁决 |
|---|---|---|
| `crates/qaqh-spy/src/lib.rs` | 导出二选一 | 取并集 `{Change, ChangeStatus, GcOutcome}` |
| `crates/qaqh-runtime/tests/workspace_change_injection.rs` | 旧工具 API vs `tool` 参数化 | 保留 main 新 API（`ProbeTool`、三档制 level 3）+ incoming 参数化；新增探针迁到 `ProbeTool` |
| `docs/plan-workspace-diff-injection.md` | 进度段落各写一版 | 合并双方结论（PR3 已完成 + 回滚防护收口 + 配置项待办） |

## 6. 验证

- `cargo check --workspace --all-targets` → exit 0；
- `workspace_change_injection` 3/3、`qaqh-spy` 17 + 1、`tool_ledger` 20、
  `input_accepted_producer` 1、`interaction_request_ledger` 1 → 全过。

## 7. 备份

合并前为 6 个 worktree 各建 HEAD 与脏状态快照：
`refs/backup/pre-merge-20261006/*`（共 12 个 ref）。