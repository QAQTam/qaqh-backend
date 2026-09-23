# P4 显式 SandboxSpec 字段化 Handoff

日期：2026-09-23

## 完成

- `ToolCallContext` 新增 `sandbox_spec: qaqh_policy::SandboxSpec` 正式字段。
- `qaqh_workspace::tool_api` re-export `SandboxSpec`，外部调用方无需直接依赖 `qaqh-policy`。
- `ToolCallContext::sandbox_spec()` 改为返回显式字段的只读引用，不再按 `workspace_root` 临时推导。
- runtime intent hash 与 exec handler 继续通过同一访问器消费策略。
- 迁移全部生产、测试和辅助构造点；当前仍统一初始化为
  `SandboxSpec::workspace_write(workspace_root)`，因此行为保持不变。
- approval challenge 在规范化 `workspace_root` 时同步重建 `sandbox_spec`，避免显式字段与审批上下文分叉。
- 新增契约测试，证明显式 `SandboxSpec` 不会随 `workspace_root` 推导。

## 验证

```text
cargo test -p qaqh-workspace --lib -- --test-threads=1 PASS
cargo test -p qaqh-runtime --test cancel_keeps_tool_results \
  --test tool_ordering_contract \
  --test tool_ledger_wiring \
  --test tool_ledger_open_intent \
  --test tool_ledger_finish_failure -- --test-threads=1 PASS
cargo test --workspace -- --test-threads=1 PASS
cargo clippy --workspace --all-targets -- -D warnings PASS
cargo check --workspace --all-targets PASS
```

`cargo fmt --all -- --check` 仍只报告未改动的
`crates/qaqh-client/src/lib.rs` 与 `crates/qaqh-client/src/types.rs` 既有差异。

## 后续

- approval amend rule：sandbox denial 后的一次性升级审批。
- Linux 读隔离：`.git/hooks` 只读、私有 `/tmp`、bwrap 读白名单。
- cgroup v2 CPU/内存限制。
- 将路径、trusted-folder 与规则决策完整迁入 `qaqh-policy`。
