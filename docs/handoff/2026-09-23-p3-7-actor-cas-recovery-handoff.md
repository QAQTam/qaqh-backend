# P3-7 SessionActor CAS 与 Recovery Provenance Handoff

> 日期：2026-09-23
> 状态：第一阶段实现完成，已在本地分支验证；继续挂在 PR #288 的 P3-6/P3-7 交付线上
> Base：`betav2`
> Branch：`feat/p3-tool-ledger-production-wiring`

## 1. 本次范围

本切片只收口 P3-7 的两个高风险窗口：

1. resume/terminal/cancel 与 `ToolIntent` 的串行 CAS。
2. crash recovery 补写 `ToolFinished::Indeterminate` 时的 canonical
   `recovery_ref` provenance。

未在本切片完成：

- canonical `InteractionResolved/InteractionExpired` 与 ToolFinished 的完整
  fact 顺序提交。
- Reconcile probe 的真正执行与 evidence 写入。
- recovery executor 的完整 batch driver。
- `tool_outbox` 与 canonical ToolLedger 的最终退场策略。

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

## 4. Recovery Provenance

`crates/qaqh-session/src/canonical/tool_ledger.rs` 新增：

- `ToolLedger::seal_recovery_intent`：
  - `NoReplay` open intent → 写唯一
    `ToolFinished::Indeterminate { recovery_ref: Some(batch_ref) }`；
  - `IdempotentReplay` → 返回 `ReplayAllowed`，保持 open；
  - `Reconcile` → 返回 `ReconcileRequired`，保持 open；
  - 重复 recovery run 幂等，不产生第二个终态。

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
  - cancel batch 写 executionless Cancelled；
  - cancel 先于 resume 时返回 terminal 且不写 intent。
- `crates/qaqh-runtime/src/agent/turn_actor.rs` unit tests
  - resume admission 必须有 interaction resolution；
  - cancel-before-resume 不进入 handler。
- `crates/qaqh-session/tests/tool_ledger.rs`
  - recovery seal 携带 `RecoveryRef` 且重复执行幂等。

## 6. 下一步

1. 将 `InteractionResolved/InteractionExpired` 与 `ToolFinished` 纳入同一
   canonical batch 顺序。
2. 实现 Reconcile probe，写 evidence 后再闭合 call。
3. 建 recovery executor：读取 `RecoveryIntent`，逐个调用
   `seal_recovery_intent`，最后写 `SessionRecovered`。
4. 完成 `tool_outbox` 与 canonical ledger 对账，逐步退场旧 outbox。
