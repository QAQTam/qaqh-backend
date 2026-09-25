# Subagent V2 Agent State / Direct Input Handoff

> 日期：2026-09-26
> 基线：`a623fb6`（`main`）
> 工作方式：直接在 `main` 推进，不创建 worktree
> 状态：`list_agents` status/residency 与 parent-owned direct input gate 已完成
> 范围：`qaqh-subagent`、`qaqh-runtime`

## 1. 本次结论

`list_agents` 不再只返回静态 metadata：

```text
AgentCatalog metadata
  + canonical status facts
  + explicit registry residency
  + loaded activity overlay
  -> ListedAgent { status, residency, ... }
```

同时，parent-owned child 的 direct/app-server `ConversationSendMessage` 会在
registry 的 Ringing 命令咽喉处 fail closed，不能借由直连命令绕过 parent
ownership 或意外 reload child。

## 2. 已落地

### 2.1 `list_agents` status / residency

- 新增 `ListedAgentStatus`：
  - `pending_init`
  - `running`
  - `waiting_user`
  - `interrupted`
  - `completed`
  - `errored`
  - `shutdown`
  - `not_found`
- 新增 `ListedAgentResidency`：`loaded` / `unloaded`。
- `ListedAgent` 新增 `status` 与 `residency` 字段。
- unloaded agent 继续返回完整逻辑 metadata 和 `unloaded` residency。
- status 由 canonical facts 重建：
  - child log 的 `TurnStarted` / `TurnInterrupted` / interaction facts；
  - parent log 的 `SubagentFinished` 在 unloaded child 上作为终态；
  - loaded 时叠加当前 activity tracker 的 starting/working/waiting/disconnected。
- residency 使用 registry 内的显式生命周期表，在 spawn/reload 置 loaded、
  unload/close 置 unloaded；不从 `instances` worker table 猜测。

### 2.2 Parent-owned direct input gate

`AgentRegistry::send_ringing` 现在在加载或 reload 目标之前检查：

```text
target is a non-root logical child
AND command is ConversationSendMessage
AND inter_agent == None
-> reject
```

合法投递必须携带 canonical `InterAgentEnvelope`。因此：

- app-server/UI 直连 child 的用户消息被拒绝；
- 拒绝不会触发 unloaded child reload；
- root direct input 仍走原有路径；
- `send_message` / `followup_task` / initial task 的 inter-agent 投递不受影响。

## 3. 验收证据

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --offline -- -D warnings
cargo test --workspace --offline -- --test-threads=1
```

新增/更新覆盖：

- spawn 后 child 为 `loaded + pending_init`；
- close/idle unload 后仍可 list，且为 `unloaded`；
- Queue delivery reload 后恢复 `loaded`；
- unloaded child 的 parent `SubagentFinished` 收敛为 `completed + unloaded`；
- direct `ConversationSendMessage { inter_agent: None }` 稳定拒绝；
- 拒绝 direct input 后 child 保持 `unloaded`，没有被意外 reload。

## 4. 未决项

- `TeamDelta::AgentResidencyChanged` 仍没有运行时 producer；当前 list_agents
  residency 是 registry 显式状态，不是 durable canonical fact。
- unload 是运行时生命周期，不写 canonical log；daemon 重启后按无 resident
  worker 重建为 unloaded。
- depth 与 sender/outbound 配额仍未实现。
- 大正文 `content_ref` 外置仍未实现。
- TUI/WinUI 尚未消费 TeamSnapshot/TeamDelta。

## 5. 接手注意事项

- `unloaded != completed`：residency 变化不能覆盖 canonical terminal status。
- 不得从 `instances.contains_key` 或进程表推导 `list_agents` residency。
- child 投递必须保留 `InterAgentEnvelope`；不要为直连 UI 消息添加绕过参数。
- 后续若把 residency 升级为 canonical fact，必须同时提供重建、冲突校验与
  `AgentResidencyChanged` delta，不能只补写日志。
