# buglist（2026-09-17）— `exec` 在 Level 4 可写工作区外路径

> 状态口径：`open` / `fixed（工作区，待提交）` / `fixed @{commit}` / `verified` / `wontfix @{commit}` / `wontfix`。
>
> 来源：
> [`../todo/2026-09-17-buglist复核-checklist.md`](../todo/2026-09-17-buglist复核-checklist.md)
> 的 N-5；原安全审查 P0-2 的未闭合半条。
> 交接背景：
> [`../handoff/2026-09-17-buglist复核九批次执行-handoff.md`](../handoff/2026-09-17-buglist复核九批次执行-handoff.md)
> §5.1。

## 缺陷

| ID | 状态 | 项 |
|---|---|---|
| BUG-2026-09-17-08 | `wontfix @5c7dd4d`（**P0**） | N-5 的原问题是 `exec` 在 Level 4 自动批准后可写工作区外。权限语义重新定案后，Level 4 明确为危险 bypass，允许普通工具（含 Exec/Net）自动放行；不再把它当作“普通档位需要拦截”的缺陷。 |
| BUG-2026-09-18-01 | `open`（**P0，后续**） | Level 4 bypass 下的 `exec` 还没有系统调用级沙箱；等移植 Codex 沙箱能力后，确保删除/写入的是隔离视图而非真实磁盘。 |

## 证据链

```text
Level 1/2/3
  -> needs_permission(/mcp Exec/Net) => AskUser
  -> UI approval (L3) -> AuthorizedToolCall -> execute

Level 4 (explicit bypass)
  -> needs_permission => AutoApprove
  -> AuthorizedToolCall -> execute
  -> 待补：exec sandbox / virtual filesystem
```

关键位置：

- `crates/qaqh-workspace/src/permission.rs`
- `crates/qaqh-workspace/src/authorization.rs`
- `crates/qaqh-workspace/src/manager.rs`
- `crates/qaqh-workspace/src/safety.rs`
- `crates/qaqh-workspace/src/exec/handler.rs`

## 权限语义定案

- **Level 3（默认新档）**：工作区内 Write 放行；Exec/Net 进入审批；跨区 Write 按 trust folder。
- **Level 4**：显式危险 bypass，普通工具全部自动放行，包括 Exec/Net 和动态 MCP。
- `ask`、会话内 todo 操作保持无递归弹窗的既有例外。
- 子代理沙箱继续自动拒绝 Exec/Net，不提供审批通道。
- Level 4 的 `exec` 工作区外风险暂按已接受风险处理；sandbox 完成后重新关闭 `BUG-2026-09-18-01`。

## 验收

```bash
cargo test -p qaqh-workspace
cargo test -p qaqh-runtime --no-fail-fast
cargo test --workspace --no-fail-fast
cargo clippy --workspace --all-targets -- -D warnings
```

以上命令在当前工作区全部通过。权限语义提交：`5c7dd4d`；当前状态：N-5 `wontfix @5c7dd4d`，L4 sandbox 为 `open`。

补充（2026-09-18）：B-5 保留给 Level 3 的 Exec 审批。后端 `eef1231` 在
`ToolPermissionRequested` 增加可选 `action_summary`，并由 `PermissionChallenge::action_summary()`
为 `exec` 生成有界、无 `env` 的命令摘要；TUI `4a795b3` 渲染“执行:”行，`2ce1fec` 修复真实联调
发现的空会话崩溃。已用隔离 daemon + TUI + mock 模型在 Level 3 验证：命令可见、批准后真实执行。
