# Subagent V2 Agent Message Tools Handoff

> 日期：2026-09-25
> 基线：`88a78be`（`main`）
> 实现分支：`feat/subv2-agent-message-tools-20260925`
> 状态：实现完成，待 PR 审核
> 范围：`qaqh-subagent`、`qaqh-runtime`

## 1. 本次结论

#374 的第一组工具已接入：

```text
send_message   -> Queue delivery
followup_task  -> Trigger delivery
```

两者共用：

```text
AgentPath resolve
  -> canonical target metadata
  -> InterAgentEnvelope
  -> ConversationSendMessage
  -> target canonical communication + InputAccepted
```

## 2. 行为

### send_message

- 只投递到目标 mailbox；
- 不启动 idle agent；
- 对应 canonical `delivery = queue`。

### followup_task

- 投递到目标 mailbox；
- idle 时触发 turn；
- 对应 canonical `delivery = trigger`；
- child 默认不能 trigger root。

### 安全边界

- target 必须是 caller 同一 root tree；
- relative path 从 caller 向下解析；
- absolute path 不能跨 namespace；
- self message 拒绝；
- inline message 超过 8 KiB 拒绝；
- Interrupt 仍稳定拒绝，等待 `interrupt_agent` 切片。

## 3. 验收证据

```text
cargo test -p qaqh-subagent --offline -- --test-threads=1
cargo test -p qaqh-runtime --test host_direct --offline -- --test-threads=1
```

`host_direct` 覆盖：

- root 向 `/root/review_code` 发送 Queue message；
- child canonical log 写入 `InterAgentCommunication`；
- 匹配 `InputAccepted` 写入；
- `MailboxProjection` 收敛为 delivered，`pending_count == 0`。

## 4. 未决项

- `wait_agent` 未接入。
- `interrupt_agent` 未接入。
- completion result 仍走当前 injection，尚未切为 queue-only。
- 大正文 content_ref 外置未实现。

## 5. 接手注意事项

- Queue 不得启动 idle agent；Trigger 才允许。
- 发送工具只构造 envelope，canonical communication 由目标 session 写入。
- message id 必须贯穿 command 和 canonical communication。
