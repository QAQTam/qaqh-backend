# P4-2a ApprovalRegistry 第一刀 Handoff

日期：2026-09-23

## 完成

- 新增 `ApprovalRegistry`，把 approval 的 pending 与 resolved 状态分开：
  - 第一个有效 response 消费 pending；
  - 后续重复 response 返回稳定的 `ApprovalDecision`；
  - 同一 call key 重新插入时开启新的 interaction。
- `ToolEngine` 的 pending approval 改为由 registry 管理。
- `ToolPermissionRespond` 重复响应现在返回：
  - code：`interaction_already_resolved`
  - message：包含既有 decision（`approved` / `rejected` / `expired`）
- 重复响应不会生成第二个 `AuthorizedToolCall`，不会二次执行工具。
- 新增 registry 单测和真实 daemon/pipe 生命周期回归：
  - `crates/qaqh-runtime/src/agent/approval_registry.rs`
  - `crates/qaqh-runtime/tests/permission_lifecycle.rs::llm_duplicate_permission_response_is_stable_and_does_not_execute_twice`

## 语义边界

- 本刀解决的是 actor 内 first-answer-wins 与重复 response 稳定性。
- `PermissionChallenge::approve()` 原有的一次性 grant 语义不变。
- canonical `InteractionRequested/Resolved` 持久化、跨重启 replay 和
  amend rule 仍留给 P4-2b。
- `clear_pending()` 仍清理当前 pending；正常 TurnComplete 不清理 resolved
  tombstone，因此迟到 response 能稳定返回既有 decision。
- session switch / undo / abort 仍可清理 registry，避免旧 turn 的 response
  影响新会话。

## 验证

```text
cargo test -p qaqh-runtime approval_registry -- --test-threads=1 PASS
cargo test -p qaqh-runtime --test permission_lifecycle \
  llm_duplicate_permission_response_is_stable_and_does_not_execute_twice \
  -- --exact --test-threads=1 PASS
cargo clippy -p qaqh-runtime --all-targets -- -D warnings PASS
```

完整 workspace 门禁在提交前补跑。

## 后续

- P4-2b：把 permission/ask/plan 统一接入 canonical InteractionRegistry。
- P4-2c：amend rule 与 sandbox denial 后的一次性 escalation approval。
- P4-3：Linux 读隔离、`.git/hooks` 只读、私有 `/tmp`、cgroup 资源限制。
