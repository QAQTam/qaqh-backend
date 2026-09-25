# Subagent V2 Phase 2 Mailbox Handoff

> 日期：2026-09-25
> 基线：`3c4f666`（main）
> 实现分支：`feat/subv2-05-mailbox-20260925`
> 当前提交：`9204b33`
> 状态：canonical mailbox 核心完成，PR #387 待审
> 范围：`qaqh-session` 的 canonical fact、validation、projection 与测试

## 1. 本次结论

Phase 1（AgentPath / AgentCatalog / AgentGraph / canonical producer）已经通过
#384、#385、#386 合并到 main。本次开始 Phase 2，先完成 SUBV2-05 的 mailbox 核心：

```text
InterAgentCommunication
  -> canonical FactPayload
  -> MailboxProjection
  -> MailboxDelta / ProjectionSlot::Mailbox
```

本 PR 不实现工具面，也不把 legacy result injection 当作 V2 delivery。

## 2. 已落地

### 2.1 Canonical 类型

新增：

```text
MessageId
InterAgentDelivery
InterAgentContent
InterAgentCommunication
```

`InterAgentCommunication` 当前形状：

```text
message_id
root_session_id
author
recipient
other_recipients
task_id?
content: inline | content_ref
reply_to?
causation_id?
delivery: queue | trigger | interrupt
created_at_ms
```

delivery policy：

- `Queue`：只进 mailbox，不触发 idle turn；
- `Trigger`：idle 时允许触发 turn；
- `Interrupt`：中断当前 turn，agent 身份保留。

### 2.2 Mailbox projection

新增：

```text
crates/qaqh-session/src/projection/mailbox.rs
```

行为：

- `InterAgentCommunication` 首次出现 -> `Queued`；
- `InputAccepted.client_request_id == message_id` -> `Delivered`；
- 同一 `message_id` 重复出现幂等；
- `pending_for(path)` 支持 primary / other recipients；
- `last_activity_fact_seq` 为后续 `wait_agent` 提供 mailbox activity 水位。

“消息接受不等于模型已读”仍是后续工具/运行时层的语义约束；当前 projection 只记录
canonical communication 与匹配的 InputAccepted。

### 2.3 Projection 接线

新增：

```text
ProjectionSlot::Mailbox = 5
MailboxDelta::Queued
MailboxDelta::Delivered
```

`InputAccepted` 现在同时参与 `Conversation`、`Timeline`、`Mailbox` 三个 slot 的 reducer；
只有确实匹配 queued communication 时才产出 mailbox delta。

`ProjectionSetSnapshot.mailbox` 带 `#[serde(default)]`，旧 snapshot 缺字段时可回落到空 mailbox。

### 2.4 校验

新增校验：

- `message_id` / `reply_to` 必须是 `msg_` + ULID；
- `root_session_id` 必须是 UUIDv7；
- author / recipient / other recipients 必须是 `/root` namespace；
- recipients 去重；
- inline / content_ref 二选一；
- inline 正文非空且有 8 KiB 上限；
- `task_id` 非空且有 256 byte 上限；
- `causation_id` 必须是 ULID；
- `created_at_ms` 必须为正。

## 3. 验收证据

已通过：

```text
cargo test --workspace --offline -- --test-threads=1
cargo clippy --workspace --all-targets --offline -- -D warnings
cargo fmt --all -- --check
git diff --check
```

新增/更新测试：

```text
crates/qaqh-session/tests/mailbox_projection.rs
crates/qaqh-session/tests/session_fact_v2.rs
crates/qaqh-session/tests/projection_replay.rs
crates/qaqh-session/tests/projection_set.rs
crates/qaqh-session/tests/projection_slots.rs
```

覆盖：

- communication fact roundtrip / golden fixture；
- queue -> matching InputAccepted -> delivered；
- duplicate message id 幂等；
- primary / other recipient filtering；
- delivery policy；
- `ProjectionSlot::Mailbox` 与 reliable event；
- 非法 namespace / 空正文拒绝。

## 4. 当前 PR

```text
#387 feat(subagent-v2): 增加 canonical mailbox 与 delivery 策略（#372）
base: main
head: feat/subv2-05-mailbox-20260925
state: open / mergeable
```

范围边界：

- `spawn_agent` initial message 走 mailbox：留给 #373；
- `send_message` / `followup_task` / `wait_agent` / `interrupt_agent`：留给 #374；
- residency reload / parent ownership：留给 #375；
- 前端 Team projection / inbox：留给 #377。

## 5. 下一步

```text
#387 审核合并
  -> #373 spawn_agent initial message 改为 InterAgentCommunication
  -> #374 send_message / followup_task / wait_agent / interrupt_agent
  -> #375 residency reload + parent ownership
  -> #376 list_agents path prefix / status snapshot
  -> #377 TeamSnapshot/TeamDelta + TUI roster/inbox
```

并行注意：

- BETA-01 `#381` 仍是 beta 硬门禁，优先于长期 Team 协作层扩展；
- CNB 自动 PR 预审已经移除，NPC review 只能显式触发，不能把 CNB check 当作唯一审批凭据。

## 6. 接手注意事项

- 当前 `other_recipients` 只作为 communication metadata / projection filter；
  多收件人 fan-out 还没有在 runtime delivery 层实现。
- `MailboxProjection` 是 canonical projection，不是实际投递器；不要把 projection
  成功误判为 task 已发送或模型已读。
- 后续实际投递时，目标 session canonical log 应先写 `InterAgentCommunication`，
  再在进入模型输入时写带相同 `message_id` 的 `InputAccepted.client_request_id`。
- `Queue` 不得启动 idle agent；`Trigger` 才允许触发；`Interrupt` 只中断当前 turn。
- `MailboxDelta::Queued.message` 使用 `Box<MailboxMessage>` 仅为控制 enum 体积，
  serde wire shape 不因此改变。
- 新增 fact kind 时同步更新 `payloads.jsonl`、`projection_slots`、golden 数量断言和
  `ProjectionSet` snapshot 断言，否则 session contract 测试会失败。
