# Handoff：剩余 TypedTool 迁移与 TUI 契约补充任务（2026-09-23）

> 状态：工具迁移继续挂在 PR #288；TUI 契约补充为独立并行任务，不混入本次工具迁移，P0 优先排期。
> 当前分支：`feat/p3-tool-ledger-production-wiring`
> 当前 head：`14fd7b7`
> 关联 PR：
> - #288：P3 ToolLedger / typed tool 迁移主线
> - #289：`ConversationInputPurpose` 独立 re-export PR，base `betav2`
>
> TUI 输入：
> [`../../../qaqh-tui-app/docs/spec/2026-09-23-TUI对后端的协作需求-spec.md`](../../../qaqh-tui-app/docs/spec/2026-09-23-TUI对后端的协作需求-spec.md)

## 1. 当前状态

### 1.1 已完成的 typed 工具

已迁移到 `TypedTool` 或 typed output 面：

- `todo_write` / `todo_update` / `todo_list`
- `skills`
- `plan.read` / `plan.action`（service 侧 typed output）
- `process`
- `spawn_subagent`
- `glob`
- `grep`
- `web_fetch`
- `read`

`read` 已完成：

- 批量/范围读取；
- `if_hash`；
- 文件账本基线；
- 行号修正；
- legacy error code；
- 错误 details 回填 wire `data`。

### 1.2 仍为 legacy executor 的内置工具

按当前 `registration.rs` 与各模块注册面统计：

| 工具 | 模块 | 当前状态 |
|---|---|---|
| `write` | `file_mutate.rs` | legacy |
| `delete` | `file_mutate.rs` | legacy |
| `edit` | `edit/handler.rs` | legacy |
| `apply_patch` | `apply_patch.rs` | legacy |
| `copy_range` | `copy_range.rs` | legacy |
| `exec` | `exec/register.rs` | legacy |
| `read_image` | `read_image/mod.rs` | legacy |
| `journal` | `journal.rs` | legacy |
| `ask` | `ask_user.rs` | legacy |
| `confirm_apply` | `confirm_apply.rs` | legacy |

`read` 已完成，因此原先的 `exec/read/write` 组现在剩：

- `exec`
- `write`

以及文件变更族和交互/资源族。

## 2. 后续工具迁移顺序

### Wave 1：文件写入基础族

目标：

- `write`
- `delete`

原因：

- 同属 `file_mutate.rs`；
- 共享 `expected_hash`、`file_state`、journal、diff 和 path guard；
- 先建立 typed file-mutation output，后续 `edit/apply_patch/copy_range` 可复用。

Gate：

- 注册项 `legacy.is_none()`；
- `dry_run` / append / overwrite / expected_hash 语义不变；
- `file_state` 与 journal 写入不变；
- model/display 同源；
- legacy error code 保持。

### Wave 2：文本编辑族

目标：

- `edit`
- `apply_patch`
- `copy_range`

原因：

- 三者都产生 diff、可能触发 partial failure、并写 file state / journal；
- 应共享 typed mutation result 和 display diff 语义；
- 先把 Wave 1 的 mutation output 定型，避免三套 adapter。

Gate：

- 失败 diff、partial application、ambiguous match、nearest match 均保持；
- `copy_range` 的 source/target 两侧账本语义不变；
- apply_patch dry-run / confirm_apply 链路不破；
- 错误码、错误 details、`ToolResult.diff` 保持。

### Wave 3：exec

目标：

- `exec`

原因：

- 输出面包含 foreground / backgrounded / timeout / cancelled；
- 与 `process` 已有 typed 输出，但 exec 还需要 shell/argv/schema/后台派生语义；
- 适合在文件变更族之后单独处理，避免和 mutation 输出耦合。

Gate：

- argv / command / shell 参数契约不变；
- `exit_code`、`process_id`、backgrounded、timeout、cancelled 终态不变；
- progress/cancel 仍走显式 runtime 路径；
- display 的 shell body 保持。

### Wave 4：图像读取

目标：

- `read_image`

原因：

- 该工具不仅返回文本，还要把图片附件送进 tool result；
- 迁移前必须先补 typed image projection / adapter：
  - `ToolContentBlock::Image`
  - `ToolOutcome.images`
  - model text 与 image parts 的边界
- 不应在 image adapter 未收口前直接把 `read_image` 改成 `TypedTool`。

Gate：

- `image_index` / `path` 两种来源；
- resize / MIME / size guard；
- image attachment 不进入文本 JSON 考古；
- provider-native image lowering 不回归。

### Wave 5：journal

目标：

- `journal`

原因：

- 有 `query` / `export` / `replay` 三种 action；
- 需要 typed output 表示 steps / patches / replay result；
- 需要显式 `ToolCallContext` 解析 replay `out` 和 session；
- 写风险较高，但边界清晰，适合放在文件族之后。

Gate：

- query/session/file/since 过滤不变；
- replay `at`、`out`、`exists=false` 语义不变；
- patch export 不丢；
- journal 仍不是 canonical session fact 的替代源。

### Wave 6：交互与收尾

目标：

- `ask`
- `confirm_apply`

原因：

- `ask` 依赖 interaction / suspension / resume 路径；
- `confirm_apply` 依赖 dry-run 与内存 pending 语义；
- 放在最后，避免和 P3 interaction ledger 的迁移同时改。

Gate：

- ask 答案校验、stale answer、cancel/dismiss 不回归；
- confirm_apply 的 pending id / dry-run / apply 语义不变；
- 与 canonical interaction lifecycle 一致。

## 3. 每波统一验收

每个 Wave 至少执行：

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace -- --test-threads=1
```

每个工具迁移必须验证：

1. 注册项不再携带 legacy executor；
2. descriptor / schema 保持模型面兼容；
3. model/display/canonical data 同源；
4. legacy error code 和结构化 details 保持；
5. `file_state` / journal / audit / permission 不回归；
6. 失败、取消、超时、partial 路径不被伪装为成功；
7. 不再新增文本 projector 或 JSON 考古路径。

## 4. TUI 契约补充任务

这是**独立并行任务**，不纳入 Wave 1-6 的工具迁移 PR；可立即启动，P0 优先于剩余工具迁移。

### 4.1 归属

由 backend/root agent 单独领，产出 backend 侧 plan/handoff，TUI spec 只作为输入。

### 4.2 范围

P0：

- 锚点 tag / 完整 SHA / daemon 构建证据；
- plan review 可控触发 hook；
- permission / ask 故障注入；
- SSE `lagged` / `stream_terminated`；
- timeline gap；
- session 404；
- command ack 挂起/超时。

P1：

- `QAQH_TEST_*` 集中登记；
- `qaqh-client` service typed 变体：
  - `session.meta`
  - `plan.read`
  - `plan.context_stats`
  - `stats.token_usage`
  - `git.*`
- typed todo 的 TUI 消费路径说明。

P2：

- P5 wire 大改的 wire 级说明、兼容窗口和时间点。

### 4.3 非目标

- 不为 TUI 视觉/交互改 wire；
- 不做 TUI 专用旁路接口；
- 不保证旧 backend rev 永远可编译；
- 不让 TUI 自建 client mirror；
- 不为 TUI 私有需求引入兼容层。

### 4.4 当前已先行的项

- `tui-anchor-2026-09-23`
  - `5ec1900d6c937b6ff927d8f65fcd37d465de7988`
  - daemon 构建已实测。
- PR #289
  - `ConversationInputPurpose` re-export；
  - 独立于 #288，base `betav2`。

## 5. 协作原则

- 公共 client 类型只从 `qaqh-client` 出；
- 测试 hook 实现留在 backend/daemon；
- TUI 只负责 PTY harness、fake provider、断言和测试编排；
- 任何临时兼容放在 backend adapter，不放在 TUI；
- TUI spec 中的需求先由 backend 吸收为正式 plan，再进入实现。

## 6. 下一步

1. 工具迁移 Wave 1-6 已完成并合入 `betav2`，见 §7。
2. 后续工作转入 typed 迁移后的 follow-up 收口，不再按 Wave 拆分。
3. TUI 契约补充任务保持独立：P0 hook 已随 #290 合入，TUI 侧消费任务继续按 `qaqh-tui-app#41` 跟踪。

## 7. 实施收口（2026-09-23）

### 7.1 已合入

| PR | 内容 | 合入提交 |
|---|---|---|
| #294 | `write` / `delete` typed 迁移 | `9e12de1` |
| #295 | `edit` / `apply_patch` / `copy_range` typed 迁移 | `2f5f373` |
| #296 | `exec` typed 迁移与真实终态映射 | `eb667ce` |
| #297 | `read_image` typed 迁移与 image attachment projection | `4cdcecb` |
| #298 | `journal` typed 迁移 | `f9defd1` |
| #299 | `ask` / `confirm_apply` typed 迁移 | `8e8f665` |

上述 PR 均通过 `cargo check --workspace --all-targets`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace -- --test-threads=1`。生产注册表已不再为这些内置工具保留 legacy executor；仅 `cfg(test)` 兼容入口仍存在。

### 7.2 迁移后 follow-up（不阻塞 2.0）

- `#288` 遗留：recovery failure fail-open、display `truncated` 与工具自身 flag 合并、`append_interaction_expired` 冲突分类、legacy WAL prune、两处 `ulid_from_text` 拼接、`tool_runtime.rs` 生产 `expect`、Windows hard_link 实测。
- `#290` 遗留：gap token eager consumption、`plan_review_enabled()` startup snapshot、`test_hooks` cfg gating、ack ms 上限、`SKIPPED` 非法值告警。
- TUI 侧：`QAQ-Harness/qaqh-tui-app#41`（P1）：`MODE=plan` e2e、permission/ask hang timeout 断言、迁移到 `PlanReviewItem`。
- 仓库级：`cargo fmt --all --check` 当前仍会在未改动的 `crates/qaqh-client/src/lib.rs`、`types.rs` 报既有 rustfmt 差异；如需恢复全仓 fmt gate，应单独开 PR 收敛，不混入工具迁移。
