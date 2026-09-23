# P3-7 SessionActor CAS 与 Recovery Provenance Handoff

> 日期：2026-09-23
> 状态：P3-7 第一阶段与 recovery executor/outbox 对账已完成，已在本地分支验证；继续挂在 PR #288 的 P3-6/P3-7 交付线上
> Base：`betav2`
> Branch：`feat/p3-tool-ledger-production-wiring`

## 1. 本次范围

本切片收口 P3-7 的四个高风险窗口：

1. resume/terminal/cancel 与 `ToolIntent` 的串行 CAS。
2. interaction 生命周期 canonical 化：`InteractionRequested` →
   `InteractionResolved` / `InteractionExpired` 的 first-answer-wins。
3. cancel 时 `InteractionExpired { reason=turn_cancelled }` →
   `ToolFinished::Cancelled` 的顺序提交，包括显式 `ConversationCancel`。
4. crash recovery 补写 `ToolFinished::Indeterminate` 时的 canonical
   `recovery_ref` provenance 与 batch primitive。

本切片后续追加完成：

- Reconcile probe 的 durable evidence 闭合 primitive。
- recovery executor：读取 `RecoveryIntent`、校验 open set、seal `NoReplay`、
  在无 pending disposition 后写唯一 `SessionRecovered` 并清理 stale intent。
- `tool_outbox` 与 canonical ToolLedger 的只读双写对账观测。
- todo typed output 的 model/display/service 同源 gate。

仍未完成：

- typed output 剩余迁移（plan/process/subagent）。
- `tool_outbox` 最终退场策略（当前只做对账观测，不删除旧写）。

## 2. SessionActor CAS

`crates/qaqh-session/src/actor.rs` 新增：

- `ToolAdmission` / `ToolAdmissionError`。
- `SessionActor::admit_tool_intent`：
  - 在同一个 `&mut SessionActor` 临界区内检查 active/terminal turn；
  - suspended turn 先执行 `Resume`，再 append durable `ToolIntent`；
  - append 失败时恢复原 turn state，不留下半提交的 actor 状态；
  - 已有终态返回 `TurnTerminal` / `ExistingFinished`，不再次 admit；
  - 同一 intent 重复提交返回 `ExistingIntent`。
- `SessionActor::cancel_tool_batch`：
  - 先对 pending interaction 写唯一
    `InteractionExpired { reason=turn_cancelled }`；
  - 对未开始、无 intent 的 call 写 executionless `Cancelled`；
  - 已存在 intent 的 call 保持 open，交给 recovery 判 `Indeterminate`；
  - terminal 写完后才把 turn 置为 `Cancelled`，append 失败可重试。

actor state 使用 wire turn id，canonical fact 使用 `turn_{ULID}`；两者在
CAS API 中显式分开，避免既有 `t1`/wire id 与 canonical validation 冲突。

## 3. Runtime 接线

`crates/qaqh-runtime/src/agent/turn_actor.rs`：

- resume admission 必须携带已接受/合法的 interaction id；
- actor 仍为 pending 的 interaction 拒绝 resume；
- legacy/recovery 下 actor 不认识的 interaction 继续由 ToolEngine 校验；
- `cancel_tool_batch` 成功后清空 pending interaction 内存集合。

`crates/qaqh-runtime/src/agent/tool_runtime.rs`：

- 新增 `ToolBatchOrigin::{Normal, Resume}`；
- 新 `ToolIntent` 通过 `TurnActor::admit_tool_intent` 提交；
- normal run 与 approved-resume run 使用同一 ToolRuntime 执行边界；
- 取消收尾通过 actor 批量写 executionless `Cancelled`；
- UI 直调路径保持 `actor=None`，不改变既有快捷工具语义。

`crates/qaqh-runtime/src/agent/engine_turn.rs`：

- `YieldToUser` 为 pending permission/ask/plan 写 canonical
  `InteractionRequested`；
- permission/ask/plan 决策在 handler 执行前写 canonical
  `InteractionResolved`；
- 显式 `ConversationCancel` 改走 `cancel_with_ledger`，在 actor 临界区内
  写 interaction expiry 与 call cancellation。

## 4. Recovery Provenance

`crates/qaqh-session/src/canonical/tool_ledger.rs` 新增：

- interaction lifecycle index：
  - `append_interaction_requested` /
    `append_interaction_resolved` / `append_interaction_expired`；
  - 同一 interaction 只允许一个 request 和一个 resolved/expired 终态；
  - 重复同语义 request/terminal 幂等，冲突 fail-closed；
  - reopen 后从 committed facts 重建 index。
- `ToolLedger::seal_recovery_intent`：
  - `NoReplay` open intent → 写唯一
    `ToolFinished::Indeterminate { recovery_ref: Some(batch_ref) }`；
  - `IdempotentReplay` → 返回 `ReplayAllowed`，保持 open；
  - `Reconcile` → 返回 `ReconcileRequired`，保持 open；
  - 重复 recovery run 幂等，不产生第二个终态。
- `ToolLedger::recover_open_intents`：
  - 按 canonical call-id 顺序处理全部 open intent；
  - 使用同一 batch `RecoveryRef`；
  - 只 seal `NoReplay`，返回 replay/reconcile disposition 给后续步骤。

## 5. 验证

已执行并通过：

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

新增/更新回归：

- `crates/qaqh-session/tests/session_actor.rs`
  - suspend → resume admission 成功并提交 intent；
  - 已有 terminal 不重复写 intent；
  - cancel batch 先写 interaction expiry，再写 executionless Cancelled；
  - cancel 先于 resume 时返回 terminal 且不写 intent。
- `crates/qaqh-runtime/src/agent/turn_actor.rs` unit tests
  - resume admission 必须有 interaction resolution；
  - cancel-before-resume 不进入 handler；
  - pending interaction cancel 写 `InteractionExpired(turn_cancelled)`。
- `crates/qaqh-runtime/tests/interaction_request_ledger.rs`
  - `YieldToUser` 写 canonical `InteractionRequested`；
  - resolution 继承 request 的 turn/call envelope；
  - `InteractionResolved` 在 durable facts 中可见。
- `crates/qaqh-session/tests/tool_ledger.rs`
  - interaction request/terminal first-answer-wins 且 reopen 可重建；
  - recovery seal 携带 `RecoveryRef` 且重复执行幂等；
  - recovery batch 只 seal `NoReplay`，保留 replay/reconcile。
- `crates/qaqh-session/tests/recovery_executor.rs`
  - `NoReplay` 批量 seal 后写唯一 `SessionRecovered`；
  - replay/reconcile pending 时不提前写 `SessionRecovered`；
  - reconcile probe mismatch fail-closed；
  - 未列入 recovery plan 的 open intent fail-closed；
  - stale intent 清理不产生第二个 `SessionRecovered`。
- `crates/qaqh-runtime/src/agent/state/lifecycle.rs` unit test
  - session resume 前自动发现 open intent 并完成 canonical recovery。
- `crates/qaqh-runtime/src/agent/tool_outbox.rs` unit test
  - outbox 与 canonical ledger 的 matched/missing/status mismatch 对账。
- `crates/qaqh-runtime/tests/tool_crash_recovery.rs`
  - 真实子进程在 `ToolIntent` 后崩溃，恢复后唯一 `Indeterminate`，不重跑；
  - 真实子进程在 handler 副作用后、`ToolFinished` 前崩溃，恢复后唯一
    `Indeterminate`，副作用标记保留且不重放。
- `crates/qaqh-runtime/tests/tool_output_projection_equivalence.rs`
  - todo typed output 的 model/display/service 同源。
- `crates/qaqh-workspace/tests/tool_sdk_parity.rs`
  - 19 个内置工具 descriptor 与 capability 表逐项一致，动态工具走默认回退。
- `crates/qaqh-workspace/tests/skills_typed_output.rs`
  - `skills` typed activation 返回同一 output 派生的 model/display；
  - skill activation 作为可信 `ToolEffect` 进入宿主，不再从文本回解析。

## 6. 下一步

1. 迁移 plan/process/subagent 到 typed output，并删除对应 JSON
   字符串错误路径。
2. 在 outbox 对账观测稳定后设计并执行旧 `tool_outbox` 退场，保留 canonical
   ToolLedger 作为唯一终态事实源。
3. 收口 display/model/resource/service 的全工具同源验收，完成 P3 gate。
