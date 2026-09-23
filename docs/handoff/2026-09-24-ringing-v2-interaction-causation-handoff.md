# Ringing v2 interaction causation Handoff（2026-09-24）

状态：**P0-4 第一刀已实现**，待 PR 合并。

## 1. 本次完成

- `InteractionResolved` canonical fact 支持 `causation_id = command_id`。
- v2 `send_command_v2` 默认生成 ULID `command_id`，满足 canonical `EventId`
  约束。
- permission / ask / plan resolution 都从命令派发路径透传 `command_id`。
- 重复 ask / plan 回答稳定返回 `interaction_already_resolved`，不再返回
  `interaction_not_found`。
- permission 的重复回答继续由 `ApprovalRegistry` 返回
  `interaction_already_resolved`，不会二次执行。
- bootstrap pending set 继续来自 canonical `ControlSnapshot`，并已在 r2 锚点
  对齐 TUI reducer 的 `ClientV2ControlState.interactions` 形状。

## 2. 验证

```text
cargo test -p qaqh-runtime --test permission_lifecycle --test ask_user_lifecycle --test plan_review_hook --test interaction_request_ledger -- --test-threads=1 PASS
cargo clippy -p qaqh-runtime -p qaqh-session -p qaqh-client --all-targets -- -D warnings PASS
```

`interaction_request_ledger` 新增断言：

```text
InteractionResolved.causation_id == submitted command_id
```

## 3. 仍未完成

- v2 command ack 携带“已有结果”的 typed payload；当前只保证稳定
  `interaction_already_resolved` code 和 message。
- permission request 进入 canonical `InteractionRequested` 的统一路径；
  当前 permission 仍主要由 `ApprovalRegistry` 管理，ask/plan 由
  TurnActor suspended state 管理。
- amend rule 与 sandbox denial escalation。
- full V2-R1..R4 Gate matrix。
- v2 command ack 与 reliable resolution event 的端到端断言。

## 4. 下一步

继续把 permission request/response 收口到同一 canonical interaction
registry，并给 v2 command ack 增加 typed existing-result 字段；随后补
V2-R1..R4 fixture。
