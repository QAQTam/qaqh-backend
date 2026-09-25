# Subagent V2 Canonical Input Producer Handoff

> 日期：2026-09-25
> 基线：`23faca2`（`main`）
> 实现分支：`feat/subv2-input-accepted-producer-20260925`
> 状态：实现完成，待 PR 审核
> 范围：`qaqh-session`、`qaqh-runtime`

## 1. 本次结论

Phase 2 mailbox 的第一个前置缺口已补齐：

```text
conversation accepted
  -> canonical InputAccepted
  -> projection Conversation / Timeline / Mailbox
```

同时 canonical ledger 已支持写入 `InterAgentCommunication`，供下一步 initial task
mailbox delivery 使用。

## 2. 已落地

### 2.1 ToolLedger

新增：

- `append_input_accepted()`；
- `append_inter_agent_communication()`；
- reopen 时从 canonical facts 重建两张幂等索引；
- 同 id 同 payload 幂等，同 id 冲突稳定拒绝。

### 2.2 Runtime conversation 边界

- `as_system=true` 输入在进入 injection 前写 `InputAccepted`；
- 普通用户输入在 compact/suspend 守卫通过后、进入 turn 前写；
- canonical append 失败时返回 `input_accept_append_failed`，不继续执行；
- ephemeral session 仍跳过 canonical fact；
- 正文超过 8 KiB 时暂不伪造 dangling `content_ref`，记录告警并跳过 canonical
  `InputAccepted`；大正文外置是后续独立切片。

## 3. 验收证据

```text
cargo test -p qaqh-session --test tool_ledger --offline -- --test-threads=1
cargo test -p qaqh-runtime --test input_accepted_producer --offline -- --test-threads=1
```

新增覆盖：

- InputAccepted / InterAgentCommunication append 幂等与冲突；
- ledger reopen 后索引可重建；
- 真实 Loop 执行 `SessionCreate -> QueueOnly input -> shutdown` 后，canonical log
  中存在且只存在一条 `InputAccepted`。

## 4. 未决项

- subagent initial task 还没有改用 `InterAgentCommunication`；需要扩展 delivery
  命令或宿主投递边界。
- 大正文尚未外置为 `content_ref`。
- `send_message` / `followup_task` / `wait_agent` / `interrupt_agent` 工具面未接入。
- mailbox projection 已能消费 `InterAgentCommunication + InputAccepted`，但尚未由
  真实 subagent delivery 驱动。

## 5. 接手注意事项

- `InputAccepted.client_request_id` 必须与对应 `InterAgentCommunication.message_id`
  一致，mailbox 才会从 queued 收敛为 delivered。
- 不得在 compact 拒绝、stale session 或 append 失败后继续执行输入。
- `InterAgentCommunication` 必须先于对应 `InputAccepted` 写入目标 session。
