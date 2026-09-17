# buglist（2026-09-17）— `exec` 在 Level 4 可写工作区外路径

> 状态口径：`open` / `fixed（工作区，待提交）` / `fixed @{commit}` / `verified` / `wontfix`。
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
| BUG-2026-09-17-08 | `fixed @cef3faa`（**P0**） | `exec` 在 Level 4 原先自动批准后可在工作区外写入。第二轮修复已选择权限层收口：`needs_permission` 保留 Level 4 的 Read/Write 免审批，但 Exec/Net 统一进入 `AskUser`；子代理沙箱继续拒绝 Exec/Net，MCP 动态 Exec/Net 同步执行该规则。 |

## 证据链

```text
ToolEngine::admit_batch
  -> qaqh_workspace::authorize_call
  -> admit
  -> needs_permission
       Level 4 + Exec/Net => AskUser
  -> UI approval required
  -> AuthorizedToolCall
  -> execute_authorized
  -> handle_run_exec
```

关键位置：

- `crates/qaqh-workspace/src/permission.rs:467`
- `crates/qaqh-workspace/src/authorization.rs:244`
- `crates/qaqh-workspace/src/manager.rs:544`
- `crates/qaqh-workspace/src/safety.rs:14`
- `crates/qaqh-workspace/src/exec/handler.rs:61`

## 收口方案

已采用**权限层收口**：

- Level 4 的 Read/Write 继续自动放行。
- Level 4 的 Exec/Net 与 Level 1/2/3 一样进入审批。
- `ask`、会话内 todo 操作保持无递归弹窗的既有例外。
- 子代理沙箱继续自动拒绝 Exec/Net，不提供审批通道。
- MCP 动态工具若声明为 Exec/Net，同样不能在 Level 4 绕过审批。

## 验收

```bash
cargo test -p qaqh-workspace
cargo test -p qaqh-runtime --no-fail-fast
cargo test --workspace --no-fail-fast
cargo clippy --workspace --all-targets -- -D warnings
```

以上命令在当前工作区全部通过。修复提交：`cef3faa`；当前状态为 `fixed @cef3faa`，待 PR 评审。
