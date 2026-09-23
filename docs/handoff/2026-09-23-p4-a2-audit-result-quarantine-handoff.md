# P4 A2：result-side audit quarantine Handoff

日期：2026-09-23

## 完成

- write/exec/net 的 result audit 写入失败后：
  - 写 `audit/quarantine.json`；
  - 写 `audit/emergency.jsonl`；
  - 返回 `AUDIT_QUARANTINED`；
  - runtime 将该终态映射为 canonical `ToolTerminalStatus::Indeterminate`；
  - 当前进程后续 write/exec/net 被拒绝，不进入 handler。
- 只读工具保持 fail-open：result audit 失败只记录日志，不改变读取结果。
- 新增测试：
  - `crates/qaqh-workspace/tests/audit_fail_closed.rs::audit_result_failure_quarantines_and_blocks_subsequent_high_risk_tools`
  - `crates/qaqh-runtime/src/agent/tool_runtime.rs::tests::audit_quarantine_maps_to_canonical_indeterminate`

## 验证

```text
cargo test -p qaqh-workspace --test audit_fail_closed -- --test-threads=1 PASS
cargo test -p qaqh-runtime --lib agent::tool_runtime::tests -- --test-threads=1 PASS
cargo test --workspace -- --test-threads=1 PASS
cargo clippy --workspace --all-targets -- -D warnings PASS
```

## 后续

- Linux sandbox 第一刀在同日后续 PR 落地。
- audit 留存/查询/签名与 approval amend rule 仍属 P4 后续。
