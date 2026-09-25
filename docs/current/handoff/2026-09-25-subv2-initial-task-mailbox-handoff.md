# Subagent V2 Initial Task Mailbox Handoff

> 日期：2026-09-25
> 基线：`e0fea17`（`main`）
> 实现分支：`feat/subv2-initial-task-mailbox-20260925`
> 状态：实现完成，待 PR 审核
> 范围：`qaqh-domain`、`qaqh-subagent`、`qaqh-runtime`

## 1. 本次结论

`spawn_subagent` 的 initial task 已从裸 `ConversationSendMessage` 切换为
canonical inter-agent communication：

```text
host start_subagent
  -> InterAgentEnvelope
  -> target child canonical InterAgentCommunication
  -> matching InputAccepted(client_request_id = message_id)
  -> MailboxProjection queued -> delivered
```

任务正文仍是 `ConversationSendMessage.text`；envelope 只承载 canonical
author/recipient/delivery metadata。

## 2. 已落地

### 2.1 Domain wire

`ConversationSendMessage` 新增可选 `inter_agent: InterAgentEnvelope`：

- `message_id`
- `root_session_id`
- `author` / `recipient` / `other_recipients`
- `task_id` / `reply_to` / `causation_id`
- `delivery`
- `created_at_ms`

旧命令省略该字段，反序列化保持兼容。

### 2.2 目标 session 写入

目标 Loop 在输入接受边界：

1. 写 canonical `InterAgentCommunication`；
2. 写 `InputAccepted`，`client_request_id == message_id`；
3. `InputAccepted.actor.kind = subagent`，actor id 使用 author path；
4. Trigger 走普通 turn 输入；Queue 走 QueueOnly injection；
5. Interrupt 暂时稳定拒绝，留到后续 delivery 扩展。

### 2.3 Subagent start

`QaqhService::start_subagent` 从 child canonical metadata 构造 envelope：

- root 来自 `root_session_id`；
- author 来自 parent agent path；
- recipient 来自 child agent path；
- message id 使用 `msg_<ULID>`；
- delivery 固定为 Trigger。

collector 的 command message id 与 canonical communication message id 一致。

## 3. 验收证据

新增真实 Loop 测试覆盖：

- 普通输入 + inter-agent Queue 输入各写一条 `InputAccepted`；
- canonical log 中存在一条 `InterAgentCommunication`；
- inter-agent `InputAccepted.actor.kind == subagent`；
- `MailboxProjection` 状态从 queued 收敛为 delivered，`pending_count == 0`。

## 4. 未决项

- Interrupt delivery 尚未实现。
- `send_message` / `followup_task` / `wait_agent` / `interrupt_agent` 工具面未接入。
- 正文仍受 canonical inline 8 KiB 上限约束；超出时 initial task fail closed，
  等待 content_ref 外置 producer。
- completion 仍保留当前 result injection；Phase 2 后续需切为 queue-only mailbox。

## 5. 接手注意事项

- communication 必须先于匹配的 `InputAccepted` 写入目标 session。
- 两条 fact 的 message id 必须完全一致，否则 mailbox 不会标记 delivered。
- 不得重新用文本前缀或工具卡 JSON 推导 agent identity。
