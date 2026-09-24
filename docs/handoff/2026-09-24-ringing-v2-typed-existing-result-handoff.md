# Ringing v2 typed existing-result handoff（2026-09-24）

状态：**P0-4 typed existing-result 第一刀已实现**，待 PR。承接
`docs/handoff/2026-09-24-ringing-v2-p0-3-p0-4-handoff.md` 的「下一步顺序 1」。

## 1. 本次完成

v2 command ack / command status 新增 typed「已有结果」载荷，v1 线协议零改动：

- `qaqh-ringing::v2` 新增（v1 类型未原地修改）：
  - `RingingV2CommandAck`：v1 ack 的 wire 超集，新增 `existing: Option<RingingV2ExistingCommand>`。
  - `RingingV2ExistingCommand`：`state` / `terminal_event_id` / `error_code` / `result`。
  - `RingingV2CommandResult`：`AskResolved { interaction_id, outcome }`、
    `PlanReviewResolved { interaction_id, approved }`。
  - `RingingV2AskOutcome`：`Answered` / `Dismissed`。
  - `RingingV2CommandStatus`：v1 status 的超集，新增 `result`。
  - `into_v1()` 投影，保证 v1 形状可无损取回。
- `PendingCommandStore` 新增 receipt `result` 字段（随 receipt 持久化），
  `observe_terminal_event` 从 canonical `InteractionResolved` /
  `PlanReviewResolved` 派生 typed result；新增
  `existing_receipt_for_session` / `v2_status_for_session`。
- daemon v2：
  - `POST /ringing/v2/commands/{channel}` 命中 TTL 内已有 `command_id` 时不再下发给
    worker，直接返回 `existing`（含 typed result）；指纹不一致仍 409
    `duplicate_command_mismatch`。
  - `GET /ringing/v2/commands/{command_id}` 返回带 `result` 的 v2 status。
  - v1/v2 共用 `command_fingerprint`，避免幂等判定在两个协议面漂移。
- `qaqh-client`（**纯增量，旧签名不变**）：
  - `send_command_v2_typed` → `ClientV2CommandAck`
  - `command_status_v2_typed` → `ClientV2CommandStatus`
  - 旧 `send_command_v2` / `command_status_v2` 保留并内部转 v1 形状，
    TUI pin 不动也能编译。

## 2. 验证证据

```text
cargo test --workspace -- --test-threads=1 PASS
cargo clippy --workspace --all-targets -- -D warnings PASS
cargo check -p qaqh-client --all-targets PASS
```

新增/更新的定点用例：

```text
qaqh-ringing  v2::types::tests::v2_command_ack_carries_typed_existing_result PASS
qaqh-ringing  v2::types::tests::v1_ack_body_deserializes_as_v2_ack_without_existing PASS
qaqh-ringing  v2::types::tests::v2_command_status_round_trips_typed_result PASS
qaqh-runtime  ringing::pending_store::tests::interaction_resolution_records_typed_result_for_replay PASS
qaqh-runtime  ringing::pending_store::tests::plan_review_resolution_records_typed_result PASS
qaqh-daemon   axum_tests::v2_command_replay_returns_typed_existing_result PASS
qaqh-daemon   axum_tests::v2_command_replay_with_other_payload_is_conflict PASS
qaqh-client   v2_public_api PASS
```

## 3. 关键发现：完整「已有结果」被 canonical decision 编码卡住

本刀覆盖的是**同一 `command_id` 重放**（ACK 丢失后重发）的已有结果。
「第二次回答 / 另一个 `command_id` 回答已解决的 interaction」这条路径
（V2-R4 first-answer-wins）**还不能**带 typed result，原因是：

- canonical `InteractionResolved.decision_ref` 是 `ContentRef`（**只有 hash**），
  control projection 把它投影成 `ContentValue::Ref`，**不含** `approved` /
  `answered` / `denied` 这类语义值。
- 因此从 canonical control snapshot 里读不出「已有的结果是什么」，只能拿到
  决策内容的 hash。
- worker 侧确实知道 decision（`PermissionDisposition::AlreadyResolved { decision }`、
  `ApprovalRegistry`），但 ack 在下发前就已返回，异步事件模型里 daemon
  拿不到该同步结果。

结论：要让「第二次回答」也带 typed existing result，必须先做
P0-4 的下一项——**permission/ask/plan 收口到同一 canonical interaction
registry**，让 canonical fact（或 registry 投影）直接携带结构化 decision，
而不是只存 `decision_ref` hash。这也是 `interaction_already_resolved`
从「只有 code/message」升级为 typed payload 的前提。

## 4. 仍未完成 / 下一步

1. canonical interaction registry：`InteractionResolved` 携带结构化 decision
   （或 registry 投影暴露 decision），permission 进入
   `InteractionRequested` 统一路径。
2. 基于 1，给「不同 `command_id` 回答已解决 interaction」的 v2 ack 也补
   `existing.result`，并补 V2-R4 first-answer-wins 端到端断言。
3. V2-R1..R4 fixture（permission / ask / plan reconnect + concurrent answers）。
4. 之后再进 P0-5 driver capability。

## 5. 接手注意

- 不要移动已有 v2 tag。
- 不要原地修改 v1 `RingingCommandAck` / `RingingCommandStatus`；v2 用超集类型。
- `existing` 只在 TTL 内的 receipt 上出现；指纹不一致必须仍 409。
- 新增 interaction 结果类型时，同步补 `qaqh-client` 别名与
  `v2_public_api` 编译断言。
