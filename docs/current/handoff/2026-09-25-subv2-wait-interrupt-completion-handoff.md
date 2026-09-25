# Subagent V2 Wait / Interrupt / Queue-only Completion Handoff

> 日期：2026-09-25
> 基线：`e650b38`（`main`）
> 工作方式：直接在 `main` 推进，不创建 worktree
> 状态：实现与定向验收完成
> 范围：`qaqh-subagent`、`qaqh-runtime`

## 1. 本次结论

Phase 2 最后三项工具/调度语义已接入：

```text
wait_agent       -> 等待 caller canonical mailbox activity
interrupt_agent  -> 只取消目标当前 turn，不删除 identity
child completion -> queue-only InterAgentCommunication 投递父 mailbox
```

V2 child completion 不再默认 `TriggerTurn` 父会话。父代理必须显式
`wait_agent`，或在下一 turn 处理 queue-only 结果。

## 2. 行为与边界

### wait_agent

- 只读 caller 的 `MailboxProjection.last_activity_fact_seq`；
- 初始水位之后出现新的 `InterAgentCommunication` 或匹配
  `InputAccepted` 才返回；
- 不返回正文；
- `timeout_ms` 范围 `1000..=3_600_000`，默认 `30000`；
- timeout 返回结构化输出 `{ message, timed_out: true }`；
- 每 25ms 轮询 committed canonical facts，并在轮询点观察工具取消。

### interrupt_agent

- target 必须是 caller 同一 root tree；
- root 和 self 稳定拒绝；
- loaded target 只发送 `ConversationCancel`，不调用 close；
- unloaded target 返回 `previous_status = "unloaded"`，不触发 reload；
- logical identity、AgentPath、canonical edge 全部保留；
- target 后续仍可接收 queue/trigger delivery。

### completion queue-only

- collector 使用 initial task 的 author/recipient 反向构造 completion route；
- completion command 携带 `inter_agent.delivery = Queue`；
- `input_purpose = QueueOnly`，`as_system = false`；
- `message_id` 同时用于 command、`InterAgentCommunication` 与匹配
  `InputAccepted`；
- `SubagentFinished` terminal notification 仍随命令写父 canonical log；
- 旧的无 route fallback 暂时保留 legacy system injection，待所有 caller 迁移后删除。

## 3. 验收证据

```text
cargo test -p qaqh-subagent --offline -- --test-threads=1
cargo test -p qaqh-runtime --test host_direct --offline -- --test-threads=1
cargo test -p qaqh-runtime --test input_accepted_producer --test session_lifecycle --offline -- --test-threads=1
cargo clippy -p qaqh-subagent -p qaqh-runtime --all-targets --offline -- -D warnings
```

覆盖：

- collector 产出 queue-only inter-agent completion；
- cancelled child 不注入结果正文，但保留 terminal notification；
- `wait_agent` timeout 稳定；
- child 发给父的 queue-only activity 能唤醒 `wait_agent`；
- interrupt 后 child 仍出现在 `list_agents`，并可继续接收消息；
- root/self interrupt fail closed。

## 4. 未决项

- `steer` / `interject` 仍是后续阶段；
- wait cursor 目前是单次工具调用水位，不持久化“已读”语义；
- queue-only completion 的 legacy fallback 仍需删除；
- residency/reload、parent ownership 与 child reload path 恢复属于 #375；
- 大正文 `content_ref` 外置未实现。

## 5. 接手注意事项

- completion 必须保持 queue-only，禁止恢复无界 TriggerTurn；
- `InterAgentCommunication` 必须先于匹配 `InputAccepted` 写入；
- wait 只返回活动事实，不得从正文 regex 或工具卡 JSON 推导状态；
- interrupt 不是 close，不得删除 canonical identity 或目录。
