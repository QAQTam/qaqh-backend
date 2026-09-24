# P4 Policy 词汇抽离与 alpha1 版本回调 Handoff

日期：2026-09-23

## 完成

### 版本回调

- `version.txt`、`Cargo.toml`、`Cargo.lock`、`package.json`、`qaqh-backend.lock.json` 统一为 `2.0.0-alpha1`。
- `QAQH_UA_VERSION` / `QAQH_USER_AGENT` 按既有发布纪律保留主版本 `2.0.0`，不带预发布后缀。
- 发布就绪报告改为 alpha1 预发布口径。

### qaqh-policy 抽离

新增 `qaqh-policy`：

- `ToolCategory`
- `PermissionRisk`
- `PermissionLevel`
- `PermissionDecision`
- `SandboxBackend` / `NetworkPolicy` / `SandboxSpec`

边界：

- `qaqh-policy` 不依赖 workspace、sandbox、runtime 或平台探测。
- `qaqh-sandbox` 改为消费并 re-export policy 的 sandbox 类型。
- `qaqh-workspace::permission` 保留路径解析、trusted-folder、`needs_permission()` 适配层，并 re-export policy 核心类型，避免一次性改动全部调用方。
- `ToolCallContext::sandbox_spec()` 作为显式上下文到 canonical sandbox policy 的桥；runtime intent hash 与 exec handler 共用该入口。

## 验证

```text
cargo test -p qaqh-policy -- --test-threads=1 PASS
cargo test -p qaqh-workspace permission:: -- --test-threads=1 PASS
cargo test -p qaqh-workspace --lib tool_api::context::tests::context_is_constructible_with_explicit_fields PASS
cargo test -p qaqh-sandbox -- --test-threads=1 PASS
cargo check --workspace --all-targets PASS
```

## 未完成

- `needs_permission()`、路径 helper、TrustedFolderSet 仍在 `qaqh-workspace`，完整规则引擎尚未迁入 policy。
- `ToolCallContext` 仍是 `sandbox_spec()` 派生入口，不是显式字段；后续可把解析后的 spec 作为字段传入。
- Linux 读白名单、`.git/hooks` 只读、namespace/cgroup 后端仍未落地。
